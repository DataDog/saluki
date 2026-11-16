//! Receiver telemetry of the native trace intake.
//!
//! Mirrors the trace-agent's per-tag receiver stats (`ReceiverStats` and `TagStats` in `pkg/trace/info/stats.go`), which
//! `handleTracesV1` (`pkg/trace/api/api.go`) counts into and the receiver publishes as `datadog.trace_agent.receiver.*`,
//! and the `datadog.trace_agent.receiver.error` counts of `responses.go`. As with the trace normalizer and sampler,
//! counts accumulate for one window and are reported as metric events on the source's `metrics` output, so the metrics
//! reach the backend under the trace-agent's names. Only the native intake counts: the trace-agent counts the payloads
//! forwarded to it.
//!
//! `serve_traces_ms` is a histogram the trace-agent sends through DogStatsD, so it is reported as DogStatsD reports
//! histograms: as the configured aggregates and percentiles (`histogram_aggregates`, `histogram_percentiles`), and as a
//! distribution with `histogram_copy_to_distribution`.
//!
//! Unlike the trace-agent, which keeps every tag combination it has seen and keeps publishing zeros for idle ones, a
//! window only holds the combinations seen during it.

use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use ddsketch::DDSketch;
use saluki_common::collections::FastHashMap;
use saluki_core::data_model::event::metric::context::Context;
use saluki_core::data_model::event::metric::{Histogram, Metric, MetricMetadata, MetricValues, SampleRate};
use saluki_core::data_model::event::trace::ReceiverMetadata;
use saluki_core::data_model::event::Event;
use saluki_core::data_model::tags::TagSet;
use stringtheory::MetaString;

use crate::common::datadog::traces::telemetry_window_interval;
use crate::transforms::HistogramConfiguration;

/// How long a telemetry window accumulates counts before reporting.
pub(super) const WINDOW: Duration = Duration::from_secs(10);

/// Cap on distinct tag combinations held in one window.
///
/// The tags come from request headers and payloads, so this bounds the window against a client sending many distinct
/// values. Combinations beyond the cap count under the untagged key, so the window holds at most `MAX_TAG_KEYS + 1`
/// keys. The trace-agent caps its map at 10,000 combinations, but over its lifetime rather than per window.
pub(super) const MAX_TAG_KEYS: usize = 1_000;

const METRIC_TRACES_DROPPED: &str = "datadog.trace_agent.normalizer.traces_dropped";
const METRIC_SERVE_TRACES_MS: &str = "datadog.trace_agent.receiver.serve_traces_ms";
const METRIC_ERROR: &str = "datadog.trace_agent.receiver.error";

/// The tags receiver stats are kept under, the reference's `info.Tags`.
#[derive(Clone, Debug, Default, Eq, Hash, PartialEq)]
pub(super) struct ReceiverTags {
    pub lang: MetaString,
    pub lang_version: MetaString,
    pub lang_vendor: MetaString,
    pub interpreter: MetaString,
    pub tracer_version: MetaString,
    pub endpoint_version: MetaString,
    pub connection_type: MetaString,
    pub service: MetaString,
}

impl ReceiverTags {
    pub(super) fn from_receiver(receiver: &ReceiverMetadata) -> Self {
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

    /// Assembles the tag set, in the order of the reference's `Tags.toArray`, omitting empty tags as it does.
    fn tag_set(&self, extra: Option<&str>) -> TagSet {
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
                tag_set.insert_tag(MetaString::from(format!("{name}:{value}")));
            }
        }
        if let Some(extra) = extra {
            tag_set.insert_tag(MetaString::from(extra));
        }
        tag_set
    }
}

/// Why a payload was dropped before its traces reached the pipeline, the receiver's share of the reference's
/// `TracesDropped` reasons.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum DropReason {
    DecodingError,
    PayloadTooLarge,
    Timeout,
    UnexpectedEof,
}

impl DropReason {
    const ALL: [Self; 4] = [
        Self::DecodingError,
        Self::PayloadTooLarge,
        Self::Timeout,
        Self::UnexpectedEof,
    ];

    /// Returns the `reason` tag of `normalizer.traces_dropped`.
    fn reason_tag(self) -> &'static str {
        match self {
            Self::DecodingError => "reason:decoding_error",
            Self::PayloadTooLarge => "reason:payload_too_large",
            Self::Timeout => "reason:timeout",
            Self::UnexpectedEof => "reason:unexpected_eof",
        }
    }

    /// Returns the `error` tag value of `receiver.error`, as `httpDecodingError` names it.
    pub(super) fn error_tag(self) -> &'static str {
        match self {
            Self::DecodingError => "decoding-error",
            Self::PayloadTooLarge => "payload-too-large",
            Self::Timeout => "timeout",
            Self::UnexpectedEof => "unexpected-eof",
        }
    }
}

/// Counts for one tag combination, the reference's `info.Stats` fields the receiver updates.
#[derive(Default)]
pub(super) struct TagStats {
    pub traces_received: i64,
    pub traces_bytes: i64,
    pub spans_received: i64,
    pub payload_accepted: i64,
    pub payload_refused: i64,
    pub payload_timeout: i64,
    pub client_dropped_p0_traces: i64,
    pub client_dropped_p0_spans: i64,
    traces_dropped: [i64; DropReason::ALL.len()],
    serve_ok: Histogram,
    serve_failed: Histogram,
}

impl TagStats {
    pub(super) fn drop_traces(&mut self, reason: DropReason, count: i64) {
        self.traces_dropped[reason as usize] += count;
    }

    /// Records the time a payload took to be served, under `success:true` or `success:false`.
    pub(super) fn serve_time(&mut self, elapsed: Duration, success: bool) {
        let histogram = if success {
            &mut self.serve_ok
        } else {
            &mut self.serve_failed
        };
        histogram.insert(elapsed.as_secs_f64() * 1_000.0, SampleRate::unsampled());
    }
}

struct Window {
    started: Instant,
    stats: FastHashMap<ReceiverTags, TagStats>,
    /// `receiver.error` counts, keyed by endpoint version and error tag, which the reference doesn't tag by tracer.
    errors: FastHashMap<(&'static str, &'static str), i64>,
}

impl Default for Window {
    fn default() -> Self {
        Self {
            started: Instant::now(),
            stats: FastHashMap::default(),
            errors: FastHashMap::default(),
        }
    }
}

/// Receiver telemetry shared by every request handler.
pub(super) struct ReceiverTelemetry {
    window: Mutex<Window>,
    histogram: HistogramConfiguration,
}

impl ReceiverTelemetry {
    pub(super) fn new(histogram: HistogramConfiguration) -> Self {
        Self {
            window: Mutex::new(Window::default()),
            histogram,
        }
    }

    /// Updates the counts of a tag combination.
    pub(super) fn record(&self, mut tags: ReceiverTags, update: impl FnOnce(&mut TagStats)) {
        let mut window = self.window.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(stats) = window.stats.get_mut(&tags) {
            update(stats);
            return;
        }
        if window.stats.len() >= MAX_TAG_KEYS {
            tags = ReceiverTags::default();
        }
        update(window.stats.entry(tags).or_default());
    }

    /// Counts a `receiver.error` for a payload sent to the endpoint of `version`.
    pub(super) fn record_error(&self, version: &'static str, reason: DropReason) {
        let mut window = self.window.lock().unwrap_or_else(PoisonError::into_inner);
        *window.errors.entry((version, reason.error_tag())).or_default() += 1;
    }

    /// Reports the window's counts, in the order of the reference's `publishAndReset`, then resets the window.
    ///
    /// Counts are reported as rates over the window, as DogStatsD flushes the trace-agent's counts.
    pub(super) fn take_window_events(&self) -> Vec<Event> {
        let window = std::mem::take(&mut *self.window.lock().unwrap_or_else(PoisonError::into_inner));
        let interval = telemetry_window_interval(window.started);
        let mut events = Vec::new();

        for (tags, mut stats) in window.stats {
            let tag_set = tags.tag_set(None);
            // Like the reference without `send_all_internal_stats`, the less used counts are only reported when
            // positive.
            let mut count = |name: &'static str, value: i64, always: bool| {
                if always || value > 0 {
                    events.push(counter(name, tag_set.clone(), value, interval));
                }
            };
            count("datadog.trace_agent.receiver.trace", stats.traces_received, true);
            count(
                "datadog.trace_agent.receiver.traces_received",
                stats.traces_received,
                true,
            );
            count("datadog.trace_agent.receiver.traces_bytes", stats.traces_bytes, true);
            count(
                "datadog.trace_agent.receiver.spans_received",
                stats.spans_received,
                true,
            );
            count(
                "datadog.trace_agent.receiver.payload_accepted",
                stats.payload_accepted,
                true,
            );
            count(
                "datadog.trace_agent.receiver.payload_refused",
                stats.payload_refused,
                true,
            );
            count(
                "datadog.trace_agent.receiver.payload_timeout",
                stats.payload_timeout,
                false,
            );
            count(
                "datadog.trace_agent.receiver.client_dropped_p0_spans",
                stats.client_dropped_p0_spans,
                false,
            );
            count(
                "datadog.trace_agent.receiver.client_dropped_p0_traces",
                stats.client_dropped_p0_traces,
                false,
            );

            for (reason, value) in DropReason::ALL.iter().zip(stats.traces_dropped) {
                if value > 0 {
                    let tag_set = tags.tag_set(Some(reason.reason_tag()));
                    events.push(counter(METRIC_TRACES_DROPPED, tag_set, value, interval));
                }
            }

            for (histogram, success) in [
                (&mut stats.serve_ok, "success:true"),
                (&mut stats.serve_failed, "success:false"),
            ] {
                if !histogram.samples().is_empty() {
                    let context = Context::from_parts(
                        MetaString::from_static(METRIC_SERVE_TRACES_MS),
                        tags.tag_set(Some(success)),
                    );
                    self.push_histogram(&mut events, context, histogram, interval);
                }
            }
        }

        for ((version, error), value) in window.errors {
            let mut tag_set = TagSet::with_capacity(3);
            tag_set.insert_tag(MetaString::from_static("handler:traces"));
            tag_set.insert_tag(MetaString::from(format!("v:{version}")));
            tag_set.insert_tag(MetaString::from(format!("error:{error}")));
            events.push(counter(METRIC_ERROR, tag_set, value, interval));
        }

        events
    }

    /// Reports a histogram as DogStatsD's aggregation reports one.
    fn push_histogram(&self, events: &mut Vec<Event>, context: Context, histogram: &mut Histogram, interval: Duration) {
        if self.histogram.copy_to_distribution() {
            let mut sketch = DDSketch::default();
            for sample in histogram.samples() {
                sketch.insert_n(sample.value.into_inner(), sample.weight.0 as u64);
            }
            let name = format!("{}{}", self.histogram.copy_to_distribution_prefix(), context.name());
            events.push(Event::Metric(Metric::from_parts(
                context.with_name(name),
                MetricValues::distribution(sketch),
                MetricMetadata::default(),
            )));
        }

        let summary = histogram.summary_view();
        for statistic in self.histogram.statistics() {
            let value = statistic.value_from_histogram(&summary);
            let values = if statistic.is_rate_statistic() {
                MetricValues::rate(value, interval)
            } else {
                MetricValues::gauge(value)
            };
            let name = format!("{}.{}", context.name(), statistic.suffix());
            events.push(Event::Metric(Metric::from_parts(
                context.with_name(name),
                values,
                MetricMetadata::default(),
            )));
        }
    }
}

fn counter(name: &'static str, tags: TagSet, value: i64, interval: Duration) -> Event {
    Event::Metric(Metric::from_parts(
        Context::from_parts(MetaString::from_static(name), tags),
        MetricValues::rate(value as f64, interval),
        MetricMetadata::default(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The interval of a window reported as soon as it starts.
    const SECOND: Duration = Duration::from_secs(1);

    /// Returns each metric's name, tags in order, and values.
    fn metrics(events: &[Event]) -> Vec<(String, Vec<String>, MetricValues)> {
        events
            .iter()
            .map(|event| {
                let metric = event.try_as_metric().expect("only metrics are reported");
                let tags = metric.context().tags().into_iter().map(|tag| tag.as_str().to_string());
                (
                    metric.context().name().to_string(),
                    tags.collect(),
                    metric.values().clone(),
                )
            })
            .collect()
    }

    fn tags() -> ReceiverTags {
        ReceiverTags {
            lang: MetaString::from("python"),
            lang_version: MetaString::from("3.12"),
            interpreter: MetaString::from("CPython"),
            tracer_version: MetaString::from("2.0.0"),
            endpoint_version: MetaString::from("v1.0"),
            connection_type: MetaString::from("uds"),
            service: MetaString::from("checkout"),
            ..ReceiverTags::default()
        }
    }

    #[test]
    fn reason_order_matches_discriminants() {
        for (i, reason) in DropReason::ALL.iter().enumerate() {
            assert_eq!(*reason as usize, i);
        }
    }

    #[test]
    fn window_reports_counts_as_the_trace_agent_publishes_them() {
        let telemetry = ReceiverTelemetry::new(HistogramConfiguration::default());
        telemetry.record(tags(), |stats| {
            stats.traces_received += 2;
            stats.traces_bytes += 100;
            stats.spans_received += 3;
            stats.payload_accepted += 1;
            stats.client_dropped_p0_traces += 4;
        });

        let reported = metrics(&telemetry.take_window_events());

        let expected_tags = [
            "lang:python",
            "lang_version:3.12",
            "interpreter:CPython",
            "tracer_version:2.0.0",
            "endpoint_version:v1.0",
            "connection_type:uds",
            "service:checkout",
        ];
        let expected = [
            ("datadog.trace_agent.receiver.trace", 2.0),
            ("datadog.trace_agent.receiver.traces_received", 2.0),
            ("datadog.trace_agent.receiver.traces_bytes", 100.0),
            ("datadog.trace_agent.receiver.spans_received", 3.0),
            ("datadog.trace_agent.receiver.payload_accepted", 1.0),
            ("datadog.trace_agent.receiver.payload_refused", 0.0),
            // `payload_timeout` and `client_dropped_p0_spans` are zero, so they aren't reported.
            ("datadog.trace_agent.receiver.client_dropped_p0_traces", 4.0),
        ];
        assert_eq!(reported.len(), expected.len(), "{reported:?}");
        for ((name, tags, values), (expected_name, expected_value)) in reported.iter().zip(expected) {
            assert_eq!(name, expected_name);
            assert_eq!(tags, &expected_tags);
            assert_eq!(*values, MetricValues::rate(expected_value, SECOND), "{name}");
        }
        assert!(telemetry.take_window_events().is_empty(), "the window resets");
    }

    #[test]
    fn window_reports_rejections_and_serve_times() {
        let telemetry = ReceiverTelemetry::new(HistogramConfiguration::default());
        let tags = ReceiverTags {
            lang: MetaString::from("go"),
            endpoint_version: MetaString::from("v1.0"),
            ..ReceiverTags::default()
        };
        telemetry.record(tags.clone(), |stats| {
            stats.drop_traces(DropReason::UnexpectedEof, 3);
            stats.serve_time(Duration::from_millis(4), false);
            stats.serve_time(Duration::from_millis(8), false);
        });
        telemetry.record_error("v1.0", DropReason::UnexpectedEof);
        telemetry.record_error("v1.0", DropReason::UnexpectedEof);

        let reported = metrics(&telemetry.take_window_events());
        let find = |name: &str| {
            reported
                .iter()
                .find(|(n, _, _)| n == name)
                .unwrap_or_else(|| panic!("{name} is missing: {reported:?}"))
        };

        let (_, dropped_tags, dropped) = find("datadog.trace_agent.normalizer.traces_dropped");
        assert_eq!(
            dropped_tags,
            &["lang:go", "endpoint_version:v1.0", "reason:unexpected_eof"]
        );
        assert_eq!(*dropped, MetricValues::rate(3.0, SECOND));

        let (_, error_tags, errors) = find("datadog.trace_agent.receiver.error");
        assert_eq!(error_tags, &["handler:traces", "v:v1.0", "error:unexpected-eof"]);
        assert_eq!(*errors, MetricValues::rate(2.0, SECOND));

        // The serve time is summarized as DogStatsD summarizes histograms.
        let (_, serve_tags, avg) = find("datadog.trace_agent.receiver.serve_traces_ms.avg");
        assert_eq!(serve_tags, &["lang:go", "endpoint_version:v1.0", "success:false"]);
        assert_eq!(*avg, MetricValues::gauge(6.0));
        assert_eq!(
            find("datadog.trace_agent.receiver.serve_traces_ms.max").2,
            MetricValues::gauge(8.0)
        );
        assert_eq!(
            find("datadog.trace_agent.receiver.serve_traces_ms.count").2,
            MetricValues::rate(2.0, SECOND)
        );
        assert!(!reported
            .iter()
            .any(|(_, tags, _)| tags.iter().any(|tag| tag == "success:true")));
    }

    #[test]
    fn counts_are_rates_over_the_measured_window() {
        let telemetry = ReceiverTelemetry::new(HistogramConfiguration::default());
        assert!(
            telemetry.take_window_events().is_empty(),
            "an empty window reports nothing"
        );

        // As when a slow dispatch delays the window's report.
        telemetry.window.lock().unwrap().started = Instant::now() - Duration::from_millis(13_400);
        telemetry.record(tags(), |stats| stats.payload_accepted += 4);

        let reported = metrics(&telemetry.take_window_events());
        let (_, _, accepted) = reported
            .iter()
            .find(|(name, _, _)| name == "datadog.trace_agent.receiver.payload_accepted")
            .unwrap();
        assert_eq!(*accepted, MetricValues::rate(4.0, Duration::from_secs(13)));
    }

    #[test]
    fn window_overflow_counts_untagged() {
        let telemetry = ReceiverTelemetry::new(HistogramConfiguration::default());
        for i in 0..=MAX_TAG_KEYS {
            let tags = ReceiverTags {
                service: MetaString::from(i.to_string()),
                ..ReceiverTags::default()
            };
            telemetry.record(tags, |stats| stats.payload_accepted += 1);
        }
        let window = telemetry.window.lock().unwrap();
        assert_eq!(window.stats.len(), MAX_TAG_KEYS + 1);
        assert_eq!(window.stats[&ReceiverTags::default()].payload_accepted, 1);
    }
}
