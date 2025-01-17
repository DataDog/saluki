use std::{
    num::NonZeroU64,
    time::{Duration, Instant},
};

use async_trait::async_trait;
use hashbrown::{hash_map::Entry, HashMap};
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder, UsageExpr};
use saluki_config::GenericConfiguration;
use saluki_context::Context;
use saluki_core::{
    components::{transforms::*, ComponentContext},
    observability::ComponentMetricsExt as _,
    pooling::{ElasticObjectPool, ObjectPool},
    topology::{
        interconnect::{BufferedForwarder, FixedSizeEventBuffer, FixedSizeEventBufferInner, Forwarder},
        OutputDefinition,
    },
};
use saluki_env::time::get_unix_timestamp;
use saluki_error::GenericError;
use saluki_event::{metric::*, DataType, Event};
use saluki_metrics::MetricsBuilder;
use serde::Deserialize;
use smallvec::SmallVec;
use tokio::{
    select,
    time::{interval, interval_at},
};
use tracing::{debug, error, trace};

mod telemetry;
use self::telemetry::Telemetry;

mod config;
use self::config::HistogramConfiguration;

const PASSTHROUGH_EVENT_BUFFERS_MAX: usize = 16;

const fn default_window_duration() -> Duration {
    Duration::from_secs(10)
}

const fn default_primary_flush_interval() -> Duration {
    Duration::from_secs(15)
}

const fn default_context_limit() -> usize {
    5000
}

const fn default_counter_expiry_seconds() -> Option<u64> {
    Some(300)
}

const fn default_passthrough_timestamped_metrics() -> bool {
    true
}

const fn default_passthrough_idle_flush_timeout() -> Duration {
    Duration::from_secs(1)
}

const fn default_passthrough_event_buffer_len() -> usize {
    2048
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
    #[serde(rename = "aggregate_flush_interval", default = "default_primary_flush_interval")]
    primary_flush_interval: Duration,

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

    /// How long to keep idle counters alive after they've been flushed, in seconds.
    ///
    /// When metrics are flushed, they are removed from the aggregation state. However, if a counter expiration is set,
    /// counters will be kept alive in an "idle" state. For as long as a counter is idle, but not yet expired, a zero
    /// value will be emitted for it during each flush. This allows more gracefully handling sparse counters, where
    /// updates are infrequent but leaving gaps in the time series would be undesirable from a user experience
    /// perspective.
    ///
    /// After a counter has been idle (no updates) for longer than the expiry period, it will be completely removed and
    /// no further zero values will be emitted.
    ///
    /// Defaults to 300 seconds (5 minutes). Setting a value of `0` disables idle counter keep-alive.
    #[serde(alias = "dogstatsd_expiry_seconds", default = "default_counter_expiry_seconds")]
    counter_expiry_seconds: Option<u64>,

    /// Whether or not to immediately forward (passthrough) metrics with pre-defined timestamps.
    ///
    /// When enabled, this causes the aggregator to immediately forward metrics that already have a timestamp present.
    /// Only metrics without a timestamp will be aggregated. This can be useful when metrics are already pre-aggregated
    /// client-side and both timeliness and memory efficiency are paramount, as it avoids the overhead of aggregating
    /// within the pipeline.
    ///
    /// Defaults to `true`.
    #[serde(
        rename = "dogstatsd_no_aggregation_pipeline",
        default = "default_passthrough_timestamped_metrics"
    )]
    passthrough_timestamped_metrics: bool,

    /// How often to flush buffered passthrough metrics.
    ///
    /// While passthrough metrics are not re-aggregated by the transform, they will still be temporarily buffered in
    /// order to optimize the efficiency of processing them in the next component. This setting controls the maximum
    /// amount of time that passthrough metrics will be buffered before being forwarded.
    ///
    /// Defaults to 2 seconds.
    #[serde(
        rename = "aggregate_passthrough_idle_flush_timeout",
        default = "default_passthrough_idle_flush_timeout"
    )]
    passthrough_idle_flush_timeout: Duration,

    /// Length of event buffers used exclusive for passthrough metrics.
    ///
    /// While passthrough metrics are not re-aggregated by the transform, they will still be temporarily buffered in
    /// order to optimize the efficiency of processing them in the next component. This setting controls the maximum
    /// number of passthrough metrics that can be buffered in a single batch before being forwarded.
    ///
    /// Defaults to 2048.
    #[serde(
        rename = "dogstatsd_no_aggregation_pipeline_batch_size",
        default = "default_passthrough_event_buffer_len"
    )]
    passthrough_event_buffer_len: usize,

    /// Histogram aggregation configuration.
    ///
    /// Controls the aggregates/percentiles that are generated for distributions in "histogram" mode (client-side
    /// distribution aggregation).
    #[serde(flatten)]
    hist_config: HistogramConfiguration,
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
            primary_flush_interval: default_primary_flush_interval(),
            context_limit: default_context_limit(),
            flush_open_windows: false,
            counter_expiry_seconds: default_counter_expiry_seconds(),
            passthrough_timestamped_metrics: default_passthrough_timestamped_metrics(),
            passthrough_idle_flush_timeout: default_passthrough_idle_flush_timeout(),
            passthrough_event_buffer_len: default_passthrough_event_buffer_len(),
            hist_config: HistogramConfiguration::default(),
        }
    }
}

#[async_trait]
impl TransformBuilder for AggregateConfiguration {
    async fn build(&self, context: ComponentContext) -> Result<Box<dyn Transform + Send>, GenericError> {
        let metrics_builder = MetricsBuilder::from_component_context(context);
        let telemetry = Telemetry::new(&metrics_builder);

        let state = AggregationState::new(
            self.window_duration,
            self.context_limit,
            self.counter_expiry_seconds.filter(|s| *s != 0).map(Duration::from_secs),
            self.hist_config.clone(),
            telemetry.clone(),
        );

        Ok(Box::new(Aggregate {
            state,
            telemetry,
            primary_flush_interval: self.primary_flush_interval,
            flush_open_windows: self.flush_open_windows,
            passthrough_timestamped_metrics: self.passthrough_timestamped_metrics,
            passthrough_idle_flush_timeout: self.passthrough_idle_flush_timeout,
            passthrough_event_buffer_len: self.passthrough_event_buffer_len,
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
        // TODO: While we account for the aggregation state map accurately, what we don't currently account for is the
        // fact that a metric could have multiple distinct values. For the common pipeline of metrics in via DogStatsD,
        // this generally shouldn't be a problem because the values don't have a timestamp, so they get aggregated into
        // the same bucket, leading to two values per `MetricValues` at most, which is already baked into the size of
        // `MetricValues` due to using `SmallVec`.
        //
        // However, there could be many more values in a single metric, and we don't account for that.

        let passthrough_event_buffer_min_elements = self.passthrough_event_buffer_len;
        let passthrough_event_buffer_max_elements =
            self.passthrough_event_buffer_len * (PASSTHROUGH_EVENT_BUFFERS_MAX - 1);

        builder
            .minimum()
            // Capture the size of the heap allocation when the component is built.
            .with_single_value::<Aggregate>("component struct")
            .with_array::<Event>(
                "passthrough event buffer pool (minimum)",
                passthrough_event_buffer_min_elements,
            );
        builder
            .firm()
            // Account for the aggregation state map, where we map contexts to the merged metric.
            .with_expr(UsageExpr::product(
                "aggregation state map",
                UsageExpr::sum(
                    "context map entry",
                    UsageExpr::struct_size::<Context>("context"),
                    UsageExpr::struct_size::<AggregatedMetric>("aggregated metric"),
                ),
                UsageExpr::config("aggregate_context_limit", self.context_limit),
            ))
            // Upper bound of our passthrough event buffer object pool.
            .with_array::<Event>("passthrough event buffer pool", passthrough_event_buffer_max_elements);
    }
}

pub struct Aggregate {
    state: AggregationState,
    telemetry: Telemetry,
    primary_flush_interval: Duration,
    flush_open_windows: bool,
    passthrough_timestamped_metrics: bool,
    passthrough_idle_flush_timeout: Duration,
    passthrough_event_buffer_len: usize,
}

#[async_trait]
impl Transform for Aggregate {
    async fn run(mut self: Box<Self>, mut context: TransformContext) -> Result<(), GenericError> {
        let mut health = context.take_health_handle();

        let mut primary_flush = interval_at(
            tokio::time::Instant::now() + self.primary_flush_interval,
            self.primary_flush_interval,
        );
        let mut final_primary_flush = false;

        let passthrough_flush = interval(Duration::from_secs(2));
        let mut passthrough_batcher = PassthroughBatcher::new(
            self.passthrough_event_buffer_len,
            self.passthrough_idle_flush_timeout,
            self.telemetry.clone(),
        )
        .await;

        health.mark_ready();
        debug!("Aggregation transform started.");

        tokio::pin!(passthrough_flush);

        loop {
            select! {
                _ = health.live() => continue,
                _ = primary_flush.tick() => {
                    // We've reached the end of the current window. Flush our aggregation state and forward the metrics
                    // onwards. Regardless of whether any metrics were aggregated, we always update the aggregation
                    // state to track the start time of the current aggregation window.
                    if !self.state.is_empty() {
                        debug!("Flushing aggregated metrics...");

                        let should_flush_open_windows = final_primary_flush && self.flush_open_windows;

                        let mut forwarder = context.forwarder().buffered().expect("default output should always exist");
                        if let Err(e) = self.state.flush(get_unix_timestamp(), should_flush_open_windows, &mut forwarder).await {
                            error!(error = %e, "Failed to flush aggregation state.");
                        }

                        self.telemetry.increment_flushes();

                        match forwarder.flush().await {
                            Ok(aggregated_events) => debug!(aggregated_events, "Forwarded events."),
                            Err(e) => error!(error = %e, "Failed to flush aggregated events."),
                        }
                    }

                    // If this is the final flush, we break out of the loop.
                    if final_primary_flush {
                        debug!("All aggregation complete.");
                        break
                    }
                },
                _ = passthrough_flush.tick() => passthrough_batcher.try_flush(context.forwarder()).await,
                maybe_events = context.event_stream().next(), if !final_primary_flush => match maybe_events {
                    Some(events) => {
                        trace!(events_len = events.len(), "Received events.");

                        let current_time = get_unix_timestamp();
                        let mut processed_passthrough_metrics = false;

                        for event in events {
                            if let Some(metric) = event.try_into_metric() {
                                let metric = if self.passthrough_timestamped_metrics {
                                    // Try splitting out any timestamped values, and if we have any, we'll buffer them
                                    // separately and process the remaining nontimestamped metric (if any) by
                                    // aggregating it like normal.
                                    let (maybe_timestamped_metric, maybe_nontimestamped_metric) = try_split_timestamped_values(metric);

                                    // If we have a timestamped metric, then batch it up out-of-band.
                                    if let Some(timestamped_metric) = maybe_timestamped_metric {
                                        passthrough_batcher.push_metric(timestamped_metric, context.forwarder()).await;
                                        processed_passthrough_metrics = true;
                                    }

                                    // If we have an nontimestamped metric, we'll process it like normal.
                                    //
                                    // Otherwise, continue to the next event.
                                    match maybe_nontimestamped_metric {
                                        Some(metric) => metric,
                                        None => continue,
                                    }
                                } else {
                                    metric
                                };

                                if !self.state.insert(current_time, metric) {
                                    trace!("Dropping metric due to context limit.");
                                    self.telemetry.increment_events_dropped();
                                }
                            }
                        }

                        if processed_passthrough_metrics {
                            passthrough_batcher.update_last_processed_at();
                        }
                    },
                    None => {
                        // We've reached the end of our input stream, so mark ourselves for a final flush and reset the
                        // interval so it ticks immediately on the next loop iteration.
                        final_primary_flush = true;
                        primary_flush.reset_immediately();

                        debug!("Aggregation transform stopping...");
                    }
                },
            }
        }

        // Do a final flush of any timestamped metrics that we've buffered up.
        passthrough_batcher.try_flush(context.forwarder()).await;

        debug!("Aggregation transform stopped.");

        Ok(())
    }
}

fn try_split_timestamped_values(mut metric: Metric) -> (Option<Metric>, Option<Metric>) {
    if metric.values().all_timestamped() {
        (Some(metric), None)
    } else if metric.values().any_timestamped() {
        // Only _some_ of the values are timestamped, so we'll split the timestamped values into a new metric.
        let new_metric_values = metric.values_mut().split_timestamped();
        let new_metric = Metric::from_parts(metric.context().clone(), new_metric_values, metric.metadata().clone());

        (Some(new_metric), Some(metric))
    } else {
        // No timestamped values, so we need to aggregate this metric.
        (None, Some(metric))
    }
}

struct PassthroughBatcher {
    buffer_pool: ElasticObjectPool<FixedSizeEventBuffer>,
    active_buffer: FixedSizeEventBuffer,
    active_buffer_start: Instant,
    last_processed_at: Instant,
    idle_flush_timeout: Duration,
    telemetry: Telemetry,
}

impl PassthroughBatcher {
    async fn new(event_buffer_len: usize, idle_flush_timeout: Duration, telemetry: Telemetry) -> Self {
        let (buffer_pool, pool_shrinker) = ElasticObjectPool::<FixedSizeEventBuffer>::with_builder(
            "agg_passthrough_event_buffers",
            1,
            PASSTHROUGH_EVENT_BUFFERS_MAX,
            move || FixedSizeEventBufferInner::with_capacity(event_buffer_len),
        );
        tokio::spawn(pool_shrinker);

        let active_buffer = buffer_pool.acquire().await;

        Self {
            buffer_pool,
            active_buffer,
            active_buffer_start: Instant::now(),
            last_processed_at: Instant::now(),
            idle_flush_timeout,
            telemetry,
        }
    }

    async fn push_metric(&mut self, metric: Metric, forwarder: &Forwarder) {
        // Try pushing the metric into our active buffer.
        //
        // If our active buffer is full, then we'll flush the buffer, grab a new one, and push the metric into it.
        if let Some(event) = self.active_buffer.try_push(Event::Metric(metric)) {
            debug!("Passthrough event buffer was full. Flushing...");
            self.forward_events(forwarder).await;

            if self.active_buffer.try_push(event).is_some() {
                error!("Event buffer is full even after forwarding events. Dropping event.");
            }
        }

        // If this is the first metric in the buffer, we've started a new batch, so track when it started.
        if self.active_buffer.len() == 1 {
            self.active_buffer_start = Instant::now();
        }

        self.telemetry.increment_passthrough_metrics();
    }

    fn update_last_processed_at(&mut self) {
        // We expose this as a standalone method, rather than just doing it automatically in `push_metric`, because
        // otherwise we might be calling this 10-20K times per second, instead of simply doing it after the end of each
        // input event buffer in the transform's main loop, which should be much less frequent.
        self.last_processed_at = Instant::now();
    }

    async fn try_flush(&mut self, forwarder: &Forwarder) {
        // If our active buffer isn't empty, and we've exceeded our idle flush timeout, then flush the buffer.
        if !self.active_buffer.is_empty() && self.last_processed_at.elapsed() >= self.idle_flush_timeout {
            debug!("Passthrough processing exceeded idle flush timeout. Flushing...");

            self.forward_events(forwarder).await;
        }
    }

    async fn forward_events(&mut self, forwarder: &Forwarder) {
        if !self.active_buffer.is_empty() {
            let unaggregated_events = self.active_buffer.len();

            // Track how long this batch was alive for.
            let batch_duration = self.active_buffer_start.elapsed();
            self.telemetry.record_passthrough_batch_duration(batch_duration);

            self.telemetry.increment_passthrough_flushes();

            // Swap our active buffer with a new, empty one, and then forward the old one.
            let new_active_buffer = self.buffer_pool.acquire().await;
            let old_active_buffer = std::mem::replace(&mut self.active_buffer, new_active_buffer);

            match forwarder.forward_buffer(old_active_buffer).await {
                Ok(()) => debug!(unaggregated_events, "Forwarded events."),
                Err(e) => error!(error = %e, "Failed to flush unaggregated events."),
            }
        }
    }
}

#[derive(Clone)]
struct AggregatedMetric {
    values: MetricValues,
    metadata: MetricMetadata,
    last_seen: u64,
}

struct AggregationState {
    contexts: HashMap<Context, AggregatedMetric, ahash::RandomState>,
    contexts_remove_buf: Vec<Context>,
    context_limit: usize,
    bucket_width_secs: u64,
    counter_expire_secs: Option<NonZeroU64>,
    last_flush: u64,
    hist_config: HistogramConfiguration,
    telemetry: Telemetry,
}

impl AggregationState {
    fn new(
        bucket_width: Duration, context_limit: usize, counter_expiration: Option<Duration>,
        hist_config: HistogramConfiguration, telemetry: Telemetry,
    ) -> Self {
        let counter_expire_secs = counter_expiration.map(|d| d.as_secs()).and_then(NonZeroU64::new);

        Self {
            contexts: HashMap::default(),
            contexts_remove_buf: Vec::new(),
            context_limit,
            bucket_width_secs: bucket_width.as_secs(),
            counter_expire_secs,
            last_flush: 0,
            hist_config,
            telemetry,
        }
    }

    fn is_empty(&self) -> bool {
        self.contexts.is_empty()
    }

    fn insert(&mut self, timestamp: u64, metric: Metric) -> bool {
        // If we haven't seen this context yet, and it would put us over the limit to insert it, then return early.
        if !self.contexts.contains_key(metric.context()) && self.contexts.len() >= self.context_limit {
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
                self.telemetry.increment_contexts(&values);

                entry.insert(AggregatedMetric {
                    values,
                    metadata,
                    last_seen: timestamp,
                });
            }
        }

        true
    }

    async fn flush<O>(
        &mut self, current_time: u64, flush_open_buckets: bool, forwarder: &mut BufferedForwarder<'_, O>,
    ) -> Result<(), GenericError>
    where
        O: ObjectPool<Item = FixedSizeEventBuffer>,
    {
        self.contexts_remove_buf.clear();

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

            for bucket_start in (start..current_time).step_by(bucket_width_secs as usize) {
                if is_bucket_closed(current_time, bucket_start, bucket_width_secs, flush_open_buckets) {
                    zero_value_buckets.push((bucket_start, MetricValues::counter((bucket_start, 0.0))));
                }
            }
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
                    counter_expire_secs != 0 && am.last_seen + counter_expire_secs < current_time
                }
                _ => true,
            };

            // If we're dealing with a counter, we'll merge in our calculated set of zero values. We only merge in the
            // values that represent now-closed buckets.
            //
            // This is also safe to do even when there are real values in those buckets since adding zero to anything is
            // a no-op from the perspective of what we end up flushing, and it doesn't mess with the "last seen" time.
            if let MetricValues::Counter(..) = &mut am.values {
                let expires_at = am.last_seen + counter_expire_secs;
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
                // We got some closed bucket values, so flush those out.
                transform_and_push_metric(
                    context.clone(),
                    closed_bucket_values,
                    am.metadata.clone(),
                    bucket_width_secs,
                    &self.hist_config,
                    forwarder,
                )
                .await?;
            }

            if am.values.is_empty() && should_expire_if_empty {
                self.telemetry.decrement_contexts(&am.values);
                self.contexts_remove_buf.push(context.clone());
            }
        }

        // Remove any contexts that were marked as needing to be removed.
        for context in &self.contexts_remove_buf {
            self.contexts.remove(context);
        }

        self.last_flush = current_time;

        Ok(())
    }
}

async fn transform_and_push_metric<O>(
    context: Context, mut values: MetricValues, metadata: MetricMetadata, bucket_width_secs: u64,
    hist_config: &HistogramConfiguration, forwarder: &mut BufferedForwarder<'_, O>,
) -> Result<(), GenericError>
where
    O: ObjectPool<Item = FixedSizeEventBuffer>,
{
    match values {
        // If we're dealing with a histogram, we calculate a configured set of aggregates/percentiles from it, and emit
        // them as individual metrics.
        MetricValues::Histogram(ref mut points) => {
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
                    MetricValues::rate(new_points, Duration::from_secs(bucket_width_secs))
                } else {
                    MetricValues::gauge(new_points)
                };

                let new_context = context.with_name(format!("{}.{}", context.name(), statistic.suffix()));
                let new_metric = Metric::from_parts(new_context, new_values, metadata.clone());
                forwarder.push(Event::Metric(new_metric)).await?;
            }

            Ok(())
        }

        // If we're not dealing with a histogram, then all we need to worry about is converting counters to rates before
        // forwarding our single, aggregated metric.
        values => {
            let adjusted_values = match values {
                MetricValues::Counter(values) => MetricValues::rate(values, Duration::from_secs(bucket_width_secs)),
                values => values,
            };

            let metric = Metric::from_parts(context, adjusted_values, metadata);
            forwarder.push(Event::Metric(metric)).await
        }
    }
}

const fn align_to_bucket_start(timestamp: u64, bucket_width_secs: u64) -> u64 {
    timestamp - (timestamp % bucket_width_secs)
}

const fn is_bucket_closed(
    current_time: u64, bucket_start: u64, bucket_width_secs: u64, flush_open_buckets: bool,
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
    (bucket_start + bucket_width_secs - 1) < current_time || flush_open_buckets
}

// TODO: One thing we ought to consider is a property test, specifically a state machine property test, where we
// generate a randomized offset to start time from, a bucket width, flush interval, and operations, and so on... and
// then we run it to make sure that we are always generating sequential timestamps for data points, etc.
#[cfg(test)]
mod tests {
    use float_cmp::ApproxEqRatio as _;
    use saluki_core::{
        components::ComponentContext,
        pooling::ElasticObjectPool,
        topology::{
            interconnect::{FixedSizeEventBufferInner, Forwarder},
            ComponentId, OutputName,
        },
    };
    use saluki_metrics::test::TestRecorder;
    use tokio::sync::mpsc;

    use super::config::HistogramStatistic;
    use super::*;

    const BUCKET_WIDTH_SECS: u64 = 10;
    const BUCKET_WIDTH: Duration = Duration::from_secs(BUCKET_WIDTH_SECS);
    const COUNTER_EXPIRE_SECS: u64 = 20;
    const COUNTER_EXPIRE: Option<Duration> = Some(Duration::from_secs(COUNTER_EXPIRE_SECS));

    /// Gets the bucket start timestamp for the given step.
    const fn bucket_ts(step: u64) -> u64 {
        align_to_bucket_start(insert_ts(step), BUCKET_WIDTH_SECS)
    }

    /// Gets the insert timestamp for the given step.
    const fn insert_ts(step: u64) -> u64 {
        (BUCKET_WIDTH_SECS * (step + 1)) - 2
    }

    /// Gets the flush timestamp for the given step.
    const fn flush_ts(step: u64) -> u64 {
        BUCKET_WIDTH_SECS * (step + 1)
    }

    async fn get_flushed_metrics(timestamp: u64, state: &mut AggregationState) -> Vec<Metric> {
        // Create the forwarder that we'll use to flush the metrics into.
        //
        // NOTE: This is more involved than it really ought to be, but alas.
        let (sender, mut receiver) = mpsc::channel(1);

        let (object_pool, _) =
            ElasticObjectPool::with_builder("test", 1, 1, || FixedSizeEventBufferInner::with_capacity(8));
        let component_id = ComponentId::try_from("test").expect("should not fail to create component ID");
        let mut forwarder = Forwarder::new(ComponentContext::transform(component_id), object_pool);
        forwarder.add_output(OutputName::Default, sender);

        let mut buffered_forwarder = forwarder.buffered().expect("default output should always exist");

        // Flush the metrics to an event buffer.
        state
            .flush(timestamp, true, &mut buffered_forwarder)
            .await
            .expect("should not fail to flush aggregation state");

        // Flush our buffered forwarder, which should ensure that the event buffer is sent out, and then read it from the
        // receiver:
        buffered_forwarder
            .flush()
            .await
            .expect("should not fail to flush buffered sender");

        match receiver.try_recv() {
            Ok(event_buffer) => {
                // Map all of the metrics from their `Event` representation back to `Metric`, and sort them by name.
                let mut metrics = event_buffer
                    .into_iter()
                    .filter_map(|event| event.try_into_metric())
                    .collect::<Vec<_>>();

                metrics.sort_by(|a, b| a.context().name().cmp(b.context().name()));
                metrics
            }
            Err(_) => Vec::new(),
        }
    }

    macro_rules! compare_points {
        (scalar, $expected:expr, $actual:expr, $error_ratio:literal) => {
            for (idx, (expected_value, actual_value)) in $expected.into_iter().zip($actual.into_iter()).enumerate() {
                let (expected_ts, expected_point) = expected_value;
                let (actual_ts, actual_point) = actual_value;

                assert_eq!(
                    expected_ts, actual_ts,
                    "timestamp for value #{} does not match: {:?} (expected) vs {:?} (actual)",
                    idx, expected_ts, actual_ts
                );
                assert!(
                    expected_point.approx_eq_ratio(&actual_point, $error_ratio),
                    "point for value #{} does not match: {} (expected) vs {} actual",
                    idx,
                    expected_point,
                    actual_point
                );
            }
        };
        (distribution, $expected:expr, $actual:expr) => {
            for (idx, (expected_value, actual_value)) in $expected.into_iter().zip($actual.into_iter()).enumerate() {
                let (expected_ts, expected_sketch) = expected_value;
                let (actual_ts, actual_sketch) = actual_value;

                assert_eq!(
                    expected_ts, actual_ts,
                    "timestamp for value #{} does not match: {:?} (expected) vs {:?} (actual)",
                    idx, expected_ts, actual_ts
                );
                assert_eq!(
                    expected_sketch, actual_sketch,
                    "sketch for value #{} does not match: {:?} (expected) vs {:?} (actual)",
                    idx, expected_sketch, actual_sketch
                );
            }
        };
    }

    macro_rules! assert_flushed_scalar_metric {
        ($original:expr, $actual:expr, [$($ts:expr => $value:expr),+]) => {
            assert_flushed_scalar_metric!($original, $actual, [$($ts => $value),+], error_ratio => 0.000001);
        };
        ($original:expr, $actual:expr, [$($ts:expr => $value:expr),+], error_ratio => $error_ratio:literal) => {
            let actual_metric = $actual;

            assert_eq!($original.context(), actual_metric.context(), "expected context ({}) and actual context ({}) do not match", $original.context(), actual_metric.context());

            let expected_points = ScalarPoints::from([$(($ts, $value)),+]);

            match actual_metric.values() {
                MetricValues::Counter(ref actual_points) | MetricValues::Gauge(ref actual_points) | MetricValues::Rate(ref actual_points, _) => {
                    assert_eq!(expected_points.len(), actual_points.len(), "expected and actual values have different number of points");
                    compare_points!(scalar, expected_points, actual_points, $error_ratio);
                },
                _ => panic!("only counters, rates, and gauges are supported in assert_flushed_scalar_metric"),
            }
        };
    }

    macro_rules! assert_flushed_distribution_metric {
        ($original:expr, $actual:expr, [$($ts:expr => $value:expr),+]) => {
            assert_flushed_distribution_metric!($original, $actual, [$($ts => $value),+], error_ratio => 0.000001);
        };
        ($original:expr, $actual:expr, [$($ts:expr => $value:expr),+], error_ratio => $error_ratio:literal) => {
            let actual_metric = $actual;

            assert_eq!($original.context(), actual_metric.context());

            match actual_metric.values() {
                MetricValues::Distribution(ref actual_points) => {
                    let expected_points = SketchPoints::from([$(($ts, $value)),+]);
                    assert_eq!(expected_points.len(), actual_points.len(), "expected and actual values have different number of points");

                    compare_points!(distribution, &expected_points, actual_points);
                },
                _ => panic!("only distributions are supported in assert_flushed_distribution_metric"),
            }
        };
    }

    #[test]
    fn bucket_is_closed() {
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

        for (current_time, bucket_start, bucket_width_secs, flush_open_buckets, expected) in cases {
            let expected_reason = if expected {
                "closed, was open"
            } else {
                "open, was closed"
            };

            assert_eq!(
                is_bucket_closed(current_time, bucket_start, bucket_width_secs, flush_open_buckets),
                expected,
                "expected bucket to be {} (current_time={}, bucket_start={}, bucket_width={}, flush_open_buckets={})",
                expected_reason,
                current_time,
                bucket_start,
                bucket_width_secs,
                flush_open_buckets
            );
        }
    }

    #[tokio::test]
    async fn context_limit() {
        // Create our aggregation state with a context limit of 2.
        let mut state = AggregationState::new(
            BUCKET_WIDTH,
            2,
            COUNTER_EXPIRE,
            HistogramConfiguration::default(),
            Telemetry::noop(),
        );

        // Create four unique gauges, and insert all of them. The third and fourth should fail because we've reached
        // the context limit.
        let input_metrics = vec![
            Metric::gauge("metric1", 1.0),
            Metric::gauge("metric2", 2.0),
            Metric::gauge("metric3", 3.0),
            Metric::gauge("metric4", 4.0),
        ];

        assert!(state.insert(insert_ts(1), input_metrics[0].clone()));
        assert!(state.insert(insert_ts(1), input_metrics[1].clone()));
        assert!(!state.insert(insert_ts(1), input_metrics[2].clone()));
        assert!(!state.insert(insert_ts(1), input_metrics[3].clone()));

        // We should only see the first two gauges after flushing.
        let flushed_metrics = get_flushed_metrics(flush_ts(1), &mut state).await;
        assert_eq!(flushed_metrics.len(), 2);
        assert_eq!(input_metrics[0].context(), flushed_metrics[0].context());
        assert_eq!(input_metrics[1].context(), flushed_metrics[1].context());

        // We should be able to insert the third and fourth gauges now as the first two have been flushed, and along
        // with them, their contexts should no longer be tracked in the aggregation state:
        assert!(state.insert(insert_ts(2), input_metrics[2].clone()));
        assert!(state.insert(insert_ts(2), input_metrics[3].clone()));

        let flushed_metrics = get_flushed_metrics(flush_ts(2), &mut state).await;
        assert_eq!(flushed_metrics.len(), 2);
        assert_eq!(input_metrics[2].context(), flushed_metrics[0].context());
        assert_eq!(input_metrics[3].context(), flushed_metrics[1].context());
    }

    #[tokio::test]
    async fn context_limit_with_zero_value_counters() {
        // We test here to ensure that zero-value counters contribute to the context limit.
        let mut state = AggregationState::new(
            BUCKET_WIDTH,
            2,
            COUNTER_EXPIRE,
            HistogramConfiguration::default(),
            Telemetry::noop(),
        );

        // Create our input metrics.
        let input_metrics = vec![
            Metric::counter("metric1", 1.0),
            Metric::counter("metric2", 2.0),
            Metric::counter("metric3", 3.0),
        ];

        assert!(state.insert(insert_ts(1), input_metrics[0].clone()));
        assert!(state.insert(insert_ts(1), input_metrics[1].clone()));

        // Flush the aggregation state, and observe they're both present.
        let flushed_metrics = get_flushed_metrics(flush_ts(1), &mut state).await;
        assert_eq!(flushed_metrics.len(), 2);
        assert_flushed_scalar_metric!(&input_metrics[0], &flushed_metrics[0], [bucket_ts(1) => 1.0]);
        assert_flushed_scalar_metric!(&input_metrics[1], &flushed_metrics[1], [bucket_ts(1) => 2.0]);

        // Flush _again_ to ensure that we then emit zero-value variants for both counters.
        let flushed_metrics = get_flushed_metrics(flush_ts(2), &mut state).await;
        assert_eq!(flushed_metrics.len(), 2);
        assert_flushed_scalar_metric!(&input_metrics[0], &flushed_metrics[0], [bucket_ts(2) => 0.0]);
        assert_flushed_scalar_metric!(&input_metrics[1], &flushed_metrics[1], [bucket_ts(2) => 0.0]);

        // Now try to insert a third counter, which should fail because we've reached the context limit.
        assert!(!state.insert(insert_ts(3), input_metrics[2].clone()));

        // Flush the aggregation state, and observe that we only see the two original counters.
        let flushed_metrics = get_flushed_metrics(flush_ts(3), &mut state).await;
        assert_eq!(flushed_metrics.len(), 2);
        assert_flushed_scalar_metric!(&input_metrics[0], &flushed_metrics[0], [bucket_ts(3) => 0.0]);
        assert_flushed_scalar_metric!(&input_metrics[1], &flushed_metrics[1], [bucket_ts(3) => 0.0]);

        // With a fourth flush interval, the two counters should now have expired, and thus be dropped and no longer
        // contributing to the context limit.
        let flushed_metrics = get_flushed_metrics(flush_ts(4), &mut state).await;
        assert_eq!(flushed_metrics.len(), 0);

        // Now we should be able to insert the third counter, and it should be the only one present after flushing.
        assert!(state.insert(insert_ts(5), input_metrics[2].clone()));

        let flushed_metrics = get_flushed_metrics(flush_ts(5), &mut state).await;
        assert_eq!(flushed_metrics.len(), 1);
        assert_flushed_scalar_metric!(&input_metrics[2], &flushed_metrics[0], [bucket_ts(5) => 3.0]);
    }

    #[tokio::test]
    async fn zero_value_counters() {
        // We're testing that we properly emit and expire zero-value counters in all relevant scenarios.
        let mut state = AggregationState::new(
            BUCKET_WIDTH,
            10,
            COUNTER_EXPIRE,
            HistogramConfiguration::default(),
            Telemetry::noop(),
        );

        // Create two unique counters, and insert both of them.
        let input_metrics = vec![Metric::counter("metric1", 1.0), Metric::counter("metric2", 2.0)];

        assert!(state.insert(insert_ts(1), input_metrics[0].clone()));
        assert!(state.insert(insert_ts(1), input_metrics[1].clone()));

        // Flush the aggregation state, and observe they're both present.
        let flushed_metrics = get_flushed_metrics(flush_ts(1), &mut state).await;
        assert_eq!(flushed_metrics.len(), 2);
        assert_flushed_scalar_metric!(&input_metrics[0], &flushed_metrics[0], [bucket_ts(1) => 1.0]);
        assert_flushed_scalar_metric!(&input_metrics[1], &flushed_metrics[1], [bucket_ts(1) => 2.0]);

        // Perform our second flush, which should have them as zero-value counters.
        let flushed_metrics = get_flushed_metrics(flush_ts(2), &mut state).await;
        assert_eq!(flushed_metrics.len(), 2);
        assert_flushed_scalar_metric!(&input_metrics[0], &flushed_metrics[0], [bucket_ts(2) => 0.0]);
        assert_flushed_scalar_metric!(&input_metrics[1], &flushed_metrics[1], [bucket_ts(2) => 0.0]);

        // Now, we'll pretend to skip a flush period and add updates to them again after that.
        assert!(state.insert(insert_ts(4), input_metrics[0].clone()));
        assert!(state.insert(insert_ts(4), input_metrics[1].clone()));

        // Flush the aggregation state, and observe that we have two zero-value counters for the flush period we
        // skipped, but that we see them appear again in the fourth flush period.
        let flushed_metrics = get_flushed_metrics(flush_ts(4), &mut state).await;
        assert_eq!(flushed_metrics.len(), 2);
        assert_flushed_scalar_metric!(&input_metrics[0], &flushed_metrics[0], [bucket_ts(3) => 0.0, bucket_ts(4) => 1.0]);
        assert_flushed_scalar_metric!(&input_metrics[1], &flushed_metrics[1], [bucket_ts(3) => 0.0, bucket_ts(4) => 2.0]);

        // Now we'll skip multiple flush periods and ensure that we emit zero-value counters up until the point they
        // expire. As our zero-value counter expiration is 20 seconds, this is two flush periods, so we skip by three
        // flush periods, and we should only see the counters emitted for the first two.
        let flushed_metrics = get_flushed_metrics(flush_ts(7), &mut state).await;
        assert_eq!(flushed_metrics.len(), 2);
        assert_flushed_scalar_metric!(&input_metrics[0], &flushed_metrics[0], [bucket_ts(5) => 0.0, bucket_ts(6) => 0.0]);
        assert_flushed_scalar_metric!(&input_metrics[1], &flushed_metrics[1], [bucket_ts(5) => 0.0, bucket_ts(6) => 0.0]);
    }

    #[tokio::test]
    async fn merge_identical_timestamped_values_on_flush() {
        // We're testing that we properly emit and expire zero-value counters in all relevant scenarios.
        let mut state = AggregationState::new(
            BUCKET_WIDTH,
            10,
            COUNTER_EXPIRE,
            HistogramConfiguration::default(),
            Telemetry::noop(),
        );

        // Create one multi-value counter, and insert it.
        let input_metric = Metric::counter("metric1", [1.0, 2.0, 3.0, 4.0, 5.0]);

        assert!(state.insert(insert_ts(1), input_metric.clone()));

        // Flush the aggregation state, and observe the metric is present _and_ that we've properly merged all of the
        // values within the same timestamp.
        let flushed_metrics = get_flushed_metrics(flush_ts(1), &mut state).await;
        assert_eq!(flushed_metrics.len(), 1);
        assert_flushed_scalar_metric!(&input_metric, &flushed_metrics[0], [bucket_ts(1) => 15.0]);
    }

    #[tokio::test]
    async fn histogram_statistics() {
        // We're testing that we properly emit individual metrics (min, max, sum, etc) for a histogram.
        let hist_config = HistogramConfiguration::from_statistics(&[
            HistogramStatistic::Count,
            HistogramStatistic::Sum,
            HistogramStatistic::Percentile {
                q: 0.5,
                suffix: "p50".into(),
            },
        ]);
        let mut state = AggregationState::new(BUCKET_WIDTH, 10, COUNTER_EXPIRE, hist_config, Telemetry::noop());

        // Create one multi-value histogram and insert it.
        let input_metric = Metric::histogram("metric1", [1.0, 2.0, 3.0, 4.0, 5.0]);
        assert!(state.insert(insert_ts(1), input_metric.clone()));

        // Flush the aggregation state, and observe that we've emitted all of the configured distribution statistics in
        // the form of three metrics: count, sum, and p50.
        let flushed_metrics = get_flushed_metrics(flush_ts(1), &mut state).await;
        assert_eq!(flushed_metrics.len(), 3);

        // Create versions of the metric for each of the statistics we're expecting to emit. The values themselves don't
        // matter here, but we do need a `Metric` for it to compare the context to.
        let count_metric = Metric::rate("metric1.count", 0.0, Duration::from_secs(BUCKET_WIDTH_SECS));
        let sum_metric = Metric::gauge("metric1.sum", 0.0);
        let p50_metric = Metric::gauge("metric1.p50", 0.0);

        // We use a less strict error ratio (how much the expected vs actual) for the percentile check, as we generally
        // expect the value to be somewhat off the exact value due to the lossy nature of `DDSketch`.
        assert_flushed_scalar_metric!(count_metric, &flushed_metrics[0], [bucket_ts(1) => 5.0]);
        assert_flushed_scalar_metric!(p50_metric, &flushed_metrics[1], [bucket_ts(1) => 3.0], error_ratio => 0.0025);
        assert_flushed_scalar_metric!(sum_metric, &flushed_metrics[2], [bucket_ts(1) => 15.0]);
    }

    #[tokio::test]
    async fn distributions() {
        // We're testing that we pass through distributions untouched.
        let mut state = AggregationState::new(
            BUCKET_WIDTH,
            10,
            COUNTER_EXPIRE,
            HistogramConfiguration::default(),
            Telemetry::noop(),
        );

        // Create one multi-value distribution, with server-side aggregation, and insert it.
        let values = [1.0, 2.0, 3.0, 4.0, 5.0];
        let input_metric = Metric::distribution("metric1", &values[..]);

        assert!(state.insert(insert_ts(1), input_metric.clone()));

        // Flush the aggregation state, and observe that we've emitted the original distribution.
        let flushed_metrics = get_flushed_metrics(flush_ts(1), &mut state).await;
        assert_eq!(flushed_metrics.len(), 1);

        assert_flushed_distribution_metric!(&input_metric, &flushed_metrics[0], [bucket_ts(1) => &values[..]]);
    }

    #[tokio::test]
    async fn telemetry() {
        // TODO: We don't check `component_events_dropped_total` or `aggregate_passthrough_metrics_total` here as
        // they're set directly in the aggregate component future rather than `AggregationState`, which is harder to
        // drive overall and would have required even more boilerplate.
        //
        // Leaving that as a future improvement.

        let recorder = TestRecorder::default();
        let _local = metrics::set_default_local_recorder(&recorder);

        let builder = MetricsBuilder::default();
        let telemetry = Telemetry::new(&builder);

        let mut state = AggregationState::new(
            BUCKET_WIDTH,
            2,
            COUNTER_EXPIRE,
            HistogramConfiguration::default(),
            telemetry,
        );

        // Make sure our telemetry is registered at default values.
        assert_eq!(recorder.gauge("aggregate_active_contexts"), Some(0.0));
        assert_eq!(recorder.counter("aggregate_passthrough_metrics_total"), Some(0));
        assert_eq!(
            recorder.counter(("component_events_dropped_total", &[("intentional", "true")])),
            Some(0)
        );
        for metric_type in &["counter", "gauge", "rate", "set", "histogram", "distribution"] {
            assert_eq!(
                recorder.gauge(("aggregate_active_contexts_by_type", &[("metric_type", *metric_type)])),
                Some(0.0)
            );
        }

        // Insert a counter with a non-timestamped value.
        assert!(state.insert(insert_ts(1), Metric::counter("metric1", 42.0)));
        assert_eq!(recorder.gauge("aggregate_active_contexts"), Some(1.0));
        assert_eq!(
            recorder.gauge(("aggregate_active_contexts_by_type", &[("metric_type", "counter")])),
            Some(1.0)
        );
        assert_eq!(recorder.counter("aggregate_passthrough_metrics_total"), Some(0));

        // Insert a gauge with a timestamped value.
        assert!(state.insert(insert_ts(1), Metric::gauge("metric2", (insert_ts(1), 42.0))));
        assert_eq!(recorder.gauge("aggregate_active_contexts"), Some(2.0));
        assert_eq!(
            recorder.gauge(("aggregate_active_contexts_by_type", &[("metric_type", "gauge")])),
            Some(1.0)
        );

        // We've reached our context limit at this point, so the next metric should not be inserted.
        assert!(!state.insert(insert_ts(1), Metric::counter("metric3", 42.0)));
        assert_eq!(recorder.gauge("aggregate_active_contexts"), Some(2.0));
        assert_eq!(
            recorder.gauge(("aggregate_active_contexts_by_type", &[("metric_type", "counter")])),
            Some(1.0)
        );

        // Now let's flush the state which should flush the gauge entirely, reducing the context count, but not flush
        // the counter, since it'll be in zero-value mode.
        let _ = get_flushed_metrics(flush_ts(1), &mut state).await;
        assert_eq!(recorder.gauge("aggregate_active_contexts"), Some(1.0));
        assert_eq!(
            recorder.gauge(("aggregate_active_contexts_by_type", &[("metric_type", "counter")])),
            Some(1.0)
        );
        assert_eq!(
            recorder.gauge(("aggregate_active_contexts_by_type", &[("metric_type", "gauge")])),
            Some(0.0)
        );
    }
}
