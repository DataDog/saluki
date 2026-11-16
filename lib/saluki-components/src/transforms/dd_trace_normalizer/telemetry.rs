//! Normalizer telemetry, emitted as metric events.
//!
//! This matches the trace-agent's normalizer counters, which it publishes as
//! `datadog.trace_agent.normalizer.traces_dropped` and `datadog.trace_agent.normalizer.spans_malformed` with a `reason`
//! tag, plus the receiver tags of the payload. As with the trace sampler, counts accumulate for one window and are
//! reported through the transform's metrics output, so the metrics reach the backend under their published names.

use std::time::Duration;

use saluki_common::collections::FastHashMap;
use saluki_core::data_model::{
    event::{
        metric::{context::Context, Metric},
        trace::PayloadFields,
        Event,
    },
    tags::TagSet,
};
use stringtheory::MetaString;

/// How long a telemetry window accumulates counts before reporting.
pub(super) const WINDOW: Duration = Duration::from_secs(10);

/// Cap on distinct tracer tag combinations held in one window.
///
/// The tags come from tracer payloads, so this bounds the window against a client sending many distinct values.
/// Combinations beyond the cap count under the untagged key, so the window holds at most `MAX_TAG_KEYS + 1` keys.
pub(super) const MAX_TAG_KEYS: usize = 1_000;

const METRIC_TRACES_DROPPED: &str = "datadog.trace_agent.normalizer.traces_dropped";
const METRIC_SPANS_MALFORMED: &str = "datadog.trace_agent.normalizer.spans_malformed";

/// Why a trace chunk was dropped, with the trace-agent's `reason` tag values.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum DropReason {
    EmptyTrace,
    TraceIdZero,
    SpanIdZero,
}

impl DropReason {
    const ALL: [Self; 3] = [Self::EmptyTrace, Self::TraceIdZero, Self::SpanIdZero];

    /// Returns the `reason` tag value.
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::EmptyTrace => "empty_trace",
            Self::TraceIdZero => "trace_id_zero",
            Self::SpanIdZero => "span_id_zero",
        }
    }
}

/// Why a span was repaired, with the trace-agent's `reason` tag values.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum MalformedReason {
    DuplicateSpanId,
    ServiceEmpty,
    ServiceTruncate,
    ServiceInvalid,
    PeerServiceTruncate,
    PeerServiceInvalid,
    BaseServiceTruncate,
    BaseServiceInvalid,
    SpanNameEmpty,
    SpanNameTruncate,
    SpanNameInvalid,
    ResourceEmpty,
    TypeTruncate,
    InvalidStartDate,
    InvalidDuration,
    InvalidHttpStatusCode,
}

impl MalformedReason {
    const ALL: [Self; 16] = [
        Self::DuplicateSpanId,
        Self::ServiceEmpty,
        Self::ServiceTruncate,
        Self::ServiceInvalid,
        Self::PeerServiceTruncate,
        Self::PeerServiceInvalid,
        Self::BaseServiceTruncate,
        Self::BaseServiceInvalid,
        Self::SpanNameEmpty,
        Self::SpanNameTruncate,
        Self::SpanNameInvalid,
        Self::ResourceEmpty,
        Self::TypeTruncate,
        Self::InvalidStartDate,
        Self::InvalidDuration,
        Self::InvalidHttpStatusCode,
    ];

    /// Returns the `reason` tag value.
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::DuplicateSpanId => "duplicate_span_id",
            Self::ServiceEmpty => "service_empty",
            Self::ServiceTruncate => "service_truncate",
            Self::ServiceInvalid => "service_invalid",
            Self::PeerServiceTruncate => "peer_service_truncate",
            Self::PeerServiceInvalid => "peer_service_invalid",
            Self::BaseServiceTruncate => "base_service_truncate",
            Self::BaseServiceInvalid => "base_service_invalid",
            Self::SpanNameEmpty => "span_name_empty",
            Self::SpanNameTruncate => "span_name_truncate",
            Self::SpanNameInvalid => "span_name_invalid",
            Self::ResourceEmpty => "resource_empty",
            Self::TypeTruncate => "type_truncate",
            Self::InvalidStartDate => "invalid_start_date",
            Self::InvalidDuration => "invalid_duration",
            Self::InvalidHttpStatusCode => "invalid_http_status_code",
        }
    }
}

/// Per-reason counts of dropped traces and malformed spans.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct ReasonCounts {
    dropped: [u64; DropReason::ALL.len()],
    malformed: [u64; MalformedReason::ALL.len()],
}

impl ReasonCounts {
    pub(super) fn drop(&mut self, reason: DropReason) {
        self.dropped[reason as usize] += 1;
    }

    pub(super) fn malformed(&mut self, reason: MalformedReason) {
        self.malformed[reason as usize] += 1;
    }

    fn is_empty(&self) -> bool {
        self.dropped
            .iter()
            .chain(self.malformed.iter())
            .all(|count| *count == 0)
    }

    fn merge(&mut self, other: &Self) {
        for (total, count) in self.dropped.iter_mut().zip(other.dropped) {
            *total += count;
        }
        for (total, count) in self.malformed.iter_mut().zip(other.malformed) {
            *total += count;
        }
    }
}

/// The tags a count is reported under: the tags the receiver resolved for the payload.
///
/// As in the trace-agent, the language and tracer tags come from the request headers rather than the payload body.
#[derive(Clone, Debug, Default, Eq, Hash, PartialEq)]
pub(super) struct TracerTags {
    lang: MetaString,
    lang_version: MetaString,
    lang_vendor: MetaString,
    interpreter: MetaString,
    tracer_version: MetaString,
    endpoint_version: MetaString,
    connection_type: MetaString,
    service: MetaString,
}

impl TracerTags {
    fn from_payload(payload: &PayloadFields) -> Self {
        let receiver = payload.receiver();
        Self {
            lang: receiver.lang.clone(),
            lang_version: receiver.lang_version.clone(),
            lang_vendor: receiver.lang_vendor.clone(),
            interpreter: receiver.interpreter.clone(),
            tracer_version: receiver.tracer_version.clone(),
            endpoint_version: receiver.endpoint_version.clone(),
            connection_type: receiver.connection_type.clone(),
            service: receiver.service.clone(),
        }
    }

    /// Assembles the tag set with the given reason, omitting empty tags as the trace-agent does.
    fn tag_set(&self, reason: &str) -> TagSet {
        let mut tag_set = TagSet::with_capacity(9);
        for (name, value) in [
            ("lang", &self.lang),
            ("lang_version", &self.lang_version),
            ("lang_vendor", &self.lang_vendor),
            ("interpreter", &self.interpreter),
            ("tracer_version", &self.tracer_version),
            ("endpoint_version", &self.endpoint_version),
            ("connection_type", &self.connection_type),
            ("service", &self.service),
        ] {
            if !value.is_empty() {
                tag_set.insert_tag(MetaString::from(format!("{}:{}", name, value)));
            }
        }
        tag_set.insert_tag(MetaString::from(format!("reason:{}", reason)));
        tag_set
    }
}

/// Counts accumulated over one telemetry window, keyed by tracer tags.
#[derive(Default)]
pub(super) struct NormalizerWindow {
    counts: FastHashMap<TracerTags, ReasonCounts>,
}

impl NormalizerWindow {
    /// Adds one trace's counts under the tags of its payload.
    pub(super) fn record(&mut self, payload: &PayloadFields, counts: &ReasonCounts) {
        if counts.is_empty() {
            return;
        }
        let mut tags = TracerTags::from_payload(payload);
        if self.counts.len() >= MAX_TAG_KEYS && !self.counts.contains_key(&tags) {
            tags = TracerTags::default();
        }
        self.counts.entry(tags).or_default().merge(counts);
    }

    /// Reports every non-zero count as a delta counter, then resets the window.
    pub(super) fn take_window_events(&mut self) -> Vec<Event> {
        let mut events = Vec::new();
        for (tags, counts) in self.counts.drain() {
            for (reason, count) in DropReason::ALL.iter().zip(counts.dropped) {
                if count > 0 {
                    events.push(counter_event(
                        METRIC_TRACES_DROPPED,
                        tags.tag_set(reason.as_str()),
                        count,
                    ));
                }
            }
            for (reason, count) in MalformedReason::ALL.iter().zip(counts.malformed) {
                if count > 0 {
                    events.push(counter_event(
                        METRIC_SPANS_MALFORMED,
                        tags.tag_set(reason.as_str()),
                        count,
                    ));
                }
            }
        }
        events
    }
}

fn counter_event(name: &'static str, tags: TagSet, count: u64) -> Event {
    Event::Metric(Metric::counter(Context::from_parts(name, tags), count as f64))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use saluki_core::data_model::event::{metric::MetricValues, trace::ReceiverMetadata};

    use super::*;

    fn has_counter(events: &[Event], name: &str, tags: &[&str], value: f64) -> bool {
        events.iter().any(|event| {
            let Some(metric) = event.try_as_metric() else {
                return false;
            };
            metric.context().name() == name
                && metric.context().tags().len() == tags.len()
                && tags.iter().all(|tag| metric.context().tags().has_tag(tag))
                && *metric.values() == MetricValues::counter(value)
        })
    }

    #[test]
    fn reason_order_matches_discriminants() {
        // `ReasonCounts` indexes by discriminant, so `ALL` must list the variants in declaration order.
        for (i, reason) in DropReason::ALL.iter().enumerate() {
            assert_eq!(*reason as usize, i);
        }
        for (i, reason) in MalformedReason::ALL.iter().enumerate() {
            assert_eq!(*reason as usize, i);
        }
    }

    #[test]
    fn window_reports_non_zero_counts_with_tracer_tags() {
        let payload = PayloadFields {
            // The body's language and tracer version don't tag the counts: the request headers do.
            language_name: MetaString::from("body-lang"),
            tracer_version: MetaString::from("body-version"),
            receiver: Some(Arc::new(ReceiverMetadata {
                lang: MetaString::from("python"),
                tracer_version: MetaString::from("2.0.0"),
                service: MetaString::from("checkout"),
                interpreter: MetaString::from("CPython"),
                endpoint_version: MetaString::from("v0.4"),
                connection_type: MetaString::from("uds"),
                ..ReceiverMetadata::new()
            })),
            ..PayloadFields::default()
        };
        let mut counts = ReasonCounts::default();
        counts.drop(DropReason::TraceIdZero);
        counts.malformed(MalformedReason::ServiceEmpty);
        counts.malformed(MalformedReason::ServiceEmpty);

        let mut window = NormalizerWindow::default();
        window.record(&payload, &counts);
        window.record(&payload, &ReasonCounts::default());
        let events = window.take_window_events();

        assert_eq!(events.len(), 2, "{events:?}");
        assert!(has_counter(
            &events,
            METRIC_TRACES_DROPPED,
            &[
                "lang:python",
                "interpreter:CPython",
                "tracer_version:2.0.0",
                "endpoint_version:v0.4",
                "connection_type:uds",
                "service:checkout",
                "reason:trace_id_zero"
            ],
            1.0
        ));
        assert!(has_counter(
            &events,
            METRIC_SPANS_MALFORMED,
            &[
                "lang:python",
                "interpreter:CPython",
                "tracer_version:2.0.0",
                "endpoint_version:v0.4",
                "connection_type:uds",
                "service:checkout",
                "reason:service_empty"
            ],
            2.0
        ));
        assert!(window.take_window_events().is_empty(), "the window resets");
    }

    #[test]
    fn window_overflow_counts_untagged() {
        let mut counts = ReasonCounts::default();
        counts.malformed(MalformedReason::ResourceEmpty);

        let mut window = NormalizerWindow::default();
        for i in 0..=MAX_TAG_KEYS {
            let payload = PayloadFields {
                receiver: Some(Arc::new(ReceiverMetadata {
                    tracer_version: MetaString::from(i.to_string()),
                    ..ReceiverMetadata::new()
                })),
                ..PayloadFields::default()
            };
            window.record(&payload, &counts);
        }
        assert_eq!(window.counts.len(), MAX_TAG_KEYS + 1);
        assert_eq!(window.counts.get(&TracerTags::default()), Some(&counts));
    }
}
