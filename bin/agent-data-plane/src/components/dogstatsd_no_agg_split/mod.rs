//! DogStatsD no-aggregation-pipeline split transform.
//!
//! Splits incoming DogStatsD metrics by timestamp state, ahead of `dogstatsd_tag_filterlist` and `dsd_agg`: metrics
//! that carry an explicit per-sample timestamp bypass aggregation entirely and are forwarded, rate-converted and
//! batched, on the `"passthrough"` output; metrics with no timestamp are forwarded unchanged on the default output
//! to continue through tag filtering and aggregation as normal.
//!
//! This split exists as its own transform, rather than living inside `dsd_agg` (where it originated), specifically
//! so that it can run *before* `dogstatsd_tag_filterlist`. Both halves of that transform exist to strip tags so that
//! samples differing only in a filtered tag get merged into a single aggregated context: `metric_tag_filterlist`
//! removes whole tags, and `metric_tag_value_allowlist` removes or rewrites tag values. That merging only happens
//! during aggregation, so applying either to passthrough/timestamped metrics, which are never aggregated, would
//! discard tags for nothing. The Datadog Agent likewise leaves its no-aggregation pipeline unfiltered.
//!
//! Splitting first, rather than teaching `dogstatsd_tag_filterlist` to skip timestamped metrics in place, is what
//! makes that exclusion exact. A single `Metric` can hold both timestamped and non-timestamped values, and the
//! filterlist rewrites the context those values share, so in-place skipping can only choose between filtering all of
//! a mixed metric or none of it. Splitting the values into two metrics first gives each half the treatment it should
//! get: the timestamped half keeps every tag, and the remainder is filtered and aggregated as normal. That exactness
//! is why this is a separate transform and not a few lines inside the filterlist.
//!
//! The transform is only wired into the topology when the Datadog `dogstatsd_no_aggregation_pipeline` key is enabled
//! (the default). When it's disabled there is nothing to split out -- every metric is aggregated -- so the component
//! is left out entirely rather than sitting in the hot path as a pure forwarder.

use std::{
    num::NonZeroU64,
    sync::LazyLock,
    time::{Duration, Instant},
};

use async_trait::async_trait;
use saluki_core::{
    accounting::{MemoryBounds, MemoryBoundsBuilder},
    components::{
        transforms::{Transform, TransformBuilder, TransformContext},
        BuildContext,
    },
    data_model::event::{
        metric::{Metric, MetricValues},
        Event, EventType,
    },
    observability::ComponentMetricsExt as _,
    topology::{EventsBuffer, EventsDispatcher, OutputDefinition},
};
use saluki_error::GenericError;
use saluki_metrics::MetricsBuilder;
use tokio::{pin, select, time::interval};
use tracing::{debug, error, trace};

mod telemetry;
use self::telemetry::Telemetry;

const PASSTHROUGH_OUTPUT: &str = "passthrough";
const PASSTHROUGH_IDLE_FLUSH_CHECK_INTERVAL: Duration = Duration::from_secs(2);

/// DogStatsD no-aggregation-pipeline split transform configuration.
pub struct DogStatsDNoAggSplitConfiguration {
    /// How long to buffer passthrough metrics before flushing them while idle.
    ///
    /// While passthrough metrics aren't aggregated, they're still temporarily buffered in order to optimize the
    /// efficiency of processing them in the next component. This setting controls the maximum amount of time that
    /// passthrough metrics will be buffered before being forwarded, measured from the last batch of metrics that was
    /// processed. Buffers are also flushed as soon as they're full, regardless of this setting.
    ///
    /// The default is one second. Lowering it trades throughput for lower end-to-end latency on pre-aggregated
    /// metrics.
    passthrough_idle_flush_timeout: Duration,

    /// Length, in seconds, of the aggregation window used as the rate interval for pre-aggregated counters.
    ///
    /// Passthrough counters are converted to rates over this interval to match the Datadog Agent, so this **MUST**
    /// be the same value given to the aggregate transform's `window_duration_seconds`, even though passthrough
    /// metrics never reach it. The default is 10 seconds.
    window_duration_seconds: NonZeroU64,
}

impl DogStatsDNoAggSplitConfiguration {
    /// Creates a new `DogStatsDNoAggSplitConfiguration`.
    pub fn new(passthrough_idle_flush_timeout: Duration, window_duration_seconds: NonZeroU64) -> Self {
        Self {
            passthrough_idle_flush_timeout,
            window_duration_seconds,
        }
    }
}

#[async_trait]
impl TransformBuilder for DogStatsDNoAggSplitConfiguration {
    fn input_event_type(&self) -> EventType {
        EventType::Metric
    }

    fn outputs(&self) -> &[OutputDefinition<EventType>] {
        static OUTPUTS: LazyLock<Vec<OutputDefinition<EventType>>> = LazyLock::new(|| {
            vec![
                OutputDefinition::default_output(EventType::Metric),
                OutputDefinition::named_output(PASSTHROUGH_OUTPUT, EventType::Metric),
            ]
        });
        &OUTPUTS
    }

    async fn build(&self, context: BuildContext) -> Result<Box<dyn Transform + Send>, GenericError> {
        let metrics_builder = MetricsBuilder::from_component_context(context.component_context());
        let telemetry = Telemetry::new(&metrics_builder);

        let passthrough_batcher = PassthroughBatcher::new(
            self.passthrough_idle_flush_timeout,
            self.window_duration_seconds,
            telemetry.clone(),
        );

        Ok(Box::new(DogStatsDNoAggSplit { passthrough_batcher }))
    }
}

impl MemoryBounds for DogStatsDNoAggSplitConfiguration {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        builder
            .minimum()
            .with_single_value::<DogStatsDNoAggSplit>("component struct");
    }
}

fn has_timestamped_metrics(events: &EventsBuffer) -> bool {
    events
        .into_iter()
        .filter_map(|event| event.try_as_metric())
        .any(|metric| metric.values().any_timestamped())
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
        // No timestamped values, so this metric isn't part of the no-aggregation pipeline.
        (None, Some(metric))
    }
}

fn counter_values_to_rate(values: MetricValues, interval_secs: NonZeroU64) -> MetricValues {
    match values {
        MetricValues::Counter(points) => MetricValues::rate(points, Duration::from_secs(interval_secs.get())),
        values => values,
    }
}

struct PassthroughBatcher {
    active_buffer: EventsBuffer,
    active_buffer_start: Instant,
    last_processed_at: Instant,
    idle_flush_timeout: Duration,
    bucket_width_secs: NonZeroU64,
    telemetry: Telemetry,
}

impl PassthroughBatcher {
    fn new(idle_flush_timeout: Duration, bucket_width_secs: NonZeroU64, telemetry: Telemetry) -> Self {
        Self {
            active_buffer: EventsBuffer::default(),
            active_buffer_start: Instant::now(),
            last_processed_at: Instant::now(),
            idle_flush_timeout,
            bucket_width_secs,
            telemetry,
        }
    }

    async fn push_metric(&mut self, metric: Metric, dispatcher: &EventsDispatcher) {
        // Convert counters to rates before we batch them up.
        //
        // This involves specifying the rate interval as the bucket width of the aggregate transform itself, which when
        // you say it out loud is sort of confusing and nonsensical since the whole point is that these are
        // _pre-aggregated_ metrics but we have to match the behavior of the Datadog Agent. ¯\_(ツ)_/¯
        let (context, values, metadata) = metric.into_parts();
        let adjusted_values = counter_values_to_rate(values, self.bucket_width_secs);
        let metric = Metric::from_parts(context, adjusted_values, metadata);

        // Try pushing the metric into our active buffer.
        //
        // If our active buffer is full, then we'll flush the buffer, grab a new one, and push the metric into it.
        if let Some(event) = self.active_buffer.try_push(Event::Metric(metric)) {
            debug!("Passthrough event buffer was full. Flushing...");
            self.dispatch_events(dispatcher).await;

            // A buffer that was just flushed and replaced always has room, so reaching this means an invariant
            // broke rather than that we're merely under pressure. Count it as an unintentional drop so it shows up
            // in telemetry and not only in the logs.
            if self.active_buffer.try_push(event).is_some() {
                error!("Event buffer is full even after dispatching events. Dropping event.");
                self.telemetry.increment_events_dropped();
                return;
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

    async fn try_flush(&mut self, dispatcher: &EventsDispatcher) {
        // If our active buffer isn't empty, and we've exceeded our idle flush timeout, then flush the buffer.
        if !self.active_buffer.is_empty() && self.last_processed_at.elapsed() >= self.idle_flush_timeout {
            debug!("Passthrough processing exceeded idle flush timeout. Flushing...");

            self.dispatch_events(dispatcher).await;
        }
    }

    async fn dispatch_events(&mut self, dispatcher: &EventsDispatcher) {
        if !self.active_buffer.is_empty() {
            let unaggregated_events = self.active_buffer.len();

            // Track how long this batch was alive for.
            let batch_duration = self.active_buffer_start.elapsed();
            self.telemetry.record_passthrough_batch_duration(batch_duration);

            self.telemetry.increment_passthrough_flushes();

            // Swap our active buffer with a new, empty one, and then forward the old one.
            let new_active_buffer = EventsBuffer::default();
            let old_active_buffer = std::mem::replace(&mut self.active_buffer, new_active_buffer);

            match dispatcher.dispatch_named(PASSTHROUGH_OUTPUT, old_active_buffer).await {
                Ok(()) => debug!(unaggregated_events, "Dispatched events."),
                Err(e) => error!(error = %e, "Failed to flush unaggregated events."),
            }
        }
    }
}

struct DogStatsDNoAggSplit {
    passthrough_batcher: PassthroughBatcher,
}

#[async_trait]
impl Transform for DogStatsDNoAggSplit {
    async fn run(mut self: Box<Self>, mut context: TransformContext) -> Result<(), GenericError> {
        let mut health = context.take_health_handle();
        health.mark_ready();

        debug!("DogStatsD no-aggregation-pipeline split transform started.");

        let passthrough_flush = interval(PASSTHROUGH_IDLE_FLUSH_CHECK_INTERVAL);
        pin!(passthrough_flush);

        loop {
            select! {
                _ = health.live() => continue,
                _ = passthrough_flush.tick() => self.passthrough_batcher.try_flush(context.dispatcher()).await,
                maybe_events = context.events().next() => match maybe_events {
                    Some(events) => {
                        trace!(events_len = events.len(), "Received events.");

                        // Fast path: when nothing in this buffer is timestamped -- overwhelmingly the common case,
                        // since only clients using the timestamp extension produce them -- there's nothing to split
                        // out, so forward the buffer as-is instead of moving every event into a new one.
                        if !has_timestamped_metrics(&events) {
                            if let Err(e) = context.dispatcher().dispatch(events).await {
                                error!(error = %e, "Failed to dispatch events.");
                            }
                            continue;
                        }

                        let mut default_dispatcher = context
                            .dispatcher()
                            .buffered()
                            .expect("default output should always exist");

                        for event in events {
                            if let Some(metric) = event.try_into_metric() {
                                let (maybe_timestamped, maybe_remainder) = try_split_timestamped_values(metric);

                                if let Some(timestamped_metric) = maybe_timestamped {
                                    self.passthrough_batcher
                                        .push_metric(timestamped_metric, context.dispatcher())
                                        .await;
                                }

                                if let Some(remainder) = maybe_remainder {
                                    // A push only fails when the downstream output can no longer be sent to, which
                                    // means the buffered events it was flushing are gone along with this one. There's
                                    // nowhere left to put them, so log and keep draining our input.
                                    if let Err(e) = default_dispatcher.push(Event::Metric(remainder)).await {
                                        error!(error = %e, "Failed to dispatch non-timestamped events.");
                                    }
                                }
                            }
                        }

                        // We only get here when the buffer held at least one timestamped value, so the batcher has
                        // taken at least one metric and its idle timer needs to be pushed forward.
                        self.passthrough_batcher.update_last_processed_at();

                        if let Err(e) = default_dispatcher.flush().await {
                            error!(error = %e, "Failed to dispatch events.");
                        }
                    },
                    None => break,
                },
            }
        }

        // Do a final flush of any timestamped metrics that we've buffered up.
        self.passthrough_batcher.try_flush(context.dispatcher()).await;

        debug!("DogStatsD no-aggregation-pipeline split transform stopped.");

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use saluki_context::Context;
    use saluki_core::{
        components::ComponentContext,
        data_model::event::metric::ScalarPoints,
        topology::{interconnect::Dispatcher, OutputName},
    };
    use tokio::sync::mpsc;

    use super::*;

    struct DispatcherReceiver {
        receiver: mpsc::Receiver<EventsBuffer>,
    }

    impl DispatcherReceiver {
        fn collect_next(&mut self) -> Vec<Metric> {
            match self.receiver.try_recv() {
                Ok(event_buffer) => event_buffer
                    .into_iter()
                    .filter_map(|event| event.try_into_metric())
                    .collect(),
                Err(_) => Vec::new(),
            }
        }
    }

    /// Constructs a `Dispatcher` with a fixed-size event buffer attached to the named `"passthrough"` output.
    fn build_passthrough_dispatcher() -> (EventsDispatcher, DispatcherReceiver) {
        let context = ComponentContext::test_transform("test");
        let mut dispatcher = Dispatcher::new(context);

        let (buffer_tx, buffer_rx) = mpsc::channel(1);
        let output_name = OutputName::Given(PASSTHROUGH_OUTPUT.into());
        dispatcher.add_output(output_name.clone()).unwrap();
        dispatcher.attach_sender_to_output(&output_name, buffer_tx).unwrap();

        (dispatcher, DispatcherReceiver { receiver: buffer_rx })
    }

    const BUCKET_WIDTH_SECS: NonZeroU64 = NonZeroU64::new(10).expect("not zero");
    const BUCKET_WIDTH: Duration = Duration::from_secs(BUCKET_WIDTH_SECS.get());

    #[tokio::test]
    async fn preaggregated_counters_to_rate() {
        let counter_value = 42.0;
        let timestamp = 123456;

        // Create a basic passthrough batcher and forwarder.
        let mut batcher = PassthroughBatcher::new(Duration::from_nanos(1), BUCKET_WIDTH_SECS, Telemetry::noop());
        let (dispatcher, mut dispatcher_receiver) = build_passthrough_dispatcher();

        // Create a simple pre-aggregated counter, and batch it.
        let input_metric = Metric::counter("metric1", (timestamp, counter_value));
        batcher.push_metric(input_metric.clone(), &dispatcher).await;

        // Flush the batcher, and observe that we've emitted the expected counter and that it has the right
        // value, but specifically that it's a rate with an interval that matches our configured bucket width:
        batcher.try_flush(&dispatcher).await;

        let mut flushed_metrics = dispatcher_receiver.collect_next();
        assert_eq!(flushed_metrics.len(), 1);
        assert_eq!(
            Metric::rate("metric1", (timestamp, counter_value), BUCKET_WIDTH),
            flushed_metrics.remove(0)
        );
    }

    #[test]
    fn has_timestamped_metrics_detects_any_timestamped_value() {
        let mut buffer = EventsBuffer::default();
        assert!(!has_timestamped_metrics(&buffer));

        assert!(buffer.try_push(Event::Metric(Metric::gauge("metric1", 1.0))).is_none());
        assert!(!has_timestamped_metrics(&buffer));

        // A metric where only _some_ values are timestamped still has to take the splitting path.
        let mixed_values = ScalarPoints::from_iter([(None, 2.0), (NonZeroU64::new(123), 3.0)]);
        let mixed_metric = Metric::counter(Context::from_static_name("metric2"), mixed_values);
        assert!(buffer.try_push(Event::Metric(mixed_metric)).is_none());
        assert!(has_timestamped_metrics(&buffer));
    }

    #[test]
    fn split_all_timestamped_values_go_to_passthrough() {
        let metric = Metric::gauge("metric1", (123, 1.0));
        let (timestamped, remainder) = try_split_timestamped_values(metric.clone());
        assert_eq!(timestamped, Some(metric));
        assert_eq!(remainder, None);
    }

    #[test]
    fn split_non_timestamped_values_go_to_remainder() {
        let metric = Metric::gauge("metric1", 1.0);
        let (timestamped, remainder) = try_split_timestamped_values(metric.clone());
        assert_eq!(timestamped, None);
        assert_eq!(remainder, Some(metric));
    }

    #[test]
    fn split_mixed_timestamped_values_go_to_both_outputs() {
        let context = Context::from_static_name("metric1");
        let mixed_values = ScalarPoints::from_iter([(None, 2.0), (NonZeroU64::new(123), 3.0)]);
        let metric = Metric::counter(context.clone(), mixed_values);

        let (timestamped, remainder) = try_split_timestamped_values(metric);

        let timestamped = timestamped.expect("mixed metric should yield a timestamped half");
        assert_eq!(timestamped.context(), &context);
        assert!(timestamped.values().all_timestamped());

        let remainder = remainder.expect("mixed metric should yield a non-timestamped remainder");
        assert_eq!(remainder.context(), &context);
        assert!(!remainder.values().any_timestamped());
    }
}
