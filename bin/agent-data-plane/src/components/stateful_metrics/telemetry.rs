//! Retained sender counters, including idle periods between metric batches.

use foldspace_core::{MetricDictionaryStats, MetricStreamFailureKind};
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
