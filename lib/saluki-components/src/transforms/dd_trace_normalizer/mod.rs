//! Datadog trace normalizer transform.
//!
//! Normalizes Datadog-origin trace chunks the way the trace-agent does before processing them. Repairable spans are
//! fixed in place and counted under `datadog.trace_agent.normalizer.spans_malformed`; chunks that can't be repaired
//! are dropped and counted under `datadog.trace_agent.normalizer.traces_dropped`. The counts are reported on the
//! `metrics` output.
//!
//! The normalization rules live in `libdd-trace-normalization`, which works on the trace in place through the
//! `libdd-trace-model` traits. This transform drops the empty chunks, normalizes the payload env, and keeps the
//! telemetry window.
//!
//! OTLP traces are normalized by the OTLP translator instead, with an OTLP-specific service fallback.

use std::sync::LazyLock;

use async_trait::async_trait;
use libdd_trace_normalization::normalize_utils::{is_normalized_tag_value, normalize_tag_value};
use libdd_trace_normalization::normalizer_v1::{
    count_duplicate_span_ids, normalize_chunk_with, DropReason, NormalizeOptions,
};
use saluki_common::collections::FastHashSet;
use saluki_core::accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_core::{
    components::{transforms::*, BuildContext},
    data_model::event::{trace::Trace, EventType},
    topology::{EventsBuffer, OutputDefinition},
};
use saluki_error::GenericError;
use stringtheory::MetaString;
use tokio::select;
use tracing::debug;

use self::telemetry::{NormalizerWindow, ReasonCounts, WINDOW};

mod telemetry;

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
}

impl DatadogTraceNormalizer {
    fn new(component2name: bool) -> Self {
        Self {
            component2name,
            telemetry: NormalizerWindow::default(),
            seen_span_ids: FastHashSet::default(),
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
    ///
    /// The span env goes through the same `libdd-trace-normalization` functions, so both get the same result.
    fn normalize_payload_env(&mut self, trace: &mut Trace) {
        let env = &trace.payload.env;
        if env.is_empty() || is_normalized_tag_value(env) {
            return;
        }
        let mut normalized = env.to_string();
        normalize_tag_value(&mut normalized);
        trace.payload.env = MetaString::from(normalized);
    }

    /// Normalizes a trace chunk with `libdd-trace-normalization`, as the trace-agent's `normalizeTraceChunkV1` does.
    ///
    /// Returns the drop reason when the chunk is invalid beyond repair. Spans before the offending one have already
    /// been normalized and counted by then, as in the trace-agent.
    fn normalize_trace(&mut self, trace: &mut Trace, counts: &mut ReasonCounts) -> Result<(), DropReason> {
        // The options borrow the language while the normalizer borrows the trace mutably. Taking the language out
        // for the call doesn't copy it.
        let lang = std::mem::take(&mut trace.payload.language_name);
        let opts = NormalizeOptions {
            lang: &lang,
            component2name: self.component2name,
        };
        let result = normalize_chunk_with(trace, &opts, counts);
        trace.payload.language_name = lang;

        // The trace-agent counts duplicate span IDs as it normalizes each span, so only the spans that it reached.
        if matches!(result, Ok(()) | Err(DropReason::SpanIdZero)) {
            count_duplicate_span_ids(trace, &mut self.seen_span_ids, counts);
        }
        if let Err(reason) = result {
            counts.drop(reason);
        }
        result
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
