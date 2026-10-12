//! Aggregation telemetry.

use metrics::{Counter, Gauge};
use saluki_core::data_model::event::metric::{context::Context, MetricValues};
use saluki_metrics::MetricsBuilder;

/// The number of metric kinds the typed gauges are tagged with.
const KINDS: usize = 6;

#[derive(Clone)]
struct MetricTypedGauge {
    /// By kind: counter, gauge, rate, set, histogram, distribution.
    by_kind: [Gauge; KINDS],
}

impl MetricTypedGauge {
    fn new(builder: &MetricsBuilder, name: &'static str) -> Self {
        Self {
            by_kind: [
                builder.register_gauge_with_tags(name, ["metric_type:counter"]),
                builder.register_gauge_with_tags(name, ["metric_type:gauge"]),
                builder.register_gauge_with_tags(name, ["metric_type:rate"]),
                builder.register_gauge_with_tags(name, ["metric_type:set"]),
                builder.register_gauge_with_tags(name, ["metric_type:histogram"]),
                builder.register_gauge_with_tags(name, ["metric_type:distribution"]),
            ],
        }
    }

    fn noop() -> Self {
        Self {
            by_kind: std::array::from_fn(|_| Gauge::noop()),
        }
    }

    fn for_values(&self, values: &MetricValues) -> &Gauge {
        &self.by_kind[kind_index(values)]
    }
}

/// Returns the index of a value set's kind among the typed gauges.
fn kind_index(values: &MetricValues) -> usize {
    match values {
        MetricValues::Counter(_) => 0,
        MetricValues::Gauge(_) => 1,
        MetricValues::Rate(_, _) => 2,
        MetricValues::Set(_) => 3,
        MetricValues::Histogram(_) => 4,
        MetricValues::Distribution(_) => 5,
    }
}

/// The handles aggregation telemetry writes to.
#[derive(Clone)]
struct Handles {
    active_contexts: Gauge,
    active_contexts_by_type: MetricTypedGauge,
    active_contexts_bytes_by_type: MetricTypedGauge,
    events_dropped: Counter,
    flushes: Counter,
    series_flushed: Counter,
    sketches_flushed: Counter,
}

/// What an accumulating telemetry counted since it last published: changes of its gauges, and increments of its
/// counters.
#[derive(Default)]
struct Pending {
    active_contexts: i64,
    active_contexts_by_type: [i64; KINDS],
    active_contexts_bytes_by_type: [i64; KINDS],
    events_dropped: u64,
    flushes: u64,
    series_flushed: u64,
    sketches_flushed: u64,
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
///
/// # Accumulating
///
/// Telemetry created with [`new`][Self::new] writes its handles as it counts: on every new or removed context, every
/// dropped metric, and every context flushed. Telemetry created with [`accumulating`][Self::accumulating] counts in
/// plain integers of its own instead, and writes what it counted to the same handles, as one change of each, when its
/// host calls [`publish`][Self::publish], and when it is dropped. Several cores that report under one component, such
/// as instances of one aggregation in several tasks, then never write a shared handle per metric, and each series is
/// the sum of what they counted, as for one core. Between publications, an accumulating telemetry's series lag what
/// it counted.
///
/// A clone shares the handles, and accumulates on its own from nothing.
pub struct AggregateTelemetry {
    handles: Handles,
    /// What the telemetry counted and has not published, if it accumulates.
    pending: Option<Pending>,
}

impl AggregateTelemetry {
    /// Registers aggregation telemetry with `builder`, writing its handles as it counts.
    pub fn new(builder: &MetricsBuilder) -> Self {
        Self {
            handles: Handles {
                active_contexts: builder.register_debug_gauge("aggregate_active_contexts"),
                active_contexts_by_type: MetricTypedGauge::new(builder, "aggregate_active_contexts_by_type"),
                active_contexts_bytes_by_type: MetricTypedGauge::new(
                    builder,
                    "aggregate_active_contexts_bytes_by_type",
                ),
                events_dropped: builder
                    .register_counter_with_tags("component_events_dropped_total", ["intentional:true"]),
                flushes: builder.register_counter("aggregate_flushes_total"),
                series_flushed: builder.register_counter_with_tags("aggregate_flushed_total", ["data_type:series"]),
                sketches_flushed: builder.register_counter_with_tags("aggregate_flushed_total", ["data_type:sketches"]),
            },
            pending: None,
        }
    }

    /// Registers aggregation telemetry with `builder` that accumulates what it counts until its host
    /// [publishes][Self::publish] it; see [Accumulating](Self#accumulating).
    pub fn accumulating(builder: &MetricsBuilder) -> Self {
        let mut telemetry = Self::new(builder);
        telemetry.pending = Some(Pending::default());
        telemetry
    }

    /// Returns telemetry that records nothing, for hosts outside a topology, such as the reference interpreter.
    pub fn noop() -> Self {
        Self {
            handles: Handles {
                active_contexts: Gauge::noop(),
                active_contexts_by_type: MetricTypedGauge::noop(),
                active_contexts_bytes_by_type: MetricTypedGauge::noop(),
                events_dropped: Counter::noop(),
                flushes: Counter::noop(),
                series_flushed: Counter::noop(),
                sketches_flushed: Counter::noop(),
            },
            pending: None,
        }
    }

    /// Writes what the telemetry counted since it last published to its handles, if it accumulates; otherwise, does
    /// nothing, since it wrote everything as it counted.
    pub fn publish(&mut self) {
        let Some(pending) = self.pending.as_mut() else {
            return;
        };
        let pending = std::mem::take(pending);
        let handles = &self.handles;
        change_gauge(&handles.active_contexts, pending.active_contexts);
        for kind in 0..KINDS {
            change_gauge(
                &handles.active_contexts_by_type.by_kind[kind],
                pending.active_contexts_by_type[kind],
            );
            change_gauge(
                &handles.active_contexts_bytes_by_type.by_kind[kind],
                pending.active_contexts_bytes_by_type[kind],
            );
        }
        increment_counter(&handles.events_dropped, pending.events_dropped);
        increment_counter(&handles.flushes, pending.flushes);
        increment_counter(&handles.series_flushed, pending.series_flushed);
        increment_counter(&handles.sketches_flushed, pending.sketches_flushed);
    }

    pub(super) fn increment_contexts(&mut self, context: &Context, values: &MetricValues) {
        self.change_contexts(context, values, 1);
    }

    pub(super) fn decrement_contexts(&mut self, context: &Context, values: &MetricValues) {
        self.change_contexts(context, values, -1);
    }

    /// Adds `sign` contexts, of the kind of `values` and the size of `context`, to the context gauges.
    fn change_contexts(&mut self, context: &Context, values: &MetricValues, sign: i64) {
        let bytes = i64::try_from(context.size_of()).unwrap_or(i64::MAX);
        match self.pending.as_mut() {
            Some(pending) => {
                let kind = kind_index(values);
                pending.active_contexts += sign;
                pending.active_contexts_by_type[kind] += sign;
                pending.active_contexts_bytes_by_type[kind] += sign * bytes;
            }
            None => {
                let handles = &self.handles;
                change_gauge(&handles.active_contexts, sign);
                change_gauge(handles.active_contexts_by_type.for_values(values), sign);
                change_gauge(handles.active_contexts_bytes_by_type.for_values(values), sign * bytes);
            }
        }
    }

    pub(super) fn increment_events_dropped(&mut self) {
        match self.pending.as_mut() {
            Some(pending) => pending.events_dropped += 1,
            None => self.handles.events_dropped.increment(1),
        }
    }

    pub(super) fn increment_flushes(&mut self) {
        match self.pending.as_mut() {
            Some(pending) => pending.flushes += 1,
            None => self.handles.flushes.increment(1),
        }
    }

    pub(super) fn increment_flushed(&mut self, values: &MetricValues) {
        let (pending, handle) = if values.is_serie() {
            (
                self.pending.as_mut().map(|pending| &mut pending.series_flushed),
                &self.handles.series_flushed,
            )
        } else if values.is_sketch() {
            (
                self.pending.as_mut().map(|pending| &mut pending.sketches_flushed),
                &self.handles.sketches_flushed,
            )
        } else {
            return;
        };
        match pending {
            Some(count) => *count += 1,
            None => handle.increment(1),
        }
    }
}

impl Clone for AggregateTelemetry {
    fn clone(&self) -> Self {
        Self {
            handles: self.handles.clone(),
            pending: self.pending.as_ref().map(|_| Pending::default()),
        }
    }
}

impl Drop for AggregateTelemetry {
    fn drop(&mut self) {
        // Whatever was counted since the last publication still reaches the handles: a host that drops a core, as it
        // does to rebuild one after a panic or to release a retired one, loses none of its counts.
        self.publish();
    }
}

/// Changes a gauge by `delta`, as one write, or not at all when it is zero.
fn change_gauge(gauge: &Gauge, delta: i64) {
    match delta {
        0 => {}
        delta if delta > 0 => gauge.increment(delta as f64),
        delta => gauge.decrement(delta.unsigned_abs() as f64),
    }
}

/// Increments a counter by `count`, as one write, or not at all when it is zero.
fn increment_counter(counter: &Counter, count: u64) {
    if count > 0 {
        counter.increment(count);
    }
}
