//! Normalizer telemetry, emitted as metric events.
//!
//! This matches the trace-agent's normalizer counters, which it publishes as
//! `datadog.trace_agent.normalizer.traces_dropped` and `datadog.trace_agent.normalizer.spans_malformed` with a `reason`
//! tag, plus the tracer tags of the payload. As with the trace sampler, counts accumulate for one window and are
//! reported through the transform's metrics output, so the metrics reach the backend under their published names.

use std::time::Duration;

use libdd_trace_normalization::normalizer_v1::{DropReason, MalformedReason, NormalizeObserver};
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

/// The tracer tags a count is reported under, the subset of the trace-agent's tracer tags a trace carries.
#[derive(Clone, Debug, Default, Eq, Hash, PartialEq)]
pub(super) struct TracerTags {
    lang: MetaString,
    lang_version: MetaString,
    tracer_version: MetaString,
}

impl TracerTags {
    fn from_payload(payload: &PayloadFields) -> Self {
        Self {
            lang: payload.language_name.clone(),
            lang_version: payload.language_version.clone(),
            tracer_version: payload.tracer_version.clone(),
        }
    }

    /// Assembles the tag set with the given reason, omitting empty tags as the trace-agent does.
    fn tag_set(&self, reason: &str) -> TagSet {
        let mut tag_set = TagSet::with_capacity(4);
        for (name, value) in [
            ("lang", &self.lang),
            ("lang_version", &self.lang_version),
            ("tracer_version", &self.tracer_version),
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
            for reason in DropReason::ALL {
                let count = counts.dropped[drop_index(reason)];
                if count > 0 {
                    events.push(counter_event(
                        METRIC_TRACES_DROPPED,
                        tags.tag_set(reason.as_str()),
                        count,
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
    use saluki_core::data_model::event::metric::MetricValues;

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
            language_name: MetaString::from("python"),
            tracer_version: MetaString::from("2.0.0"),
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
            &["lang:python", "tracer_version:2.0.0", "reason:trace_id_zero"],
            1.0
        ));
        assert!(has_counter(
            &events,
            METRIC_SPANS_MALFORMED,
            &["lang:python", "tracer_version:2.0.0", "reason:service_empty"],
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
                tracer_version: MetaString::from(i.to_string()),
                ..PayloadFields::default()
            };
            window.record(&payload, &counts);
        }
        assert_eq!(window.counts.len(), MAX_TAG_KEYS + 1);
        assert_eq!(window.counts.get(&TracerTags::default()), Some(&counts));
    }
}
