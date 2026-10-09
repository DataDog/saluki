//! A frozen copy of native aggregation as it was before its state machine moved into this crate.
//!
//! [`AggregationState`] and the functions after it are the original `saluki-components` aggregation state machine,
//! copied verbatim with only their imports and visibility adjusted, and with snapshot entries built through their
//! constructor. [`RunLoop`] holds the decisions the original `Aggregate::run` loop made around it, copied from the
//! loop's tick and event branches without the logging, health, and channel handling around them. `config` and
//! `telemetry` are the original histogram configuration and telemetry. The property tests compare [`AggregateCore`]
//! with them, so that the core keeps the native behavior while the legacy transform calls the core. Do not change
//! them.
//!
//! [`AggregateCore`]: super::AggregateCore

use std::{num::NonZeroU64, time::Duration};

use ddsketch::DDSketch;
use hashbrown::{hash_map::Entry, HashMap};
use saluki_core::{
    data_model::event::{
        metric::{context::Context, *},
        Event,
    },
    topology::{interconnect::BufferedDispatcher, EventsBuffer},
};
use saluki_error::GenericError;
use smallvec::SmallVec;
use stringtheory::MetaString;
use tracing::{debug, trace};

pub(super) mod config;
use self::config::{HistogramConfiguration, HistogramStatistic};

pub(super) mod telemetry;
use self::telemetry::Telemetry;
use super::snapshot::{AggregateContextSnapshotEntry, AggregateMetricType};

/// The decisions the original `Aggregate::run` loop made around the state machine.
pub(super) struct RunLoop {
    pub(super) state: AggregationState,
    pub(super) telemetry: Telemetry,
    pub(super) flush_open_windows: bool,
}

impl RunLoop {
    /// The loop's event branch, for one received buffer whose arrival time was `current_time`.
    ///
    /// Returns whether each metric was inserted, and whether the loop warned that the context limit was reached.
    pub(super) fn events(&mut self, current_time: u64, events: impl IntoIterator<Item = Event>) -> (Vec<bool>, bool) {
        let mut inserted = Vec::new();
        let mut warned = false;
        for event in events {
            if let Some(metric) = event.try_into_metric() {
                let was_breached = self.state.context_limit_breached();
                let accepted = self.state.insert(current_time, metric);
                if !accepted {
                    trace!("Dropping metric due to context limit.");
                    if !was_breached {
                        // First drop since the last recovery — emit a single warning.
                        warned = true;
                    }
                    self.telemetry.increment_events_dropped();
                }
                inserted.push(accepted);
            }
        }
        (inserted, warned)
    }

    /// The loop's tick branch at `now`, with `final_primary_flush` set for the final tick.
    ///
    /// Returns `None` if the state was empty, and otherwise whether the flush recovered from a breach.
    pub(super) async fn tick(
        &mut self, now: u64, final_primary_flush: bool, dispatcher: &mut BufferedDispatcher<'_, EventsBuffer>,
    ) -> Option<Result<bool, GenericError>> {
        if !self.state.is_empty() {
            let should_flush_open_windows = final_primary_flush && self.flush_open_windows;

            // Remember if the context limit had been surpassed before this flush.
            let was_breached = self.state.context_limit_breached();

            let result = self.state.flush(now, should_flush_open_windows, dispatcher).await;

            self.telemetry.increment_flushes();

            // If flush recovered us from a breach, log the recovery.
            Some(result.map(|()| was_breached && !self.state.context_limit_breached()))
        } else {
            None
        }
    }
}

#[derive(Clone)]
struct AggregatedMetric {
    values: MetricValues,
    metadata: MetricMetadata,
    last_seen: u64,
}

pub(super) struct AggregationState {
    contexts: HashMap<Context, AggregatedMetric, foldhash::quality::RandomState>,
    contexts_remove_buf: Vec<Context>,
    context_limit: usize,
    bucket_width_secs: NonZeroU64,
    counter_expire_secs: Option<NonZeroU64>,
    last_flush: u64,
    hist_config: HistogramConfiguration,
    telemetry: Telemetry,
    /// Tracks whether the context limit has been breached. Starts out as `false`. Set to `true` on the first dropped
    /// metric. Reset to `false` when the context count drops below the limit during flush.
    context_limit_breached: bool,
}

impl AggregationState {
    pub(super) fn new(
        bucket_width_secs: NonZeroU64, context_limit: usize, counter_expiration: Option<Duration>,
        hist_config: HistogramConfiguration, telemetry: Telemetry,
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
            telemetry,
            context_limit_breached: false,
        }
    }

    pub(super) fn is_empty(&self) -> bool {
        self.contexts.is_empty()
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
        // If we haven't seen this context yet, and it would put us over the limit to insert it, then return early.
        if !self.contexts.contains_key(metric.context()) && self.contexts.len() >= self.context_limit {
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
        match self.contexts.entry(context) {
            Entry::Occupied(mut entry) => {
                let aggregated = entry.get_mut();

                // We ignore metadata changes within a flush interval to keep things simple.
                aggregated.last_seen = timestamp;
                aggregated.values.merge(values);
            }
            Entry::Vacant(entry) => {
                self.telemetry.increment_contexts(entry.key(), &values);

                entry.insert(AggregatedMetric {
                    values,
                    metadata,
                    last_seen: timestamp,
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

    pub(super) async fn flush(
        &mut self, current_time: u64, flush_open_buckets: bool, dispatcher: &mut BufferedDispatcher<'_, EventsBuffer>,
    ) -> Result<(), GenericError> {
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
            // gap); a forward jump makes the loop below run once per bucket across the whole jumped span — O(jump) work
            // and allocation. Assert before the loop so a flood fails fast rather than after the damage is done.
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

        for (context, am) in self.contexts.iter_mut() {
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
                        u64::MAX - counter_expire_secs,
                        "aggregate counter expiry add does not overflow",
                        { "last_seen": am.last_seen, "counter_expire_secs": counter_expire_secs }
                    );
                    counter_expire_secs != 0 && am.last_seen.saturating_add(counter_expire_secs) < current_time
                }
                _ => true,
            };

            // If we're dealing with a counter, we'll merge in our calculated set of zero values. We only merge in the
            // values that represent now-closed buckets.
            //
            // This is also safe to do even when there are real values in those buckets since adding zero to anything is
            // a no-op from the perspective of what we end up flushing, and it doesn't mess with the "last seen" time.
            if let MetricValues::Counter(..) = &mut am.values {
                let expires_at = am.last_seen.saturating_add(counter_expire_secs);
                for (zv_bucket_start, zero_value) in &zero_value_buckets {
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
            if let Some(closed_bucket_values) = am.values.split_at_timestamp(split_timestamp) {
                self.telemetry.increment_flushed(&closed_bucket_values);

                // We got some closed bucket values, so flush those out.
                transform_and_push_metric(
                    context.clone(),
                    closed_bucket_values,
                    am.metadata.clone(),
                    bucket_width_secs,
                    &self.hist_config,
                    dispatcher,
                )
                .await?;
            }

            if am.values.is_empty() && should_expire_if_empty {
                self.telemetry.decrement_contexts(context, &am.values);
                self.contexts_remove_buf.push(context.clone());
            }
        }

        // Remove any contexts that were marked as needing to be removed.
        let contexts_len_before = self.contexts.len();
        for context in self.contexts_remove_buf.drain(..) {
            self.contexts.remove(&context);
        }
        let contexts_len_after = self.contexts.len();

        let contexts_delta = contexts_len_before.saturating_sub(contexts_len_after);
        let target_contexts_capacity = contexts_len_after.saturating_add(contexts_delta / 2);
        self.contexts.shrink_to(target_contexts_capacity);

        if self.context_limit_breached && self.contexts.len() < self.context_limit {
            self.context_limit_breached = false;
        }

        self.last_flush = current_time;

        Ok(())
    }

    pub(super) fn context_limit_breached(&self) -> bool {
        self.context_limit_breached
    }
}

async fn transform_and_push_metric(
    context: Context, mut values: MetricValues, metadata: MetricMetadata, bucket_width_secs: NonZeroU64,
    hist_config: &HistogramConfiguration, dispatcher: &mut BufferedDispatcher<'_, EventsBuffer>,
) -> Result<(), GenericError> {
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
                dispatcher.push(Event::Metric(new_metric)).await?;
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
                dispatcher.push(Event::Metric(new_metric)).await?;
            }

            Ok(())
        }

        // If we're not dealing with a histogram, then all we need to worry about is converting counters to rates before
        // forwarding our single, aggregated metric.
        values => {
            let adjusted_values = counter_values_to_rate(values, bucket_width_secs);

            let metric = Metric::from_parts(context, adjusted_values, metadata);
            dispatcher.push(Event::Metric(metric)).await
        }
    }
}

fn counter_values_to_rate(values: MetricValues, interval_secs: NonZeroU64) -> MetricValues {
    match values {
        MetricValues::Counter(points) => MetricValues::rate(points, Duration::from_secs(interval_secs.get())),
        values => values,
    }
}

const fn align_to_bucket_start(timestamp: u64, bucket_width_secs: NonZeroU64) -> u64 {
    timestamp - (timestamp % bucket_width_secs.get())
}

const fn is_bucket_closed(
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
