//! Telemetry for the timestamp split.

use std::time::Duration;

use metrics::{Counter, Histogram};
use saluki_core::{components::ComponentContext, observability::ComponentMetricsExt as _};
use saluki_metrics::MetricsBuilder;

/// The native telemetry of the timestamp split, under the names the native no-aggregation-pipeline split transform
/// reports.
///
/// The `aggregate_` prefix of the passthrough metrics is historical: they describe the timestamp split, not
/// aggregation. The component reporting them is the one the builder was created for.
#[derive(Clone)]
pub struct TimestampSplitTelemetry {
    events_dropped: Counter,
    passthrough_metrics: Counter,
    passthrough_flushes: Counter,
    passthrough_batch_duration: Histogram,
}

impl TimestampSplitTelemetry {
    /// Registers the telemetry with `builder`.
    pub fn new(builder: &MetricsBuilder) -> Self {
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

    /// Registers only the telemetry a [`TimestampSplit`][super::TimestampSplit] kernel records, for the component
    /// `component`: the passthrough metric and flush counts.
    ///
    /// The buffer-full drop counter and the batch duration histogram describe a passthrough batcher, which the kernel
    /// does not have, so they are not registered and record nothing.
    pub fn for_kernel(component: &ComponentContext) -> Self {
        let builder = MetricsBuilder::from_component_context(component);
        Self {
            events_dropped: Counter::noop(),
            passthrough_metrics: builder.register_counter("aggregate_passthrough_metrics_total"),
            passthrough_flushes: builder.register_counter("aggregate_passthrough_flushes_total"),
            passthrough_batch_duration: Histogram::noop(),
        }
    }

    /// Returns telemetry that records nothing.
    pub fn noop() -> Self {
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
    pub fn increment_events_dropped(&self) {
        self.events_dropped.increment(1);
    }

    /// Records metrics sent to the passthrough port.
    pub fn increment_passthrough_metrics(&self, count: u64) {
        self.passthrough_metrics.increment(count);
    }

    /// Records one buffer of passthrough metrics sent on.
    pub fn increment_passthrough_flushes(&self) {
        self.passthrough_flushes.increment(1);
    }

    /// Records how long a buffer of passthrough metrics was filled for before it was sent on.
    pub fn record_passthrough_batch_duration(&self, duration: Duration) {
        self.passthrough_batch_duration.record(duration.as_secs_f64());
    }
}
