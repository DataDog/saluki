//! Normalizer telemetry, emitted as metric events.
//!
//! This matches the trace-agent's normalizer counters, which it publishes as
//! `datadog.trace_agent.normalizer.traces_dropped` and `datadog.trace_agent.normalizer.spans_malformed` with a `reason`
//! tag, plus the receiver tags of the payload. As with the trace sampler, counts accumulate for one window and are
//! reported through the transform's metrics output, so the metrics reach the backend under their published names.

use std::time::{Duration, Instant};

use libdd_trace_normalization::normalizer_v1::{DropReason, MalformedReason, NormalizeObserver};
use saluki_common::collections::FastHashMap;
use saluki_core::data_model::{
    event::{
        metric::{context::Context, Metric, MetricMetadata, MetricValues},
        trace::PayloadFields,
        Event,
    },
    tags::TagSet,
};
use stringtheory::MetaString;

use crate::common::traces::telemetry_window_interval;

/// How long a telemetry window accumulates counts before reporting.
pub(super) const WINDOW: Duration = Duration::from_secs(10);

/// Cap on distinct tracer tag combinations held in one window.
///
/// The tags come from tracer payloads, so this bounds the window against a client sending many distinct values.
/// Combinations beyond the cap count under the untagged key, so the window holds at most `MAX_TAG_KEYS + 1` keys.
pub(super) const MAX_TAG_KEYS: usize = 1_000;

const METRIC_TRACES_DROPPED: &str = "datadog.trace_agent.normalizer.traces_dropped";
const METRIC_SPANS_MALFORMED: &str = "datadog.trace_agent.normalizer.spans_malformed";

/// Returns the index of `reason` in [`ReasonCounts`].
///
/// The match is exhaustive, so a reason that `libdd-trace-normalization` adds doesn't compile until it has an index.
const fn drop_index(reason: DropReason) -> usize {
    match reason {
        DropReason::EmptyTrace => 0,
        DropReason::TraceIdZero => 1,
        DropReason::SpanIdZero => 2,
    }
}

/// Returns the index of `reason` in [`ReasonCounts`]. See [`drop_index`].
const fn malformed_index(reason: MalformedReason) -> usize {
    match reason {
        MalformedReason::ServiceEmpty => 0,
        MalformedReason::ServiceTruncate => 1,
        MalformedReason::ServiceInvalid => 2,
        MalformedReason::PeerServiceTruncate => 3,
        MalformedReason::PeerServiceInvalid => 4,
        MalformedReason::BaseServiceTruncate => 5,
        MalformedReason::BaseServiceInvalid => 6,
        MalformedReason::SpanNameEmpty => 7,
        MalformedReason::SpanNameTruncate => 8,
        MalformedReason::SpanNameInvalid => 9,
        MalformedReason::ResourceEmpty => 10,
        MalformedReason::TypeTruncate => 11,
        MalformedReason::InvalidStartDate => 12,
        MalformedReason::InvalidDuration => 13,
        MalformedReason::InvalidHttpStatusCode => 14,
        MalformedReason::DuplicateSpanId => 15,
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
        self.dropped[drop_index(reason)] += 1;
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

impl NormalizeObserver for ReasonCounts {
    fn malformed(&mut self, reason: MalformedReason) {
        self.malformed[malformed_index(reason)] += 1;
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
pub(super) struct NormalizerWindow {
    started: Instant,
    counts: FastHashMap<TracerTags, ReasonCounts>,
}

impl Default for NormalizerWindow {
    fn default() -> Self {
        Self {
            started: Instant::now(),
            counts: FastHashMap::default(),
        }
    }
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

    /// Reports every non-zero count as a rate over the window, as DogStatsD flushes the trace-agent's counts, then
    /// resets the window.
    pub(super) fn take_window_events(&mut self) -> Vec<Event> {
        let interval = telemetry_window_interval(self.started);
        self.started = Instant::now();
        let mut events = Vec::new();
        for (tags, counts) in self.counts.drain() {
            for reason in DropReason::ALL {
                let count = counts.dropped[drop_index(reason)];
                if count > 0 {
                    events.push(counter_event(
                        METRIC_TRACES_DROPPED,
                        tags.tag_set(reason.as_str()),
                        count,
                        interval,
                    ));
                }
            }
            for reason in MalformedReason::ALL {
                let count = counts.malformed[malformed_index(reason)];
                if count > 0 {
                    events.push(counter_event(
                        METRIC_SPANS_MALFORMED,
                        tags.tag_set(reason.as_str()),
                        count,
                        interval,
                    ));
                }
            }
        }
        events
    }
}

fn counter_event(name: &'static str, tags: TagSet, count: u64, interval: Duration) -> Event {
    Event::Metric(Metric::from_parts(
        Context::from_parts(MetaString::from_static(name), tags),
        MetricValues::rate(count as f64, interval),
        MetricMetadata::default(),
    ))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use saluki_core::data_model::event::trace::ReceiverMetadata;

    use super::*;

    fn has_counter(events: &[Event], name: &str, tags: &[&str], value: f64) -> bool {
        events.iter().any(|event| {
            let Some(metric) = event.try_as_metric() else {
                return false;
            };
            metric.context().name() == name
                && metric.context().tags().len() == tags.len()
                && tags.iter().all(|tag| metric.context().tags().has_tag(tag))
                && *metric.values() == MetricValues::rate(value, Duration::from_secs(1))
        })
    }

    #[test]
    fn reason_indices_are_distinct() {
        // `ReasonCounts` holds one count for each reason, so each reason needs its own index.
        let mut dropped: Vec<_> = DropReason::ALL.into_iter().map(drop_index).collect();
        dropped.sort_unstable();
        assert_eq!(dropped, (0..DropReason::ALL.len()).collect::<Vec<_>>());
        let mut malformed: Vec<_> = MalformedReason::ALL.into_iter().map(malformed_index).collect();
        malformed.sort_unstable();
        assert_eq!(malformed, (0..MalformedReason::ALL.len()).collect::<Vec<_>>());
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
    fn counts_are_rates_over_the_measured_window() {
        let mut counts = ReasonCounts::default();
        counts.drop(DropReason::SpanIdZero);
        // As when a slow dispatch delays the window's report.
        let mut window = NormalizerWindow {
            started: Instant::now() - Duration::from_millis(13_400),
            ..NormalizerWindow::default()
        };
        window.record(&PayloadFields::default(), &counts);

        let events = window.take_window_events();
        assert_eq!(
            *events[0].try_as_metric().unwrap().values(),
            MetricValues::rate(1.0, Duration::from_secs(13))
        );
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
