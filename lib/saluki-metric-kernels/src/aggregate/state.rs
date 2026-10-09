//! The native aggregation state machine: one entry per context, merged on insert and flushed by closed bucket.

use std::{hash::BuildHasher as _, num::NonZeroU64, time::Duration};

use ddsketch::DDSketch;
use hashbrown::HashTable;
use saluki_core::data_model::event::metric::{context::Context, *};
use smallvec::SmallVec;
use stringtheory::MetaString;
use tracing::{debug, trace};

use super::{
    histogram::{HistogramConfiguration, HistogramStatistic},
    snapshot::{AggregateContextSnapshotEntry, AggregateMetricType},
    telemetry::AggregateTelemetry,
};

/// One context's aggregated values, the metadata of its first insertion, and when it was last updated.
#[derive(Clone)]
pub(super) struct AggregatedMetric {
    values: MetricValues,
    metadata: MetricMetadata,
    last_seen: u64,
}

/// The hasher of the context table: randomly seeded per state, as the native map's is.
type ContextHasher = foldhash::quality::RandomState;

/// The aggregation state: every context held, and what a flush needs to know about previous flushes.
///
/// Contexts are held in a hash table keyed by context identity, its 64-bit hash, so colliding contexts merge into the
/// first one inserted. The table is a [`HashTable`] rather than a map so that a flush can visit its entries one at a
/// time by bucket position and resume between them; positions stay valid because nothing is inserted or removed while a
/// flush is in progress.
pub(super) struct AggregationState {
    contexts: HashTable<(Context, AggregatedMetric)>,
    hasher: ContextHasher,
    contexts_remove_buf: Vec<Context>,
    context_limit: usize,
    bucket_width_secs: NonZeroU64,
    counter_expire_secs: Option<NonZeroU64>,
    last_flush: u64,
    hist_config: HistogramConfiguration,
    telemetry: AggregateTelemetry,
    /// Tracks whether the context limit has been breached. Starts out as `false`. Set to `true` on the first dropped
    /// metric. Reset to `false` when the context count drops below the limit during flush.
    context_limit_breached: bool,
}

/// What a flush computes once, before it visits each context.
pub(super) struct FlushPlan {
    current_time: u64,
    split_timestamp: u64,
    counter_expire_secs: u64,
    zero_value_buckets: SmallVec<[(u64, MetricValues); 4]>,
}

/// A flush in progress: its plan, the position of the next table bucket to visit, and the context whose removal has
/// yet to be queued.
///
/// A context is queued for removal only once its metrics have been emitted and the next context is visited, or the
/// flush finishes, so a flush abandoned while its last context's metrics are being delivered leaves that context
/// queued exactly as the native flush, which stopped at the failed delivery, did.
pub(super) struct FlushCursor {
    plan: FlushPlan,
    next_bucket: usize,
    pending_removal: Option<usize>,
}

impl AggregationState {
    pub(super) fn new(
        bucket_width_secs: NonZeroU64, context_limit: usize, counter_expire_secs: Option<NonZeroU64>,
        hist_config: HistogramConfiguration, telemetry: AggregateTelemetry,
    ) -> Self {
        Self {
            contexts: HashTable::new(),
            hasher: ContextHasher::default(),
            contexts_remove_buf: Vec::new(),
            context_limit,
            bucket_width_secs,
            counter_expire_secs,
            last_flush: 0,
            hist_config,
            telemetry,
            context_limit_breached: false,
        }
    }

    pub(super) fn is_empty(&self) -> bool {
        self.contexts.is_empty()
    }

    pub(super) fn len(&self) -> usize {
        self.contexts.len()
    }

    pub(super) fn context_limit(&self) -> usize {
        self.context_limit
    }

    pub(super) fn context_limit_breached(&self) -> bool {
        self.context_limit_breached
    }

    pub(super) fn telemetry(&self) -> &AggregateTelemetry {
        &self.telemetry
    }

    /// Returns the number of contexts holding values, and the number of points they hold.
    pub(super) fn held_points(&self) -> (usize, usize) {
        self.contexts
            .iter()
            .filter(|(_, aggregated)| !aggregated.values.is_empty())
            .fold((0, 0), |(contexts, points), (_, aggregated)| {
                (contexts + 1, points + aggregated.values.len())
            })
    }

    pub(super) fn snapshot_contexts(&self) -> Vec<AggregateContextSnapshotEntry> {
        let mut snapshot = Vec::with_capacity(self.contexts.len());
        for (context, aggregated) in &self.contexts {
            snapshot.push(AggregateContextSnapshotEntry::new(
                context.clone(),
                AggregateMetricType::from(&aggregated.values),
                aggregated.metadata.unit.clone(),
            ));
        }
        snapshot
    }

    pub(super) fn insert(&mut self, timestamp: u64, metric: Metric) -> bool {
        let hash = self.hasher.hash_one(metric.context());

        // If we haven't seen this context yet, and it would put us over the limit to insert it, then return early.
        if self
            .contexts
            .find(hash, |(context, _)| context == metric.context())
            .is_none()
            && self.contexts.len() >= self.context_limit
        {
            self.context_limit_breached = true;
            return false;
        }

        let (context, mut values, metadata) = metric.into_parts();

        // Collapse all non-timestamped values into a single timestamped value.
        //
        // We do this pre-aggregation step because unless we're merging into an existing context, we'll end up with
        // however many values were in the original metric instead of full aggregated values.
        let bucket_ts = align_to_bucket_start(timestamp, self.bucket_width_secs);
        values.collapse_non_timestamped(bucket_ts);

        trace!(
            bucket_ts,
            kind = values.as_str(),
            "Inserting metric into aggregation state."
        );

        // If we're already tracking this context, update the last seen time and merge the new values into the existing
        // values. Otherwise, create a new entry.
        match self.contexts.find_mut(hash, |(existing, _)| *existing == context) {
            Some((_, aggregated)) => {
                // We ignore metadata changes within a flush interval to keep things simple.
                aggregated.last_seen = timestamp;
                aggregated.values.merge(values);
            }
            None => {
                self.telemetry.increment_contexts(&context, &values);

                let hasher = &self.hasher;
                self.contexts.insert_unique(
                    hash,
                    (
                        context,
                        AggregatedMetric {
                            values,
                            metadata,
                            last_seen: timestamp,
                        },
                    ),
                    |(context, _)| hasher.hash_one(context),
                );

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

    /// Starts a flush at `current_time`: computes the split timestamp and the zero-value buckets for idle counters.
    pub(super) fn start_flush(&self, current_time: u64, flush_open_buckets: bool) -> FlushCursor {
        let bucket_width_secs = self.bucket_width_secs;
        let counter_expire_secs = self.counter_expire_secs.map(|d| d.get()).unwrap_or(0);

        // We want our split timestamp to be before the start of the current bucket, which ensures any timestamp that is
        // less than or equal to `split_timestamp` resides in a closed bucket.
        let split_timestamp = align_to_bucket_start(current_time, bucket_width_secs).saturating_sub(1);

        // Calculate the buckets we need to potentially generate zero-value counters for.
        //
        // We only need to do this if we've flushed before, since we won't have any knowledge of which counters are idle
        // or not until that happens.
        let mut zero_value_buckets = SmallVec::<[(u64, MetricValues); 4]>::new();
        if self.last_flush != 0 {
            let start = align_to_bucket_start(self.last_flush, bucket_width_secs);

            // Clock-skew guards. Bucketing reads the wall clock while the flush cadence is monotonic, so a wall-clock
            // jump is not bounded by the flush interval. A backward jump empties the zero-value range (a silent counter
            // gap); a forward jump makes the loop below run once per bucket across the whole jumped span — O(jump)
            // work and allocation. Assert before the loop so a flood fails fast rather than after the damage is done.
            saluki_antithesis::always_ge!(
                current_time,
                self.last_flush,
                "aggregate flush wall-clock did not move backward",
                { "current_time": current_time, "last_flush": self.last_flush }
            );
            // The 10_000 bound is generous. A default 15s flush over a 10s bucket yields one or two buckets. The bound
            // trips only on a multi-hour wall-clock jump, never on a slow-but-sane flush.
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

            // Anti-vacuity anchor: prove the idle-counter zero-value path actually runs in some timeline.
            saluki_antithesis::sometimes!(
                !zero_value_buckets.is_empty(),
                "aggregate flush generated zero-value counter buckets",
                { "count": zero_value_buckets.len() }
            );
        }

        // Iterate over each context we're tracking, and flush any values that are in buckets which are now closed.
        debug!(timestamp = current_time, "Flushing buckets.");

        FlushCursor {
            plan: FlushPlan {
                current_time,
                split_timestamp,
                counter_expire_secs,
                zero_value_buckets,
            },
            next_bucket: 0,
            pending_removal: None,
        }
    }

    /// Queues the context whose removal the flush deferred, if any.
    fn queue_pending_removal(&mut self, cursor: &mut FlushCursor) {
        if let Some(index) = cursor.pending_removal.take() {
            if let Some((context, am)) = self.contexts.get_bucket(index) {
                self.telemetry.decrement_contexts(context, &am.values);
                self.contexts_remove_buf.push(context.clone());
            }
        }
    }

    /// Flushes the next context of a flush in progress, emitting the metrics of its closed buckets into `sink`.
    ///
    /// Returns `false`, without emitting anything, once every context has been visited.
    pub(super) fn flush_next<F>(&mut self, cursor: &mut FlushCursor, mut sink: F) -> bool
    where
        F: FnMut(Metric),
    {
        self.queue_pending_removal(cursor);

        let plan = &cursor.plan;
        let bucket_width_secs = self.bucket_width_secs;
        let num_buckets = self.contexts.num_buckets();
        while cursor.next_bucket < num_buckets {
            let index = cursor.next_bucket;
            cursor.next_bucket += 1;
            let Some((context, am)) = self.contexts.get_bucket_mut(index) else {
                continue;
            };

            // Figure out if we should remove this metric or not if it has no values in open buckets.
            //
            // We have a special carve-out for counters here, which we have the ability to keep alive after they are
            // flushed, based on a configured expiration period. This allows us to continue emitting a zero value for
            // counters when they're idle, which can make them appear "live" in downstream systems, even when they're
            // not.
            //
            // This is useful for sparsely-updated counters.
            let should_expire_if_empty = match &am.values {
                MetricValues::Counter(..) => {
                    saluki_antithesis::always_le!(
                        am.last_seen,
                        u64::MAX - plan.counter_expire_secs,
                        "aggregate counter expiry add does not overflow",
                        { "last_seen": am.last_seen, "counter_expire_secs": plan.counter_expire_secs }
                    );
                    plan.counter_expire_secs != 0
                        && am.last_seen.saturating_add(plan.counter_expire_secs) < plan.current_time
                }
                _ => true,
            };

            // If we're dealing with a counter, we'll merge in our calculated set of zero values. We only merge in the
            // values that represent now-closed buckets.
            //
            // This is also safe to do even when there are real values in those buckets since adding zero to anything is
            // a no-op from the perspective of what we end up flushing, and it doesn't mess with the "last seen" time.
            if let MetricValues::Counter(..) = &mut am.values {
                let expires_at = am.last_seen.saturating_add(plan.counter_expire_secs);
                for (zv_bucket_start, zero_value) in &plan.zero_value_buckets {
                    if expires_at > *zv_bucket_start {
                        am.values.merge(zero_value.clone());
                    } else {
                        // Since zero-value buckets are in order, we can break early if this bucket is past the
                        // expiration cutoff of the counter. No other bucket will be within the expiration range.
                        break;
                    }
                }
            }

            // Finally, figure out if the current metric can be removed.
            //
            // For any metric with values that are in open buckets, we split off the values that are in closed buckets
            // and keep the metric alive. When all the values are in closed buckets, or there are no values, we'll
            // remove the metric if `should_remove_if_empty` is `true`.
            //
            // This means we'll always remove all-closed/empty non-counter metrics, and we _may_ remove all-closed/empty
            // counters.
            if let Some(closed_bucket_values) = am.values.split_at_timestamp(plan.split_timestamp) {
                self.telemetry.increment_flushed(&closed_bucket_values);

                // We got some closed bucket values, so flush those out.
                expand_flushed(
                    context.clone(),
                    closed_bucket_values,
                    am.metadata.clone(),
                    bucket_width_secs,
                    &self.hist_config,
                    &mut sink,
                );
            }

            if am.values.is_empty() && should_expire_if_empty {
                cursor.pending_removal = Some(index);
            }
            return true;
        }
        false
    }

    /// Completes a flush: removes the contexts it emptied or expired, and records it as the last flush.
    pub(super) fn finish_flush(&mut self, mut cursor: FlushCursor) {
        debug_assert!(
            cursor.next_bucket >= self.contexts.num_buckets(),
            "a flush finished before visiting every context"
        );
        self.queue_pending_removal(&mut cursor);

        // Remove any contexts that were marked as needing to be removed.
        let contexts_len_before = self.contexts.len();
        let hasher = &self.hasher;
        for context in self.contexts_remove_buf.drain(..) {
            let hash = hasher.hash_one(&context);
            if let Ok(entry) = self.contexts.find_entry(hash, |(existing, _)| *existing == context) {
                entry.remove();
            }
        }
        let contexts_len_after = self.contexts.len();

        let contexts_delta = contexts_len_before.saturating_sub(contexts_len_after);
        let target_contexts_capacity = contexts_len_after.saturating_add(contexts_delta / 2);
        self.contexts
            .shrink_to(target_contexts_capacity, |(context, _)| hasher.hash_one(context));

        if self.context_limit_breached && self.contexts.len() < self.context_limit {
            self.context_limit_breached = false;
        }

        self.last_flush = cursor.plan.current_time;
    }
}

/// Emits the metrics one context's closed buckets flush into.
///
/// Counters leave as rates over the bucket width. Histograms expand: first into a distribution, when they are copied
/// to distributions, then into one series per statistic, in order. Every other kind leaves unchanged.
fn expand_flushed<F>(
    context: Context, mut values: MetricValues, metadata: MetricMetadata, bucket_width_secs: NonZeroU64,
    hist_config: &HistogramConfiguration, sink: &mut F,
) where
    F: FnMut(Metric),
{
    let bucket_width = Duration::from_secs(bucket_width_secs.get());

    match values {
        // If we're dealing with a histogram, we calculate a configured set of aggregates/percentiles from it, and emit
        // them as individual metrics.
        MetricValues::Histogram(ref mut points) => {
            // Convert histogram to distribution
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
                let new_metric = Metric::from_parts(metric_context, distribution_values, metadata.clone());
                sink(new_metric);
            }
            // We collect our histogram points in their "summary" view, which sorts the underlying samples allowing
            // proper quantile queries to be answered, hence our "sorted" points. We do it this way because rather than
            // sort every time we insert, or cloning the points, we only sort when a summary view is constructed, which
            // requires mutable access to sort the samples in-place.
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
                let new_metric = Metric::from_parts(new_context, new_values, new_metadata);
                sink(new_metric);
            }
        }

        // If we're not dealing with a histogram, then all we need to worry about is converting counters to rates before
        // forwarding our single, aggregated metric.
        values => {
            let adjusted_values = counter_values_to_rate(values, bucket_width_secs);

            let metric = Metric::from_parts(context, adjusted_values, metadata);
            sink(metric);
        }
    }
}

fn counter_values_to_rate(values: MetricValues, interval_secs: NonZeroU64) -> MetricValues {
    match values {
        MetricValues::Counter(points) => MetricValues::rate(points, Duration::from_secs(interval_secs.get())),
        values => values,
    }
}

pub(super) const fn align_to_bucket_start(timestamp: u64, bucket_width_secs: NonZeroU64) -> u64 {
    timestamp - (timestamp % bucket_width_secs.get())
}

pub(super) const fn is_bucket_closed(
    current_time: u64, bucket_start: u64, bucket_width_secs: NonZeroU64, flush_open_buckets: bool,
) -> bool {
    // A bucket is considered "closed" if the current time is greater than the end of the bucket, or if
    // `flush_open_buckets` is `true`.
    //
    // Buckets represent a half-open interval, where the start is inclusive and the end is exclusive. This means that
    // for a bucket start of 10, and a width of 10, the bucket is 10 seconds "wide", and its start and end are 10 and
    // 20, with the 20 excluded, or [10, 20) in interval notation. Simply put, if we have a timestamp of 10, or anything
    // smaller than 20, we would consider it to fall within the bucket... but 20 or more would be outside of the bucket.
    //
    // We can also represent this visually:
    //
    // <--------- bucket 1 ----------> <--------- bucket 2 ----------> <--------- bucket 3 ---------->
    // [10 11 12 13 14 15 16 17 18 19] [20 21 22 23 24 25 26 27 28 29] [30 31 32 33 34 35 36 37 38 39]
    //
    // We can see that each bucket is 10 seconds wide (10 elements, one for each second), and that their ends are
    // effectively `start + width - 1`. This means that for any of these buckets to be considered "closed", the current
    // time has to be _greater_ than `start + width - 1`. For example, if the current time is 19, then no buckets are
    // closed, and if the current time is 29, then bucket 1 is closed but buckets 2 and 3 are still open, and if the
    // current time is 30, then both buckets 1 and 2 are closed, but bucket 3 is still open.
    (bucket_start + bucket_width_secs.get() - 1) < current_time || flush_open_buckets
}
