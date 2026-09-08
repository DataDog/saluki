//! Telemetry for the DogStatsD no-aggregation-pipeline split transform.
//!
//! The passthrough metrics keep their original `aggregate_`-prefixed names, even though they're no longer emitted by
//! the aggregate transform, so that existing dashboards and the Datadog Agent telemetry remappings keep working. The
//! `component_id` tag on them does change, from `dsd_agg` to `dsd_no_agg_split`: see
//! `crate::state::metrics::rules::get_aggregation_remappings`.

use std::time::Duration;

use metrics::{Counter, Histogram};
use saluki_metrics::MetricsBuilder;

#[derive(Clone)]
pub(super) struct Telemetry {
    events_dropped: Counter,
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
            passthrough_metrics: builder.register_counter("aggregate_passthrough_metrics_total"),
            passthrough_flushes: builder.register_counter("aggregate_passthrough_flushes_total"),
            passthrough_batch_duration: builder.register_debug_histogram("aggregate_passthrough_batch_duration_secs"),
        }
    }

    #[cfg(test)]
    pub(super) fn noop() -> Self {
        Self {
            events_dropped: Counter::noop(),
            passthrough_metrics: Counter::noop(),
            passthrough_flushes: Counter::noop(),
            passthrough_batch_duration: Histogram::noop(),
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
