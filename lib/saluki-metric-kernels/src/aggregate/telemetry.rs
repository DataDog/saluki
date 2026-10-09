//! Aggregation telemetry.

use metrics::{Counter, Gauge};
use saluki_core::data_model::event::metric::{context::Context, MetricValues};
use saluki_metrics::MetricsBuilder;

#[derive(Clone)]
struct MetricTypedGauge {
    for_counter: Gauge,
    for_gauge: Gauge,
    for_rate: Gauge,
    for_set: Gauge,
    for_histogram: Gauge,
    for_distribution: Gauge,
}

impl MetricTypedGauge {
    fn new(builder: &MetricsBuilder, name: &'static str) -> Self {
        Self {
            for_counter: builder.register_gauge_with_tags(name, ["metric_type:counter"]),
            for_gauge: builder.register_gauge_with_tags(name, ["metric_type:gauge"]),
            for_rate: builder.register_gauge_with_tags(name, ["metric_type:rate"]),
            for_set: builder.register_gauge_with_tags(name, ["metric_type:set"]),
            for_histogram: builder.register_gauge_with_tags(name, ["metric_type:histogram"]),
            for_distribution: builder.register_gauge_with_tags(name, ["metric_type:distribution"]),
        }
    }

    fn noop() -> Self {
        Self {
            for_counter: Gauge::noop(),
            for_gauge: Gauge::noop(),
            for_rate: Gauge::noop(),
            for_set: Gauge::noop(),
            for_histogram: Gauge::noop(),
            for_distribution: Gauge::noop(),
        }
    }

    fn for_values(&self, values: &MetricValues) -> &Gauge {
        match values {
            MetricValues::Counter(_) => &self.for_counter,
            MetricValues::Gauge(_) => &self.for_gauge,
            MetricValues::Rate(_, _) => &self.for_rate,
            MetricValues::Set(_) => &self.for_set,
            MetricValues::Histogram(_) => &self.for_histogram,
            MetricValues::Distribution(_) => &self.for_distribution,
        }
    }
}

/// The telemetry native aggregation reports.
///
/// Every metric is registered through the given [`MetricsBuilder`], so a host that builds it from its component's
/// context reports them with that component's labels:
///
/// - `aggregate_active_contexts` (debug level): the contexts held.
/// - `aggregate_active_contexts_by_type` and `aggregate_active_contexts_bytes_by_type`, tagged `metric_type`: the
///   contexts held and the memory their contexts take, by the kind of their first value. A context whose kind changes
///   is added under its first kind and removed under its last.
/// - `component_events_dropped_total`, tagged `intentional:true`: the metrics dropped at the context limit.
/// - `aggregate_flushes_total`: the flushes run.
/// - `aggregate_flushed_total`, tagged `data_type:series` or `data_type:sketches`: the contexts whose closed buckets a
///   flush emitted, before histograms expand.
#[derive(Clone)]
pub struct AggregateTelemetry {
    active_contexts: Gauge,
    active_contexts_by_type: MetricTypedGauge,
    active_contexts_bytes_by_type: MetricTypedGauge,
    events_dropped: Counter,
    flushes: Counter,
    series_flushed: Counter,
    sketches_flushed: Counter,
}

impl AggregateTelemetry {
    /// Registers aggregation telemetry with `builder`.
    pub fn new(builder: &MetricsBuilder) -> Self {
        Self {
            active_contexts: builder.register_debug_gauge("aggregate_active_contexts"),
            active_contexts_by_type: MetricTypedGauge::new(builder, "aggregate_active_contexts_by_type"),
            active_contexts_bytes_by_type: MetricTypedGauge::new(builder, "aggregate_active_contexts_bytes_by_type"),
            events_dropped: builder.register_counter_with_tags("component_events_dropped_total", ["intentional:true"]),
            flushes: builder.register_counter("aggregate_flushes_total"),
            series_flushed: builder.register_counter_with_tags("aggregate_flushed_total", ["data_type:series"]),
            sketches_flushed: builder.register_counter_with_tags("aggregate_flushed_total", ["data_type:sketches"]),
        }
    }

    /// Returns telemetry that records nothing, for hosts outside a topology, such as the reference interpreter.
    pub fn noop() -> Self {
        Self {
            active_contexts: Gauge::noop(),
            active_contexts_by_type: MetricTypedGauge::noop(),
            active_contexts_bytes_by_type: MetricTypedGauge::noop(),
            events_dropped: Counter::noop(),
            flushes: Counter::noop(),
            series_flushed: Counter::noop(),
            sketches_flushed: Counter::noop(),
        }
    }

    pub(super) fn increment_contexts(&self, context: &Context, values: &MetricValues) {
        self.active_contexts.increment(1);
        self.active_contexts_by_type.for_values(values).increment(1);
        self.active_contexts_bytes_by_type
            .for_values(values)
            .increment(context.size_of() as f64);
    }

    pub(super) fn decrement_contexts(&self, context: &Context, values: &MetricValues) {
        self.active_contexts.decrement(1);
        self.active_contexts_by_type.for_values(values).decrement(1);
        self.active_contexts_bytes_by_type
            .for_values(values)
            .decrement(context.size_of() as f64);
    }

    pub(super) fn increment_events_dropped(&self) {
        self.events_dropped.increment(1);
    }

    pub(super) fn increment_flushes(&self) {
        self.flushes.increment(1);
    }

    pub(super) fn increment_flushed(&self, values: &MetricValues) {
        if values.is_serie() {
            self.series_flushed.increment(1);
        } else if values.is_sketch() {
            self.sketches_flushed.increment(1);
        }
    }
}
