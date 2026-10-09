use std::{future::pending, num::NonZeroU64, sync::Mutex, time::Duration};

use async_trait::async_trait;
use saluki_common::time::get_unix_timestamp;
use saluki_core::{
    accounting::{MemoryBounds, MemoryBoundsBuilder},
    components::{transforms::*, BuildContext},
    data_model::event::{Event, EventType},
    observability::ComponentMetricsExt as _,
    topology::OutputDefinition,
};
use saluki_error::{generic_error, GenericError};
use saluki_metric_kernels::aggregate::{
    snapshot_usage, state_usage, AggregateCore, AggregateParams, AggregateTelemetry,
};
pub use saluki_metric_kernels::aggregate::{
    AggregateContextSnapshotEntry, AggregateMetricType, HistogramConfiguration,
};
use saluki_metrics::MetricsBuilder;
use tokio::{
    select,
    sync::{mpsc, oneshot},
    time::interval_at,
};
use tracing::{debug, error, info, trace, warn};

const CONTEXT_SNAPSHOT_REQUEST_CHANNEL_CAPACITY: usize = 1;

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
fn send_context_snapshot_if_open(
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
            context_snapshot_receiver,
        }
    }
}

#[async_trait]
impl TransformBuilder for AggregateConfiguration {
    async fn build(&self, context: BuildContext) -> Result<Box<dyn Transform + Send>, GenericError> {
        let context_snapshot_requests = self.context_snapshot_receiver.take_receiver()?;
        let metrics_builder = MetricsBuilder::from_component_context(context.component_context());
        let telemetry = AggregateTelemetry::new(&metrics_builder);

        let core = AggregateCore::new(
            AggregateParams {
                window: self.window_duration_seconds,
                context_limit: self.context_limit,
                counter_expiry: self.counter_expiry_seconds.and_then(NonZeroU64::new),
                flush_open_windows: self.flush_open_windows,
                histograms: self.hist_config.clone(),
            },
            telemetry,
        );

        Ok(Box::new(Aggregate {
            core,
            primary_flush_interval: self.primary_flush_interval,
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
            .with_expr(state_usage(self.context_limit))
            // A snapshot is constructed while the aggregation state remains live, so its peak allocation is additive.
            .with_expr(snapshot_usage(self.context_limit));
    }
}

/// The aggregate transform: native aggregation in its own task, flushing on a timer.
///
/// The aggregation itself, and every decision about what to hold and what to emit, belongs to the [`AggregateCore`];
/// the transform owns the clock, the flush timer, delivery, liveness, the context snapshot channel, and logging.
pub struct Aggregate {
    core: AggregateCore,
    primary_flush_interval: Duration,
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
                    // onwards. The core skips the flush while it holds no contexts.
                    if let Some(mut flush) = self.core.flush(get_unix_timestamp(), final_primary_flush) {
                        debug!("Flushing aggregated metrics...");

                        let mut dispatcher = context.dispatcher().buffered().expect("default output should always exist");
                        let flushing = async move {
                            // Deliver each context's metrics before visiting the next context, so that only one
                            // context's expansion is ever held here.
                            let mut pending = Vec::new();
                            let mut failure = None;
                            'contexts: while flush.emit_next(|metric| pending.push(metric)) {
                                for metric in pending.drain(..) {
                                    if let Err(e) = dispatcher.push(Event::Metric(metric)).await {
                                        failure = Some(e);
                                        break 'contexts;
                                    }
                                }
                            }

                            match failure {
                                None => {
                                    // If flush recovered us from a breach, log the recovery.
                                    if flush.finish().recovered {
                                        info!("Context limit no longer exceeded, metrics are being accepted again.");
                                    }
                                }
                                Some(e) => {
                                    error!(error = %e, "Failed to flush aggregation state.");
                                    flush.abandon();
                                }
                            }

                            match dispatcher.flush().await {
                                Ok(aggregated_events) => debug!(aggregated_events, "Dispatched events."),
                                Err(e) => error!(error = %e, "Failed to flush aggregated events."),
                            }
                        };
                        tokio::pin!(flushing);

                        // A flush can wait a long time for space in a full downstream channel. Keep answering liveness
                        // probes while it waits. Probes go first so that a waiting probe is seen on every wakeup, even
                        // when the flush would use up the task's cooperative budget.
                        loop {
                            select! {
                                biased;
                                _ = health.live() => {},
                                _ = &mut flushing => break,
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
                            send_context_snapshot_if_open(response, || self.core.snapshot_contexts());
                        }
                        None => self.context_snapshot_requests = None,
                    }
                },
                maybe_events = context.events().next(), if !final_primary_flush => match maybe_events {
                    Some(events) => {
                        trace!(events_len = events.len(), "Received events.");

                        let current_time = get_unix_timestamp();
                        let metrics = events.into_iter().filter_map(Event::try_into_metric);
                        let outcome = self.core.admit(current_time, metrics);
                        if outcome.limit_reached {
                            // First drop since the last recovery — emit a single warning.
                            warn!(context_limit = self.core.context_limit(), "Context limit reached, \
                            dropping metrics. Consider increasing `aggregate_context_limit`.");
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

#[cfg(test)]
mod tests {
    use std::{cell::Cell, mem::size_of};

    use saluki_core::{
        accounting::{ComponentRegistry, MemoryLimiter},
        components::{
            destinations::{Destination, DestinationBuilder, DestinationContext},
            sources::{Source, SourceBuilder, SourceContext},
        },
        data_model::{
            event::metric::{context::Context, Metric, ScalarPoints},
            tags::TagSet,
        },
        health::HealthRegistry,
        runtime::{state::ResourceRegistry, Supervisor},
        support::SubsystemIdentifier,
        topology::TopologyBlueprint,
    };
    use saluki_metric_kernels::aggregate::STATE_ENTRY_BYTES;
    use stringtheory::MetaString;
    use tokio::sync::{mpsc, oneshot};

    use super::*;

    const BUCKET_WIDTH_SECS: NonZeroU64 = NonZeroU64::new(10).expect("not zero");
    const COUNTER_EXPIRE_SECS: u64 = 20;

    /// Gets the insert timestamp for the given step.
    const fn insert_ts(step: u64) -> u64 {
        (BUCKET_WIDTH_SECS.get() * (step + 1)) - 2
    }

    /// Returns an aggregation core with a context limit of 10.
    fn test_core() -> AggregateCore {
        AggregateCore::new(
            AggregateParams {
                window: BUCKET_WIDTH_SECS,
                context_limit: 10,
                counter_expiry: NonZeroU64::new(COUNTER_EXPIRE_SECS),
                flush_open_windows: false,
                histograms: HistogramConfiguration::default(),
            },
            AggregateTelemetry::noop(),
        )
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

        async fn build(&self, _context: BuildContext) -> Result<Box<dyn Source + Send>, GenericError> {
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

        async fn build(&self, _context: BuildContext) -> Result<Box<dyn Destination + Send>, GenericError> {
            Ok(Box::new(DrainingMetricDestination))
        }
    }

    impl MemoryBounds for DrainingMetricDestinationBuilder {
        fn specify_bounds(&self, _builder: &mut MemoryBoundsBuilder) {}
    }

    /// A destination that forwards every metric it receives to a channel.
    struct CollectingMetricDestination {
        metrics: mpsc::UnboundedSender<Metric>,
    }

    #[async_trait]
    impl Destination for CollectingMetricDestination {
        async fn run(self: Box<Self>, mut context: DestinationContext) -> Result<(), GenericError> {
            while let Some(events) = context.events().next().await {
                for metric in events.into_iter().filter_map(Event::try_into_metric) {
                    let _ = self.metrics.send(metric);
                }
            }
            Ok(())
        }
    }

    struct CollectingMetricDestinationBuilder {
        metrics: mpsc::UnboundedSender<Metric>,
    }

    #[async_trait]
    impl DestinationBuilder for CollectingMetricDestinationBuilder {
        fn input_event_type(&self) -> EventType {
            EventType::Metric
        }

        async fn build(&self, _context: BuildContext) -> Result<Box<dyn Destination + Send>, GenericError> {
            Ok(Box::new(CollectingMetricDestination {
                metrics: self.metrics.clone(),
            }))
        }
    }

    impl MemoryBounds for CollectingMetricDestinationBuilder {
        fn specify_bounds(&self, _builder: &mut MemoryBoundsBuilder) {}
    }

    #[tokio::test]
    async fn final_flush_delivers_every_closed_bucket_through_the_dispatcher() {
        // Points timestamped an hour ago sit in buckets that are closed whatever the wall clock reads, so the final
        // flush at the end of input emits them all, deterministically. There are more of them than one events buffer
        // holds, so the flush delivers several buffers, one context at a time.
        const GAUGES: usize = 1500;

        tokio::time::timeout(Duration::from_secs(10), async {
            let config = AggregateConfiguration {
                primary_flush_interval: Duration::from_secs(3600),
                ..AggregateConfiguration::for_test()
            };
            let (events_tx, events_rx) = mpsc::channel(1);
            let (metrics_tx, mut metrics_rx) = mpsc::unbounded_channel();
            let component_registry = ComponentRegistry::default();
            let mut blueprint = TopologyBlueprint::new("aggregate_final_flush", &component_registry);
            blueprint
                .add_source(
                    "source",
                    ControlledMetricSourceBuilder {
                        events: Mutex::new(Some(events_rx)),
                        outputs: vec![OutputDefinition::default_output(EventType::Metric)],
                    },
                )
                .unwrap()
                .add_transform("aggregate", config)
                .unwrap()
                .add_destination(
                    "destination",
                    CollectingMetricDestinationBuilder { metrics: metrics_tx },
                )
                .unwrap();
            blueprint
                .connect_components_in_order(["source", "aggregate", "destination"])
                .unwrap();
            blueprint
                .with_health_registry(HealthRegistry::new())
                .with_memory_limiter(MemoryLimiter::noop())
                .with_resource_registry(ResourceRegistry::new())
                .with_ambient_worker_pool();
            let mut supervisor = Supervisor::new("aggregate-final-flush").unwrap();
            supervisor.add_worker(blueprint);
            let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
            let topology_task = tokio::spawn(async move { supervisor.run_with_shutdown(shutdown_rx).await });

            let past = get_unix_timestamp() - 3600;
            for i in 0..GAUGES {
                let context = Context::from_parts(format!("gauge.{:04}", i), TagSet::default());
                events_tx
                    .send(Event::Metric(Metric::gauge(context, (past, 1.0))))
                    .await
                    .unwrap();
            }
            let histogram = Metric::histogram(Context::from_static_name("histogram"), (past, [1.0, 2.0, 3.0]));
            events_tx.send(Event::Metric(histogram)).await.unwrap();

            drop(events_tx);
            shutdown_tx.send(()).unwrap();
            let topology_result = topology_task.await.unwrap();
            assert!(
                topology_result.is_ok(),
                "topology should stop cleanly: {topology_result:?}"
            );

            let mut names = Vec::new();
            while let Ok(metric) = metrics_rx.try_recv() {
                names.push(metric.context().name().to_string());
            }
            names.sort();
            let mut expected = (0..GAUGES).map(|i| format!("gauge.{:04}", i)).collect::<Vec<_>>();
            expected.extend(
                [
                    "histogram.95percentile",
                    "histogram.avg",
                    "histogram.count",
                    "histogram.max",
                    "histogram.median",
                ]
                .map(str::to_string),
            );
            assert_eq!(names, expected);
        })
        .await
        .expect("the topology should flush and stop");
    }

    #[test]
    fn canceled_snapshot_response_skips_snapshot_construction() {
        let mut state = test_core();
        assert_eq!(
            state
                .admit(
                    insert_ts(1),
                    [Metric::gauge(Context::from_static_name("canceled.snapshot"), 1.0)],
                )
                .accepted,
            1
        );
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
        let mut state = test_core();
        let context = Context::from_static_name("open.snapshot");
        assert_eq!(
            state
                .admit(insert_ts(1), [Metric::gauge(context.clone(), 1.0)])
                .accepted,
            1
        );
        let expected = vec![AggregateContextSnapshotEntry::for_test(
            context,
            AggregateMetricType::Gauge,
            MetaString::empty(),
        )];
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
        let aggregation_state_bytes = context_limit * STATE_ENTRY_BYTES;
        let context_snapshot_bytes = context_limit * size_of::<AggregateContextSnapshotEntry>();

        assert_eq!(bounds.total_minimum_required_bytes(), expected_minimum);
        assert_eq!(
            bounds.total_firm_limit_bytes(),
            expected_minimum + aggregation_state_bytes + context_snapshot_bytes
        );
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
                .with_resource_registry(ResourceRegistry::new())
                .with_ambient_worker_pool();

            let mut supervisor =
                Supervisor::new("aggregate-snapshot-owner").expect("test supervisor should be created");
            supervisor.add_worker(blueprint);
            let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
            let topology_task = tokio::spawn(async move { supervisor.run_with_shutdown(shutdown_rx).await });

            let retained_context = Context::from_static_name("owner.loop.mixed.counter");
            let mixed_values = ScalarPoints::from_iter([(None, 2.0), (NonZeroU64::new(insert_ts(1)), 3.0)]);
            events_tx
                .send(Event::Metric(Metric::counter(retained_context.clone(), mixed_values)))
                .await
                .expect("controlled source should accept a mixed event");

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
}
