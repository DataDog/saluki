use std::{
    future::pending,
    num::NonZeroU64,
    sync::Mutex,
    time::{Duration, Instant},
};

use async_trait::async_trait;
use ddsketch::DDSketch;
use hashbrown::{hash_map::Entry, HashMap};
use saluki_common::time::get_unix_timestamp;
use saluki_config::GenericConfiguration;
use saluki_context::Context;
use saluki_core::accounting::{MemoryBounds, MemoryBoundsBuilder, UsageExpr};
use saluki_core::{
    components::{transforms::*, ComponentContext},
    data_model::event::{metric::*, Event, EventType},
    observability::ComponentMetricsExt as _,
    topology::{interconnect::BufferedDispatcher, OutputDefinition},
    topology::{EventsBuffer, EventsDispatcher},
};
use saluki_error::{generic_error, GenericError};
use saluki_metrics::MetricsBuilder;
use serde::Deserialize;
use smallvec::SmallVec;
use stringtheory::MetaString;
use tokio::{
    pin, select,
    sync::{mpsc, oneshot},
    time::{interval, interval_at},
};
use tracing::{debug, error, info, trace, warn};

mod telemetry;
use self::telemetry::Telemetry;

mod config;
use self::config::{HistogramConfiguration, HistogramStatistic};

const PASSTHROUGH_IDLE_FLUSH_CHECK_INTERVAL: Duration = Duration::from_secs(2);
const CONTEXT_SNAPSHOT_REQUEST_CHANNEL_CAPACITY: usize = 1;

/// The shape of metric values retained by the aggregate transform.
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

/// A retained metric context and its small aggregation metadata.
///
/// Cloning an entry shares the underlying context name and tags rather than copying their contents.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AggregateContextSnapshotEntry {
    context: Context,
    metric_type: AggregateMetricType,
    unit: MetaString,
}

impl AggregateContextSnapshotEntry {
    /// Returns the retained metric context.
    pub fn context(&self) -> &Context {
        &self.context
    }

    /// Returns the shape of the retained metric values.
    pub fn metric_type(&self) -> AggregateMetricType {
        self.metric_type
    }

    /// Returns the unit attached to the retained metric values, if one is set.
    pub fn unit(&self) -> Option<&str> {
        if self.unit.is_empty() {
            None
        } else {
            Some(&self.unit)
        }
    }

    /// Creates a snapshot entry for downstream test and benchmark fixtures.
    #[cfg(any(test, feature = "test-util"))]
    pub fn for_test(context: Context, metric_type: AggregateMetricType, unit: MetaString) -> Self {
        Self {
            context,
            metric_type,
            unit,
        }
    }
}

type AggregateContextSnapshot = Vec<AggregateContextSnapshotEntry>;
type AggregateContextSnapshotRequest = oneshot::Sender<AggregateContextSnapshot>;
type AggregateContextSnapshotRequestReceiver = mpsc::Receiver<AggregateContextSnapshotRequest>;

/// A handle for requesting retained-context snapshots from an aggregate transform.
///
/// Snapshot construction runs on the aggregate owner task. The returned entries contain shared context handles and
/// small metric metadata, allowing callers to perform heavier processing after the owner resumes ingestion.
#[derive(Clone, Debug)]
pub struct AggregateContextSnapshotHandle {
    requests: mpsc::Sender<AggregateContextSnapshotRequest>,
}

impl AggregateContextSnapshotHandle {
    /// Requests the aggregate transform's current retained contexts.
    ///
    /// The returned snapshot uses O(context count) memory. After delivery, the caller owns this memory, so callers that
    /// retain snapshots must include their retained size in their own memory accounting.
    ///
    /// # Errors
    ///
    /// Returns an error if the aggregate owner is unavailable or stops before responding.
    pub async fn snapshot(&self) -> Result<Vec<AggregateContextSnapshotEntry>, GenericError> {
        let (response_tx, response_rx) = oneshot::channel();
        self.requests
            .send(response_tx)
            .await
            .map_err(|_| generic_error!("aggregate context snapshot owner is unavailable"))?;

        response_rx
            .await
            .map_err(|_| generic_error!("aggregate context snapshot owner stopped before responding"))
    }
}

fn aggregate_context_snapshot_channel() -> (AggregateContextSnapshotHandle, AggregateContextSnapshotRequestReceiver) {
    let (requests, receiver) = mpsc::channel(CONTEXT_SNAPSHOT_REQUEST_CHANNEL_CAPACITY);
    (AggregateContextSnapshotHandle { requests }, receiver)
}

/// An accepted retained-context snapshot request for test fixtures.
#[cfg(any(test, feature = "test-util"))]
pub struct AggregateContextSnapshotPendingResponse {
    response: AggregateContextSnapshotRequest,
}

#[cfg(any(test, feature = "test-util"))]
impl AggregateContextSnapshotPendingResponse {
    /// Responds to the accepted snapshot request with the supplied entries.
    ///
    /// If the requester was canceled after the request was accepted, the response is discarded.
    pub fn respond(self, snapshot: Vec<AggregateContextSnapshotEntry>) {
        let _ = self.response.send(snapshot);
    }
}

/// A responder for retained-context snapshot test fixtures.
#[cfg(any(test, feature = "test-util"))]
pub struct AggregateContextSnapshotResponder {
    receiver: AggregateContextSnapshotRequestReceiver,
}

#[cfg(any(test, feature = "test-util"))]
impl AggregateContextSnapshotResponder {
    /// Waits for one snapshot request and responds with the supplied entries.
    ///
    /// A canceled requester is treated as a successful no-op delivery.
    ///
    /// # Errors
    ///
    /// Returns an error if the request channel closes before a request arrives.
    pub async fn respond(&mut self, snapshot: Vec<AggregateContextSnapshotEntry>) -> Result<(), GenericError> {
        self.receive().await?.respond(snapshot);
        Ok(())
    }

    /// Waits for one snapshot request and returns its pending response.
    ///
    /// The returned response lets tests deterministically control whether the owner responds, stops, or outlives a
    /// canceled requester after accepting the request.
    ///
    /// # Errors
    ///
    /// Returns an error if the request channel closes before a request arrives.
    pub async fn receive(&mut self) -> Result<AggregateContextSnapshotPendingResponse, GenericError> {
        let response = self
            .receiver
            .recv()
            .await
            .ok_or_else(|| generic_error!("aggregate context snapshot request channel is closed"))?;
        Ok(AggregateContextSnapshotPendingResponse { response })
    }

    /// Waits for one snapshot request and stops without responding.
    ///
    /// This accepts the request from the owner channel before dropping its one-shot response sender, allowing tests to
    /// distinguish an owner that stops mid-request from an owner whose request channel is unavailable.
    ///
    /// # Errors
    ///
    /// Returns an error if the request channel closes before a request arrives.
    pub async fn stop_after_receiving(&mut self) -> Result<(), GenericError> {
        drop(self.receive().await?);
        Ok(())
    }
}

/// Creates a retained-context snapshot handle and owner-side responder for tests.
#[cfg(any(test, feature = "test-util"))]
pub fn aggregate_context_snapshot_channel_for_test(
) -> (AggregateContextSnapshotHandle, AggregateContextSnapshotResponder) {
    let (handle, receiver) = aggregate_context_snapshot_channel();
    (handle, AggregateContextSnapshotResponder { receiver })
}

struct AggregateContextSnapshotChannel {
    handle: AggregateContextSnapshotHandle,
    receiver: Mutex<Option<AggregateContextSnapshotRequestReceiver>>,
}

impl AggregateContextSnapshotChannel {
    fn take_receiver(&self) -> Result<AggregateContextSnapshotRequestReceiver, GenericError> {
        let mut receiver = self
            .receiver
            .lock()
            .map_err(|_| generic_error!("aggregate context snapshot receiver lock is poisoned"))?;
        receiver
            .take()
            .ok_or_else(|| generic_error!("aggregate context snapshot receiver has already been taken"))
    }
}

impl Default for AggregateContextSnapshotChannel {
    fn default() -> Self {
        let (handle, receiver) = aggregate_context_snapshot_channel();
        Self {
            handle,
            receiver: Mutex::new(Some(receiver)),
        }
    }
}

#[cfg(test)]
impl std::fmt::Debug for AggregateContextSnapshotChannel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AggregateContextSnapshotChannel")
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
impl PartialEq for AggregateContextSnapshotChannel {
    fn eq(&self, _other: &Self) -> bool {
        true
    }
}

const fn default_window_duration_seconds() -> NonZeroU64 {
    NonZeroU64::new(10).expect("not zero")
}

const fn default_primary_flush_interval() -> Duration {
    Duration::from_secs(15)
}

const fn default_context_limit() -> usize {
    1_000_000
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

/// Aggregate transform.
///
/// Aggregates metrics into fixed-size windows, flushing them at a regular interval.
///
/// ## Zero-value counters
///
/// When metrics are aggregated and then flushed, they're typically removed entirely from the aggregation state. Unless
/// they're updated again, they won't be emitted again. However, for counters, a slightly different approach is
/// taken by tracking "zero-value" counters.
///
/// Counters are aggregated and flushed normally. However, when flushed, counters are added to a list of "zero-value"
/// counters, and if those counters aren't updated again, the transform emits a copy of the counter with a value of
/// zero. It does this until the counter is updated again, or the zero-value counter expires (no updates), whichever
/// comes first.
///
/// This provides a continuity in the output of a counter, from the perspective of a downstream system, when counters
/// are otherwise sparse. The expiration period is configurable, and allows a trade-off in how sparse/infrequent the
/// updates to counters can be versus how long it takes for counters that don't exist anymore to actually cease to be
/// emitted.
#[derive(Deserialize)]
#[cfg_attr(test, derive(Debug, PartialEq, serde::Serialize))]
pub struct AggregateConfiguration {
    /// Size of the aggregation window, in seconds.
    ///
    /// Metrics are aggregated into fixed-size windows, such that all updates to the same metric within a window are
    /// aggregated into a single metric. The window size controls how efficiently metrics are aggregated, and in turn,
    /// how many data points are emitted downstream.
    ///
    /// Window durations cannot be zero.
    ///
    /// Defaults to 10 seconds.
    #[serde(
        rename = "aggregate_window_duration_seconds",
        default = "default_window_duration_seconds"
    )]
    window_duration_seconds: NonZeroU64,

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
    /// Defaults to 1,000,000.
    #[serde(rename = "aggregate_context_limit", default = "default_context_limit")]
    context_limit: usize,

    /// Whether to flush open buckets when stopping the transform.
    ///
    /// Normally, open buckets (a bucket whose end hasn't yet occurred) aren't flushed when the transform is stopped.
    /// This is done to avoid the chance of flushing a partial window, restarting the process, and then flushing the
    /// same window again. Downstream systems sometimes can't cope with this gracefully, as there is no way to
    /// determine that it's an incremental update, and so they treat it as an absolute update, overwriting the
    /// previously flushed value.
    ///
    /// In cases where flushing all outstanding data is paramount, this can be enabled.
    ///
    /// Defaults to `false`.
    #[serde(
        rename = "aggregate_flush_open_windows",
        alias = "dogstatsd_flush_incomplete_buckets",
        default
    )]
    flush_open_windows: bool,

    /// How long to keep idle counters alive after they've been flushed, in seconds.
    ///
    /// When metrics are flushed, they're removed from the aggregation state. However, if a counter expiration is set,
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
    /// While passthrough metrics aren't re-aggregated by the transform, they will still be temporarily buffered in
    /// order to optimize the efficiency of processing them in the next component. This setting controls the maximum
    /// amount of time that passthrough metrics will be buffered before being forwarded.
    ///
    /// Defaults to 1 seconds.
    #[serde(
        rename = "aggregate_passthrough_idle_flush_timeout",
        default = "default_passthrough_idle_flush_timeout"
    )]
    passthrough_idle_flush_timeout: Duration,

    /// Histogram aggregation configuration.
    ///
    /// Controls the aggregates/percentiles that are generated for distributions in "histogram" mode (client-side
    /// distribution aggregation).
    #[serde(flatten)]
    hist_config: HistogramConfiguration,

    /// Owner-side channel used to coordinate retained-context snapshots.
    ///
    /// This runtime-only channel is never read from or written to configuration data.
    #[serde(skip, default)]
    context_snapshot_channel: AggregateContextSnapshotChannel,
}

impl AggregateConfiguration {
    /// Creates a new `AggregateConfiguration` from the given configuration.
    pub fn from_configuration(config: &GenericConfiguration) -> Result<Self, GenericError> {
        Ok(config.as_typed()?)
    }

    /// Creates a new `AggregateConfiguration` with default values.
    pub fn with_defaults() -> Self {
        Self {
            window_duration_seconds: default_window_duration_seconds(),
            primary_flush_interval: default_primary_flush_interval(),
            context_limit: default_context_limit(),
            flush_open_windows: false,
            counter_expiry_seconds: default_counter_expiry_seconds(),
            passthrough_timestamped_metrics: default_passthrough_timestamped_metrics(),
            passthrough_idle_flush_timeout: default_passthrough_idle_flush_timeout(),
            hist_config: HistogramConfiguration::default(),
            context_snapshot_channel: AggregateContextSnapshotChannel::default(),
        }
    }

    /// Returns a handle for requesting retained-context snapshots from the built aggregate transform.
    pub fn context_snapshot_handle(&self) -> AggregateContextSnapshotHandle {
        self.context_snapshot_channel.handle.clone()
    }
}

#[async_trait]
impl TransformBuilder for AggregateConfiguration {
    async fn build(&self, context: ComponentContext) -> Result<Box<dyn Transform + Send>, GenericError> {
        let context_snapshot_requests = self.context_snapshot_channel.take_receiver()?;
        let metrics_builder = MetricsBuilder::from_component_context(&context);
        let telemetry = Telemetry::new(&metrics_builder);

        let state = AggregationState::new(
            self.window_duration_seconds,
            self.context_limit,
            self.counter_expiry_seconds.filter(|s| *s != 0).map(Duration::from_secs),
            self.hist_config.clone(),
            telemetry.clone(),
        );

        let passthrough_batcher = PassthroughBatcher::new(
            self.passthrough_idle_flush_timeout,
            self.window_duration_seconds,
            telemetry.clone(),
        )
        .await;

        Ok(Box::new(Aggregate {
            state,
            telemetry,
            primary_flush_interval: self.primary_flush_interval,
            flush_open_windows: self.flush_open_windows,
            passthrough_batcher,
            passthrough_timestamped_metrics: self.passthrough_timestamped_metrics,
            context_snapshot_requests: Some(context_snapshot_requests),
        }))
    }

    fn input_event_type(&self) -> EventType {
        EventType::Metric
    }

    fn outputs(&self) -> &[OutputDefinition<EventType>] {
        static OUTPUTS: &[OutputDefinition<EventType>] = &[OutputDefinition::default_output(EventType::Metric)];
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

        builder
            .minimum()
            // Capture the size of the heap allocation when the component is built.
            .with_single_value::<Aggregate>("component struct");
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
            // A snapshot is constructed while the aggregation state remains live, so its peak allocation is additive.
            .with_expr(UsageExpr::product(
                "retained context snapshot",
                UsageExpr::struct_size::<AggregateContextSnapshotEntry>("snapshot entry"),
                UsageExpr::config("aggregate_context_limit", self.context_limit),
            ));
    }
}

pub struct Aggregate {
    state: AggregationState,
    telemetry: Telemetry,
    primary_flush_interval: Duration,
    flush_open_windows: bool,
    passthrough_batcher: PassthroughBatcher,
    passthrough_timestamped_metrics: bool,
    context_snapshot_requests: Option<AggregateContextSnapshotRequestReceiver>,
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

        let passthrough_flush = interval(PASSTHROUGH_IDLE_FLUSH_CHECK_INTERVAL);

        health.mark_ready();
        debug!("Aggregation transform started.");

        pin!(passthrough_flush);

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

                        // Remember if the context limit had been surpassed before this flush.
                        let was_breached = self.state.context_limit_breached();

                        let mut dispatcher = context.dispatcher().buffered().expect("default output should always exist");
                        if let Err(e) = self.state.flush(get_unix_timestamp(), should_flush_open_windows, &mut dispatcher).await {
                            error!(error = %e, "Failed to flush aggregation state.");
                        }

                        self.telemetry.increment_flushes();

                        // If flush recovered us from a breach, log the recovery.
                        if was_breached && !self.state.context_limit_breached() {
                            info!("Context limit no longer exceeded, metrics are being accepted again.");
                        }

                        match dispatcher.flush().await {
                            Ok(aggregated_events) => debug!(aggregated_events, "Dispatched events."),
                            Err(e) => error!(error = %e, "Failed to flush aggregated events."),
                        }
                    }

                    // If this is the final flush, we break out of the loop.
                    if final_primary_flush {
                        debug!("All aggregation complete.");
                        break
                    }
                },
                _ = passthrough_flush.tick() => self.passthrough_batcher.try_flush(context.dispatcher()).await,
                snapshot_request = receive_context_snapshot_request(&mut self.context_snapshot_requests) => {
                    match snapshot_request {
                        Some(response) => {
                            let snapshot = self.state.snapshot_contexts();
                            let _ = response.send(snapshot);
                        }
                        None => self.context_snapshot_requests = None,
                    }
                },
                maybe_events = context.events().next(), if !final_primary_flush => match maybe_events {
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
                                        self.passthrough_batcher.push_metric(timestamped_metric, context.dispatcher()).await;
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

                                let was_breached = self.state.context_limit_breached();
                                if !self.state.insert(current_time, metric) {
                                    trace!("Dropping metric due to context limit.");
                                    if !was_breached {
                                        // First drop since the last recovery — emit a single warning.
                                        warn!(context_limit = self.state.context_limit, "Context limit reached, \
                                        dropping metrics. Consider increasing `aggregate_context_limit`.");
                                    }
                                    self.telemetry.increment_events_dropped();
                                }
                            }
                        }

                        if processed_passthrough_metrics {
                            self.passthrough_batcher.update_last_processed_at();
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
        self.passthrough_batcher.try_flush(context.dispatcher()).await;

        debug!("Aggregation transform stopped.");

        Ok(())
    }
}

async fn receive_context_snapshot_request(
    receiver: &mut Option<AggregateContextSnapshotRequestReceiver>,
) -> Option<AggregateContextSnapshotRequest> {
    match receiver {
        Some(receiver) => receiver.recv().await,
        None => pending().await,
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
    active_buffer: EventsBuffer,
    active_buffer_start: Instant,
    last_processed_at: Instant,
    idle_flush_timeout: Duration,
    bucket_width_secs: NonZeroU64,
    telemetry: Telemetry,
}

impl PassthroughBatcher {
    async fn new(idle_flush_timeout: Duration, bucket_width_secs: NonZeroU64, telemetry: Telemetry) -> Self {
        let active_buffer = EventsBuffer::default();

        Self {
            active_buffer,
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

            match dispatcher.dispatch(old_active_buffer).await {
                Ok(()) => debug!(unaggregated_events, "Dispatched events."),
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
    fn new(
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

    fn is_empty(&self) -> bool {
        self.contexts.is_empty()
    }

    fn snapshot_contexts(&self) -> Vec<AggregateContextSnapshotEntry> {
        let mut snapshot = Vec::with_capacity(self.contexts.len());
        for (context, aggregated) in &self.contexts {
            snapshot.push(AggregateContextSnapshotEntry {
                context: context.clone(),
                metric_type: AggregateMetricType::from(&aggregated.values),
                unit: aggregated.metadata.unit.clone(),
            });
        }
        snapshot
    }

    fn insert(&mut self, timestamp: u64, metric: Metric) -> bool {
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

    async fn flush(
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

    fn context_limit_breached(&self) -> bool {
        self.context_limit_breached
    }
}

/// A benchmark fixture backed by a real aggregate state populated before snapshot timing begins.
#[cfg(any(test, feature = "test-util"))]
pub struct AggregateContextSnapshotBenchmarkHarness {
    state: AggregationState,
}

#[cfg(any(test, feature = "test-util"))]
impl AggregateContextSnapshotBenchmarkHarness {
    /// Creates a harness retaining exactly `context_count` distinct gauge contexts.
    pub fn with_contexts(context_count: usize) -> Self {
        let mut state = AggregationState::new(
            default_window_duration_seconds(),
            context_count,
            None,
            HistogramConfiguration::default(),
            Telemetry::new(&MetricsBuilder::default()),
        );

        for index in 0..context_count {
            let context = Context::from_parts(
                format!("aggregate.snapshot.benchmark.{index}"),
                saluki_context::tags::TagSet::default(),
            );
            assert!(
                state.insert(0, Metric::gauge(context, 0.0)),
                "benchmark fixture must retain every requested context"
            );
        }

        Self { state }
    }

    /// Takes a retained-context snapshot from the populated aggregate state.
    pub fn snapshot(&self) -> Vec<AggregateContextSnapshotEntry> {
        self.state.snapshot_contexts()
    }

    /// Returns the exact number of contexts retained by the aggregate state.
    pub fn retained_len(&self) -> usize {
        self.state.contexts.len()
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

// TODO: One thing we ought to consider is a property test, specifically a state machine property test, where we
// generate a randomized offset to start time from, a bucket width, flush interval, and operations, and so on... and
// then we run it to make sure that we are always generating sequential timestamps for data points, etc.
#[cfg(test)]
mod tests {
    use std::mem::size_of;

    use float_cmp::ApproxEqRatio as _;
    use saluki_context::tags::{Tag, TagSet};
    use saluki_core::{
        accounting::{ComponentRegistry, MemoryLimiter},
        components::{
            destinations::{Destination, DestinationBuilder, DestinationContext},
            sources::{Source, SourceBuilder, SourceContext},
            ComponentContext,
        },
        health::HealthRegistry,
        runtime::Supervisor,
        support::SubsystemIdentifier,
        topology::{interconnect::Dispatcher, OutputDefinition, OutputName, TopologyBlueprint},
    };
    use saluki_metrics::test::TestRecorder;
    use stringtheory::MetaString;
    use tokio::sync::{mpsc, oneshot};

    use super::config::HistogramStatistic;
    use super::*;

    const BUCKET_WIDTH_SECS: NonZeroU64 = NonZeroU64::new(10).expect("not zero");
    const BUCKET_WIDTH: Duration = Duration::from_secs(BUCKET_WIDTH_SECS.get());
    const COUNTER_EXPIRE_SECS: u64 = 20;
    const COUNTER_EXPIRE: Option<Duration> = Some(Duration::from_secs(COUNTER_EXPIRE_SECS));

    /// Gets the bucket start timestamp for the given step.
    const fn bucket_ts(step: u64) -> u64 {
        align_to_bucket_start(insert_ts(step), BUCKET_WIDTH_SECS)
    }

    /// Gets the insert timestamp for the given step.
    const fn insert_ts(step: u64) -> u64 {
        (BUCKET_WIDTH_SECS.get() * (step + 1)) - 2
    }

    /// Gets the flush timestamp for the given step.
    const fn flush_ts(step: u64) -> u64 {
        BUCKET_WIDTH_SECS.get() * (step + 1)
    }

    struct ControlledMetricSource {
        events: mpsc::Receiver<Event>,
    }

    #[async_trait]
    impl Source for ControlledMetricSource {
        async fn run(mut self: Box<Self>, mut context: SourceContext) -> Result<(), GenericError> {
            let shutdown = context.take_shutdown_handle();
            tokio::pin!(shutdown);
            let mut events_open = true;

            loop {
                select! {
                    _ = &mut shutdown => break,
                    maybe_event = self.events.recv(), if events_open => match maybe_event {
                        Some(event) => context.dispatcher().dispatch_one(event).await?,
                        None => events_open = false,
                    }
                }
            }
            Ok(())
        }
    }

    struct ControlledMetricSourceBuilder {
        events: Mutex<Option<mpsc::Receiver<Event>>>,
        outputs: Vec<OutputDefinition<EventType>>,
    }

    #[async_trait]
    impl SourceBuilder for ControlledMetricSourceBuilder {
        fn outputs(&self) -> &[OutputDefinition<EventType>] {
            &self.outputs
        }

        async fn build(&self, _context: ComponentContext) -> Result<Box<dyn Source + Send>, GenericError> {
            let events = self
                .events
                .lock()
                .map_err(|_| generic_error!("controlled metric source receiver lock is poisoned"))?
                .take()
                .ok_or_else(|| generic_error!("controlled metric source receiver has already been taken"))?;
            Ok(Box::new(ControlledMetricSource { events }))
        }
    }

    impl MemoryBounds for ControlledMetricSourceBuilder {
        fn specify_bounds(&self, _builder: &mut MemoryBoundsBuilder) {}
    }

    struct DrainingMetricDestination;

    #[async_trait]
    impl Destination for DrainingMetricDestination {
        async fn run(self: Box<Self>, mut context: DestinationContext) -> Result<(), GenericError> {
            while context.events().next().await.is_some() {}
            Ok(())
        }
    }

    struct DrainingMetricDestinationBuilder;

    #[async_trait]
    impl DestinationBuilder for DrainingMetricDestinationBuilder {
        fn input_event_type(&self) -> EventType {
            EventType::Metric
        }

        async fn build(&self, _context: ComponentContext) -> Result<Box<dyn Destination + Send>, GenericError> {
            Ok(Box::new(DrainingMetricDestination))
        }
    }

    impl MemoryBounds for DrainingMetricDestinationBuilder {
        fn specify_bounds(&self, _builder: &mut MemoryBoundsBuilder) {}
    }

    struct DispatcherReceiver {
        receiver: mpsc::Receiver<EventsBuffer>,
    }

    impl DispatcherReceiver {
        fn collect_next(&mut self) -> Vec<Metric> {
            match self.receiver.try_recv() {
                Ok(event_buffer) => {
                    let mut metrics = event_buffer
                        .into_iter()
                        .filter_map(|event| event.try_into_metric())
                        .collect::<Vec<Metric>>();

                    metrics.sort_by(|a, b| a.context().name().cmp(b.context().name()));
                    metrics
                }
                Err(_) => Vec::new(),
            }
        }
    }

    /// Constructs a basic `Dispatcher` with a fixed-size event buffer.
    fn build_basic_dispatcher() -> (EventsDispatcher, DispatcherReceiver) {
        let context = ComponentContext::test_transform("test");
        let mut dispatcher = Dispatcher::new(context);

        let (buffer_tx, buffer_rx) = mpsc::channel(1);
        dispatcher.add_output(OutputName::Default).unwrap();
        dispatcher
            .attach_sender_to_output(&OutputName::Default, buffer_tx)
            .unwrap();

        (dispatcher, DispatcherReceiver { receiver: buffer_rx })
    }

    async fn get_flushed_metrics(timestamp: u64, state: &mut AggregationState) -> Vec<Metric> {
        let (dispatcher, mut dispatcher_receiver) = build_basic_dispatcher();
        let mut buffered_dispatcher = dispatcher.buffered().expect("default output should always exist");

        // Flush the metrics to an event buffer.
        state
            .flush(timestamp, true, &mut buffered_dispatcher)
            .await
            .expect("should not fail to flush aggregation state");

        // Flush our buffered dispatcher, which should ensure that the event buffer is sent out, and then read it from the
        // receiver:
        buffered_dispatcher
            .flush()
            .await
            .expect("should not fail to flush buffered sender");

        dispatcher_receiver.collect_next()
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
                    "point for value #{} does not match: {} (expected) vs {} (actual)",
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
    fn aggregate_metric_type_matches_every_metric_shape() {
        let cases = [
            (MetricValues::counter(1.0), AggregateMetricType::Counter),
            (
                MetricValues::rate(1.0, Duration::from_secs(10)),
                AggregateMetricType::Rate,
            ),
            (MetricValues::gauge(1.0), AggregateMetricType::Gauge),
            (MetricValues::set("value"), AggregateMetricType::Set),
            (MetricValues::histogram([1.0]), AggregateMetricType::Histogram),
            (
                MetricValues::distribution(&[1.0][..]),
                AggregateMetricType::Distribution,
            ),
        ];

        for (values, expected) in cases {
            assert_eq!(AggregateMetricType::from(&values), expected);
        }
    }

    #[test]
    fn snapshot_contexts_preserves_full_context_shape_and_unit() {
        let mut state = AggregationState::new(
            BUCKET_WIDTH_SECS,
            10,
            COUNTER_EXPIRE,
            HistogramConfiguration::default(),
            Telemetry::noop(),
        );

        let histogram_context = Context::from_static_parts("request.duration", &["env:prod"])
            .with_host(Some(MetaString::from_static("host-a")))
            .with_origin_tags(TagSet::from(Tag::from_static("container:one")));
        let gauge_context = Context::from_static_parts("request.duration", &["env:prod"])
            .with_host(Some(MetaString::from_static("host-b")))
            .with_origin_tags(TagSet::from(Tag::from_static("container:two")));
        let histogram = Metric::from_parts(
            histogram_context.clone(),
            MetricValues::histogram([12.0]),
            MetricMetadata::default().with_unit(MetaString::from_static("millisecond")),
        );
        let gauge = Metric::gauge(gauge_context.clone(), 2.0);

        assert!(state.insert(insert_ts(1), histogram));
        assert!(state.insert(insert_ts(1), gauge));

        let mut snapshot = state.snapshot_contexts();
        assert_eq!(snapshot.len(), 2);
        assert!(snapshot.capacity() >= state.contexts.len());
        snapshot.sort_by(|a, b| a.context().host().cmp(&b.context().host()));

        assert_eq!(snapshot[0].context(), &histogram_context);
        assert_eq!(snapshot[0].context().tags().len(), 1);
        assert_eq!(snapshot[0].context().origin_tags().len(), 1);
        assert_eq!(snapshot[0].metric_type(), AggregateMetricType::Histogram);
        assert_eq!(snapshot[0].unit(), Some("millisecond"));

        assert_eq!(snapshot[1].context(), &gauge_context);
        assert_eq!(snapshot[1].metric_type(), AggregateMetricType::Gauge);
        assert_eq!(snapshot[1].unit(), None);
    }

    #[tokio::test]
    async fn snapshot_contexts_follows_ordinary_context_lifecycle() {
        let mut state = AggregationState::new(
            BUCKET_WIDTH_SECS,
            10,
            COUNTER_EXPIRE,
            HistogramConfiguration::default(),
            Telemetry::noop(),
        );
        let context = Context::from_static_name("active.gauge");

        assert!(state.insert(insert_ts(1), Metric::gauge(context.clone(), 1.0)));
        assert_eq!(state.snapshot_contexts().len(), 1);

        let _ = get_flushed_metrics(flush_ts(1), &mut state).await;
        assert!(state.snapshot_contexts().is_empty());
    }

    #[tokio::test]
    async fn snapshot_contexts_retains_idle_counter_until_expiry() {
        let mut state = AggregationState::new(
            BUCKET_WIDTH_SECS,
            10,
            COUNTER_EXPIRE,
            HistogramConfiguration::default(),
            Telemetry::noop(),
        );
        let context = Context::from_static_name("sparse.counter");

        assert!(state.insert(insert_ts(1), Metric::counter(context.clone(), 1.0)));
        let _ = get_flushed_metrics(flush_ts(1), &mut state).await;
        let first_snapshot = state.snapshot_contexts();
        assert_eq!(first_snapshot.len(), 1);
        assert_eq!(first_snapshot[0].context(), &context);
        assert_eq!(first_snapshot[0].metric_type(), AggregateMetricType::Counter);

        let _ = get_flushed_metrics(flush_ts(2), &mut state).await;
        assert_eq!(state.snapshot_contexts().len(), 1);

        let _ = get_flushed_metrics(flush_ts(3), &mut state).await;
        assert!(state.snapshot_contexts().is_empty());
    }

    #[test]
    fn aggregate_memory_bounds_include_peak_context_snapshot() {
        let mut config = AggregateConfiguration::with_defaults();
        config.context_limit = 17;
        let registry = ComponentRegistry::default();
        config.specify_bounds(&mut registry.bounds_builder(&SubsystemIdentifier::from_dotted("test")));
        let bounds = registry.as_bounds();

        let expected_minimum = size_of::<Aggregate>();
        let aggregation_state_bytes = config.context_limit * (size_of::<Context>() + size_of::<AggregatedMetric>());
        let context_snapshot_bytes = config.context_limit * size_of::<AggregateContextSnapshotEntry>();

        assert_eq!(bounds.total_minimum_required_bytes(), expected_minimum);
        assert_eq!(
            bounds.total_firm_limit_bytes(),
            expected_minimum + aggregation_state_bytes + context_snapshot_bytes
        );
    }

    #[tokio::test]
    async fn production_owner_loop_serves_snapshots_and_stops_after_cancellation() {
        tokio::time::timeout(Duration::from_secs(5), async {
            let mut config = AggregateConfiguration::with_defaults();
            config.primary_flush_interval = Duration::from_secs(60);
            let snapshot_handle = config.context_snapshot_handle();

            let available_request_capacity = snapshot_handle.requests.capacity();
            let canceled_request = tokio::spawn({
                let snapshot_handle = snapshot_handle.clone();
                async move { snapshot_handle.snapshot().await }
            });
            while snapshot_handle.requests.capacity() == available_request_capacity {
                tokio::task::yield_now().await;
            }
            canceled_request.abort();
            assert!(canceled_request
                .await
                .expect_err("snapshot requester should be canceled")
                .is_cancelled());

            let (events_tx, events_rx) = mpsc::channel(1);
            let source = ControlledMetricSourceBuilder {
                events: Mutex::new(Some(events_rx)),
                outputs: vec![OutputDefinition::default_output(EventType::Metric)],
            };
            let component_registry = ComponentRegistry::default();
            let mut blueprint = TopologyBlueprint::new("aggregate_snapshot_owner", &component_registry);
            blueprint
                .add_source("source", source)
                .expect("controlled source should be accepted")
                .add_transform("aggregate", config)
                .expect("aggregate transform should be accepted")
                .add_destination("destination", DrainingMetricDestinationBuilder)
                .expect("draining destination should be accepted");
            blueprint
                .connect_components_in_order(["source", "aggregate", "destination"])
                .expect("test topology should connect");
            blueprint
                .with_health_registry(HealthRegistry::new())
                .with_memory_limiter(MemoryLimiter::noop())
                .with_ambient_worker_pool();

            let mut supervisor =
                Supervisor::new("aggregate-snapshot-owner").expect("test supervisor should be created");
            supervisor.add_worker(blueprint);
            let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
            let topology_task = tokio::spawn(async move { supervisor.run_with_shutdown(shutdown_rx).await });

            let expected_context = Context::from_static_name("owner.loop.gauge");
            events_tx
                .send(Event::Metric(Metric::gauge(expected_context.clone(), 1.0)))
                .await
                .expect("controlled source should accept an event");

            let snapshot = loop {
                let snapshot = snapshot_handle
                    .snapshot()
                    .await
                    .expect("running aggregate should fulfill snapshots");
                if snapshot.iter().any(|entry| entry.context() == &expected_context) {
                    break snapshot;
                }
                tokio::task::yield_now().await;
            };
            let entry = snapshot
                .iter()
                .find(|entry| entry.context() == &expected_context)
                .expect("snapshot should retain the inserted context");
            assert_eq!(entry.metric_type(), AggregateMetricType::Gauge);
            assert_eq!(entry.unit(), None);

            drop(events_tx);
            drop(snapshot_handle);
            shutdown_tx.send(()).expect("test topology should still be running");
            let topology_result = topology_task.await.expect("topology task should not panic");
            assert!(
                topology_result.is_ok(),
                "topology should stop cleanly: {topology_result:?}"
            );
        })
        .await
        .expect("production aggregate owner loop should complete without spinning or hanging");
    }

    #[tokio::test]
    async fn snapshot_handle_round_trips_through_responder() {
        let (handle, mut responder) = aggregate_context_snapshot_channel_for_test();
        let expected = vec![AggregateContextSnapshotEntry::for_test(
            Context::from_static_name("round.trip"),
            AggregateMetricType::Gauge,
            MetaString::from_static("widget"),
        )];
        let snapshot_task = tokio::spawn(async move { handle.snapshot().await });

        responder
            .respond(expected.clone())
            .await
            .expect("responder should receive and fulfill a snapshot request");

        let actual = snapshot_task
            .await
            .expect("snapshot task should complete")
            .expect("snapshot request should succeed");
        assert_eq!(actual, expected);
    }

    #[tokio::test]
    async fn snapshot_handle_reports_dropped_receiver() {
        let (handle, responder) = aggregate_context_snapshot_channel_for_test();
        drop(responder);

        let error = handle
            .snapshot()
            .await
            .expect_err("snapshot should fail after its owner is dropped");
        assert!(error.to_string().contains("unavailable"));
    }

    #[tokio::test]
    async fn snapshot_responder_stops_after_accepting_request_and_cancels_response() {
        let (handle, mut responder) = aggregate_context_snapshot_channel_for_test();
        let snapshot_task = tokio::spawn(async move { handle.snapshot().await });

        responder
            .stop_after_receiving()
            .await
            .expect("responder should accept the snapshot request before stopping");

        let error = snapshot_task
            .await
            .expect("snapshot task should complete")
            .expect_err("snapshot should fail when the accepted response is dropped");
        assert!(error.to_string().contains("stopped before responding"));
    }

    #[tokio::test]
    async fn aggregate_configuration_receiver_can_only_be_taken_once() {
        let config = AggregateConfiguration::with_defaults();
        let _handle = config.context_snapshot_handle();
        let first = config.build(ComponentContext::test_transform("aggregate_one")).await;
        assert!(first.is_ok());

        let second = config.build(ComponentContext::test_transform("aggregate_two")).await;
        let error = match second {
            Ok(_) => panic!("second build should not take the snapshot receiver again"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("already been taken"));
    }

    #[tokio::test]
    async fn snapshot_responder_reports_closed_request_channel() {
        let (handle, mut responder) = aggregate_context_snapshot_channel_for_test();
        drop(handle);

        let error = responder
            .respond(Vec::new())
            .await
            .expect_err("responder should fail when every request handle is dropped");
        assert!(error.to_string().contains("closed"));
    }

    #[tokio::test]
    async fn snapshot_responder_ignores_canceled_requester() {
        let (handle, mut responder) = aggregate_context_snapshot_channel_for_test();
        let snapshot_task = tokio::spawn(async move { handle.snapshot().await });
        let pending_response = responder
            .receive()
            .await
            .expect("responder should accept the snapshot request");

        snapshot_task.abort();
        let _ = snapshot_task.await;

        pending_response.respond(Vec::new());
    }

    #[test]
    fn snapshot_benchmark_harness_populates_exact_context_count() {
        let empty_harness = AggregateContextSnapshotBenchmarkHarness::with_contexts(0);
        assert_eq!(empty_harness.retained_len(), 0);
        assert!(empty_harness.snapshot().is_empty());

        let harness = AggregateContextSnapshotBenchmarkHarness::with_contexts(32);
        assert_eq!(harness.retained_len(), 32);
        assert_eq!(harness.snapshot().len(), 32);
    }

    #[test]
    fn bucket_is_closed() {
        // Cases are defined as:
        // (current time, bucket start, bucket width, flush open buckets, expected result)
        let cases = [
            // Bucket goes from [995, 1005), current time of 1000, so bucket is open.
            (1000, 995, BUCKET_WIDTH_SECS, false, false),
            (1000, 995, BUCKET_WIDTH_SECS, true, true),
            // Bucket goes from [1000, 1010), current time of 1000, so bucket is open.
            (1000, 1000, BUCKET_WIDTH_SECS, false, false),
            (1000, 1000, BUCKET_WIDTH_SECS, true, true),
            // Bucket goes from [1000, 1010), current time of 1010, so bucket is closed.
            (1010, 1000, BUCKET_WIDTH_SECS, false, true),
            (1010, 1000, BUCKET_WIDTH_SECS, true, true),
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
            BUCKET_WIDTH_SECS,
            2,
            COUNTER_EXPIRE,
            HistogramConfiguration::default(),
            Telemetry::noop(),
        );

        // Create four unique gauges, and insert all of them. The third and fourth should fail because we've reached
        // the context limit.
        let input_metrics = [
            Metric::gauge("metric1", 1.0),
            Metric::gauge("metric2", 2.0),
            Metric::gauge("metric3", 3.0),
            Metric::gauge("metric4", 4.0),
        ];

        assert!(!state.context_limit_breached());

        assert!(state.insert(insert_ts(1), input_metrics[0].clone()));
        assert!(state.insert(insert_ts(1), input_metrics[1].clone()));
        assert!(!state.context_limit_breached());

        assert!(!state.insert(insert_ts(1), input_metrics[2].clone()));
        assert!(state.context_limit_breached());
        assert!(!state.insert(insert_ts(1), input_metrics[3].clone()));
        assert!(state.context_limit_breached());

        // We should only see the first two gauges after flushing. The flush should also clear the breached flag since
        // contexts drop below the limit.
        let flushed_metrics = get_flushed_metrics(flush_ts(1), &mut state).await;
        assert_eq!(flushed_metrics.len(), 2);
        assert_eq!(input_metrics[0].context(), flushed_metrics[0].context());
        assert_eq!(input_metrics[1].context(), flushed_metrics[1].context());
        assert!(!state.context_limit_breached());

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
            BUCKET_WIDTH_SECS,
            2,
            COUNTER_EXPIRE,
            HistogramConfiguration::default(),
            Telemetry::noop(),
        );

        // Create our input metrics.
        let input_metrics = [
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
            BUCKET_WIDTH_SECS,
            10,
            COUNTER_EXPIRE,
            HistogramConfiguration::default(),
            Telemetry::noop(),
        );

        // Create two unique counters, and insert both of them.
        let input_metrics = [Metric::counter("metric1", 1.0), Metric::counter("metric2", 2.0)];

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
            BUCKET_WIDTH_SECS,
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
        let hist_config = HistogramConfiguration::from_statistics(
            &[
                HistogramStatistic::Count,
                HistogramStatistic::Sum,
                HistogramStatistic::Percentile {
                    q: 0.5,
                    suffix: "p50".into(),
                },
            ],
            false,
            "".into(),
        );
        let mut state = AggregationState::new(BUCKET_WIDTH_SECS, 10, COUNTER_EXPIRE, hist_config, Telemetry::noop());

        // Create one multi-value histogram and insert it.
        let input_metric = Metric::histogram("metric1", [1.0, 2.0, 3.0, 4.0, 5.0]);
        assert!(state.insert(insert_ts(1), input_metric.clone()));

        // Flush the aggregation state, and observe that we've emitted all of the configured distribution statistics in
        // the form of three metrics: count, sum, and p50.
        let flushed_metrics = get_flushed_metrics(flush_ts(1), &mut state).await;
        assert_eq!(flushed_metrics.len(), 3);

        // Create versions of the metric for each of the statistics we're expecting to emit. The values themselves don't
        // matter here, but we do need a `Metric` for it to compare the context to.
        let count_metric = Metric::rate("metric1.count", 0.0, Duration::from_secs(BUCKET_WIDTH_SECS.get()));
        let sum_metric = Metric::gauge("metric1.sum", 0.0);
        let p50_metric = Metric::gauge("metric1.p50", 0.0);

        // We use a less strict error ratio (how much the expected vs actual) for the percentile check, as we generally
        // expect the value to be somewhat off the exact value due to the lossy nature of `DDSketch`.
        assert_flushed_scalar_metric!(count_metric, &flushed_metrics[0], [bucket_ts(1) => 5.0]);
        assert_flushed_scalar_metric!(p50_metric, &flushed_metrics[1], [bucket_ts(1) => 3.0], error_ratio => 0.0025);
        assert_flushed_scalar_metric!(sum_metric, &flushed_metrics[2], [bucket_ts(1) => 15.0]);
    }

    #[tokio::test]
    async fn histogram_statistics_unit_propagation() {
        // We're testing that the unit from the input histogram metadata propagates to all flushed output metrics.
        let hist_config = HistogramConfiguration::from_statistics(
            &[
                HistogramStatistic::Count,
                HistogramStatistic::Sum,
                HistogramStatistic::Percentile {
                    q: 0.5,
                    suffix: "p50".into(),
                },
            ],
            false,
            "".into(),
        );
        let mut state = AggregationState::new(BUCKET_WIDTH_SECS, 10, COUNTER_EXPIRE, hist_config, Telemetry::noop());

        // Build a histogram with unit = "millisecond", simulating what arrives from a DogStatsD `ms` metric.
        let context = Context::from_static_parts("metric1", &[]);
        let metadata = MetricMetadata::default().with_unit(MetaString::from_static("millisecond"));
        let input_metric = Metric::from_parts(
            context,
            MetricValues::histogram([1.0_f64, 2.0, 3.0, 4.0, 5.0]),
            metadata,
        );
        assert!(state.insert(insert_ts(1), input_metric));

        let flushed_metrics = get_flushed_metrics(flush_ts(1), &mut state).await;
        assert_eq!(flushed_metrics.len(), 3);

        // Counts are dimensionless: the `.count` series must drop the unit while all other
        // aggregate series carry the unit from the input histogram.
        for metric in &flushed_metrics {
            let name = metric.context().name();
            if name.ends_with(".count") {
                assert_eq!(
                    metric.metadata().unit(),
                    None,
                    "flushed metric '{}' should be dimensionless",
                    name
                );
            } else {
                assert_eq!(
                    metric.metadata().unit(),
                    Some("millisecond"),
                    "flushed metric '{}' should carry unit='millisecond'",
                    name
                );
            }
        }
    }

    #[tokio::test]
    async fn distributions() {
        // We're testing that we pass through distributions untouched.
        let mut state = AggregationState::new(
            BUCKET_WIDTH_SECS,
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
    async fn histogram_copy_to_distribution() {
        let hist_config = HistogramConfiguration::from_statistics(
            &[
                HistogramStatistic::Count,
                HistogramStatistic::Sum,
                HistogramStatistic::Percentile {
                    q: 0.5,
                    suffix: "p50".into(),
                },
            ],
            true,
            "dist_prefix.".into(),
        );
        let mut state = AggregationState::new(BUCKET_WIDTH_SECS, 10, COUNTER_EXPIRE, hist_config, Telemetry::noop());

        // Create one multi-value histogram and insert it.
        let values = [1.0, 2.0, 3.0, 4.0, 5.0];
        let input_metric = Metric::histogram("metric1", values);
        assert!(state.insert(insert_ts(1), input_metric.clone()));

        // Flush the aggregation state, and observe that we've emitted all of the configured distribution statistics in
        // the form of three metrics: count, sum, and p50 as well as the additional metric from copying the histogram.
        let flushed_metrics = get_flushed_metrics(flush_ts(1), &mut state).await;
        assert_eq!(flushed_metrics.len(), 4);

        // Create versions of the metric for each of the statistics we're expecting to emit. The values themselves don't
        // matter here, but we do need a `Metric` for it to compare the context to.
        let count_metric = Metric::rate("metric1.count", 0.0, BUCKET_WIDTH);
        let sum_metric = Metric::gauge("metric1.sum", 0.0);
        let p50_metric = Metric::gauge("metric1.p50", 0.0);
        let expected_distribution = Metric::distribution("dist_prefix.metric1", &values[..]);

        // We use a less strict error ratio (how much the expected vs actual) for the percentile check, as we generally
        // expect the value to be somewhat off the exact value due to the lossy nature of `DDSketch`.
        assert_flushed_distribution_metric!(expected_distribution, &flushed_metrics[0], [bucket_ts(1) => &values[..]]);
        assert_flushed_scalar_metric!(count_metric, &flushed_metrics[1], [bucket_ts(1) => 5.0]);
        assert_flushed_scalar_metric!(p50_metric, &flushed_metrics[2], [bucket_ts(1) => 3.0], error_ratio => 0.0025);
        assert_flushed_scalar_metric!(sum_metric, &flushed_metrics[3], [bucket_ts(1) => 15.0]);
    }

    #[tokio::test]
    async fn nonaggregated_counters_to_rate() {
        let counter_value = 42.0;

        // Create a basic aggregation state.
        let mut state = AggregationState::new(
            BUCKET_WIDTH_SECS,
            10,
            COUNTER_EXPIRE,
            HistogramConfiguration::default(),
            Telemetry::noop(),
        );

        // Create a simple non-aggregated counter, and insert it.
        let input_metric = Metric::counter("metric1", counter_value);
        assert!(state.insert(insert_ts(1), input_metric.clone()));

        // Flush the aggregation state, and observe that we've emitted the expected counter and that it has the right
        // value, but specifically that it's a rate with an interval that matches our configured bucket width:
        let flushed_metrics = get_flushed_metrics(flush_ts(1), &mut state).await;
        assert_eq!(flushed_metrics.len(), 1);
        let flushed_metric = &flushed_metrics[0];

        assert_flushed_scalar_metric!(&input_metric, flushed_metric, [bucket_ts(1) => counter_value]);
        assert_eq!(flushed_metric.values().as_str(), "rate");
    }

    #[tokio::test]
    async fn preaggregated_counters_to_rate() {
        let counter_value = 42.0;
        let timestamp = 123456;

        // Create a basic passthrough batcher and forwarder.
        let mut batcher = PassthroughBatcher::new(Duration::from_nanos(1), BUCKET_WIDTH_SECS, Telemetry::noop()).await;
        let (dispatcher, mut dispatcher_receiver) = build_basic_dispatcher();

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
            BUCKET_WIDTH_SECS,
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

#[cfg(test)]
mod config_smoke {
    use datadog_agent_config_testing::config_registry::structs;
    use datadog_agent_config_testing::run_config_smoke_tests;
    use serde_json::json;

    use super::AggregateConfiguration;
    use crate::config::{DatadogRemapper, KEY_ALIASES};

    #[tokio::test]
    async fn smoke_test() {
        // Duration fields serialize as {secs, nanos}. We inject whole-second values so the nanos
        // sub-fields are always 0 — they are not independently configurable.
        run_config_smoke_tests(
            structs::AGGREGATE_CONFIGURATION,
            &[
                "aggregate_flush_interval.nanos",
                "aggregate_passthrough_idle_flush_timeout.nanos",
            ],
            json!({}),
            |cfg| {
                cfg.as_typed::<AggregateConfiguration>()
                    .expect("AggregateConfiguration should deserialize")
            },
            KEY_ALIASES,
            DatadogRemapper::new,
        )
        .await
    }
}
