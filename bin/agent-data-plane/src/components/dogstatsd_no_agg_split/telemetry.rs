//! Telemetry for the DogStatsD no-aggregation-pipeline split transform.

use std::time::Duration;

use metrics::{Counter, Histogram};
use saluki_metrics::MetricsBuilder;

#[derive(Clone)]
pub(super) struct Telemetry {
    events_dropped: Counter,
    unsupported_type_dropped: Counter,
    passthrough_metrics: Counter,
    passthrough_flushes: Counter,
    passthrough_batch_duration: Histogram,
}

impl Telemetry {
    pub(super) fn new(builder: &MetricsBuilder) -> Self {
        Self {
            events_dropped: builder.register_counter_with_tags(
                "component_events_dropped_total",
                ["intentional:false", "drop_reason:buffer_full"],
            ),
            unsupported_type_dropped: builder.register_counter_with_tags(
                "component_events_dropped_total",
                ["intentional:true", "drop_reason:unsupported_type"],
            ),
            passthrough_metrics: builder.register_counter("aggregate_passthrough_metrics_total"),
            passthrough_flushes: builder.register_counter("aggregate_passthrough_flushes_total"),
            passthrough_batch_duration: builder.register_debug_histogram("aggregate_passthrough_batch_duration_secs"),
        }
    }

    /// Records a passthrough metric discarded because the active buffer was full immediately after being flushed
    /// and replaced.
    ///
    /// This is a guard against an invariant break, not a routine drop: a freshly replaced buffer always has room for
    /// at least one event, so this should never fire. It is tagged `intentional:false` because, unlike the aggregate
    /// transform's context-limit drops, discarding here is a failure rather than a deliberate shed.
    pub(super) fn increment_events_dropped(&self) {
        self.events_dropped.increment(1);
    }

    /// Records a timestamped metric discarded because its type isn't supported by the no-aggregation pipeline.
    ///
    /// Like the Datadog Agent, only gauges, counters and rates can be forwarded without aggregation, so timestamped
    /// histograms, distributions and sets are deliberately dropped.
    pub(super) fn increment_unsupported_type_dropped(&self) {
        self.unsupported_type_dropped.increment(1);
    }

    pub(super) fn increment_passthrough_metrics(&self) {
        self.passthrough_metrics.increment(1);
    }

    pub(super) fn increment_passthrough_flushes(&self) {
        self.passthrough_flushes.increment(1);
    }

    pub(super) fn record_passthrough_batch_duration(&self, duration: Duration) {
        self.passthrough_batch_duration.record(duration.as_secs_f64());
    }
}
