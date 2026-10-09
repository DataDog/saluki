//! Retained sender counters, including idle periods between metric batches.

use foldspace_core::MetricStreamFailureKind;
use saluki_common::collections::FastHashMap;
use saluki_io::net::util::retry::PushResult;
use saluki_metrics::{Counter, MetricsBuilder};
use tracing::warn;

pub(super) struct Telemetry {
    pub batches_acked: Counter,
    pub batches_retried: Counter,
    pub batches_abandoned: Counter,
    pub points_dropped: Counter,
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
            builder,
            stream_failures: FastHashMap::default(),
        }
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
