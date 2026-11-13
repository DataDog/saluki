//! Datadog trace normalizer transform.
//!
//! Normalizes Datadog-origin trace chunks the way the trace-agent does before processing them. Repairable spans are
//! fixed in place and counted under `datadog.trace_agent.normalizer.spans_malformed`; chunks that can't be repaired
//! are dropped and counted under `datadog.trace_agent.normalizer.traces_dropped`. The counts are reported on the
//! `metrics` output.
//!
//! OTLP traces are normalized by the OTLP translator instead, with an OTLP-specific service fallback.

use std::sync::LazyLock;

use async_trait::async_trait;
use saluki_common::{collections::FastHashSet, strings::StringBuilder};
use saluki_core::accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_core::{
    components::{transforms::*, BuildContext},
    data_model::event::{
        trace::{AttributeValue, Span, Trace},
        EventType,
    },
    topology::{EventsBuffer, OutputDefinition},
};
use saluki_error::GenericError;
use stringtheory::MetaString;
use tokio::select;
use tracing::debug;

use self::telemetry::{DropReason, MalformedReason, NormalizerWindow, ReasonCounts, WINDOW};
use crate::common::traces::attribute_as_string;
use crate::common::traces::normalize::{
    is_normalized_tag_value, is_valid_status_code, needs_name_normalization, normalize_name_with_reason,
    normalize_peer_service_into, normalize_service_with_lang_into, normalize_tag_value_into_unchecked,
    truncate_utf8_str, validate_and_fix_duration, validate_and_fix_start_time, NormalizeError, PeerServiceChange,
    MAX_TYPE_LEN, YEAR_2000_NANOSEC_TS,
};

mod telemetry;

const PEER_SERVICE_KEY: &str = "peer.service";
const BASE_SERVICE_KEY: &str = "_dd.base_service";
const HTTP_STATUS_CODE_KEY: &str = "http.status_code";
const LINK_NAME_KEY: &str = "link.name";

/// Datadog trace normalizer configuration.
#[derive(Clone, Debug, Default)]
pub struct DatadogTraceNormalizerConfiguration {
    component2name: bool,
}

impl DatadogTraceNormalizerConfiguration {
    /// Creates a new `DatadogTraceNormalizerConfiguration` with the default settings.
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets whether a span's `component` replaces its name, as the `component2name` APM feature does.
    ///
    /// Defaults to `false`.
    pub fn with_component2name(mut self, enabled: bool) -> Self {
        self.component2name = enabled;
        self
    }
}

#[async_trait]
impl TransformBuilder for DatadogTraceNormalizerConfiguration {
    fn input_event_type(&self) -> EventType {
        EventType::Trace
    }

    fn outputs(&self) -> &[OutputDefinition<EventType>] {
        static OUTPUTS: LazyLock<Vec<OutputDefinition<EventType>>> = LazyLock::new(|| {
            vec![
                OutputDefinition::default_output(EventType::Trace),
                OutputDefinition::named_output("metrics", EventType::Metric),
            ]
        });
        &OUTPUTS
    }

    async fn build(&self, _context: BuildContext) -> Result<Box<dyn Transform + Send>, GenericError> {
        Ok(Box::new(DatadogTraceNormalizer::new(self.component2name)))
    }
}

impl MemoryBounds for DatadogTraceNormalizerConfiguration {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        builder
            .minimum()
            .with_single_value::<DatadogTraceNormalizer>("component struct");
        // The telemetry window starts empty and grows up to its key cap.
        builder.firm().with_map::<telemetry::TracerTags, ReasonCounts>(
            "normalizer telemetry window",
            telemetry::MAX_TAG_KEYS + 1,
        );
    }
}

/// The transform that normalizes Datadog trace chunks.
struct DatadogTraceNormalizer {
    component2name: bool,
    telemetry: NormalizerWindow,
    /// Scratch state reused across traces.
    seen_span_ids: FastHashSet<u64>,
    string_builder: StringBuilder,
}

impl DatadogTraceNormalizer {
    fn new(component2name: bool) -> Self {
        Self {
            component2name,
            telemetry: NormalizerWindow::default(),
            seen_span_ids: FastHashSet::default(),
            string_builder: StringBuilder::new(),
        }
    }

    /// Normalizes every trace in the buffer, removing the ones that can't be repaired.
    fn process_buffer(&mut self, events: &mut EventsBuffer) {
        events.retain_events::<Trace, _>(|trace| {
            // The trace-agent skips empty chunks before normalizing them, without counting them.
            if trace.spans().is_empty() {
                return false;
            }
            self.normalize_payload_env(trace);
            let mut counts = ReasonCounts::default();
            let result = self.normalize_trace(trace, &mut counts);
            self.telemetry.record(&trace.payload, &counts);
            result.is_ok()
        });
    }

    /// Normalizes the payload env as a tag value, as the trace-agent does, so samplers and stats downstream key on the
    /// normalized env.
    fn normalize_payload_env(&mut self, trace: &mut Trace) {
        let env = &trace.payload.env;
        if env.is_empty() || is_normalized_tag_value(env) {
            return;
        }
        normalize_tag_value_into_unchecked(env, &mut self.string_builder);
        trace.payload.env = MetaString::from(self.string_builder.as_str());
    }

    /// Normalizes a trace chunk.
    ///
    /// Returns the drop reason when the chunk is invalid beyond repair. Spans before the offending one have already
    /// been normalized and counted by then, as in the trace-agent.
    fn normalize_trace(&mut self, trace: &mut Trace, counts: &mut ReasonCounts) -> Result<(), DropReason> {
        if trace.spans().is_empty() {
            counts.drop(DropReason::EmptyTrace);
            return Err(DropReason::EmptyTrace);
        }

        // The trace ID is already a fixed 128-bit value here, so all 128 bits are checked.
        if trace.trace_id_high == 0 && trace.trace_id_low == 0 {
            counts.drop(DropReason::TraceIdZero);
            return Err(DropReason::TraceIdZero);
        }

        let lang = trace.payload.language_name.clone();
        self.seen_span_ids.clear();
        for span in trace.spans_mut() {
            normalize_span(span, &lang, self.component2name, &mut self.string_builder, counts)?;
            if !self.seen_span_ids.insert(span.span_id()) {
                counts.malformed(MalformedReason::DuplicateSpanId);
            }
        }

        Ok(())
    }
}

/// Normalizes a span.
///
/// Returns the drop reason when the span is invalid beyond repair.
fn normalize_span(
    span: &mut Span, lang: &str, component2name: bool, sb: &mut StringBuilder, counts: &mut ReasonCounts,
) -> Result<(), DropReason> {
    if span.span_id() == 0 {
        counts.drop(DropReason::SpanIdZero);
        return Err(DropReason::SpanIdZero);
    }

    match normalize_service_with_lang_into(span.service(), lang, sb) {
        Some(NormalizeError::Empty) => counts.malformed(MalformedReason::ServiceEmpty),
        Some(NormalizeError::TooLong) => counts.malformed(MalformedReason::ServiceTruncate),
        Some(NormalizeError::Invalid) => counts.malformed(MalformedReason::ServiceInvalid),
        None => {}
    }
    if sb.as_str() != span.service() {
        span.set_service(sb.as_str());
    }

    normalize_peer_service_attribute(
        span,
        PEER_SERVICE_KEY,
        sb,
        counts,
        MalformedReason::PeerServiceTruncate,
        MalformedReason::PeerServiceInvalid,
    );
    normalize_peer_service_attribute(
        span,
        BASE_SERVICE_KEY,
        sb,
        counts,
        MalformedReason::BaseServiceTruncate,
        MalformedReason::BaseServiceInvalid,
    );

    if component2name && !span.component.is_empty() {
        span.set_name(span.component.clone());
    }

    if needs_name_normalization(span.name()) {
        let (name, reason) = normalize_name_with_reason(MetaString::from(span.name()));
        match reason {
            Some(NormalizeError::Empty) => counts.malformed(MalformedReason::SpanNameEmpty),
            Some(NormalizeError::TooLong) => counts.malformed(MalformedReason::SpanNameTruncate),
            Some(NormalizeError::Invalid) => counts.malformed(MalformedReason::SpanNameInvalid),
            None => {}
        }
        span.set_name(name);
    }

    if span.resource().is_empty() {
        counts.malformed(MalformedReason::ResourceEmpty);
        let name = MetaString::from(span.name());
        span.set_resource(name);
    }

    if span.parent_id() == span.span_id() {
        span.set_parent_id(0);
    }

    let duration = validate_and_fix_duration(span.start(), span.duration());
    if duration != span.duration() {
        counts.malformed(MalformedReason::InvalidDuration);
        span.set_duration(duration);
    }
    if span.start() < YEAR_2000_NANOSEC_TS {
        counts.malformed(MalformedReason::InvalidStartDate);
        span.set_start(validate_and_fix_start_time(span.start(), span.duration()));
    }

    if span.span_type().len() > MAX_TYPE_LEN {
        counts.malformed(MalformedReason::TypeTruncate);
        let span_type = MetaString::from(truncate_utf8_str(span.span_type(), MAX_TYPE_LEN));
        span.set_span_type(span_type);
    }

    if !span.env.is_empty() && !is_normalized_tag_value(&span.env) {
        normalize_tag_value_into_unchecked(&span.env, sb);
        span.env = MetaString::from(sb.as_str());
    }

    if let Some(code) = span.attributes.get(HTTP_STATUS_CODE_KEY) {
        if !is_valid_status_code(&attribute_as_string(code)) {
            counts.malformed(MalformedReason::InvalidHttpStatusCode);
            span.attributes.remove(HTTP_STATUS_CODE_KEY);
        }
    }

    for link in span.span_links_mut() {
        let Some(value) = link.attributes().get(LINK_NAME_KEY) else {
            continue;
        };
        let (name, _) = normalize_name_with_reason(MetaString::from(attribute_as_string(value).as_ref()));
        link.attributes_mut()
            .insert(MetaString::from_static(LINK_NAME_KEY), AttributeValue::String(name));
    }

    Ok(())
}

/// Normalizes a `peer.service`-style string attribute in place, as the trace-agent does.
///
/// Only a non-empty string value is normalized, as the trace-agent only reads string attributes there. A value that
/// normalizes to nothing is kept as an empty string.
fn normalize_peer_service_attribute(
    span: &mut Span, key: &'static str, sb: &mut StringBuilder, counts: &mut ReasonCounts,
    truncate_reason: MalformedReason, invalid_reason: MalformedReason,
) {
    let Some(value) = span.attributes.get(key).and_then(AttributeValue::as_string) else {
        return;
    };
    if value.is_empty() {
        return;
    }
    match normalize_peer_service_into(value, sb) {
        PeerServiceChange::Unchanged => {}
        PeerServiceChange::Truncated => counts.malformed(truncate_reason),
        PeerServiceChange::Invalid => counts.malformed(invalid_reason),
    }
    if sb.as_str() != value.as_ref() {
        let normalized = AttributeValue::String(MetaString::from(sb.as_str()));
        span.attributes.insert(MetaString::from_static(key), normalized);
    }
}

#[async_trait]
impl Transform for DatadogTraceNormalizer {
    async fn run(mut self: Box<Self>, mut context: TransformContext) -> Result<(), GenericError> {
        let mut health = context.take_health_handle();
        health.mark_ready();
        debug!("Datadog trace normalizer transform started.");

        let mut window = tokio::time::interval_at(tokio::time::Instant::now() + WINDOW, WINDOW);

        loop {
            select! {
                _ = health.live() => continue,
                _ = window.tick() => {
                    let events = self.telemetry.take_window_events();
                    if !events.is_empty() {
                        context.dispatcher().buffered_named("metrics")?.send_all(events).await?;
                    }
                }
                maybe_events = context.events().next() => match maybe_events {
                    Some(mut events) => {
                        self.process_buffer(&mut events);
                        if !events.is_empty() {
                            context.dispatcher().buffered()?.send_all(events).await?;
                        }
                    }
                    None => {
                        // The input stream has ended, so report the final window before stopping.
                        let events = self.telemetry.take_window_events();
                        if !events.is_empty() {
                            context.dispatcher().buffered_named("metrics")?.send_all(events).await?;
                        }
                        break;
                    }
                }
            }
        }

        debug!("Datadog trace normalizer transform stopped.");
        Ok(())
    }
}

#[cfg(test)]
mod tests;
