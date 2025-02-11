use std::time::Duration;

use metrics::{Counter, Gauge, Histogram};
use saluki_context::Context;
use saluki_event::metric::MetricValues;
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
    pub fn new(builder: &MetricsBuilder, name: &'static str) -> Self {
        Self {
            for_counter: builder.register_debug_gauge_with_tags(name, ["metric_type:counter"]),
            for_gauge: builder.register_debug_gauge_with_tags(name, ["metric_type:gauge"]),
            for_rate: builder.register_debug_gauge_with_tags(name, ["metric_type:rate"]),
            for_set: builder.register_debug_gauge_with_tags(name, ["metric_type:set"]),
            for_histogram: builder.register_debug_gauge_with_tags(name, ["metric_type:histogram"]),
            for_distribution: builder.register_debug_gauge_with_tags(name, ["metric_type:distribution"]),
        }
    }

    #[cfg(test)]
    pub fn noop() -> Self {
        Self {
            for_counter: Gauge::noop(),
            for_gauge: Gauge::noop(),
            for_rate: Gauge::noop(),
            for_set: Gauge::noop(),
            for_histogram: Gauge::noop(),
            for_distribution: Gauge::noop(),
        }
    }

    pub fn for_values(&self, values: &MetricValues) -> &Gauge {
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

#[derive(Clone)]
pub struct Telemetry {
    active_contexts: Gauge,
    active_contexts_by_type: MetricTypedGauge,
    active_contexts_bytes_by_type: MetricTypedGauge,
    events_dropped: Counter,
    flushes: Counter,
    series_flushed: Counter,
    sketches_flushed: Counter,
    passthrough_metrics: Counter,
    passthrough_flushes: Counter,
    passthrough_batch_duration: Histogram,
}

impl Telemetry {
    pub fn new(builder: &MetricsBuilder) -> Self {
        Self {
            active_contexts: builder.register_debug_gauge("aggregate_active_contexts"),
            active_contexts_by_type: MetricTypedGauge::new(builder, "aggregate_active_contexts_by_type"),
            active_contexts_bytes_by_type: MetricTypedGauge::new(builder, "aggregate_active_contexts_bytes_by_type"),
            events_dropped: builder
                .register_debug_counter_with_tags("component_events_dropped_total", ["intentional:true"]),
            flushes: builder.register_debug_counter("aggregate_flushes_total"),
            series_flushed: builder.register_debug_counter_with_tags("aggregate_flushed_total", ["data_type:series"]),
            sketches_flushed: builder
                .register_debug_counter_with_tags("aggregate_flushed_total", ["data_type:sketches"]),
            passthrough_metrics: builder.register_debug_counter("aggregate_passthrough_metrics_total"),
            passthrough_flushes: builder.register_debug_counter("aggregate_passthrough_flushes_total"),
            passthrough_batch_duration: builder.register_debug_histogram("aggregate_passthrough_batch_duration_secs"),
        }
    }

    #[cfg(test)]
    pub fn noop() -> Self {
        Self {
            active_contexts: Gauge::noop(),
            active_contexts_by_type: MetricTypedGauge::noop(),
            active_contexts_bytes_by_type: MetricTypedGauge::noop(),
            events_dropped: Counter::noop(),
            flushes: Counter::noop(),
            series_flushed: Counter::noop(),
            sketches_flushed: Counter::noop(),
            passthrough_metrics: Counter::noop(),
            passthrough_flushes: Counter::noop(),
            passthrough_batch_duration: Histogram::noop(),
        }
    }

    pub fn increment_contexts(&self, context: &Context, values: &MetricValues) {
        self.active_contexts.increment(1);
        self.active_contexts_by_type.for_values(values).increment(1);
        self.active_contexts_bytes_by_type
            .for_values(values)
            .increment(context.size_of() as f64);
    }

    pub fn decrement_contexts(&self, context: &Context, values: &MetricValues) {
        self.active_contexts.decrement(1);
        self.active_contexts_by_type.for_values(values).decrement(1);
        self.active_contexts_bytes_by_type
            .for_values(values)
            .decrement(context.size_of() as f64);
    }

    pub fn increment_events_dropped(&self) {
        self.events_dropped.increment(1);
    }

    pub fn increment_flushes(&self) {
        self.flushes.increment(1);
    }

    pub fn increment_flushed(&self, values: &MetricValues) {
        if values.is_serie() {
            self.series_flushed.increment(1);
        } else if values.is_sketch() {
            self.sketches_flushed.increment(1);
        }
    }

    pub fn increment_passthrough_metrics(&self) {
        self.passthrough_metrics.increment(1);
    }

    pub fn increment_passthrough_flushes(&self) {
        self.passthrough_flushes.increment(1);
    }

    pub fn record_passthrough_batch_duration(&self, duration: Duration) {
        self.passthrough_batch_duration.record(duration.as_secs_f64());
    }
}
