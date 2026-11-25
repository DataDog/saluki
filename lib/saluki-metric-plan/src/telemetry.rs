//! Pure per-operator telemetry delta types emitted by [`crate::engine::FusedMetricEngine`] and
//! [`crate::generations::GenerationManager`].
//!
//! Keeping telemetry as plain integer counters and snapshots inside `saluki-metric-plan` keeps the engine completely
//! decoupled from any metrics backend (such as `saluki-metrics`) while allowing the outer transform host to publish
//! RAR-compatible metrics once per batch or flush tick.

use saluki_metric_kernels::aggregate::AggregateContextStats;

use crate::ir::NodeId;

/// Accumulated counters and gauges for a single compiled operator since the last telemetry drain.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OperatorTelemetry {
    /// Metrics dropped and rule count for an `Op::PrefixFilter` operator.
    PrefixFilter {
        /// Number of points dropped by the blocklist since the last drain.
        dropped_points: u64,

        /// Current compiled blocklist size.
        rule_count: usize,
    },

    /// No-aggregation passthrough counts for an `Op::TimestampSplit` operator.
    TimestampSplit {
        /// Number of timestamped metrics routed to passthrough since the last drain.
        passthrough_metrics: u64,

        /// Number of passthrough batches emitted since the last drain.
        passthrough_flushes: u64,
    },

    /// Tag filter counts, cache statistics, and rule count for an `Op::TagFilter` operator.
    TagFilter {
        /// Number of individual tags removed or replaced since the last drain.
        tags_filtered: u64,

        /// Per-context cache hits since the last drain.
        cache_hits: u64,

        /// Per-context cache misses since the last drain.
        cache_misses: u64,

        /// Per-context cache evictions since the last drain.
        cache_evictions: u64,

        /// Current compiled whole-tag + tag-value rule count.
        rule_count: usize,
    },

    /// Ingestion, flush, and active-context telemetry for an `Op::Aggregate` operator.
    Aggregate {
        /// Number of metrics received by this aggregate operator since the last drain.
        events_received: u64,

        /// Number of flushes completed since the last drain.
        flushes: u64,

        /// Number of series metrics flushed since the last drain.
        flushed_series: u64,

        /// Number of sketch (distribution) metrics flushed since the last drain.
        flushed_sketches: u64,

        /// Number of series emitted during the most recent flush, if a flush occurred.
        last_flush_series: Option<u64>,

        /// Number of sketches emitted during the most recent flush, if a flush occurred.
        last_flush_sketches: Option<u64>,

        /// Wall-clock duration in nanoseconds of the most recent flush, if a flush occurred.
        last_flush_duration_ns: Option<u64>,

        /// Active context counts and estimated byte sizes by metric type, refreshed on flush.
        context_stats: Option<AggregateContextStats>,
    },

    /// Metrics dropped by an `Op::PostAggregateFilter` operator.
    PostAggregateFilter {
        /// Number of post-aggregate metrics filtered since the last drain.
        filtered_metrics: u64,
    },
}

/// Drained per-operator telemetry and live-reload update counts for a metric region.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RegionTelemetryDelta {
    /// Per-operator counter deltas and gauge snapshots keyed by plan node ID.
    pub operators: Vec<(NodeId, OperatorTelemetry)>,

    /// Live reconfiguration counts for `Op::PrefixFilter` nodes keyed by node ID.
    pub filterlist_updates: Vec<(NodeId, u64)>,

    /// Live reconfiguration counts for `Op::TagFilter` nodes keyed by node ID.
    pub tag_filterlist_updates: Vec<(NodeId, u64)>,
}

impl RegionTelemetryDelta {
    /// Returns `true` when there are no operator deltas or reload update counts to publish.
    pub fn is_empty(&self) -> bool {
        self.operators.is_empty() && self.filterlist_updates.is_empty() && self.tag_filterlist_updates.is_empty()
    }
}
