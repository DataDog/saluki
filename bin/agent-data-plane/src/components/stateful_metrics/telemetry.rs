//! Retained sender counters, including idle periods between metric batches.

use foldspace_core::{MetricDictionaryStats, MetricRetryStats, MetricStreamFailureKind};
use saluki_common::collections::FastHashMap;
use saluki_io::net::util::retry::PushResult;
use saluki_metrics::{Counter, Gauge, MetricsBuilder};
use tracing::warn;

pub(super) struct Telemetry {
    pub batches_acked: Counter,
    pub batches_retried: Counter,
    pub batches_abandoned: Counter,
    pub points_dropped: Counter,
    /// Retained definitions, which the entry cap governs; transient definitions are reported apart.
    dictionary_entries: Gauge,
    /// Estimated bytes of retained definitions, which the byte cap governs.
    dictionary_estimated_bytes: Gauge,
    /// Definitions sent only for the payloads that reference them, outside the caps.
    dictionary_transient_entries: Gauge,
    dictionary_transient_bytes: Gauge,
    /// Current size of the admission frequency sketch, which grows with the dictionary up to a
    /// ceiling set by the caps; outside the byte cap.
    dictionary_frequency_sketch_bytes: Gauge,
    /// Evicted definitions kept only for payloads the core retains to retry.
    dictionary_retired_entries: Gauge,
    dictionary_retired_bytes: Gauge,
    /// Payloads the core retains, compressed, until every endpoint acknowledges or gives them up.
    retry_retained_payloads: Gauge,
    retry_retained_bytes: Gauge,
    /// Endpoint copies of retained payloads waiting for their endpoint.
    retry_pending_copies: Gauge,
    /// Copies the core resent from retained bytes rather than encoding them again.
    retry_replays: Counter,
    retry_definition_batches: Counter,
    retry_rewrites: Counter,
    retry_unreadable: Counter,
    /// Copies the core moved to the retry queue, by reason.
    retry_returned: [Counter; 6],
    last_retry: MetricRetryStats,
    builder: MetricsBuilder,
    stream_failures: FastHashMap<&'static str, Counter>,
}

impl Telemetry {
    pub fn new(builder: MetricsBuilder) -> Self {
        Self {
            batches_acked: builder.register_counter("stateful_metrics_batches_acked_total"),
            batches_retried: builder.register_counter("stateful_metrics_batches_retried_total"),
            batches_abandoned: builder.register_counter("stateful_metrics_batches_abandoned_total"),
            points_dropped: builder.register_counter("stateful_metrics_points_dropped_total"),
            dictionary_entries: builder.register_gauge("stateful_metrics_dictionary_entries"),
            dictionary_estimated_bytes: builder.register_gauge("stateful_metrics_dictionary_estimated_bytes"),
            dictionary_transient_entries: builder.register_gauge("stateful_metrics_dictionary_transient_entries"),
            dictionary_transient_bytes: builder.register_gauge("stateful_metrics_dictionary_transient_bytes"),
            dictionary_frequency_sketch_bytes: builder
                .register_gauge("stateful_metrics_dictionary_frequency_sketch_bytes"),
            dictionary_retired_entries: builder.register_gauge("stateful_metrics_dictionary_retired_entries"),
            dictionary_retired_bytes: builder.register_gauge("stateful_metrics_dictionary_retired_bytes"),
            retry_retained_payloads: builder.register_gauge("stateful_metrics_internal_retry_retained_payloads"),
            retry_retained_bytes: builder.register_gauge("stateful_metrics_internal_retry_retained_bytes"),
            retry_pending_copies: builder.register_gauge("stateful_metrics_internal_retry_pending_copies"),
            retry_replays: builder.register_counter("stateful_metrics_internal_retry_replays_total"),
            retry_definition_batches: builder
                .register_counter("stateful_metrics_internal_retry_definition_batches_total"),
            retry_rewrites: builder.register_counter("stateful_metrics_internal_retry_rewrites_total"),
            retry_unreadable: builder.register_counter("stateful_metrics_internal_retry_unreadable_total"),
            retry_returned: ["budget", "age", "attempts", "encode", "suspension", "shutdown"].map(|reason| {
                builder
                    .register_counter_with_tags("stateful_metrics_internal_retry_returned_total", [("reason", reason)])
            }),
            last_retry: MetricRetryStats::default(),
            builder,
            stream_failures: FastHashMap::default(),
        }
    }

    pub fn record_dictionary(&self, stats: MetricDictionaryStats) {
        self.dictionary_entries.set(stats.entries as f64);
        self.dictionary_estimated_bytes.set(stats.estimated_bytes as f64);
        self.dictionary_transient_entries.set(stats.transient_entries as f64);
        self.dictionary_transient_bytes.set(stats.transient_bytes as f64);
        self.dictionary_frequency_sketch_bytes
            .set(stats.frequency_sketch_bytes as f64);
        self.dictionary_retired_entries.set(stats.retired_entries as f64);
        self.dictionary_retired_bytes.set(stats.retired_bytes as f64);
    }

    /// Sets the retry gauges and adds what the core's cumulative counts gained since the last call.
    pub fn record_retries(&mut self, stats: MetricRetryStats) {
        self.retry_retained_payloads.set(stats.retained_payloads as f64);
        self.retry_retained_bytes.set(stats.retained_bytes as f64);
        self.retry_pending_copies.set(stats.pending_copies as f64);
        let last = self.last_retry;
        self.retry_replays
            .increment(stats.replayed_payloads - last.replayed_payloads);
        self.retry_definition_batches
            .increment(stats.definition_batches - last.definition_batches);
        self.retry_rewrites
            .increment(stats.rewritten_payloads - last.rewritten_payloads);
        self.retry_unreadable
            .increment(stats.unreadable_payloads - last.unreadable_payloads);
        let returned = [
            (stats.returned_over_budget, last.returned_over_budget),
            (stats.returned_over_age, last.returned_over_age),
            (stats.returned_over_attempts, last.returned_over_attempts),
            (stats.returned_unencodable, last.returned_unencodable),
            (stats.returned_on_suspension, last.returned_on_suspension),
            (stats.returned_on_shutdown, last.returned_on_shutdown),
        ];
        for (counter, (now, before)) in self.retry_returned.iter().zip(returned) {
            counter.increment(now - before);
        }
        self.last_retry = stats;
    }

    pub fn stream_failed(&mut self, kind: MetricStreamFailureKind) {
        let kind = match kind {
            MetricStreamFailureKind::Unavailable => "Unavailable",
            MetricStreamFailureKind::DeadlineExceeded => "DeadlineExceeded",
            MetricStreamFailureKind::ResourceExhausted => "ResourceExhausted",
            MetricStreamFailureKind::InvalidArgument => "InvalidArgument",
            MetricStreamFailureKind::Unauthenticated => "Unauthenticated",
            MetricStreamFailureKind::FailedPrecondition => "FailedPrecondition",
            MetricStreamFailureKind::ProtocolViolation => "ProtocolViolation",
        };
        self.stream_failures
            .entry(kind)
            .or_insert_with(|| {
                self.builder
                    .register_counter_with_tags("stateful_metrics_stream_failures_total", [("kind", kind)])
            })
            .increment(1);
    }

    pub fn track_drops(&self, result: PushResult) {
        if result.had_drops() {
            warn!(
                batches = result.items_dropped,
                points = result.data_points_dropped,
                "Stateful metrics retry storage dropped queued data."
            );
            self.batches_abandoned.increment(result.items_dropped);
            self.points_dropped.increment(result.data_points_dropped);
        }
    }
}
