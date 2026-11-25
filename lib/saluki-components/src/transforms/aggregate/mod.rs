use std::{
    future::pending,
    num::NonZeroU64,
    sync::Mutex,
    time::{Duration, Instant},
};

use async_trait::async_trait;
use saluki_common::time::get_unix_timestamp;
use saluki_core::{
    accounting::{MemoryBounds, MemoryBoundsBuilder, UsageExpr},
    components::{transforms::*, BuildContext},
    data_model::event::{
        metric::{context::Context, *},
        Event, EventType,
    },
    observability::ComponentMetricsExt as _,
    topology::{interconnect::BufferedDispatcher, EventsBuffer, OutputDefinition},
};
use saluki_error::{generic_error, GenericError};
#[cfg(test)]
use saluki_metric_kernels::aggregate::{align_to_bucket_start, is_bucket_closed};
use saluki_metric_kernels::aggregate::{expand_flushed_metric, extract_closed_values, AggregateCore, AggregatedMetric};
pub use saluki_metric_kernels::aggregate::{AggregateContextSnapshotEntry, AggregateMetricType};
use saluki_metrics::MetricsBuilder;
use tokio::{
    select,
    sync::{mpsc, oneshot},
    time::interval_at,
};
use tracing::{debug, error, info, trace, warn};

mod telemetry;
use self::telemetry::{origin_telemetry_bytes, FlushCounts, OriginContextId, Telemetry};

mod config;
pub use self::config::HistogramConfiguration;

const CONTEXT_SNAPSHOT_REQUEST_CHANNEL_CAPACITY: usize = 1;

type AggregateContextSnapshot = Vec<AggregateContextSnapshotEntry>;
pub(crate) type AggregateContextSnapshotRequest = oneshot::Sender<AggregateContextSnapshot>;
pub(crate) type AggregateContextSnapshotRequestReceiver = mpsc::Receiver<AggregateContextSnapshotRequest>;

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

/// Creates a retained-context snapshot handle and its owner-side receiver.
///
/// The receiver belongs to the aggregate transform, and is supplied to it through
/// [`AggregateConfiguration::context_snapshot_receiver`]. The handle belongs to whoever requests snapshots, and can be
/// cloned to let several callers request them.
pub fn aggregate_context_snapshot_channel() -> (AggregateContextSnapshotHandle, AggregateContextSnapshotReceiver) {
    let (requests, receiver) = mpsc::channel(CONTEXT_SNAPSHOT_REQUEST_CHANNEL_CAPACITY);
    (
        AggregateContextSnapshotHandle { requests },
        AggregateContextSnapshotReceiver {
            receiver: Mutex::new(Some(receiver)),
        },
    )
}

#[inline]
pub(crate) fn send_context_snapshot_if_open(
    response: AggregateContextSnapshotRequest, build_snapshot: impl FnOnce() -> AggregateContextSnapshot,
) {
    if response.is_closed() {
        return;
    }

    let _ = response.send(build_snapshot());
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
    let (requests, receiver) = mpsc::channel(CONTEXT_SNAPSHOT_REQUEST_CHANNEL_CAPACITY);
    (
        AggregateContextSnapshotHandle { requests },
        AggregateContextSnapshotResponder { receiver },
    )
}

/// The owner side of a retained-context snapshot channel.
///
/// The aggregate transform takes the receiver out of this holder when it is built. A receiver can only be taken once,
/// so an [`AggregateConfiguration`] holding one can only build a single transform.
pub struct AggregateContextSnapshotReceiver {
    receiver: Mutex<Option<AggregateContextSnapshotRequestReceiver>>,
}

impl AggregateContextSnapshotReceiver {
    pub(crate) fn take_receiver(&self) -> Result<AggregateContextSnapshotRequestReceiver, GenericError> {
        let mut receiver = self
            .receiver
            .lock()
            .map_err(|_| generic_error!("aggregate context snapshot receiver lock is poisoned"))?;
        receiver
            .take()
            .ok_or_else(|| generic_error!("aggregate context snapshot receiver has already been taken"))
    }
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
pub struct AggregateConfiguration {
    /// Size of the aggregation window, in seconds.
    ///
    /// Metrics are aggregated into fixed-size windows, such that all updates to the same metric within a window are
    /// aggregated into a single metric. The window size controls how efficiently metrics are aggregated, and in turn,
    /// how many data points are emitted downstream.
    pub window_duration_seconds: NonZeroU64,

    /// How often to flush buckets.
    ///
    /// This represents a trade-off between the savings in network bandwidth (sending fewer requests to downstream
    /// systems, etc) and the frequency of updates (how often updates to a metric are emitted).
    pub primary_flush_interval: Duration,

    /// Maximum number of contexts to aggregate per window.
    ///
    /// A context is the unique combination of a metric name and its set of tags. For example,
    /// `metric.name.here{tag1=A,tag2=B}` represents a single context, and would be different than
    /// `metric.name.here{tag1=A,tag2=C}`.
    ///
    /// When the maximum number of contexts is reached in the current aggregation window, additional metrics are dropped
    /// until the next window starts.
    pub context_limit: usize,

    /// Whether to flush open buckets when stopping the transform.
    ///
    /// Normally, open buckets (a bucket whose end hasn't yet occurred) aren't flushed when the transform is stopped.
    /// This is done to avoid the chance of flushing a partial window, restarting the process, and then flushing the
    /// same window again. Downstream systems sometimes can't cope with this gracefully, as there is no way to
    /// determine that it's an incremental update, and so they treat it as an absolute update, overwriting the
    /// previously flushed value.
    ///
    /// In cases where flushing all outstanding data is paramount, this can be enabled.
    pub flush_open_windows: bool,

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
    /// A value of `0`, or `None`, disables idle counter keep-alive.
    pub counter_expiry_seconds: Option<u64>,

    /// Statistics to calculate over histograms, and how to copy them to distributions.
    pub hist_config: HistogramConfiguration,

    /// Whether to emit active context counts grouped by resolved origin tags.
    ///
    /// Defaults to `false` in runtime configuration. Enable this to diagnose origin-specific context usage.
    /// Contexts without origin tags are skipped. At most 199 origin groups and one `origin_overflow:true`
    /// series are registered. Groups exceeding 32 tags or 4096 bytes of encoded labels also contribute to overflow.
    /// A context stays in its original group until expiry; vacant origin slots can admit new contexts. The total
    /// across all series is exact, but an origin's own series is too low while any of its contexts are counted in
    /// overflow. An emptied group reports 0 until idle eviction removes its series.
    pub origin_telemetry_enabled: bool,

    /// Owner side of the channel used to serve retained-context snapshot requests.
    ///
    /// This is runtime wiring rather than configuration: it carries no settings, and is created by the caller with
    /// [`aggregate_context_snapshot_channel`] so that the caller keeps the matching handle.
    pub context_snapshot_receiver: AggregateContextSnapshotReceiver,
}

#[cfg(test)]
impl AggregateConfiguration {
    /// Creates a fixture configuration, for tests that exercise aggregation behavior rather than configuration.
    ///
    /// The snapshot handle paired with the configuration's receiver is dropped. Tests that request snapshots build the
    /// channel with [`aggregate_context_snapshot_channel`] and override `context_snapshot_receiver`.
    fn for_test() -> Self {
        let (_handle, context_snapshot_receiver) = aggregate_context_snapshot_channel();

        Self {
            window_duration_seconds: NonZeroU64::new(10).expect("not zero"),
            primary_flush_interval: Duration::from_secs(15),
            context_limit: 1_000_000,
            flush_open_windows: false,
            counter_expiry_seconds: Some(300),
            hist_config: HistogramConfiguration::default(),
            origin_telemetry_enabled: false,
            context_snapshot_receiver,
        }
    }
}

#[async_trait]
impl TransformBuilder for AggregateConfiguration {
    async fn build(&self, context: BuildContext) -> Result<Box<dyn Transform + Send>, GenericError> {
        let context_snapshot_requests = self.context_snapshot_receiver.take_receiver()?;
        let metrics_builder = MetricsBuilder::from_component_context(context.component_context());
        let telemetry = Telemetry::new(&metrics_builder, self.origin_telemetry_enabled);

        let state = AggregationState::new(
            self.window_duration_seconds,
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
                    UsageExpr::struct_size::<AggregatedMetric<OriginContextId>>("aggregated metric"),
                ),
                UsageExpr::config("aggregate_context_limit", self.context_limit),
            ))
            // A snapshot is constructed while the aggregation state remains live, so its peak allocation is additive.
            .with_expr(UsageExpr::product(
                "retained context snapshot",
                UsageExpr::struct_size::<AggregateContextSnapshotEntry>("snapshot entry"),
                UsageExpr::config("aggregate_context_limit", self.context_limit),
            ));

        if self.origin_telemetry_enabled {
            // Origin telemetry has independent series and byte limits, including owned label storage and
            // registry/resolver copies rather than assuming strings share the metric context's allocation.
            builder.firm().with_fixed_amount(
                "bounded origin context telemetry",
                origin_telemetry_bytes(self.primary_flush_interval),
            );
        }
    }
}

pub struct Aggregate {
    state: AggregationState,
    telemetry: Telemetry,
    primary_flush_interval: Duration,
    flush_open_windows: bool,
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

        health.mark_ready();
        debug!("Aggregation transform started.");

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
                        let flush = async {
                            let flush_start = Instant::now();

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

                            self.telemetry.record_last_flush_duration(flush_start.elapsed());
                        };
                        tokio::pin!(flush);

                        // A flush can wait a long time for space in a full downstream channel. Keep answering liveness
                        // probes while it waits. Probes go first so that a waiting probe is seen on every wakeup, even
                        // when the flush would use up the task's cooperative budget.
                        loop {
                            select! {
                                biased;
                                _ = health.live() => {},
                                _ = &mut flush => break,
                            }
                        }
                    }

                    // If this is the final flush, we break out of the loop.
                    if final_primary_flush {
                        debug!("All aggregation complete.");
                        break
                    }
                },
                snapshot_request = receive_context_snapshot_request(&mut self.context_snapshot_requests) => {
                    match snapshot_request {
                        Some(response) => {
                            send_context_snapshot_if_open(response, || self.state.snapshot_contexts());
                        }
                        None => self.context_snapshot_requests = None,
                    }
                },
                maybe_events = context.events().next(), if !final_primary_flush => match maybe_events {
                    Some(events) => {
                        trace!(events_len = events.len(), "Received events.");

                        let current_time = get_unix_timestamp();

                        for metric in events.into_events::<Metric>() {
                            let was_breached = self.state.context_limit_breached();
                            if !self.state.insert(current_time, metric) {
                                trace!("Dropping metric due to context limit.");
                                if !was_breached {
                                    // First drop since the last recovery — emit a single warning.
                                    warn!(context_limit = self.state.context_limit(), "Context limit reached, \
                                    dropping metrics. Consider increasing `aggregate_context_limit`.");
                                }
                                self.telemetry.increment_events_dropped();
                            }
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

struct AggregationState {
    core: AggregateCore<OriginContextId>,
    telemetry: Telemetry,
}

impl AggregationState {
    fn new(
        bucket_width_secs: NonZeroU64, context_limit: usize, counter_expiration: Option<Duration>,
        hist_config: HistogramConfiguration, telemetry: Telemetry,
    ) -> Self {
        Self {
            core: AggregateCore::new(bucket_width_secs, context_limit, counter_expiration, hist_config),
            telemetry,
        }
    }

    fn is_empty(&self) -> bool {
        self.core.is_empty()
    }

    fn context_limit(&self) -> usize {
        self.core.context_limit()
    }

    fn snapshot_contexts(&self) -> Vec<AggregateContextSnapshotEntry> {
        self.core.snapshot_contexts()
    }

    fn insert(&mut self, timestamp: u64, metric: Metric) -> bool {
        let telemetry = &self.telemetry;
        self.core.insert_with(timestamp, metric, |context, values| {
            telemetry.increment_contexts(context, values)
        })
    }

    async fn flush(
        &mut self, current_time: u64, flush_open_buckets: bool, dispatcher: &mut BufferedDispatcher<'_, EventsBuffer>,
    ) -> Result<(), GenericError> {
        let window = self.core.prepare_flush(current_time, flush_open_buckets);
        let bucket_width_secs = self.core.bucket_width_secs();
        let hist_config = self.core.hist_config().clone();
        let mut flush_counts = FlushCounts::default();
        let mut expanded = Vec::new();

        let (contexts, remove_buf) = self.core.contexts_and_remove_buf_mut();
        for (context, am) in contexts {
            let (closed_values, should_remove) = extract_closed_values(am, &window);
            if let Some(closed_bucket_values) = closed_values {
                self.telemetry
                    .increment_flushed(&closed_bucket_values, &mut flush_counts);

                expanded.clear();
                expand_flushed_metric(
                    context.clone(),
                    closed_bucket_values,
                    am.metadata.clone(),
                    bucket_width_secs,
                    &hist_config,
                    |metric| expanded.push(metric),
                );
                for metric in expanded.drain(..) {
                    dispatcher.push(Event::Metric(metric)).await?;
                }
            }

            if should_remove {
                remove_buf.push(context.clone());
            }
        }

        let telemetry = &self.telemetry;
        self.core.finish_flush(current_time, |context, values, origin_id| {
            telemetry.decrement_contexts(context, values, origin_id);
        });
        self.telemetry.record_last_flush_counts(&flush_counts);

        Ok(())
    }

    fn context_limit_breached(&self) -> bool {
        self.core.context_limit_breached()
    }
}

// TODO: One thing we ought to consider is a property test, specifically a state machine property test, where we
// generate a randomized offset to start time from, a bucket width, flush interval, and operations, and so on... and
// then we run it to make sure that we are always generating sequential timestamps for data points, etc.
#[cfg(test)]
mod tests {
    use std::{cell::Cell, mem::size_of};

    use float_cmp::ApproxEqRatio as _;
    use saluki_core::{
        accounting::ComponentRegistry,
        components::{test_util::TestComponentDriver, ComponentContext},
        data_model::tags::{Tag, TagSet},
        support::SubsystemIdentifier,
        topology::{interconnect::Dispatcher, EventsDispatcher, OutputName},
    };
    use saluki_metric_kernels::histogram::HistogramStatistic;
    use saluki_metrics::test::TestRecorder;
    use stringtheory::MetaString;
    use tokio::sync::{mpsc, oneshot};

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
        get_flushed_metrics_with_open_windows(timestamp, true, state).await
    }

    async fn get_flushed_metrics_with_open_windows(
        timestamp: u64, flush_open_buckets: bool, state: &mut AggregationState,
    ) -> Vec<Metric> {
        let (dispatcher, mut dispatcher_receiver) = build_basic_dispatcher();
        let mut buffered_dispatcher = dispatcher.buffered().expect("default output should always exist");

        // Flush the metrics to an event buffer.
        state
            .flush(timestamp, flush_open_buckets, &mut buffered_dispatcher)
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

    #[tokio::test]
    async fn flush_records_last_flush_counts() {
        let recorder = TestRecorder::default();
        let _local = metrics::set_default_local_recorder(&recorder);

        let builder = MetricsBuilder::default();
        let mut state = AggregationState::new(
            BUCKET_WIDTH_SECS,
            10,
            COUNTER_EXPIRE,
            HistogramConfiguration::default(),
            Telemetry::new(&builder, false),
        );

        assert!(state.insert(insert_ts(1), Metric::counter("metric1", 1.0)));
        assert!(state.insert(insert_ts(1), Metric::gauge("metric2", 2.0)));
        assert!(state.insert(insert_ts(1), Metric::distribution("metric3", 3.0)));

        let _ = get_flushed_metrics(flush_ts(1), &mut state).await;

        assert_eq!(
            recorder.gauge(("aggregate_last_flush_count", &[("data_type", "series")])),
            Some(2.0)
        );
        assert_eq!(
            recorder.gauge(("aggregate_last_flush_count", &[("data_type", "sketches")])),
            Some(1.0)
        );

        // The next flush only has the idle counter's zero value, so the gauges reflect just that flush.
        let _ = get_flushed_metrics(flush_ts(2), &mut state).await;

        assert_eq!(
            recorder.gauge(("aggregate_last_flush_count", &[("data_type", "series")])),
            Some(1.0)
        );
        assert_eq!(
            recorder.gauge(("aggregate_last_flush_count", &[("data_type", "sketches")])),
            Some(0.0)
        );
    }

    #[test]
    fn snapshot_contexts_preserves_full_context_shape() {
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
        assert!(snapshot.capacity() >= state.core.len());
        snapshot.sort_by(|a, b| a.context().host().cmp(&b.context().host()));

        assert_eq!(snapshot[0].context(), &histogram_context);
        assert_eq!(snapshot[0].context().tags().len(), 1);
        assert_eq!(snapshot[0].context().origin_tags().len(), 1);
        assert_eq!(snapshot[0].metric_type(), AggregateMetricType::Histogram);

        assert_eq!(snapshot[1].context(), &gauge_context);
        assert_eq!(snapshot[1].metric_type(), AggregateMetricType::Gauge);
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
        let active_snapshot = state.snapshot_contexts();
        assert_eq!(active_snapshot.len(), 1);
        assert_eq!(active_snapshot[0].context(), &context);
        assert_eq!(active_snapshot[0].metric_type(), AggregateMetricType::Gauge);

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
        let second_snapshot = state.snapshot_contexts();
        assert_eq!(second_snapshot.len(), 1);
        assert_eq!(second_snapshot[0].context(), &context);
        assert_eq!(second_snapshot[0].metric_type(), AggregateMetricType::Counter);

        let _ = get_flushed_metrics(flush_ts(3), &mut state).await;
        assert!(state.snapshot_contexts().is_empty());
    }

    #[test]
    fn canceled_snapshot_response_skips_snapshot_construction() {
        let mut state = AggregationState::new(
            BUCKET_WIDTH_SECS,
            10,
            COUNTER_EXPIRE,
            HistogramConfiguration::default(),
            Telemetry::noop(),
        );
        assert!(state.insert(
            insert_ts(1),
            Metric::gauge(Context::from_static_name("canceled.snapshot"), 1.0),
        ));
        let (response, receiver) = oneshot::channel();
        drop(receiver);
        let snapshot_calls = Cell::new(0);

        send_context_snapshot_if_open(response, || {
            snapshot_calls.set(snapshot_calls.get() + 1);
            state.snapshot_contexts()
        });

        assert_eq!(snapshot_calls.get(), 0);
    }

    #[test]
    fn open_snapshot_response_constructs_once_and_returns_exact_entries() {
        let mut state = AggregationState::new(
            BUCKET_WIDTH_SECS,
            10,
            COUNTER_EXPIRE,
            HistogramConfiguration::default(),
            Telemetry::noop(),
        );
        let context = Context::from_static_name("open.snapshot");
        assert!(state.insert(insert_ts(1), Metric::gauge(context.clone(), 1.0)));
        let expected = vec![AggregateContextSnapshotEntry::new(context, AggregateMetricType::Gauge)];
        let (response, mut receiver) = oneshot::channel();
        let snapshot_calls = Cell::new(0);

        send_context_snapshot_if_open(response, || {
            snapshot_calls.set(snapshot_calls.get() + 1);
            state.snapshot_contexts()
        });

        assert_eq!(snapshot_calls.get(), 1);
        assert_eq!(
            receiver.try_recv().expect("open requester should receive a snapshot"),
            expected
        );
    }

    #[test]
    fn aggregate_memory_bounds_include_peak_context_snapshot() {
        let context_limit = 17;
        let config = AggregateConfiguration {
            context_limit,
            ..AggregateConfiguration::for_test()
        };
        let registry = ComponentRegistry::default();
        config.specify_bounds(&mut registry.bounds_builder(&SubsystemIdentifier::from_dotted("test")));
        let bounds = registry.as_bounds();

        let expected_minimum = size_of::<Aggregate>();
        let aggregation_state_bytes =
            context_limit * (size_of::<Context>() + size_of::<AggregatedMetric<OriginContextId>>());
        let context_snapshot_bytes = context_limit * size_of::<AggregateContextSnapshotEntry>();

        assert_eq!(bounds.total_minimum_required_bytes(), expected_minimum);
        assert_eq!(
            bounds.total_firm_limit_bytes(),
            expected_minimum + aggregation_state_bytes + context_snapshot_bytes
        );

        let origin_config = AggregateConfiguration {
            context_limit,
            origin_telemetry_enabled: true,
            ..AggregateConfiguration::for_test()
        };
        let origin_registry = ComponentRegistry::default();
        origin_config.specify_bounds(&mut origin_registry.bounds_builder(&SubsystemIdentifier::from_dotted("test")));
        let origin_bounds = origin_registry.as_bounds();
        let origin_tracking_bytes = origin_telemetry_bytes(origin_config.primary_flush_interval);
        assert_eq!(
            origin_bounds.total_firm_limit_bytes(),
            expected_minimum + aggregation_state_bytes + context_snapshot_bytes + origin_tracking_bytes
        );
        // Faster flushes reuse slots more often while retired series wait for idle eviction.
        assert!(origin_telemetry_bytes(Duration::from_secs(1)) > origin_telemetry_bytes(Duration::from_secs(15)));
    }

    #[tokio::test]
    async fn production_owner_loop_serves_snapshots_and_stops_after_cancellation() {
        tokio::time::timeout(Duration::from_secs(5), async {
            let (snapshot_handle, context_snapshot_receiver) = aggregate_context_snapshot_channel();
            let config = AggregateConfiguration {
                primary_flush_interval: Duration::from_secs(60),
                context_snapshot_receiver,
                ..AggregateConfiguration::for_test()
            };

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

            // The default output has to stay open, since the aggregate dispatches what it doesn't retain.
            let (control, _outputs) = TestComponentDriver::transform(config)
                .await
                .expect("aggregate transform should build");

            let retained_context = Context::from_static_name("owner.loop.mixed.counter");
            let mixed_values = ScalarPoints::from_iter([(None, 2.0), (NonZeroU64::new(insert_ts(1)), 3.0)]);
            control
                .send_event(Event::Metric(Metric::counter(retained_context.clone(), mixed_values)))
                .await;

            let snapshot = loop {
                let snapshot = snapshot_handle
                    .snapshot()
                    .await
                    .expect("running aggregate should fulfill snapshots");
                if snapshot.iter().any(|entry| entry.context() == &retained_context) {
                    break snapshot;
                }
                tokio::task::yield_now().await;
            };
            assert_eq!(
                snapshot.len(),
                1,
                "only the mixed metric's retained portion belongs in state"
            );
            let entry = &snapshot[0];
            assert_eq!(entry.context(), &retained_context);
            assert_eq!(entry.metric_type(), AggregateMetricType::Counter);

            drop(snapshot_handle);
            control.shutdown().await.expect("aggregate should stop cleanly");
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
        let config = AggregateConfiguration::for_test();
        let first = config.build(BuildContext::test_transform("aggregate_one")).await;
        assert!(first.is_ok());

        let second = config.build(BuildContext::test_transform("aggregate_two")).await;
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

    /// Regression test: with idle-counter keep-alive disabled (`dogstatsd_expiry_seconds: 0`), a flushed counter used
    /// to stay in the aggregation state forever. It emitted no zero values, but still counted toward
    /// `aggregate_context_limit`, so enough distinct counters eventually blocked new contexts.
    #[tokio::test]
    async fn disabled_counter_keep_alive_releases_contexts_after_flush() {
        let mut state = AggregationState::new(
            BUCKET_WIDTH_SECS,
            10,
            None,
            HistogramConfiguration::default(),
            Telemetry::noop(),
        );

        assert!(state.insert(insert_ts(1), Metric::counter("counter", 1.0)));
        let flushed = get_flushed_metrics(flush_ts(1), &mut state).await;

        assert_eq!(flushed.len(), 1);
        assert!(state.is_empty());
    }

    /// Regression test: after the state emptied and sat idle, its last flush time stayed behind. The next flush then
    /// generated zero-value buckets for the whole idle period and merged them into a counter inserted afterward, so
    /// the counter reported points from before it existed (97 extra zeros in this scenario).
    #[tokio::test]
    async fn counter_after_idle_single_window_does_not_backfill_zeros() {
        let mut state = AggregationState::new(
            BUCKET_WIDTH_SECS,
            10,
            Some(Duration::from_secs(300)),
            HistogramConfiguration::default(),
            Telemetry::noop(),
        );

        assert!(state.insert(11, Metric::gauge("previous", 1.0)));
        let flushed = get_flushed_metrics_with_open_windows(30, false, &mut state).await;
        assert_eq!(flushed.len(), 1);
        assert!(state.is_empty());

        let counter = Metric::counter("counter", 7.0);
        assert!(state.insert(1_000, counter.clone()));
        let flushed = get_flushed_metrics_with_open_windows(1_010, false, &mut state).await;

        assert_eq!(flushed.len(), 1);
        assert_flushed_scalar_metric!(&counter, &flushed[0], [1_000 => 7.0]);
    }

    /// Regression test: flushing open windows (`dogstatsd_flush_incomplete_buckets`) only affected idle-counter zero
    /// values. Real values in the still-open window stayed behind the closed-bucket cutoff, so a shutdown flush
    /// discarded them. The Core Agent flushes every bucket, open or not, when forced:
    /// https://github.com/DataDog/datadog-agent/blob/78c2c984e848055b4aa63c5f6a9e94a534a07a44/pkg/aggregator/time_sampler.go#L184
    #[tokio::test]
    async fn flushing_open_windows_emits_values_in_the_open_window() {
        for flush_open_buckets in [false, true] {
            let mut state = AggregationState::new(
                BUCKET_WIDTH_SECS,
                10,
                COUNTER_EXPIRE,
                HistogramConfiguration::default(),
                Telemetry::noop(),
            );
            let gauge = Metric::gauge("gauge", 1.0);
            let counter = Metric::counter("counter", 2.0);
            let distribution = Metric::distribution("distribution", 3.0);
            for metric in [&gauge, &counter, &distribution] {
                assert!(state.insert(1_001, metric.clone()));
            }

            // The window [1000, 1010) is still open at 1005.
            let mut flushed = get_flushed_metrics_with_open_windows(1_005, flush_open_buckets, &mut state).await;
            if !flush_open_buckets {
                assert!(
                    flushed.is_empty(),
                    "open windows must wait unless they're being flushed"
                );
                continue;
            }

            flushed.sort_by(|a, b| a.context().name().cmp(b.context().name()));
            let names = flushed
                .iter()
                .map(|metric| metric.context().name().as_ref())
                .collect::<Vec<_>>();
            assert_eq!(names, ["counter", "distribution", "gauge"]);
            assert_flushed_scalar_metric!(&counter, &flushed[0], [1_000 => 2.0]);
            assert_flushed_distribution_metric!(&distribution, &flushed[1], [1_000 => &[3.0][..]]);
            assert_flushed_scalar_metric!(&gauge, &flushed[2], [1_000 => 1.0]);
        }
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

    fn histogram_with_unit(context: &Context, value: f64, unit: &'static str) -> Metric {
        Metric::from_parts(
            context.clone(),
            MetricValues::histogram([value]),
            MetricMetadata::default().with_unit(MetaString::from_static(unit)),
        )
    }

    fn max_only_histogram_config() -> HistogramConfiguration {
        HistogramConfiguration::from_statistics(&[HistogramStatistic::Maximum], false, "".into())
    }

    #[tokio::test]
    async fn histogram_unit_first_non_empty_wins() {
        // DogStatsD `h` and `ms` samples for one context land in one histogram, which carries a unit if any sample did.
        for units in [["", "millisecond"], ["millisecond", ""]] {
            let mut state = AggregationState::new(
                BUCKET_WIDTH_SECS,
                10,
                COUNTER_EXPIRE,
                max_only_histogram_config(),
                Telemetry::noop(),
            );
            let context = Context::from_static_name("mixed.timer");
            for (value, unit) in units.into_iter().enumerate() {
                assert!(state.insert(insert_ts(1), histogram_with_unit(&context, value as f64, unit)));
            }

            let flushed_metrics = get_flushed_metrics(flush_ts(1), &mut state).await;
            assert_eq!(flushed_metrics.len(), 1);
            assert_eq!(
                flushed_metrics[0].metadata().unit(),
                Some("millisecond"),
                "units {units:?}"
            );
        }
    }

    #[tokio::test]
    async fn histogram_unit_lasts_until_the_context_is_removed() {
        // The unit isn't tracked per bucket: every unflushed value of the context shares it until the context is removed.
        for (units, expected) in [
            (["millisecond", "millisecond"], ["millisecond", "millisecond"]),
            (["millisecond", ""], ["millisecond", "millisecond"]),
            (["", "millisecond"], ["millisecond", "millisecond"]),
            (["", ""], ["", ""]),
        ] {
            let mut state = AggregationState::new(
                BUCKET_WIDTH_SECS,
                10,
                COUNTER_EXPIRE,
                max_only_histogram_config(),
                Telemetry::noop(),
            );
            let context = Context::from_static_name("timer");
            assert!(state.insert(insert_ts(1), histogram_with_unit(&context, 1.0, units[0])));
            assert!(state.insert(insert_ts(2), histogram_with_unit(&context, 2.0, units[1])));

            for (step, unit) in [1, 2].into_iter().zip(expected) {
                let flushed_metrics = get_flushed_metrics_with_open_windows(flush_ts(step), false, &mut state).await;
                assert_eq!(flushed_metrics.len(), 1, "units {units:?}, step {step}");
                let max_metric = Metric::gauge(Context::from_static_name("timer.max"), 0.0);
                assert_flushed_scalar_metric!(max_metric, &flushed_metrics[0], [bucket_ts(step) => step as f64]);
                assert_eq!(
                    flushed_metrics[0].metadata().unit(),
                    (!unit.is_empty()).then_some(unit),
                    "units {units:?}, step {step}"
                );
            }
            assert!(state.is_empty());

            // A context recreated after removal doesn't inherit the old unit.
            assert!(state.insert(insert_ts(3), histogram_with_unit(&context, 3.0, "")));
            let flushed_metrics = get_flushed_metrics_with_open_windows(flush_ts(3), false, &mut state).await;
            assert_eq!(flushed_metrics.len(), 1, "units {units:?}, step 3");
            assert_eq!(flushed_metrics[0].metadata().unit(), None, "units {units:?}, step 3");
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

    #[test]
    fn origin_context_telemetry_groups_tag_order_and_skips_unresolved_origins() {
        let recorder = TestRecorder::default();
        let _local = metrics::set_default_local_recorder(&recorder);
        let telemetry = Telemetry::new(&MetricsBuilder::default(), true);
        let values = MetricValues::counter((insert_ts(1), 1.0));

        let mut first_origin = TagSet::default();
        first_origin.insert_tag("container:one");
        first_origin.insert_tag("namespace:default");
        let first_context = Context::from_static_parts("metric.one", &[]).with_origin_tags(first_origin);
        let mut same_origin_different_order = TagSet::default();
        same_origin_different_order.insert_tag("namespace:default");
        same_origin_different_order.insert_tag("container:one");
        let second_context =
            Context::from_static_parts("metric.two", &[]).with_origin_tags(same_origin_different_order);

        let first_id = telemetry.increment_contexts(&first_context, &values);
        let second_id = telemetry.increment_contexts(&second_context, &values);
        assert_eq!(
            recorder.gauge((
                "aggregate_active_contexts_by_origin",
                &[("container", "one"), ("namespace", "default")]
            )),
            Some(2.0)
        );

        telemetry.decrement_contexts(&first_context, &values, first_id);
        assert_eq!(
            recorder.gauge((
                "aggregate_active_contexts_by_origin",
                &[("container", "one"), ("namespace", "default")]
            )),
            Some(1.0)
        );
        telemetry.decrement_contexts(&second_context, &values, second_id);
        assert_eq!(
            recorder.gauge((
                "aggregate_active_contexts_by_origin",
                &[("container", "one"), ("namespace", "default")]
            )),
            Some(0.0)
        );

        let unresolved_context = Context::from_static_parts("metric.unresolved", &[]);
        telemetry.increment_contexts(&unresolved_context, &values);
        assert_eq!(recorder.gauge("aggregate_active_contexts_by_origin"), None);
    }

    #[tokio::test]
    async fn origin_admission_survives_failed_flush_and_new_values_before_retry() {
        let recorder = TestRecorder::default();
        let _local = metrics::set_default_local_recorder(&recorder);
        let telemetry = Telemetry::new(&MetricsBuilder::default(), true);
        let mut state = AggregationState::new(
            BUCKET_WIDTH_SECS,
            2048,
            COUNTER_EXPIRE,
            HistogramConfiguration::default(),
            telemetry,
        );
        let mut origin = TagSet::default();
        origin.insert_tag("container:one");
        let count = 1026;
        for i in 0..count {
            let context =
                Context::from_parts(format!("metric.{i}"), TagSet::default()).with_origin_tags(origin.clone());
            assert!(state.insert(insert_ts(1), Metric::gauge(context, (insert_ts(1), 1.0))));
        }
        let (dispatcher, receiver) = build_basic_dispatcher();
        drop(receiver);
        let mut buffered = dispatcher.buffered().unwrap();
        assert!(state.flush(flush_ts(1), false, &mut buffered).await.is_err());
        assert!(
            !state.core.pending_removals().is_empty(),
            "failure must follow a pending expiry"
        );
        assert_eq!(state.core.len(), count);
        let metric = ("aggregate_active_contexts_by_origin", &[("container", "one")]);
        assert_eq!(recorder.gauge(metric), Some(count as f64));

        // A context marked for removal before the failure can receive new, still-open values.
        let revived = state.core.pending_removals()[0].clone();
        let future_ts = flush_ts(1) + 100;
        assert!(state.insert(future_ts, Metric::gauge(revived.clone(), (future_ts, 1.0))));
        let _ = get_flushed_metrics_with_open_windows(flush_ts(1), false, &mut state).await;
        assert_eq!(state.core.len(), 1);
        assert!(state.core.contains_context(&revived));
        assert_eq!(recorder.gauge(metric), Some(1.0));
        let _ = get_flushed_metrics_with_open_windows(future_ts + 100, false, &mut state).await;
        assert!(state.core.is_empty());
        assert_eq!(recorder.gauge(metric), Some(0.0));
    }

    #[tokio::test]
    async fn origin_context_telemetry_tracks_aggregation_insertions_and_expirations() {
        let recorder = TestRecorder::default();
        let _local = metrics::set_default_local_recorder(&recorder);
        let telemetry = Telemetry::new(&MetricsBuilder::default(), true);
        let mut state = AggregationState::new(
            BUCKET_WIDTH_SECS,
            10,
            COUNTER_EXPIRE,
            HistogramConfiguration::default(),
            telemetry,
        );

        let mut origin_tags = TagSet::default();
        origin_tags.insert_tag("container_id:container-1");
        origin_tags.insert_tag("kube_namespace:default");
        let first_context = Context::from_static_parts("metric.one", &[]).with_origin_tags(origin_tags.clone());
        let second_context = Context::from_static_parts("metric.two", &[]).with_origin_tags(origin_tags);

        assert!(state.insert(insert_ts(1), Metric::gauge(first_context, (insert_ts(1), 1.0))));
        assert!(state.insert(insert_ts(1), Metric::gauge(second_context, (insert_ts(1), 1.0))));
        let origin_metric = (
            "aggregate_active_contexts_by_origin",
            &[("container_id", "container-1"), ("kube_namespace", "default")],
        );
        assert_eq!(recorder.gauge(origin_metric), Some(2.0));

        let _ = get_flushed_metrics(flush_ts(1), &mut state).await;
        assert!(state.is_empty());
        assert_eq!(recorder.gauge(origin_metric), Some(0.0));
    }

    #[tokio::test]
    async fn telemetry() {
        // TODO: We don't check that `component_events_dropped_total` increments when a context limit is hit,
        // since that increment happens in the aggregate component future rather than `AggregationState`,
        // which is harder to drive overall and would have required even more boilerplate.
        //
        // Leaving that as a future improvement.

        let recorder = TestRecorder::default();
        let _local = metrics::set_default_local_recorder(&recorder);

        let builder = MetricsBuilder::default();
        let telemetry = Telemetry::new(&builder, false);

        let mut state = AggregationState::new(
            BUCKET_WIDTH_SECS,
            2,
            COUNTER_EXPIRE,
            HistogramConfiguration::default(),
            telemetry,
        );

        // Make sure our telemetry is registered at default values.
        assert_eq!(recorder.gauge("aggregate_active_contexts"), Some(0.0));
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
