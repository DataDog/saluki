use std::{
    collections::hash_map::Entry,
    hash::{BuildHasher, Hash as _, Hasher},
    time::Duration,
};

use ahash::{AHashMap, AHashSet};
use async_trait::async_trait;
use tokio::{pin, select, time::sleep_until};
use tracing::{debug, error, trace};

use saluki_core::{
    buffers::FixedSizeBufferPool,
    components::{metrics::MetricsBuilder, transforms::*},
    topology::{interconnect::EventBuffer, OutputDefinition},
};
use saluki_event::{metric::*, DataType, Event};

use saluki_env::time::get_unix_timestamp;

/// Aggregate transform.
///
/// Aggregates metrics into fixed-size windows, flushing them at a regular interval.
///
/// ## Missing
///
/// - ability to handle metrics with timestamps not within the current aggregation window (currently assumes every
///   metric should be aggregated into the current window)
/// - maintaining zero-value counters after flush until expiry
pub struct AggregateConfiguration {
    window_duration: Duration,
    context_limit: Option<usize>,
    flush_open_windows: bool,
}

impl AggregateConfiguration {
    /// Creates a new `AggregateConfiguration` with the given window duration.
    pub fn from_window(window_duration: Duration) -> Self {
        Self {
            window_duration,
            context_limit: None,
            flush_open_windows: false,
        }
    }

    /// Sets the maximum number of contexts to aggregate per window.
    ///
    /// A context is the unique combination of a metric name and its set of tags. For example,
    /// `metric.name.here{tag1=A,tag2=B}` represents a single context, and would be different than
    /// `metric.name.here{tag1=A,tag2=C}`.
    ///
    /// When the maximum number of contexts is reached in the current aggregation window, additional metrics are dropped
    /// until the next window starts.
    pub fn with_context_limit(self, context_limit: usize) -> Self {
        Self {
            context_limit: Some(context_limit),
            ..self
        }
    }

    /// Sets whether to flush open buckets when stopping the transform.
    ///
    /// Normally, an open bucket -- a bucket where the full window has not yet elapsed -- is not flushed when the
    /// transform is stopped. This is done to avoid the specific case of flushing a partial window, having the process
    /// restart, and then have it flush the same window again. Downstream systems may not be able to cope with this, and
    /// so we avoid doing so by default.
    pub fn flush_open_windows(self, flush_open_windows: bool) -> Self {
        Self {
            flush_open_windows,
            ..self
        }
    }
}

#[async_trait]
impl TransformBuilder for AggregateConfiguration {
    async fn build(&self) -> Result<Box<dyn Transform + Send>, Box<dyn std::error::Error + Send + Sync>> {
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

pub struct Aggregate {
    window_duration: Duration,
    context_limit: Option<usize>,
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
        let events_dropped = metrics_builder.register_counter("component_events_dropped");

        // Create our own event buffer pool.
        //
        // We do this because aggregation often leads to a high amount of cardinality, where we're flushing a lot more
        // events per event buffer. If we use the global event buffer pool, we risk churning through many event buffers,
        // having them reserve a lot of underlying capacity, and then having a ton of event buffers in the pool with
        // high capacity when we only need one every few seconds, etc.
        let event_buffer_pool = FixedSizeBufferPool::<EventBuffer>::with_capacity(8);

        debug!("Aggregation transform started.");

        let mut final_flush = false;

        loop {
            select! {
                biased;

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
                    Some(mut event_buffer) => {
                        trace!(events_len = event_buffer.len(), "Received events.");

                        for event in event_buffer.take_events() {
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

#[derive(Clone, Debug)]
struct AggregationContext {
    context: MetricContext,
    tags_hash: u64,
}

impl AggregationContext {
    fn from_metric_context<B: BuildHasher>(context: MetricContext, hasher_builder: &B) -> Self {
        // Hash our tags first.
        let mut hasher = hasher_builder.build_hasher();
        let tags_hash = hash_context_tags(&context, &mut hasher);

        Self { context, tags_hash }
    }

    fn into_inner(self) -> MetricContext {
        self.context
    }
}

fn hash_context_tags<H: Hasher>(context: &MetricContext, hasher: &mut H) -> u64 {
    // We hash each tag individually, and then XOR the hashes together, which is commutative.  This means that we'll
    // calculate the same hash for the same set of tags even if the tags aren't in the same order.
    //
    // This is a simple fast path for hashing `AggregationContext` to avoid sorting tags right off the bat. If there's a
    // hash collision between two contexts, we'll still fall back to sorting the tags when doing the follow-up equality
    // check.

    // Start out with the FNV-1a seed so that we're not just XORing a bunch of zeros.
    let mut combined = 0xcbf29ce484222325;

    for tag in &context.tags {
        tag.hash(hasher);
        combined ^= hasher.finish();
    }

    combined
}

impl std::hash::Hash for AggregationContext {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.context.name.hash(state);
        state.write_u64(self.tags_hash);
    }
}

impl PartialEq for AggregationContext {
    fn eq(&self, other: &Self) -> bool {
        if self.context.name == other.context.name {
            let self_tags = self.context.tags.clone().sorted();
            let other_tags = other.context.tags.clone().sorted();

            self_tags == other_tags
        } else {
            false
        }
    }
}

impl Eq for AggregationContext {}

struct AggregationState {
    contexts: AHashSet<AggregationContext>,
    context_limit: Option<usize>,
    #[allow(clippy::type_complexity)]
    buckets: Vec<(u64, AHashMap<AggregationContext, (MetricValue, MetricMetadata)>)>,
    bucket_width: Duration,
}

impl AggregationState {
    fn new(bucket_width: Duration, context_limit: Option<usize>) -> Self {
        Self {
            contexts: AHashSet::default(),
            context_limit,
            buckets: Vec::with_capacity(2),
            bucket_width,
        }
    }

    fn get_next_flush_instant(&self) -> tokio::time::Instant {
        // Buckets are aligned on the modulo boundary of the current time and the window duration, so we just need to
        // figure out where the start of the current bucket is, add our bucket width, and voila.
        let bucket_width = self.bucket_width.as_secs();
        let flush_delta = align_to_bucket_start(get_unix_timestamp(), bucket_width) + bucket_width;

        tokio::time::Instant::now() + Duration::from_secs(flush_delta)
    }

    fn is_empty(&self) -> bool {
        self.contexts.is_empty()
    }

    fn insert(&mut self, metric: Metric) -> bool {
        let context_limit_reached = self
            .context_limit
            .map(|limit| self.contexts.len() >= limit)
            .unwrap_or(false);

        // Split the metric into its constituent parts, so that we can create the aggregation context object.
        let (metric_context, metric_value, mut metric_metadata) = metric.into_parts();
        let context = AggregationContext::from_metric_context(metric_context, self.contexts.hasher());

        // If we haven't seen this context yet, track it.
        if !self.contexts.contains(&context) {
            if context_limit_reached {
                return false;
            }

            self.contexts.insert(context.clone());
        }

        // Figure out what bucket we belong to, create it if necessary, and then merge the metric in.
        let bucket_start = align_to_bucket_start(metric_metadata.timestamp, self.bucket_width.as_secs());
        let bucket = self.get_or_create_bucket(bucket_start);
        match bucket.entry(context) {
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
        let bucket_width = self.bucket_width;

        let mut i = 0;
        while i < self.buckets.len() {
            if is_bucket_still_open(self.buckets[i].0, bucket_width, flush_open_buckets) {
                let (_, contexts) = self.buckets.remove(i);

                for (context, (value, metadata)) in contexts {
                    // Convert any counters to rates, so that we can properly account for their aggregated status.
                    let value = match value {
                        MetricValue::Counter { value } => MetricValue::Rate {
                            value: value / bucket_width.as_secs_f64(),
                            interval: bucket_width,
                        },
                        _ => value,
                    };
                    let metric = Metric::from_parts(context.into_inner(), value, metadata);
                    event_buffer.push(Event::Metric(metric));
                }
            } else {
                i += 1;
            }
        }
    }

    fn get_or_create_bucket(
        &mut self, bucket_start: u64,
    ) -> &mut AHashMap<AggregationContext, (MetricValue, MetricMetadata)> {
        match self.buckets.iter_mut().position(|(start, _)| *start == bucket_start) {
            Some(idx) => &mut self.buckets[idx].1,
            None => {
                self.buckets.push((bucket_start, AHashMap::default()));
                &mut self.buckets.last_mut().unwrap().1
            }
        }
    }
}

fn is_bucket_still_open(bucket_start: u64, bucket_width: Duration, flush_open_buckets: bool) -> bool {
    // Either the bucket end (start + width) is greater than the current time, or we're allowed to flush open buckets.
    ((bucket_start + bucket_width.as_secs()) > get_unix_timestamp()) && flush_open_buckets
}

const fn align_to_bucket_start(timestamp: u64, bucket_width: u64) -> u64 {
    timestamp - (timestamp % bucket_width)
}
