//! Pure synchronous metric aggregation kernel (`AggregateCore`).
//!
//! Accumulates incoming metrics into fixed-width time buckets, merges samples per context, synthesizes zero-value
//! buckets for idle counters until expiration, expands histograms into configured scalar statistics and optional
//! distributions, and converts counters to rates on flush.

use std::{num::NonZeroU64, time::Duration};

use ddsketch::DDSketch;
use hashbrown::{hash_map::Entry, HashMap};
use saluki_core::data_model::event::metric::{
    context::Context, Metric, MetricMetadata, MetricValues, ScalarPoints, SketchPoints,
};
use smallvec::SmallVec;
use stringtheory::MetaString;
use tracing::{debug, trace};

use crate::histogram::{HistogramConfiguration, HistogramStatistic};

/// The shape of metric values retained by [`AggregateCore`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AggregateMetricType {
    /// Counter values.
    Counter,

    /// Rate values.
    Rate,

    /// Gauge values.
    Gauge,

    /// Set values.
    Set,

    /// Histogram values.
    Histogram,

    /// Distribution values.
    Distribution,
}

impl From<&MetricValues> for AggregateMetricType {
    fn from(values: &MetricValues) -> Self {
        match values {
            MetricValues::Counter(_) => Self::Counter,
            MetricValues::Rate(_, _) => Self::Rate,
            MetricValues::Gauge(_) => Self::Gauge,
            MetricValues::Set(_) => Self::Set,
            MetricValues::Histogram(_) => Self::Histogram,
            MetricValues::Distribution(_) => Self::Distribution,
        }
    }
}

/// Ordered metric-type label values corresponding to indices in [`AggregateContextStats`].
pub const AGGREGATE_METRIC_TYPE_LABELS: [&str; 6] = ["counter", "gauge", "rate", "set", "histogram", "distribution"];

/// Snapshot of active context counts and estimated context byte sizes grouped by metric type.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct AggregateContextStats {
    /// Active retained context count indexed by [`AGGREGATE_METRIC_TYPE_LABELS`].
    pub by_type: [usize; 6],

    /// Estimated retained context byte size indexed by [`AGGREGATE_METRIC_TYPE_LABELS`].
    pub bytes_by_type: [usize; 6],
}

fn metric_values_type_index(values: &MetricValues) -> usize {
    match values {
        MetricValues::Counter(_) => 0,
        MetricValues::Gauge(_) => 1,
        MetricValues::Rate(_, _) => 2,
        MetricValues::Set(_) => 3,
        MetricValues::Histogram(_) => 4,
        MetricValues::Distribution(_) => 5,
    }
}

/// A retained metric context and its small aggregation metadata.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AggregateContextSnapshotEntry {
    context: Context,
    metric_type: AggregateMetricType,
}

impl AggregateContextSnapshotEntry {
    /// Creates a new snapshot entry.
    pub fn new(context: Context, metric_type: AggregateMetricType) -> Self {
        Self { context, metric_type }
    }

    /// Returns the retained metric context.
    pub fn context(&self) -> &Context {
        &self.context
    }

    /// Returns the shape of the retained metric values.
    pub fn metric_type(&self) -> AggregateMetricType {
        self.metric_type
    }

    /// Creates a snapshot entry for tests and benchmarks.
    pub fn for_test(context: Context, metric_type: AggregateMetricType) -> Self {
        Self::new(context, metric_type)
    }
}

/// One retained context entry inside [`AggregateCore`].
#[derive(Clone)]
pub struct AggregatedMetric<M = ()> {
    /// Accumulated metric values across open and closed buckets.
    pub values: MetricValues,

    /// Metric metadata (unit, origin, etc.).
    pub metadata: MetricMetadata,

    /// Wall-clock timestamp of the most recent sample merged into this entry.
    pub last_seen: u64,

    /// Caller-defined per-context attachment (for example, an origin telemetry handle).
    pub attachment: M,
}

impl<M> AggregatedMetric<M> {
    fn merge(&mut self, timestamp: u64, values: MetricValues, metadata: MetricMetadata) {
        self.last_seen = timestamp;
        self.values.merge(values);

        // The first non-empty unit wins and persists until the entry is removed.
        if self.metadata.unit.is_empty() {
            self.metadata.unit = metadata.unit;
        }
    }
}

/// Precomputed flush window parameters returned by [`AggregateCore::prepare_flush`].
pub struct FlushWindow {
    /// Flush values with timestamp `<= split_timestamp`.
    pub split_timestamp: u64,
    /// Current flush timestamp.
    pub current_time: u64,
    /// Counter expiration duration in seconds (`0` disables keep-alive).
    pub counter_expire_secs: u64,
    /// Zero-value counter buckets synthesized for idle counters since the last flush.
    pub zero_value_buckets: SmallVec<[(u64, MetricValues); 4]>,
}

/// Pure synchronous metric aggregation state machine.
pub struct AggregateCore<M = ()> {
    contexts: HashMap<Context, AggregatedMetric<M>, foldhash::quality::RandomState>,
    contexts_remove_buf: Vec<Context>,
    context_limit: usize,
    bucket_width_secs: NonZeroU64,
    counter_expire_secs: Option<NonZeroU64>,
    last_flush: u64,
    hist_config: HistogramConfiguration,
    context_limit_breached: bool,
}

impl AggregateCore<()> {
    /// Inserts `metric` into the aggregation state at `timestamp`.
    ///
    /// Returns `false` if inserting a new context would exceed `context_limit`.
    pub fn insert(&mut self, timestamp: u64, metric: Metric) -> bool {
        self.insert_with(timestamp, metric, |_, _| ())
    }

    /// Flushes all closed buckets (or all buckets when `flush_open_buckets` is `true`) into `on_metric`.
    pub fn flush_into(&mut self, current_time: u64, flush_open_buckets: bool, mut on_metric: impl FnMut(Metric)) {
        let window = self.prepare_flush(current_time, flush_open_buckets);
        let bucket_width_secs = self.bucket_width_secs;
        let hist_config = self.hist_config.clone();

        for (context, am) in self.contexts.iter_mut() {
            let (closed_values, should_remove) = extract_closed_values(am, &window);
            if let Some(closed_values) = closed_values {
                expand_flushed_metric(
                    context.clone(),
                    closed_values,
                    am.metadata.clone(),
                    bucket_width_secs,
                    &hist_config,
                    &mut on_metric,
                );
            }
            if should_remove {
                self.contexts_remove_buf.push(context.clone());
            }
        }

        self.finish_flush(current_time, |_, _, _| {});
    }
}

impl<M> AggregateCore<M> {
    /// Creates a new `AggregateCore`.
    pub fn new(
        bucket_width_secs: NonZeroU64, context_limit: usize, counter_expiration: Option<Duration>,
        hist_config: HistogramConfiguration,
    ) -> Self {
        let counter_expire_secs = counter_expiration.map(|d| d.as_secs()).and_then(NonZeroU64::new);

        Self {
            contexts: HashMap::default(),
            contexts_remove_buf: Vec::new(),
            context_limit,
            bucket_width_secs,
            counter_expire_secs,
            last_flush: 0,
            hist_config,
            context_limit_breached: false,
        }
    }

    /// Returns `true` when no contexts are retained.
    pub fn is_empty(&self) -> bool {
        self.contexts.is_empty()
    }

    /// Returns the number of currently retained contexts.
    pub fn len(&self) -> usize {
        self.contexts.len()
    }

    /// Returns the configured maximum number of retained contexts.
    pub fn context_limit(&self) -> usize {
        self.context_limit
    }

    /// Returns the configured bucket width in seconds.
    pub fn bucket_width_secs(&self) -> NonZeroU64 {
        self.bucket_width_secs
    }

    /// Returns the configured histogram aggregation settings.
    pub fn hist_config(&self) -> &HistogramConfiguration {
        &self.hist_config
    }

    /// Returns `true` if a metric was dropped due to `context_limit` since the last recovery.
    pub fn context_limit_breached(&self) -> bool {
        self.context_limit_breached
    }

    /// Returns `true` if `context` is currently retained.
    pub fn contains_context(&self, context: &Context) -> bool {
        self.contexts.contains_key(context)
    }

    /// Returns a snapshot of active context counts and estimated byte sizes grouped by metric type.
    pub fn context_stats(&self) -> AggregateContextStats {
        let mut stats = AggregateContextStats::default();
        for (context, aggregated) in &self.contexts {
            let idx = metric_values_type_index(&aggregated.values);
            stats.by_type[idx] += 1;
            stats.bytes_by_type[idx] += context.size_of();
        }
        stats
    }

    /// Returns a snapshot of all currently retained contexts and their metric types.
    pub fn snapshot_contexts(&self) -> Vec<AggregateContextSnapshotEntry> {
        let mut snapshot = Vec::with_capacity(self.contexts.len());
        for (context, aggregated) in &self.contexts {
            snapshot.push(AggregateContextSnapshotEntry::new(
                context.clone(),
                AggregateMetricType::from(&aggregated.values),
            ));
        }
        snapshot
    }

    /// Inserts `metric` at `timestamp`, invoking `on_new_context` only when creating a new context entry.
    ///
    /// Returns `false` if inserting a new context would exceed `context_limit`.
    pub fn insert_with(
        &mut self, timestamp: u64, metric: Metric, on_new_context: impl FnOnce(&Context, &MetricValues) -> M,
    ) -> bool {
        if !self.contexts.contains_key(metric.context()) && self.contexts.len() >= self.context_limit {
            self.context_limit_breached = true;
            return false;
        }

        // An empty state has no counters to keep alive; reset `last_flush` so a newly inserted counter never
        // synthesizes zero buckets from before it existed.
        if self.contexts.is_empty() {
            self.last_flush = 0;
        }

        let (context, mut values, metadata) = metric.into_parts();
        let bucket_ts = align_to_bucket_start(timestamp, self.bucket_width_secs);
        values.collapse_non_timestamped(bucket_ts);

        trace!(
            bucket_ts,
            kind = values.as_str(),
            "Inserting metric into aggregation state."
        );

        match self.contexts.entry(context) {
            Entry::Occupied(mut entry) => entry.get_mut().merge(timestamp, values, metadata),
            Entry::Vacant(entry) => {
                let attachment = on_new_context(entry.key(), &values);

                entry.insert(AggregatedMetric {
                    values,
                    metadata,
                    last_seen: timestamp,
                    attachment,
                });

                saluki_antithesis::always_le!(
                    self.contexts.len(),
                    self.context_limit,
                    "aggregate context map within context_limit",
                    { "len": self.contexts.len(), "limit": self.context_limit }
                );
            }
        }

        true
    }

    /// Prepares a flush pass over retained contexts, computing the split timestamp and idle-counter zero buckets.
    pub fn prepare_flush(&mut self, current_time: u64, flush_open_buckets: bool) -> FlushWindow {
        self.contexts_remove_buf.clear();
        let bucket_width_secs = self.bucket_width_secs;
        let counter_expire_secs = self.counter_expire_secs.map(|d| d.get()).unwrap_or(0);

        let split_timestamp = if flush_open_buckets {
            u64::MAX
        } else {
            align_to_bucket_start(current_time, bucket_width_secs).saturating_sub(1)
        };

        let mut zero_value_buckets = SmallVec::<[(u64, MetricValues); 4]>::new();
        if self.last_flush != 0 {
            let start = align_to_bucket_start(self.last_flush, bucket_width_secs);

            saluki_antithesis::always_ge!(
                current_time,
                self.last_flush,
                "aggregate flush wall-clock did not move backward",
                { "current_time": current_time, "last_flush": self.last_flush }
            );
            saluki_antithesis::always_le!(
                current_time.saturating_sub(self.last_flush) / bucket_width_secs.get(),
                10_000,
                "aggregate zero-value bucket span bounded across a flush",
                {
                    "current_time": current_time,
                    "last_flush": self.last_flush,
                    "bucket_width_secs": bucket_width_secs.get()
                }
            );

            for bucket_start in (start..current_time).step_by(bucket_width_secs.get() as usize) {
                if is_bucket_closed(current_time, bucket_start, bucket_width_secs, flush_open_buckets) {
                    zero_value_buckets.push((bucket_start, MetricValues::counter((bucket_start, 0.0))));
                }
            }

            saluki_antithesis::sometimes!(
                !zero_value_buckets.is_empty(),
                "aggregate flush generated zero-value counter buckets",
                { "count": zero_value_buckets.len() }
            );
        }

        debug!(timestamp = current_time, "Flushing buckets.");

        FlushWindow {
            split_timestamp,
            current_time,
            counter_expire_secs,
            zero_value_buckets,
        }
    }

    /// Returns a mutable iterator over retained contexts and access to the pending-removal buffer for custom flush
    /// drivers (such as async dispatchers).
    pub fn contexts_and_remove_buf_mut(
        &mut self,
    ) -> (
        hashbrown::hash_map::IterMut<'_, Context, AggregatedMetric<M>>,
        &mut Vec<Context>,
    ) {
        (self.contexts.iter_mut(), &mut self.contexts_remove_buf)
    }

    /// Returns the pending-removal buffer from the most recent flush pass.
    pub fn pending_removals(&self) -> &[Context] {
        &self.contexts_remove_buf
    }

    /// Completes a flush pass by removing expired contexts, shrinking the table, and recording `last_flush`.
    pub fn finish_flush(&mut self, current_time: u64, mut on_removed: impl FnMut(&Context, &MetricValues, M)) {
        let contexts_len_before = self.contexts.len();
        for context in self.contexts_remove_buf.drain(..) {
            if let Some(am) = self.contexts.remove(&context) {
                on_removed(&context, &am.values, am.attachment);
            }
        }
        let contexts_len_after = self.contexts.len();

        let contexts_delta = contexts_len_before.saturating_sub(contexts_len_after);
        let target_contexts_capacity = contexts_len_after.saturating_add(contexts_delta / 2);
        self.contexts.shrink_to(target_contexts_capacity);

        if self.context_limit_breached && self.contexts.len() < self.context_limit {
            self.context_limit_breached = false;
        }

        self.last_flush = current_time;
    }
}

/// Applies idle-counter zero synthesis and splits closed bucket values from `am`.
///
/// Returns `(closed_values, should_remove_context)`.
pub fn extract_closed_values<M>(am: &mut AggregatedMetric<M>, window: &FlushWindow) -> (Option<MetricValues>, bool) {
    let should_expire_if_empty = match &am.values {
        MetricValues::Counter(..) => {
            saluki_antithesis::always_le!(
                am.last_seen,
                u64::MAX - window.counter_expire_secs,
                "aggregate counter expiry add does not overflow",
                { "last_seen": am.last_seen, "counter_expire_secs": window.counter_expire_secs }
            );
            window.counter_expire_secs == 0
                || am.last_seen.saturating_add(window.counter_expire_secs) < window.current_time
        }
        _ => true,
    };

    if let MetricValues::Counter(..) = &mut am.values {
        let expires_at = am.last_seen.saturating_add(window.counter_expire_secs);
        for (zv_bucket_start, zero_value) in &window.zero_value_buckets {
            if expires_at > *zv_bucket_start {
                am.values.merge(zero_value.clone());
            } else {
                break;
            }
        }
    }

    let closed_values = am.values.split_at_timestamp(window.split_timestamp);
    let should_remove = am.values.is_empty() && should_expire_if_empty;

    (closed_values, should_remove)
}

/// Transforms closed bucket values for emission (expanding histograms and converting counters to rates).
pub fn expand_flushed_metric(
    context: Context, mut values: MetricValues, metadata: MetricMetadata, bucket_width_secs: NonZeroU64,
    hist_config: &HistogramConfiguration, mut emit: impl FnMut(Metric),
) {
    let bucket_width = Duration::from_secs(bucket_width_secs.get());

    match values {
        MetricValues::Histogram(ref mut points) => {
            if hist_config.copy_to_distribution() {
                let sketch_points = points
                    .into_iter()
                    .map(|(ts, hist)| {
                        let mut sketch = DDSketch::default();
                        for sample in hist.samples() {
                            sketch.insert_n(sample.value.into_inner(), sample.weight.0 as u64);
                        }
                        (ts, sketch)
                    })
                    .collect::<SketchPoints>();
                let distribution_values = MetricValues::distribution(sketch_points);
                let metric_context = if !hist_config.copy_to_distribution_prefix().is_empty() {
                    context.with_name(format!(
                        "{}{}",
                        hist_config.copy_to_distribution_prefix(),
                        context.name()
                    ))
                } else {
                    context.clone()
                };
                emit(Metric::from_parts(
                    metric_context,
                    distribution_values,
                    metadata.clone(),
                ));
            }

            let mut sorted_points = Vec::new();
            for (ts, h) in points {
                sorted_points.push((ts, h.summary_view()));
            }

            for statistic in hist_config.statistics() {
                let new_points = sorted_points
                    .iter()
                    .map(|(ts, hs)| (*ts, statistic.value_from_histogram(hs)))
                    .collect::<ScalarPoints>();

                let new_values = if statistic.is_rate_statistic() {
                    MetricValues::rate(new_points, bucket_width)
                } else {
                    MetricValues::gauge(new_points)
                };

                // Counts are dimensionless, so clear any unit inherited from the input histogram.
                let new_metadata = if matches!(statistic, HistogramStatistic::Count) {
                    metadata.clone().with_unit(MetaString::empty())
                } else {
                    metadata.clone()
                };

                let new_context = context.with_name(format!("{}.{}", context.name(), statistic.suffix()));
                emit(Metric::from_parts(new_context, new_values, new_metadata));
            }
        }
        values => {
            let adjusted_values = counter_values_to_rate(values, bucket_width_secs);
            emit(Metric::from_parts(context, adjusted_values, metadata));
        }
    }
}

/// Converts counter values into rates normalized over `interval_secs`.
pub fn counter_values_to_rate(values: MetricValues, interval_secs: NonZeroU64) -> MetricValues {
    match values {
        MetricValues::Counter(points) => MetricValues::rate(points, Duration::from_secs(interval_secs.get())),
        values => values,
    }
}

/// Aligns `timestamp` down to the start of its `bucket_width_secs` bucket.
pub const fn align_to_bucket_start(timestamp: u64, bucket_width_secs: NonZeroU64) -> u64 {
    timestamp - (timestamp % bucket_width_secs.get())
}

/// Returns whether the half-open bucket `[bucket_start, bucket_start + bucket_width_secs)` is closed at `current_time`.
pub const fn is_bucket_closed(
    current_time: u64, bucket_start: u64, bucket_width_secs: NonZeroU64, flush_open_buckets: bool,
) -> bool {
    (bucket_start + bucket_width_secs.get() - 1) < current_time || flush_open_buckets
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aggregates_counters_into_rates_and_emits_zero_buckets_until_expiry() {
        let width = NonZeroU64::new(10).expect("non-zero");
        let mut core = AggregateCore::<()>::new(
            width,
            100,
            Some(Duration::from_secs(20)),
            HistogramConfiguration::default(),
        );

        assert!(core.insert(12, Metric::counter("hits", 3.0)));
        assert!(core.insert(15, Metric::counter("hits", 7.0)));

        // Flush at t=20 closes bucket [10, 20).
        let mut flushed = Vec::new();
        core.flush_into(20, false, |m| flushed.push(m));
        assert_eq!(flushed.len(), 1);
        assert_eq!(flushed[0].context().name(), "hits");
        assert!(matches!(
            flushed[0].values(),
            MetricValues::Rate(_, interval) if *interval == Duration::from_secs(10)
        ));

        // Idle flush at t=30 emits a zero-value bucket for [20, 30) because last_seen (15) + 20 > 20.
        flushed.clear();
        core.flush_into(30, false, |m| flushed.push(m));
        assert_eq!(flushed.len(), 1);
        assert!(!core.is_empty());

        // Flush at t=50 expires the counter.
        flushed.clear();
        core.flush_into(50, false, |m| flushed.push(m));
        assert!(core.is_empty());
    }

    #[test]
    fn enforces_context_limit_and_recovers_after_flush() {
        let width = NonZeroU64::new(10).expect("non-zero");
        let mut core = AggregateCore::<()>::new(width, 1, None, HistogramConfiguration::default());

        assert!(core.insert(10, Metric::gauge("a", 1.0)));
        assert!(!core.insert(10, Metric::gauge("b", 2.0)));
        assert!(core.context_limit_breached());

        let mut flushed = Vec::new();
        core.flush_into(20, false, |m| flushed.push(m));
        assert_eq!(flushed.len(), 1);
        assert!(!core.context_limit_breached());
        assert!(core.insert(20, Metric::gauge("b", 2.0)));
    }
}
