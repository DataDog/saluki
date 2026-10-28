//! DogStatsD no-aggregation-pipeline split transform.
//!
//! Splits incoming DogStatsD metrics by timestamp state, ahead of `dogstatsd_tag_filterlist` and `dsd_agg`: metrics
//! that carry an explicit per-sample timestamp bypass aggregation entirely and are forwarded, rate-converted and
//! batched, on the `"passthrough"` output; metrics with no timestamp are forwarded unchanged on the default output
//! to continue through tag filtering and aggregation as normal.
//!
//! Only wired into the topology when the Datadog `dogstatsd_no_aggregation_pipeline` key is enabled (the default).

use std::{
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

// Timestamped counters carry no duration. Core Agent encodes them as rates using this fixed compatibility interval,
// independently of its time sampler's interval. Changing an Agent-owned aggregation window must not reinterpret
// a client-provided point. This is a protocol convention, not a configurable aggregation default.
// https://github.com/DataDog/datadog-agent/blob/78c2c984e848055b4aa63c5f6a9e94a534a07a44/pkg/aggregator/no_aggregation_stream_worker.go#L223-L235
const PASSTHROUGH_COUNTER_NORMALIZATION_INTERVAL: Duration = Duration::from_secs(10);

/// DogStatsD no-aggregation-pipeline split transform configuration.
///
/// Timestamped counters become rates with a fixed 10-second normalization interval, independently of the configured
/// aggregation windows. Other timestamped metric values pass through unchanged.
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
}

impl DogStatsDNoAggSplitConfiguration {
    /// Creates a new `DogStatsDNoAggSplitConfiguration`.
    pub fn new(passthrough_idle_flush_timeout: Duration) -> Self {
        Self {
            passthrough_idle_flush_timeout,
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

        let passthrough_batcher = PassthroughBatcher::new(self.passthrough_idle_flush_timeout, telemetry.clone());

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

fn counter_values_to_rate(values: MetricValues) -> MetricValues {
    match values {
        MetricValues::Counter(points) => MetricValues::rate(points, PASSTHROUGH_COUNTER_NORMALIZATION_INTERVAL),
        values => values,
    }
}

struct PassthroughBatcher {
    active_buffer: EventsBuffer,
    active_buffer_start: Instant,
    last_processed_at: Instant,
    idle_flush_timeout: Duration,
    telemetry: Telemetry,
}

impl PassthroughBatcher {
    fn new(idle_flush_timeout: Duration, telemetry: Telemetry) -> Self {
        Self {
            active_buffer: EventsBuffer::default(),
            active_buffer_start: Instant::now(),
            last_processed_at: Instant::now(),
            idle_flush_timeout,
            telemetry,
        }
    }

    async fn push_metric(&mut self, metric: Metric, dispatcher: &EventsDispatcher) {
        // Convert counters to rates before we batch them up.
        //
        // Pin the compatibility interval on each metric before buffering it. Later configuration or flush timing
        // cannot change the interpretation of a point already accepted by this path.
        let (context, values, metadata) = metric.into_parts();
        let adjusted_values = counter_values_to_rate(values);
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

        // Forward any timestamped metrics we've buffered up. Unlike the periodic check, this doesn't wait for the idle
        // flush timeout: nothing else will flush them once we stop.
        self.passthrough_batcher.dispatch_events(context.dispatcher()).await;

        debug!("DogStatsD no-aggregation-pipeline split transform stopped.");

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU64;

    use agent_data_plane_config::{shared::SharedConfiguration, ConfigValue};
    use http_body_util::BodyExt as _;
    use saluki_components::encoders::DatadogMetricsConfiguration;
    use saluki_core::{
        components::test_util::TestComponentDriver,
        data_model::event::metric::{context::Context, ScalarPoints},
    };

    use super::*;

    /// Returns a metrics encoder configuration that writes uncompressed-size-limited V1 JSON series payloads.
    ///
    /// V1 JSON makes the encoded rate and interval directly readable, without decoding protobuf.
    fn v1_series_encoder() -> DatadogMetricsConfiguration {
        let mut shared = SharedConfiguration::default();
        shared.endpoints.dd_url = ConfigValue::explicit("https://example.com".to_string());
        shared.endpoints.compression.compressor_kind = "zstd".to_string();
        shared.metrics_encoding.max_payload_size = 64 * 1024;
        shared.metrics_encoding.max_uncompressed_payload_size = 64 * 1024;
        shared.metrics_encoding.max_series_payload_size = 64 * 1024;
        shared.metrics_encoding.max_series_uncompressed_payload_size = 64 * 1024;
        shared.metrics_encoding.max_series_points_per_payload = 100;
        shared.metrics_encoding.flush_timeout = Duration::from_millis(10);
        shared.metrics_encoding.use_v2_series_api = false;
        DatadogMetricsConfiguration::from_configuration(&shared)
    }

    /// Regression test: the passthrough path once normalized timestamped counters by the configured aggregation window
    /// (`aggregate_window_duration_seconds`), so a 60-second window reported one sixth of the Core Agent's rate. The
    /// Core Agent always divides by its fixed 10-second bucket size, independently of its time sampler's interval:
    /// https://github.com/DataDog/datadog-agent/blob/78c2c984e848055b4aa63c5f6a9e94a534a07a44/pkg/aggregator/no_aggregation_stream_worker.go#L223-L235
    ///
    /// This drives the real split transform and the real metrics encoder, since the encoder performs the division.
    #[tokio::test]
    async fn timestamped_counters_encode_as_ten_second_rates() {
        let (split, mut split_outputs) =
            TestComponentDriver::transform(DogStatsDNoAggSplitConfiguration::new(Duration::from_millis(1)))
                .await
                .expect("split transform should build");
        split
            .send_event(Event::Metric(Metric::counter("app.requests", (1_700_000_000, 60.0))))
            .await;
        split.shutdown().await.expect("split transform should stop cleanly");
        let passthrough = split_outputs.named_output(PASSTHROUGH_OUTPUT).collect_events().await;
        assert!(split_outputs.default_output().collect_events().await.is_empty());

        let (encoder, mut payloads) = TestComponentDriver::encoder(v1_series_encoder())
            .await
            .expect("metrics encoder should build");
        encoder.send_events(passthrough).await;
        encoder.shutdown().await.expect("metrics encoder should stop cleanly");
        let mut payloads = payloads.collect().await;
        assert_eq!(payloads.len(), 1);

        let (_, request) = payloads
            .remove(0)
            .try_into_http_payload()
            .expect("metrics are sent over HTTP")
            .into_parts();
        assert_eq!(request.uri().path(), "/api/v1/series");
        let body = request
            .into_body()
            .collect()
            .await
            .expect("body should collect")
            .to_bytes();
        let body = zstd::stream::decode_all(body.as_ref()).expect("body should decompress");
        let json: serde_json::Value = serde_json::from_slice(&body).expect("body should be JSON");
        let series = json["series"].as_array().expect("payload should contain series");
        assert_eq!(series.len(), 1);
        assert_eq!(series[0]["metric"], "app.requests");
        assert_eq!(series[0]["type"], "rate");
        assert_eq!(series[0]["interval"], 10);
        assert_eq!(series[0]["points"], serde_json::json!([[1_700_000_000, 6.0]]));
    }

    #[tokio::test(start_paused = true)]
    async fn idle_timestamped_counters_flush_to_passthrough_as_rates() {
        // The idle timeout has always elapsed by the next periodic check, so that check, rather than the shutdown,
        // forwards the counter.
        let config = DogStatsDNoAggSplitConfiguration::new(Duration::from_nanos(1));
        let (split, mut outputs) = TestComponentDriver::transform(config)
            .await
            .expect("split transform should build");
        split
            .send_events([
                Event::Metric(Metric::counter("metric1", (123456, 42.0))),
                Event::Metric(Metric::counter("metric2", 1.0)),
            ])
            .await;

        // Counters become rates with the fixed 10-second compatibility interval, rather than an aggregation window.
        let flushed = outputs
            .named_output(PASSTHROUGH_OUTPUT)
            .next()
            .await
            .expect("the periodic check should forward the counter");
        let flushed = flushed
            .into_iter()
            .filter_map(Event::try_into_metric)
            .collect::<Vec<_>>();
        assert_eq!(
            flushed,
            [Metric::rate("metric1", (123456, 42.0), Duration::from_secs(10))]
        );

        // Timestamped metrics never reach the default output, which leads to aggregation and the tag filterlist.
        split.shutdown().await.expect("split transform should stop cleanly");
        let aggregated = outputs.default_output().collect_events().await;
        let aggregated = aggregated
            .into_iter()
            .filter_map(Event::try_into_metric)
            .collect::<Vec<_>>();
        assert_eq!(aggregated, [Metric::counter("metric2", 1.0)]);
        assert!(outputs
            .named_output(PASSTHROUGH_OUTPUT)
            .collect_events()
            .await
            .is_empty());
    }

    /// Regression test: the final flush at shutdown used to dispatch only once the idle flush timeout had elapsed since
    /// the last batch, so timestamped metrics received within that timeout before stopping (one second by default)
    /// were dropped. The Core Agent's no-aggregation worker sends its pending batch unconditionally when stopped:
    /// https://github.com/DataDog/datadog-agent/blob/78c2c984e848055b4aa63c5f6a9e94a534a07a44/pkg/aggregator/no_aggregation_stream_worker.go#L178-L182
    #[tokio::test]
    async fn shutdown_forwards_buffered_metrics_before_the_idle_timeout() {
        // Far longer than the test, so only the shutdown flush can forward the metric.
        let config = DogStatsDNoAggSplitConfiguration::new(Duration::from_secs(60));
        let (split, mut outputs) = TestComponentDriver::transform(config)
            .await
            .expect("split transform should build");
        let metric = Metric::counter("app.requests", (1_700_000_000, 1.0));
        split.send_event(Event::Metric(metric.clone())).await;
        split.shutdown().await.expect("split transform should stop cleanly");

        let passthrough = outputs.named_output(PASSTHROUGH_OUTPUT).collect_events().await;
        assert_eq!(passthrough.len(), 1, "buffered metrics must be forwarded at shutdown");
        assert_eq!(
            passthrough[0].clone().try_into_metric(),
            Some(Metric::rate(
                "app.requests",
                (1_700_000_000, 1.0),
                Duration::from_secs(10)
            ))
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
