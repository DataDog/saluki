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
use tokio::{pin, select, time::sleep_until};
use tracing::{debug, error, trace};

const EVENT_BUFFER_POOL_SIZE: usize = 8;

const fn default_window_duration() -> Duration {
    Duration::from_secs(10)
}

const fn default_context_limit() -> usize {
    1000
}

/// Aggregate transform.
///
/// Aggregates metrics into fixed-size windows, flushing them at a regular interval.
///
/// ## Missing
///
/// - maintaining zero-value counters after flush until expiry
#[derive(Deserialize)]
pub struct AggregateConfiguration {
    /// Size of the aggregation window.
    ///
    /// Metrics are aggregated into fixed-size windows, such that all updates to the same metric within a window are
    /// aggregated into a single metric. The window size controls how efficiently metrics are aggregated, but also how
    /// often they're flushed downstream. This represents a trade-off between the savings in network bandwidth (sending
    /// fewer requests to downstream systems, etc) and the frequency of updates (how often updates to a metric are emitted).
    ///
    /// Defaults to 10 seconds.
    #[serde(rename = "aggregate_window_duration", default = "default_window_duration")]
    window_duration: Duration,

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
    /// Normally, an open bucket -- a bucket where the full window has not yet elapsed -- is not flushed when the
    /// transform is stopped. This is done to avoid the specific case of flushing a partial window, having the process
    /// restart, and then have it flush the same window again. Downstream systems may not be able to cope with this, and
    /// so we avoid doing so by default
    #[serde(rename = "aggregate_flush_open_windows", default)]
    flush_open_windows: bool,
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
            context_limit: default_context_limit(),
            flush_open_windows: false,
        }
    }
}

#[async_trait]
impl TransformBuilder for AggregateConfiguration {
    async fn build(&self) -> Result<Box<dyn Transform + Send>, GenericError> {
        Ok(Box::new(Aggregate {
            window_duration: self.window_duration,
            context_limit: self.context_limit,
            flush_open_windows: self.flush_open_windows,
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
        // Since we use our own event buffer pool, we account for that directly here, and we use our knowledge of the
        // context limit to determine how large we'd expect those event buffers to grow to in the worst case. With a
        // context limit of N, we would only aggregate N metrics at any given time, and thus we should flush a maximum
        // of N metrics per flush interval.
        let event_buffer_pool_size = EVENT_BUFFER_POOL_SIZE * self.context_limit * std::mem::size_of::<Event>();

        // TODO: This is a very specific shortcut we take to estimate the size of a context. Since metrics coming into
        // via DogStatsD are limited in size based on the buffer size ("packet" in Datadog Agent parlance), this means
        // the sum of the metric name and tags can't exceed the buffer size, which is generally fixed. Instead of
        // calculating the theoretical maximum -- maximum number of tags * maximum tag length -- we can just derive it
        // empirically knowing that they can't add up to more than the buffer size.
        //
        // Currently, we do not set/override the buffer size in testing/benchmarking, so we're hardcoding the default of
        // 8KB buffers here for our calculations. We _will_ need to eventually calculate this for real, though.
        let context_size = 8192;

        builder
            .firm()
            .with_fixed_amount(event_buffer_pool_size)
            // Account for our context limiter map, which is just a `HashSet`.
            .with_array::<Context>(self.context_limit)
            .with_fixed_amount(self.context_limit * context_size)
            // Account for the actual aggregation state map, where we map contexts to the merged metric.
            //
            // TODO: We're not considering the fact there could be multiple buckets here since that's rare, but it's
            // something we may need to consider in the near term.
            .with_map::<Context, (MetricValue, MetricMetadata)>(self.context_limit);
    }
}

pub struct Aggregate {
    window_duration: Duration,
    context_limit: usize,
    flush_open_windows: bool,
}

#[async_trait]
impl Transform for Aggregate {
    async fn run(mut self: Box<Self>, mut context: TransformContext) -> Result<(), ()> {
        let mut state = AggregationState::new(self.window_duration, self.context_limit);
        let next_flush = state.get_next_flush_instant();

        let flush = sleep_until(next_flush);
        pin!(flush);

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

        debug!("Aggregation transform started.");

        let mut final_flush = false;

        loop {
            select! {
                _ = &mut flush => {
                    // We've reached the end of the current window. Flush our aggregation state and forward the metrics
                    // onwards. Regardless of whether any metrics were aggregated, we always update the aggregation
                    // state to track the start time of the current aggregation window.
                    if !state.is_empty() {
                        debug!("Flushing aggregated metrics...");

                        let should_flush_open_windows = final_flush && self.flush_open_windows;

                        let mut event_buffer = event_buffer_pool.acquire().await;
                        debug!(buf_cap = event_buffer.capacity(), "Acquired event buffer.");
                        state.flush(should_flush_open_windows, &mut event_buffer);

                        let events_forwarded = event_buffer.len();

                        if let Err(e) = context.forwarder().forward(event_buffer).await {
                            error!(error = %e, "Failed to forward events.");
                            return Err(());
                        }

                        debug!(events_len = events_forwarded, "Forwarded events.");
                    }

                    // If this is the final flush, we break out of the loop.
                    if final_flush {
                        debug!("All aggregation complete.");
                        break
                    }

                    flush.as_mut().reset(state.get_next_flush_instant());
                },
                maybe_event_buffer = context.event_stream().next(), if !final_flush => match maybe_event_buffer {
                    Some(event_buffer) => {
                        trace!(events_len = event_buffer.len(), "Received events.");

                        for event in event_buffer {
                            if let Some(metric) = event.into_metric() {
                                if !state.insert(metric) {
                                    trace!("Dropping metric due to context limit.");
                                    events_dropped.increment(1);
                                }
                            }
                        }
                    },
                    None => {
                        // We've reached the end of our input stream, so mark ourselves for a final flush and reset the
                        // interval so it ticks immediately on the next loop iteration.
                        final_flush = true;

                        flush.as_mut().reset(tokio::time::Instant::now());

                        debug!("Aggregation transform stopping...");
                    }
                },
            }
        }

        debug!("Aggregation transform stopped.");

        Ok(())
    }
}

struct AggregationState {
    contexts: AHashSet<Context>,
    context_limit: usize,
    #[allow(clippy::type_complexity)]
    buckets: Vec<(u64, AHashMap<Context, (MetricValue, MetricMetadata)>)>,
    bucket_width: Duration,
}

impl AggregationState {
    fn new(bucket_width: Duration, context_limit: usize) -> Self {
        Self {
            contexts: AHashSet::default(),
            context_limit,
            buckets: Vec::with_capacity(2),
            bucket_width,
        }
    }

    fn get_next_flush_instant(&self) -> tokio::time::Instant {
        // We align our flushes to the middle of the next bucket, so that we don't flush the current bucket too early
        // when there's outstanding metrics that haven't been aggregated yet.
        let bucket_width = self.bucket_width.as_secs();
        let current_time = get_unix_timestamp();
        let next_bucket_midpoint =
            align_to_bucket_start(current_time, bucket_width) + bucket_width + (bucket_width / 2);
        let flush_delta = next_bucket_midpoint - current_time;

        tokio::time::Instant::now() + Duration::from_secs(flush_delta)
    }

    fn is_empty(&self) -> bool {
        self.contexts.is_empty()
    }

    fn insert(&mut self, metric: Metric) -> bool {
        // Split the metric into its constituent parts, so that we can create the aggregation context object.
        let (metric_context, metric_value, mut metric_metadata) = metric.into_parts();

        // If we haven't seen this context yet, track it.
        if !self.contexts.contains(&metric_context) {
            if self.contexts.len() >= self.context_limit {
                return false;
            }

            self.contexts.insert(metric_context.clone());
        }

        // Figure out what bucket we belong to, create it if necessary, and then merge the metric in.
        let bucket_start = align_to_bucket_start(metric_metadata.timestamp, self.bucket_width.as_secs());
        let bucket = self.get_or_create_bucket(bucket_start);
        match bucket.entry(metric_context) {
            Entry::Occupied(mut entry) => {
                let (existing_value, _) = entry.get_mut();
                existing_value.merge(metric_value);
            }
            Entry::Vacant(entry) => {
                // Set the metric's timestamp to the start of the bucket.
                metric_metadata.timestamp = bucket_start;

                entry.insert((metric_value, metric_metadata));
            }
        }

        true
    }

    fn flush(&mut self, flush_open_buckets: bool, event_buffer: &mut EventBuffer) {
        debug!(
            buckets_len = self.buckets.len(),
            timestamp = get_unix_timestamp(),
            "Flushing buckets."
        );

        let bucket_width = self.bucket_width;

        let mut i = 0;
        while i < self.buckets.len() {
            if is_bucket_closed(self.buckets[i].0, bucket_width, flush_open_buckets) {
                let (bucket_start, contexts) = self.buckets.remove(i);
                debug!(bucket_start, "Flushing bucket...");

                for (context, (value, metadata)) in contexts {
                    // Convert any counters to rates, so that we can properly account for their aggregated status.
                    let value = match value {
                        MetricValue::Counter { value } => MetricValue::Rate {
                            value: value / bucket_width.as_secs_f64(),
                            interval: bucket_width,
                        },
                        _ => value,
                    };
                    let metric = Metric::from_parts(context, value, metadata);
                    event_buffer.push(Event::Metric(metric));
                }
            } else {
                i += 1;
            }
        }
    }

    fn get_or_create_bucket(&mut self, bucket_start: u64) -> &mut AHashMap<Context, (MetricValue, MetricMetadata)> {
        match self.buckets.iter_mut().position(|(start, _)| *start == bucket_start) {
            Some(idx) => &mut self.buckets[idx].1,
            None => {
                self.buckets.push((bucket_start, AHashMap::default()));
                &mut self.buckets.last_mut().unwrap().1
            }
        }
    }
}

fn is_bucket_closed(bucket_start: u64, bucket_width: Duration, flush_open_buckets: bool) -> bool {
    // Either the bucket end (start + width) is less than than the current time (closed), or we're allowed to flush open buckets.
    ((bucket_start + bucket_width.as_secs()) <= get_unix_timestamp()) || flush_open_buckets
}

const fn align_to_bucket_start(timestamp: u64, bucket_width: u64) -> u64 {
    timestamp - (timestamp % bucket_width)
}
