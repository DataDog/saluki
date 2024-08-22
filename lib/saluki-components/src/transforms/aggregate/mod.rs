use std::{collections::hash_map::Entry, time::Duration};

use ahash::{AHashMap, AHashSet};
use async_trait::async_trait;
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_config::GenericConfiguration;
use saluki_context::Context;
use saluki_core::{
    components::{transforms::*, MetricsBuilder},
    pooling::{FixedSizeObjectPool, ObjectPool as _},
    topology::{interconnect::EventBuffer, OutputDefinition},
};
use saluki_env::time::get_unix_timestamp;
use saluki_error::GenericError;
use saluki_event::{metric::*, DataType, Event};
use serde::Deserialize;
use tokio::{select, time::interval_at};
use tracing::{debug, error, trace};

const EVENT_BUFFER_POOL_SIZE: usize = 8;

const fn default_window_duration() -> Duration {
    Duration::from_secs(10)
}

const fn default_flush_interval() -> Duration {
    Duration::from_secs(15)
}

const fn default_context_limit() -> usize {
    1000
}

const fn default_counter_expiry_seconds() -> u64 {
    300
}

const fn default_forward_timestamped_metrics() -> bool {
    true
}

/// Aggregate transform.
///
/// Aggregates metrics into fixed-size windows, flushing them at a regular interval.
///
/// ## Zero-value counters
///
/// When metrics are aggregated and then flushed, they are typically removed entirely from the aggregation state. Unless
/// they are updated again, they will not be emitted again. However, for counters, a slightly different approach is
/// taken by tracking "zero-value" counters.
///
/// Counters are aggregated and flushed normally. However, when flushed, counters are added to a list of "zero-value"
/// counters, and if those counters are not updated again, the transform emits a copy of the counter with a value of
/// zero. It does this until the counter is updated again, or the zero-value counter expires (no updates), whichever
/// comes first.
///
/// This provides a continuity in the output of a counter, from the perspective of a downstream system, when counters
/// are otherwise sparse. The expiration period is configurable, and allows a trade-off in how sparse/infrequent the
/// updates to counters can be versus how long it takes for counters that don't exist anymore to actually cease to be
/// emitted.
#[derive(Deserialize)]
pub struct AggregateConfiguration {
    /// Size of the aggregation window.
    ///
    /// Metrics are aggregated into fixed-size windows, such that all updates to the same metric within a window are
    /// aggregated into a single metric. The window size controls how efficiently metrics are aggregated, and in turn,
    /// how many data points are emitted downstream.
    ///
    /// Defaults to 10 seconds.
    #[serde(rename = "aggregate_window_duration", default = "default_window_duration")]
    window_duration: Duration,

    /// How often to flush buckets.
    ///
    /// This represents a trade-off between the savings in network bandwidth (sending fewer requests to downstream
    /// systems, etc) and the frequency of updates (how often updates to a metric are emitted).
    ///
    /// Defaults to 15 seconds.
    #[serde(rename = "aggregate_flush_interval", default = "default_flush_interval")]
    flush_interval: Duration,

    /// Maximum number of contexts to aggregate per window.
    ///
    /// A context is the unique combination of a metric name and its set of tags. For example,
    /// `metric.name.here{tag1=A,tag2=B}` represents a single context, and would be different than
    /// `metric.name.here{tag1=A,tag2=C}`.
    ///
    /// When the maximum number of contexts is reached in the current aggregation window, additional metrics are dropped
    /// until the next window starts.
    ///
    /// Defaults to 1000.
    #[serde(rename = "aggregate_context_limit", default = "default_context_limit")]
    context_limit: usize,

    /// Whether to flush open buckets when stopping the transform.
    ///
    /// Normally, open buckets (a bucket whose end has not yet occurred) are not flushed when the transform is stopped.
    /// This is done to avoid the chance of flushing a partial window, restarting the process, and then flushing the
    /// same window again. Downstream systems sometimes cannot cope with this gracefully, as there is no way to
    /// determine that it is an incremental update, and so they treat it as an absolute update, overwriting the
    /// previously flushed value.
    ///
    /// In cases where flushing all outstanding data is paramount, this can be enabled.
    ///
    /// Defaults to `false`.
    #[serde(rename = "aggregate_flush_open_windows", default)]
    flush_open_windows: bool,

    /// How long to keep zero-value counters after they've been flushed, in seconds.
    ///
    /// When a counter is flushed, it's reset to zero. During the next flush, if the counter has not been updated since,
    /// it will be flushed with a zero value. This is done to provide continuity in the output of counter metrics to
    /// avoid breaks in the time series when updates are sparse.
    ///
    /// After a counter has been idle (no updates) for the expiry period, though, it will be removed and no longer
    /// emitted until it is updated again.
    ///
    /// Defaults to 300 seconds (5 minutes).
    #[serde(alias = "dogstatsd_expiry_seconds", default = "default_counter_expiry_seconds")]
    counter_expiry_seconds: u64,

    /// Whether or not to immediately forward metrics with pre-defined timestamps.
    ///
    /// When enabled, this causes the aggregator to immediately forward metrics that already have a timestamp present.
    /// Only metrics without a timestamp will be aggregated. This can be useful when metrics are already pre-aggregated
    /// client-side and both timeliness and memory efficiency are paramount, as it avoids the overhead of aggregating
    /// within the pipeline.
    ///
    /// Defaults to `true`.
    #[serde(
        rename = "dogstatsd_no_aggregation_pipeline",
        default = "default_forward_timestamped_metrics"
    )]
    forward_timestamped_metrics: bool,
}

impl AggregateConfiguration {
    /// Creates a new `AggregateConfiguration` from the given configuration.
    pub fn from_configuration(config: &GenericConfiguration) -> Result<Self, GenericError> {
        Ok(config.as_typed()?)
    }

    /// Creates a new `AggregateConfiguration` with default values.
    pub fn with_defaults() -> Self {
        Self {
            window_duration: default_window_duration(),
            flush_interval: default_flush_interval(),
            context_limit: default_context_limit(),
            flush_open_windows: false,
            counter_expiry_seconds: default_counter_expiry_seconds(),
            forward_timestamped_metrics: default_forward_timestamped_metrics(),
        }
    }
}

#[async_trait]
impl TransformBuilder for AggregateConfiguration {
    async fn build(&self) -> Result<Box<dyn Transform + Send>, GenericError> {
        Ok(Box::new(Aggregate {
            window_duration: self.window_duration,
            flush_interval: self.flush_interval,
            context_limit: self.context_limit,
            flush_open_windows: self.flush_open_windows,
            counter_expiry_seconds: self.counter_expiry_seconds,
            forward_timestamped_metrics: self.forward_timestamped_metrics,
        }))
    }

    fn input_data_type(&self) -> DataType {
        DataType::Metric
    }

    fn outputs(&self) -> &[OutputDefinition] {
        static OUTPUTS: &[OutputDefinition] = &[OutputDefinition::default_output(DataType::Metric)];

        OUTPUTS
    }
}

impl MemoryBounds for AggregateConfiguration {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        // TODO: We should be able to better account for multiple buckets based on figuring out the maximum number of
        // buckets based on the window duration and flush interval, and whether or not we're configured to forward
        // timestamped metrics. Essentially, if we're not aggregating metrics with timestamps, then they can only be
        // inserted into the current bucket... which then means our upper bound on the bucket count is simply a factor
        // of the window duration and flush interval.

        // Since we use our own event buffer pool, we account for that directly here, and we use our knowledge of the
        // context limit to determine how large we'd expect those event buffers to grow to in the worst case. With a
        // context limit of N, we would only aggregate N metrics at any given time, and thus we should flush a maximum
        // of N metrics per flush interval.
        let event_buffer_pool_size = EVENT_BUFFER_POOL_SIZE * self.context_limit * std::mem::size_of::<Event>();

        builder
            .minimum()
            // Capture the size of the heap allocation when the component is built.
            .with_single_value::<Aggregate>();
        builder
            .firm()
            .with_fixed_amount(event_buffer_pool_size)
            // Account for our context limiter map, which is just a `HashSet`.
            .with_array::<Context>(self.context_limit)
            // Account for the actual aggregation state map, where we map contexts to the merged metric.
            //
            // TODO: Similar to the above TODO, we're not accounting for late buckets here.
            .with_map::<Context, (MetricValue, MetricMetadata)>(self.context_limit)
            // Account for our zero-value counter tracking, which could be as large as the context limit.
            .with_map::<Context, (u64, MetricMetadata)>(self.context_limit);
    }
}

pub struct Aggregate {
    window_duration: Duration,
    flush_interval: Duration,
    context_limit: usize,
    flush_open_windows: bool,
    counter_expiry_seconds: u64,
    forward_timestamped_metrics: bool,
}

#[async_trait]
impl Transform for Aggregate {
    async fn run(mut self: Box<Self>, mut context: TransformContext) -> Result<(), ()> {
        let mut health = context.take_health_handle();

        let mut state = AggregationState::new(
            self.window_duration,
            self.context_limit,
            Duration::from_secs(self.counter_expiry_seconds),
        );

        let mut flush = interval_at(tokio::time::Instant::now() + self.flush_interval, self.flush_interval);

        let metrics_builder = MetricsBuilder::from_component_context(context.component_context());
        let events_dropped =
            metrics_builder.register_counter_with_labels("component_events_dropped_total", &[("intentional", "true")]);

        // Create our own event buffer pool.
        //
        // We do this because aggregation often leads to a high amount of cardinality, where we're flushing a lot more
        // events per event buffer. If we use the global event buffer pool, we risk churning through many event buffers,
        // having them reserve a lot of underlying capacity, and then having a ton of event buffers in the pool with
        // high capacity when we only need one every few seconds, etc.
        let event_buffer_pool = FixedSizeObjectPool::<EventBuffer>::with_capacity(EVENT_BUFFER_POOL_SIZE);

        health.mark_ready();
        debug!("Aggregation transform started.");

        let mut final_flush = false;

        loop {
            let mut unaggregated_events = 0;
            let mut flushed_events = 0;

            let mut event_buffer = event_buffer_pool.acquire().await;
            debug!(buf_cap = event_buffer.capacity(), "Acquired event buffer.");

            select! {
                _ = health.live() => continue,
                _ = flush.tick() => {
                    // We've reached the end of the current window. Flush our aggregation state and forward the metrics
                    // onwards. Regardless of whether any metrics were aggregated, we always update the aggregation
                    // state to track the start time of the current aggregation window.
                    if !state.is_empty() {
                        debug!("Flushing aggregated metrics...");

                        let should_flush_open_windows = final_flush && self.flush_open_windows;

                        let event_buffer_len = event_buffer.len();
                        state.flush(get_unix_timestamp(), should_flush_open_windows, &mut event_buffer);

                        flushed_events = event_buffer.len() - event_buffer_len;
                    }

                    // If this is the final flush, we break out of the loop.
                    if final_flush {
                        debug!("All aggregation complete.");
                        break
                    }
                },
                maybe_events = context.event_stream().next(), if !final_flush => match maybe_events {
                    Some(events) => {
                        trace!(events_len = events.len(), "Received events.");

                        let event_buffer_len = event_buffer.len();
                        let current_time = get_unix_timestamp();

                        for event in events {
                            if let Some(metric) = event.try_into_metric() {
                                if self.forward_timestamped_metrics && metric.metadata().timestamp().is_some() {
                                    event_buffer.push(Event::Metric(metric));
                                } else if !state.insert(current_time, metric) {
                                    trace!("Dropping metric due to context limit.");
                                    events_dropped.increment(1);
                                }
                            }
                        }

                        unaggregated_events = event_buffer.len() - event_buffer_len;
                    },
                    None => {
                        // We've reached the end of our input stream, so mark ourselves for a final flush and reset the
                        // interval so it ticks immediately on the next loop iteration.
                        final_flush = true;

                        flush.reset_immediately();

                        debug!("Aggregation transform stopping...");
                    }
                },
            }

            if !event_buffer.is_empty() {
                if let Err(e) = context.forwarder().forward(event_buffer).await {
                    error!(error = %e, "Failed to forward events.");
                    return Err(());
                }

                debug!(unaggregated_events, flushed_events, "Forwarded events.");
            }
        }

        debug!("Aggregation transform stopped.");

        Ok(())
    }
}

struct Bucket {
    start: u64,
    width: Duration,
    contexts: AHashMap<Context, (MetricValue, MetricMetadata)>,
}

impl Bucket {
    fn new(start: u64, width: Duration) -> Self {
        Self {
            start,
            width,
            contexts: AHashMap::default(),
        }
    }

    const fn start(&self) -> u64 {
        self.start
    }

    const fn end(&self) -> u64 {
        self.start + self.width.as_secs() - 1
    }

    fn is_closed(&self, current_time: u64, flush_open_buckets: bool) -> bool {
        // A bucket is considered "closed" if the current time is greater than the end of the bucket, or if
        // `flush_open_buckets` is `true`.
        //
        // Buckets represent a half-open interval, where the start is inclusive and the end is exclusive. This means
        // that for a bucket start of 10, and a width of 10, the bucket is 10 seconds "wide", and its start and end are
        // 10 and 20, with the 20 excluded, or [10, 20) in interval notation. Simply put, if we have a timestamp of 10,
        // or anything smaller than 20, we would consider it to fall within the bucket... but 20 or more would be
        // outside of the bucket.
        //
        // We can also represent this visually:
        //
        // <--------- bucket 1 ----------> <--------- bucket 2 ----------> <--------- bucket 3 ---------->
        // [10 11 12 13 14 15 16 17 18 19] [20 21 22 23 24 25 26 27 28 29] [30 31 32 33 34 35 36 37 38 39]
        //
        // We can see that each bucket is 10 seconds wide (10 elements, one for each second), and that their ends are
        // effectively `start + width - 1`. This means that for any of these buckets to be considered "closed", the
        // current time has to be _greater_ than `start + width - 1`. For example, if the current time is 19, then no
        // buckets are closed, and if the current time is 29, then bucket 1 is closed but buckets 2 and 3 are still
        // open, and if the current time is 30, then both buckets 1 and 2 are closed, but bucket 3 is still open.
        self.end() < current_time || flush_open_buckets
    }

    fn insert(&mut self, metric: Metric) {
        let bucket_start = self.start();

        let (metric_context, metric_value, mut metric_metadata) = metric.into_parts();
        match self.contexts.entry(metric_context) {
            Entry::Occupied(mut entry) => {
                let (existing_value, _) = entry.get_mut();
                existing_value.merge(metric_value);
            }
            Entry::Vacant(entry) => {
                // Set the metric's timestamp to the start of the bucket.
                metric_metadata.set_timestamp(bucket_start);

                entry.insert((metric_value, metric_metadata));
            }
        }
    }

    fn into_parts(self) -> (u64, u64, AHashMap<Context, (MetricValue, MetricMetadata)>) {
        (self.start, self.end(), self.contexts)
    }
}

struct AggregationState {
    contexts: AHashSet<Context>,
    context_limit: usize,

    buckets: Vec<Bucket>,
    bucket_width: Duration,

    zero_value_counters: AHashMap<Context, (u64, MetricMetadata)>,
    counter_expiry_duration: Duration,
    last_flush: u64,
}

impl AggregationState {
    fn new(bucket_width: Duration, context_limit: usize, counter_expiry_duration: Duration) -> Self {
        Self {
            contexts: AHashSet::default(),
            context_limit,
            buckets: Vec::with_capacity(2),
            bucket_width,
            zero_value_counters: AHashMap::default(),
            counter_expiry_duration,
            last_flush: 0,
        }
    }

    fn is_empty(&self) -> bool {
        self.contexts.is_empty()
    }

    fn get_or_create_bucket(&mut self, timestamp: u64) -> &mut Bucket {
        let bucket_start = align_to_bucket_start(timestamp, self.bucket_width);
        match self.buckets.iter_mut().position(|bucket| bucket.start() == bucket_start) {
            Some(idx) => &mut self.buckets[idx],
            None => {
                self.buckets.push(Bucket::new(bucket_start, self.bucket_width));
                self.buckets.last_mut().expect("bucket was just pushed")
            }
        }
    }

    fn insert(&mut self, timestamp: u64, metric: Metric) -> bool {
        // If we haven't seen this context yet, track it.
        if !self.contexts.contains(metric.context()) {
            if self.contexts.len() >= self.context_limit {
                return false;
            }

            self.contexts.insert(metric.context().clone());
        }

        // Find the bucket this metric belongs in, creating it if necessary, and then insert it into that bucket.
        let metric_timestamp = metric.metadata().timestamp().unwrap_or(timestamp);
        let bucket = self.get_or_create_bucket(metric_timestamp);
        bucket.insert(metric);

        true
    }

    fn flush(&mut self, current_time: u64, flush_open_buckets: bool, event_buffer: &mut EventBuffer) {
        let zero_value = MetricValue::rate_seconds(0.0, self.bucket_width);
        let counter_expiry_secs = self.counter_expiry_duration.as_secs();
        let mut zero_value_counters_flushed = 0;

        debug!(
            buckets_len = self.buckets.len(),
            timestamp = current_time,
            "Flushing buckets."
        );

        // Before iterating over our buckets, sort them from oldest to newest. This provides us the ability to more
        // efficiently handle zero-value counter updates since we can just track last seen timestamps against the bucket
        // start.
        self.buckets.sort_unstable_by_key(|bucket| bucket.start());

        // Create a list of all possible buckets that could exist between the last flush and now, which will let us
        // figure out which buckets we need to emit zero-value counters for when there's no activity.
        let mut possible_buckets = Vec::new();

        let mut i = 0;
        while i < self.buckets.len() {
            if self.buckets[i].is_closed(current_time, flush_open_buckets) {
                // Consume the bucket since it's closed and needs to be flushed.
                let bucket = self.buckets.remove(i);
                let (bucket_start, bucket_end, contexts) = bucket.into_parts();

                debug!(bucket_start, bucket_len = contexts.len(), "Flushing bucket.");

                for (context, (value, metadata)) in contexts {
                    // If this is a counter metric, handle tracking it for the purpose of zero-value counters.
                    if let MetricValue::Counter { .. } = &value {
                        // We use the bucket end as the last seen timestamp because we don't know if the counter was
                        // added at the very beginning of the bucket, or the end of it, so by using the bucket end, we
                        // ensure that we don't premature expire zero-value counters... even if it means we keep them
                        // around a little longer than necessary.
                        if let Some((last_seen, _)) = self.zero_value_counters.get_mut(&context) {
                            *last_seen = bucket_end;
                        } else {
                            self.zero_value_counters
                                .insert(context.clone(), (bucket_end, metadata.clone()));
                        }
                    } else {
                        // Remove the context from our tracked contexts since it's now going away. We'll handle removing
                        // contexts from expired zero-value counters further down.
                        //
                        // TODO: This isn't necessarily correct since a subsequent bucket that isn't closed yet could
                        // still contain this context. We need more of a reference counting mechanism.
                        self.contexts.remove(&context);
                    }

                    // Convert any counters to rates, so that we can properly account for their aggregated status.
                    let value = match value {
                        MetricValue::Counter { value } => MetricValue::rate_seconds(value, self.bucket_width),
                        _ => value,
                    };
                    let metric = Metric::from_parts(context, value, metadata);
                    event_buffer.push(Event::Metric(metric));
                }

                // For any zero-value counters we're tracking, that weren't seen in this bucket, emit a zero-value data
                // point for them.
                for (context, (last_seen, metadata)) in &self.zero_value_counters {
                    // The counter's last seen should be equal to the current bucket's end if we actually observed it in
                    // this bucket, especially since we iterate over the buckets from oldest to newest.
                    if *last_seen != bucket_end {
                        // Update our timestamp to coincide with the start of the bucket.
                        let metadata = metadata.clone().with_timestamp(bucket_start);
                        let metric = Metric::from_parts(context.clone(), zero_value.clone(), metadata);
    
                        event_buffer.push(Event::Metric(metric));
    
                        zero_value_counters_flushed += 1;
                    }
                }
            } else {
                i += 1;
            }
        }

        if zero_value_counters_flushed > 0 {
            debug!(num_flushed = zero_value_counters_flushed, "Flushed zero-value counters.");
        }

        // Now we take a final pass over all tracked zero-value counters to see if any of them have completely expired,
        // and if so, we'll remove them.
        let mut counters_to_remove = Vec::new();

        for (context, (last_seen, _)) in &self.zero_value_counters {
            if *last_seen + counter_expiry_secs < current_time {
                counters_to_remove.push(context.clone());
            }
        }
        if !counters_to_remove.is_empty() {
            let num_removed = counters_to_remove.len();
            for context in counters_to_remove {
                self.zero_value_counters.remove(&context);
                self.contexts.remove(&context);
            }

            debug!(num_removed, "Removed expired zero-value counters.");
        }
    }
}

const fn align_to_bucket_start(timestamp: u64, bucket_width: Duration) -> u64 {
    timestamp - (timestamp % bucket_width.as_secs())
}

// TODO: Some of these tests have the potential, I believe, to spuriously fail if they're executed when the current
// timestamp happens to align with the end of a bucket... even if it's extremely unlikely. We should _probably_ think
// about flushing out our idea to create a time provider in `saluki-env` so that time can be mocked out in tests.
#[cfg(test)]
mod tests {
    use saluki_context::{ContextRef, ContextResolver};
    use saluki_core::pooling::helpers::get_pooled_object_via_default;

    use super::*;

    const BUCKET_WIDTH_SECS: u64 = 10;
    const BUCKET_WIDTH: Duration = Duration::from_secs(BUCKET_WIDTH_SECS);
    const FIRST_INSERT_TS: u64 = BUCKET_WIDTH_SECS - 1;
    const FIRST_FLUSH_TS: u64 = BUCKET_WIDTH_SECS;
    const SECOND_INSERT_TS: u64 = (BUCKET_WIDTH_SECS * 2) - 1;
    const SECOND_FLUSH_TS: u64 = BUCKET_WIDTH_SECS * 2;
    const THIRD_INSERT_TS: u64 = (BUCKET_WIDTH_SECS * 3) - 1;
    const THIRD_FLUSH_TS: u64 = BUCKET_WIDTH_SECS * 3;

    fn get_event_buffer() -> EventBuffer {
        get_pooled_object_via_default::<EventBuffer>()
    }

    fn get_flushed_metrics(timestamp: u64, state: &mut AggregationState) -> Vec<Metric> {
        let mut event_buffer = get_event_buffer();
        state.flush(timestamp, true, &mut event_buffer);

        let mut metrics = event_buffer
            .into_iter()
            .filter_map(|event| event.try_into_metric())
            .collect::<Vec<_>>();
        metrics.sort_by(|a, b| a.context().name().cmp(b.context().name()));
        metrics
    }

    fn create_metric(name: &str, value: MetricValue) -> Metric {
        const EMPTY_TAGS: &[&str] = &[];

        let resolver: ContextResolver = ContextResolver::with_noop_interner();
        let context_ref = ContextRef::from_name_and_tags(name, EMPTY_TAGS);
        let context = resolver.resolve(context_ref).unwrap();

        Metric::from_parts(context, value, MetricMetadata::default())
    }

    fn create_counter(name: &str, value: f64) -> Metric {
        create_metric(name, MetricValue::Counter { value })
    }

    fn create_gauge(name: &str, value: f64) -> Metric {
        create_metric(name, MetricValue::Gauge { value })
    }

    #[test]
    fn is_bucket_closed_with_and_without_flush_open_buckets() {
        // Cases are defined as:
        // (current time, bucket start, bucket width, flush open buckets, expected result)
        let cases = [
            // Bucket goes from [995, 1005), current time of 1000, so bucket is open.
            (1000, 995, 10, false, false),
            (1000, 995, 10, true, true),
            // Bucket goes from [1000, 1010), current time of 1000, so bucket is open.
            (1000, 1000, 10, false, false),
            (1000, 1000, 10, true, true),
            // Bucket goes from [1000, 1010), current time of 1010, so bucket is closed.
            (1010, 1000, 10, false, true),
            (1010, 1000, 10, true, true),
        ];

        for (current_time, bucket_start, bucket_width, flush_open_buckets, expected) in cases {
            let expected_reason = if expected {
                "closed, was open"
            } else {
                "open, was closed"
            };

            let bucket = Bucket::new(bucket_start, Duration::from_secs(bucket_width));

            assert_eq!(
                bucket.is_closed(current_time, flush_open_buckets),
                expected,
                "expected bucket to be {} (current_time={}, bucket_start={}, bucket_width={}, flush_open_buckets={})",
                expected_reason,
                current_time,
                bucket_start,
                bucket_width,
                flush_open_buckets
            );
        }
    }

    #[test]
    fn context_limit() {
        // Create our aggregation state with a context limit of 2.
        let mut state = AggregationState::new(BUCKET_WIDTH, 2, Duration::from_secs(300));

        // Create four unique gauges, and insert all of them. The third and fourth should fail because we've reached
        // the context limit.
        let metric1 = create_gauge("metric1", 1.0);
        let metric2 = create_gauge("metric2", 2.0);
        let metric3 = create_gauge("metric3", 3.0);
        let metric4 = create_gauge("metric4", 4.0);

        assert!(state.insert(FIRST_INSERT_TS, metric1.clone()));
        assert!(state.insert(FIRST_INSERT_TS, metric2.clone()));
        assert!(!state.insert(FIRST_INSERT_TS, metric3.clone()));
        assert!(!state.insert(FIRST_INSERT_TS, metric4.clone()));

        // We should only see the first two gauges after flushing.
        let metrics = get_flushed_metrics(FIRST_FLUSH_TS, &mut state);
        assert_eq!(metrics.len(), 2);
        assert_eq!(metrics[0].context(), metric1.context());
        assert_eq!(metrics[1].context(), metric2.context());

        // We should be able to insert the third and fourth gauges now as the first two have been flushed, and along
        // with them, their contexts should no longer be tracked in the aggregation state:
        assert!(state.insert(SECOND_INSERT_TS, metric3.clone()));
        assert!(state.insert(SECOND_INSERT_TS, metric4.clone()));

        let metrics = get_flushed_metrics(SECOND_FLUSH_TS, &mut state);
        assert_eq!(metrics.len(), 2);
        assert_eq!(metrics[0].context(), metric3.context());
        assert_eq!(metrics[1].context(), metric4.context());
    }

    #[test]
    fn context_limit_with_zero_value_counters() {
        // We test here to ensure that zero-value counters contribute to the context limit.
        let mut state = AggregationState::new(BUCKET_WIDTH, 2, Duration::from_secs(300));

        // Create two unique counters, and insert both of them.
        let metric1 = create_counter("metric1", 1.0);
        let metric2 = create_counter("metric2", 2.0);

        assert!(state.insert(FIRST_INSERT_TS, metric1.clone()));
        assert!(state.insert(FIRST_INSERT_TS, metric2.clone()));

        // Flush the aggregation state, and observe they're both present.
        let metrics = get_flushed_metrics(FIRST_FLUSH_TS, &mut state);
        assert_eq!(metrics.len(), 2);
        assert_eq!(metrics[0].context(), metric1.context());
        assert_eq!(
            metrics[0].value(),
            &MetricValue::rate_seconds(1.0, BUCKET_WIDTH),
        );
        assert_eq!(metrics[1].context(), metric2.context());
        assert_eq!(
            metrics[1].value(),
            &MetricValue::rate_seconds(2.0, BUCKET_WIDTH),
        );

        // Flush _again_ to ensure that we then emit zero-value variants for both counters.
        let metrics = get_flushed_metrics(SECOND_FLUSH_TS, &mut state);
        assert_eq!(metrics.len(), 2);
        assert_eq!(metrics[0].context(), metric1.context());
        assert_eq!(
            metrics[0].value(),
            &MetricValue::rate_seconds(0.0, BUCKET_WIDTH),
        );
        assert_eq!(metrics[1].context(), metric2.context());
        assert_eq!(
            metrics[1].value(),
            &MetricValue::rate_seconds(0.0, BUCKET_WIDTH),
        );

        // Now try to insert a third counter, which should fail because we've reached the context limit.
        let metric3 = create_counter("metric3", 3.0);
        assert!(!state.insert(THIRD_INSERT_TS, metric3.clone()));

        // Flush the aggregation state, and observe that we only see the two original counters.
        let metrics = get_flushed_metrics(THIRD_FLUSH_TS, &mut state);
        assert_eq!(metrics.len(), 2);
        assert_eq!(metrics[0].context(), metric1.context());
        assert_eq!(
            metrics[0].value(),
            &MetricValue::rate_seconds(0.0, BUCKET_WIDTH),
        );
        assert_eq!(metrics[1].context(), metric2.context());
        assert_eq!(
            metrics[1].value(),
            &MetricValue::rate_seconds(0.0, BUCKET_WIDTH),
        );
    }
}
