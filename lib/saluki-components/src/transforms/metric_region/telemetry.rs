use std::collections::BTreeMap;

use metrics::{Counter, Gauge};
use saluki_metric_kernels::aggregate::AGGREGATE_METRIC_TYPE_LABELS;
use saluki_metric_plan::{NodeId, OperatorTelemetry, RegionTelemetryDelta};
use saluki_metrics::MetricsBuilder;

fn node_builder(node_id: &NodeId) -> MetricsBuilder {
    MetricsBuilder::default()
        .add_default_tag(("component_id", node_id.to_string()))
        .add_default_tag(("component_type", "transform"))
}

struct PrefixFilterHandles {
    size: Gauge,
    updates: Counter,
    dropped_points: Counter,
}

impl PrefixFilterHandles {
    fn new(node_id: &NodeId) -> Self {
        let builder = node_builder(node_id);
        Self {
            size: builder.register_gauge("metric_filterlist_size"),
            updates: builder.register_counter("metric_filterlist_updates_total"),
            dropped_points: builder.register_counter("dogstatsd_listener_filtered_points_total"),
        }
    }
}

struct TimestampSplitHandles {
    passthrough_metrics: Counter,
    passthrough_flushes: Counter,
}

impl TimestampSplitHandles {
    fn new(node_id: &NodeId) -> Self {
        let builder = node_builder(node_id);
        Self {
            passthrough_metrics: builder.register_counter("aggregate_passthrough_metrics_total"),
            passthrough_flushes: builder.register_counter("aggregate_passthrough_flushes_total"),
        }
    }
}

struct TagFilterHandles {
    size: Gauge,
    updates: Counter,
    tags_filtered: Counter,
    cache_hits: Counter,
    cache_misses: Counter,
    cache_evictions: Counter,
}

impl TagFilterHandles {
    fn new(node_id: &NodeId) -> Self {
        let builder = node_builder(node_id);
        let cache_builder = MetricsBuilder::default().add_default_tag(("cache_id", "tag_filterlist/context_cache"));
        Self {
            size: builder.register_gauge("tag_filterlist_size"),
            updates: builder.register_counter("tag_filterlist_updates_total"),
            tags_filtered: builder.register_counter("tag_filterlist_tags_filtered_total"),
            cache_hits: cache_builder.register_counter("cache_hits_total"),
            cache_misses: cache_builder.register_counter("cache_misses_total"),
            cache_evictions: cache_builder.register_counter("cache_items_evicted_total"),
        }
    }
}

struct AggregateHandles {
    events_received: Counter,
    flushes: Counter,
    series_flushed: Counter,
    sketches_flushed: Counter,
    last_flush_series: Gauge,
    last_flush_sketches: Gauge,
    last_flush_duration_ns: Gauge,
    active_contexts: Gauge,
    active_contexts_by_type: [Gauge; 6],
    active_contexts_bytes_by_type: [Gauge; 6],
}

impl AggregateHandles {
    fn new(node_id: &NodeId) -> Self {
        let builder = node_builder(node_id);
        let active_contexts_by_type = AGGREGATE_METRIC_TYPE_LABELS.map(|label| {
            builder.register_gauge_with_tags("aggregate_active_contexts_by_type", [("metric_type", label)])
        });
        let active_contexts_bytes_by_type = AGGREGATE_METRIC_TYPE_LABELS.map(|label| {
            builder.register_gauge_with_tags("aggregate_active_contexts_bytes_by_type", [("metric_type", label)])
        });
        Self {
            events_received: builder.register_counter("component_events_received_total"),
            flushes: builder.register_counter("aggregate_flushes_total"),
            series_flushed: builder.register_counter_with_tags("aggregate_flushed_total", ["data_type:series"]),
            sketches_flushed: builder.register_counter_with_tags("aggregate_flushed_total", ["data_type:sketches"]),
            last_flush_series: builder.register_gauge_with_tags("aggregate_last_flush_count", ["data_type:series"]),
            last_flush_sketches: builder.register_gauge_with_tags("aggregate_last_flush_count", ["data_type:sketches"]),
            last_flush_duration_ns: builder.register_gauge("aggregate_last_flush_duration_nanoseconds"),
            active_contexts: builder.register_debug_gauge("aggregate_active_contexts"),
            active_contexts_by_type,
            active_contexts_bytes_by_type,
        }
    }
}

struct PostAggregateFilterHandles {
    filtered_metrics: Counter,
}

impl PostAggregateFilterHandles {
    fn new(node_id: &NodeId) -> Self {
        let builder = node_builder(node_id);
        Self {
            filtered_metrics: builder.register_counter("dogstatsd_post_aggregate_filtered_metrics_total"),
        }
    }
}

/// Publishes per-operator telemetry deltas from [`saluki_metric_plan`] onto `saluki-metrics` using each operator's
/// logical plan node ID as `component_id` so existing RAR remapping rules work unchanged.
#[derive(Default)]
pub(super) struct RegionTelemetryEmitter {
    prefix_filters: BTreeMap<NodeId, PrefixFilterHandles>,
    timestamp_splits: BTreeMap<NodeId, TimestampSplitHandles>,
    tag_filters: BTreeMap<NodeId, TagFilterHandles>,
    aggregates: BTreeMap<NodeId, AggregateHandles>,
    post_agg_filters: BTreeMap<NodeId, PostAggregateFilterHandles>,
}

impl RegionTelemetryEmitter {
    pub(super) fn publish(&mut self, delta: RegionTelemetryDelta) {
        if delta.is_empty() {
            return;
        }

        for (node_id, count) in delta.filterlist_updates {
            if count > 0 {
                let handles = self
                    .prefix_filters
                    .entry(node_id.clone())
                    .or_insert_with(|| PrefixFilterHandles::new(&node_id));
                handles.updates.increment(count);
            }
        }

        for (node_id, count) in delta.tag_filterlist_updates {
            if count > 0 {
                let handles = self
                    .tag_filters
                    .entry(node_id.clone())
                    .or_insert_with(|| TagFilterHandles::new(&node_id));
                handles.updates.increment(count);
            }
        }

        for (node_id, op_delta) in delta.operators {
            match op_delta {
                OperatorTelemetry::PrefixFilter {
                    dropped_points,
                    rule_count,
                } => {
                    let handles = self
                        .prefix_filters
                        .entry(node_id.clone())
                        .or_insert_with(|| PrefixFilterHandles::new(&node_id));
                    handles.size.set(rule_count as f64);
                    if dropped_points > 0 {
                        handles.dropped_points.increment(dropped_points);
                    }
                }
                OperatorTelemetry::TimestampSplit {
                    passthrough_metrics,
                    passthrough_flushes,
                } => {
                    let handles = self
                        .timestamp_splits
                        .entry(node_id.clone())
                        .or_insert_with(|| TimestampSplitHandles::new(&node_id));
                    if passthrough_metrics > 0 {
                        handles.passthrough_metrics.increment(passthrough_metrics);
                    }
                    if passthrough_flushes > 0 {
                        handles.passthrough_flushes.increment(passthrough_flushes);
                    }
                }
                OperatorTelemetry::TagFilter {
                    tags_filtered,
                    cache_hits,
                    cache_misses,
                    cache_evictions,
                    rule_count,
                } => {
                    let handles = self
                        .tag_filters
                        .entry(node_id.clone())
                        .or_insert_with(|| TagFilterHandles::new(&node_id));
                    handles.size.set(rule_count as f64);
                    if tags_filtered > 0 {
                        handles.tags_filtered.increment(tags_filtered);
                    }
                    if cache_hits > 0 {
                        handles.cache_hits.increment(cache_hits);
                    }
                    if cache_misses > 0 {
                        handles.cache_misses.increment(cache_misses);
                    }
                    if cache_evictions > 0 {
                        handles.cache_evictions.increment(cache_evictions);
                    }
                }
                OperatorTelemetry::Aggregate {
                    events_received,
                    flushes,
                    flushed_series,
                    flushed_sketches,
                    last_flush_series,
                    last_flush_sketches,
                    last_flush_duration_ns,
                    context_stats,
                } => {
                    let handles = self
                        .aggregates
                        .entry(node_id.clone())
                        .or_insert_with(|| AggregateHandles::new(&node_id));
                    if events_received > 0 {
                        handles.events_received.increment(events_received);
                    }
                    if flushes > 0 {
                        handles.flushes.increment(flushes);
                    }
                    if flushed_series > 0 {
                        handles.series_flushed.increment(flushed_series);
                    }
                    if flushed_sketches > 0 {
                        handles.sketches_flushed.increment(flushed_sketches);
                    }
                    if let Some(series) = last_flush_series {
                        handles.last_flush_series.set(series as f64);
                    }
                    if let Some(sketches) = last_flush_sketches {
                        handles.last_flush_sketches.set(sketches as f64);
                    }
                    if let Some(dur_ns) = last_flush_duration_ns {
                        handles.last_flush_duration_ns.set(dur_ns as f64);
                    }
                    if let Some(stats) = context_stats {
                        let total: usize = stats.by_type.iter().sum();
                        handles.active_contexts.set(total as f64);
                        for idx in 0..stats.by_type.len() {
                            handles.active_contexts_by_type[idx].set(stats.by_type[idx] as f64);
                            handles.active_contexts_bytes_by_type[idx].set(stats.bytes_by_type[idx] as f64);
                        }
                    }
                }
                OperatorTelemetry::PostAggregateFilter { filtered_metrics } => {
                    let handles = self
                        .post_agg_filters
                        .entry(node_id.clone())
                        .or_insert_with(|| PostAggregateFilterHandles::new(&node_id));
                    if filtered_metrics > 0 {
                        handles.filtered_metrics.increment(filtered_metrics);
                    }
                }
            }
        }
    }
}
