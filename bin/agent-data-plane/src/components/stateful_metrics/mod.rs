//! Experimental ADP destination for configuration-selected Foldspace series delivery.
//!
//! Each sender worker owns one sans-I/O core, one gRPC stream per configured endpoint, and a
//! retry queue. The core sends every payload to every endpoint, sharing one dictionary. Batches
//! an endpoint could not carry come back tagged with that endpoint and wait in its own retry
//! lane; other logical batches wait in the shared ADP priority queue. Both can spill to disk.
//! Retried data is encoded against the current core state; original ADP metrics are not
//! retained. Stable series routing assigns fresh input to independent sender tasks.
//!
//! # Missing
//!
//! - TODO: Add TLS/proxy support before using this path beyond plaintext integration tests.
//! - TODO: Add a byte budget for core inflight metrics and dictionary state.
//! - TODO: Support per-endpoint API keys; every endpoint uses the primary API key.

use std::{
    collections::VecDeque,
    future::{pending, poll_fn},
    num::NonZeroUsize,
    task::Poll,
    time::Duration,
};

use agent_data_plane_config::Live;
use async_trait::async_trait;
use foldspace_core::{
    proto::stateful::batch_status, CoreConfig, LogicalMetricBatch, MetricClientEffect, MetricClientError,
    MetricDictionaryEvictionConfig, MetricEndpointId, MetricFailureAction, MetricStreamError, MetricStreamFailure,
    MetricStreamFailureKind, SenderConfig, StatefulMetricsClient, StreamId, TimerKind, ZstdBatchCompressor,
};
use futures::{future::BoxFuture, stream::FuturesUnordered, FutureExt as _, StreamExt as _};
use saluki_common::task::JoinSetExt as _;
use saluki_components::forwarders::queue::{DeliveryQueueConfiguration, PendingTransaction};
use saluki_core::{
    accounting::{MemoryBounds, MemoryBoundsBuilder},
    components::{
        destinations::{Destination, DestinationBuilder, DestinationContext},
        BuildContext,
    },
    data_model::event::EventType,
    observability::ComponentMetricsExt as _,
};
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use saluki_metrics::MetricsBuilder;
use stringtheory::MetaString;
use tokio::{
    select,
    sync::mpsc,
    task::JoinSet,
    time::{sleep, sleep_until, Instant},
};
use tonic::{
    metadata::{Ascii, MetadataValue},
    transport::Endpoint,
    Code, Status,
};
use tracing::{debug, warn};

mod conversion;
mod retry;
mod router;
mod sharding;
mod telemetry;
#[cfg(test)]
mod tests;
mod transport;

pub use self::router::StatefulMetricsRouterConfiguration;
use self::{
    retry::{prepare_storage, LanedRetryQueue},
    telemetry::Telemetry,
    transport::{next_transport_event, Transport, TransportEvent, TransportEventKind, TransportState},
};

const WORKER_INPUT_CAPACITY: usize = 2;
const MIN_FLUSH_TIMEOUT: Duration = Duration::from_millis(10);
const MAX_INFLIGHT_BATCHES: usize = 8;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const ACK_TIMEOUT: Duration = Duration::from_secs(30);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(30);
const INITIAL_BACKOFF: Duration = Duration::from_millis(250);
const MAX_BACKOFF: Duration = Duration::from_secs(30);
// Per worker. Keep both limits well above the live working set: a cap near it re-sends a large
// share of definitions every flush, and the grace period stops a cap below it from bounding memory.
const DICTIONARY_MAX_ENTRIES: usize = 20_000;
const DICTIONARY_MAX_ESTIMATED_BYTES: i64 = 16 * 1024 * 1024;
const DICTIONARY_STALE_AFTER: Duration = Duration::from_secs(30 * 60);

/// Opt-in destination for stateful series; delivery never switches automatically to HTTP.
pub struct StatefulMetricsConfiguration {
    /// Explicit plaintext test intake origins, primary first. There is no default endpoint.
    ///
    /// Every payload goes to every endpoint. Retry storage is namespaced by the primary endpoint. The
    /// shared queue and one lane per endpoint share the retry memory budget and split the disk budget evenly.
    pub endpoints: Vec<MetaString>,
    /// Independent sender tasks. Defaults to three; changing this requires a restart.
    pub workers: NonZeroUsize,
    /// Live primary API key. A change clears dictionary state and resumes suspended delivery.
    pub api_key: Live<String>,
    /// Zstd level, supplied from the existing serializer configuration (default 3).
    pub compression_level: i32,
    /// Maximum wait for a partial batch; zero uses the encoder's 10 millisecond fallback.
    pub flush_timeout: Duration,
    /// Series-count threshold for automatic flushing; an input buffer may exceed this threshold.
    pub batch_capacity: usize,
    /// Existing forwarder settings for high-priority capacity, retry memory, and disk storage.
    pub queue: DeliveryQueueConfiguration,
    /// Total component stop budget. At most half (capped at 30 seconds) is used waiting for delivery.
    pub stop_timeout: Duration,
}

#[async_trait]
impl DestinationBuilder for StatefulMetricsConfiguration {
    fn input_event_type(&self) -> EventType {
        EventType::Metric
    }
    async fn build(&self, context: BuildContext) -> Result<Box<dyn Destination + Send>, GenericError> {
        if self.endpoints.is_empty() {
            return Err(generic_error!("stateful metrics requires at least one endpoint"));
        }
        let endpoints = self
            .endpoints
            .iter()
            .map(|endpoint| Ok(Endpoint::from_shared(endpoint.to_string())?.connect_timeout(CONNECT_TIMEOUT)))
            .collect::<Result<Vec<_>, GenericError>>()?;
        let builder = MetricsBuilder::from_component_context(context.component_context());
        let api_key = parse_api_key(&self.api_key)?;
        prepare_storage(&self.queue, &self.endpoints, self.workers).await?;
        let mut workers = Vec::with_capacity(self.workers.get());
        for worker_id in 0..self.workers.get() {
            let builder = builder.clone().add_default_tag(("worker", worker_id.to_string()));
            let queue = LanedRetryQueue::build(&self.queue, &self.endpoints, worker_id, &builder).await?;
            workers.push(StatefulMetricsWorker::new(
                endpoints.clone(),
                api_key.clone(),
                self.compression_level,
                self.flush_timeout,
                self.batch_capacity,
                queue,
                builder,
            ));
        }
        Ok(Box::new(StatefulMetrics {
            workers,
            api_key: self.api_key.clone(),
            delivery_shutdown_timeout: (self.stop_timeout / 2).min(SHUTDOWN_TIMEOUT),
        }))
    }
}

impl MemoryBounds for StatefulMetricsConfiguration {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        builder
            .minimum()
            .with_single_value::<StatefulMetrics>("component struct")
            .with_array::<StatefulMetricsWorker>("sender workers", self.workers.get());
    }
}

struct StatefulMetrics {
    workers: Vec<StatefulMetricsWorker>,
    api_key: Live<String>,
    delivery_shutdown_timeout: Duration,
}

#[async_trait]
impl Destination for StatefulMetrics {
    async fn run(mut self: Box<Self>, mut context: DestinationContext) -> Result<(), GenericError> {
        let mut health = context.take_health_handle();
        let mut tasks = JoinSet::new();
        let mut inputs = Vec::with_capacity(self.workers.len());
        for (worker_id, worker) in self.workers.into_iter().enumerate() {
            let (tx, rx) = mpsc::channel(WORKER_INPUT_CAPACITY);
            inputs.push(tx);
            tasks.spawn_traced_named(
                format!("stateful-metrics-worker-{worker_id}"),
                worker.run(rx, self.api_key.clone(), self.delivery_shutdown_timeout),
            );
        }
        health.mark_ready();
        let mut result = Ok(());
        loop {
            select! {
                _ = health.live() => {},
                completed = tasks.join_next() => {
                    result = match completed {
                        Some(Ok(Err(error))) => Err(error),
                        Some(Err(error)) => Err(error.into()),
                        _ => Err(generic_error!("stateful metrics worker stopped before input closed")),
                    };
                    break;
                },
                events = context.events().next() => {
                    let Some(events) = events else { break };
                    let batches = sharding::partition(events, inputs.len());
                    // Poll all sends together so a busy worker does not delay dispatch to its peers.
                    let sends = inputs.iter().zip(batches)
                        .filter(|(_, batch)| !batch.is_empty())
                        .map(|(tx, batch)| tx.send(batch));
                    if futures::future::join_all(sends).await.iter().any(Result::is_err) {
                        result = Err(generic_error!("stateful metrics worker input closed"));
                        break;
                    }
                },
            }
        }
        // Close every worker input queue before waiting: delivery budgets run concurrently across workers.
        drop(inputs);
        while let Some(completed) = tasks.join_next().await {
            match completed {
                Ok(Ok(())) => {}
                Ok(Err(error)) => result = Err(error),
                Err(error) => result = Err(error.into()),
            }
        }
        result
    }
}

struct StatefulMetricsWorker {
    core: StatefulMetricsClient<ZstdBatchCompressor>,
    telemetry: Telemetry,
    api_key: MetadataValue<Ascii>,
    queue: LanedRetryQueue,
    flush_timeout: Duration,
    buffered_deadline: Option<Instant>,
    stream_lifetime: Duration,
    /// Per-endpoint state, indexed by `MetricEndpointId`.
    endpoints: Vec<EndpointState>,
    /// Kept apart from `endpoints` so one wait can borrow every transport at once.
    transports: Vec<Option<Transport>>,
}

struct EndpointState {
    address: Endpoint,
    /// The last stream opened, which reconnect and drain timers name after it closes.
    stream_id: Option<StreamId>,
    timers: FuturesUnordered<BoxFuture<'static, (StreamId, TimerKind)>>,
    unacknowledged: usize,
    ack_deadline: Option<Instant>,
    backoff: Duration,
    suspended: bool,
}

impl EndpointState {
    fn new(address: Endpoint) -> Self {
        Self {
            address,
            stream_id: None,
            timers: FuturesUnordered::new(),
            unacknowledged: 0,
            ack_deadline: None,
            backoff: INITIAL_BACKOFF,
            suspended: false,
        }
    }

    fn stream_closed(&mut self) {
        self.timers.clear();
        self.unacknowledged = 0;
        self.ack_deadline = None;
    }
}

impl StatefulMetricsWorker {
    fn new(
        endpoints: Vec<Endpoint>, api_key: MetadataValue<Ascii>, compression_level: i32, flush_timeout: Duration,
        batch_capacity: usize, queue: LanedRetryQueue, builder: MetricsBuilder,
    ) -> Self {
        let config = CoreConfig {
            batch_capacity,
            sender: SenderConfig {
                max_inflight_payloads: MAX_INFLIGHT_BATCHES,
                ..SenderConfig::default()
            },
            metrics_dictionary_eviction: Some(MetricDictionaryEvictionConfig {
                max_item_count: DICTIONARY_MAX_ENTRIES,
                max_memory_bytes: DICTIONARY_MAX_ESTIMATED_BYTES,
                stale_after: DICTIONARY_STALE_AFTER,
                ..MetricDictionaryEvictionConfig::default()
            }),
            metrics_endpoints: endpoints.len(),
        };
        let stream_lifetime = config.sender.stream_lifetime;
        Self {
            core: StatefulMetricsClient::new(config, ZstdBatchCompressor::new(compression_level)),
            telemetry: Telemetry::new(builder),
            api_key,
            queue,
            flush_timeout: if flush_timeout.is_zero() {
                MIN_FLUSH_TIMEOUT
            } else {
                flush_timeout
            },
            buffered_deadline: None,
            stream_lifetime,
            transports: endpoints.iter().map(|_| None).collect(),
            endpoints: endpoints.into_iter().map(EndpointState::new).collect(),
        }
    }

    async fn run(
        mut self, mut input: mpsc::Receiver<LogicalMetricBatch>, mut api_key: Live<String>,
        delivery_shutdown_timeout: Duration,
    ) -> Result<(), GenericError> {
        let result = self.drive(&mut input, &mut api_key, delivery_shutdown_timeout).await;
        // Preserve accepted input queue contents even when the event loop returns an error.
        input.close();
        while let Some(batch) = input.recv().await {
            self.enqueue(batch).await;
        }
        let shutdown = self.shutdown().await;
        result.and(shutdown)
    }

    async fn drive(
        &mut self, input: &mut mpsc::Receiver<LogicalMetricBatch>, api_key: &mut Live<String>,
        delivery_shutdown_timeout: Duration,
    ) -> Result<(), GenericError> {
        let effects = self.core.start();
        self.apply(effects).await?;
        let mut input_closed = false;
        let mut shutdown_deadline = None;
        loop {
            self.pump().await?;
            if input_closed {
                if self.core.has_send_capacity() {
                    self.flush().await?;
                }
                if self.is_drained() || self.all_suspended() {
                    break;
                }
            }
            let flush_deadline = self.flush_deadline();
            let (ack_endpoint, ack_deadline) = self.next_ack_deadline();
            select! {
                _ = tokio::task::yield_now(), if self.can_pump() => {},
                key = api_key.changed() => { self.update_credentials(&key).await?; },
                event = next_transport_event(&mut self.transports) => { self.on_transport(event).await?; },
                (stream_id, kind) = next_timer(&mut self.endpoints) => {
                    let effects = self.core.handle_timer(stream_id, kind);
                    self.apply(effects).await?;
                },
                _ = wait_deadline(flush_deadline) => { self.flush().await?; },
                _ = wait_deadline(ack_deadline) => {
                    self.fail(ack_endpoint, MetricStreamFailureKind::DeadlineExceeded, "acknowledgement timed out").await?;
                },
                _ = wait_deadline(shutdown_deadline) => break,
                batch = input.recv(), if !input_closed => {
                    match batch {
                        Some(batch) => self.enqueue(batch).await,
                        None => {
                            input_closed = true;
                            shutdown_deadline = Some(Instant::now() + delivery_shutdown_timeout);
                        },
                    }
                },
            }
        }
        Ok(())
    }

    async fn enqueue(&mut self, batch: LogicalMetricBatch) {
        if !batch.is_empty() {
            self.telemetry.track_drops(self.queue.push_fresh(batch).await);
        }
    }

    fn lanes(&self) -> impl Iterator<Item = MetricEndpointId> {
        (0..self.queue.lane_count()).map(MetricEndpointId)
    }

    fn can_pump(&self) -> bool {
        (self.core.has_send_capacity() && !self.queue.shared_is_empty())
            || self
                .lanes()
                .any(|endpoint| self.core.endpoint_has_send_capacity(endpoint) && !self.queue.lane_is_empty(endpoint))
    }

    /// Whether everything deliverable has been delivered; suspended endpoints keep their lanes.
    fn is_drained(&self) -> bool {
        self.queue.shared_is_empty()
            && self
                .lanes()
                .all(|endpoint| self.endpoints[endpoint.get()].suspended || self.queue.lane_is_empty(endpoint))
            && self.core.inflight_len() == 0
            && self.core.buffered_series_len() == 0
    }

    fn all_suspended(&self) -> bool {
        self.endpoints.iter().all(|endpoint| endpoint.suspended)
    }

    fn flush_deadline(&self) -> Option<Instant> {
        self.core
            .has_send_capacity()
            .then_some(self.buffered_deadline)
            .flatten()
    }

    fn next_ack_deadline(&self) -> (MetricEndpointId, Option<Instant>) {
        self.endpoints
            .iter()
            .enumerate()
            .filter_map(|(index, endpoint)| Some((MetricEndpointId(index), endpoint.ack_deadline?)))
            .min_by_key(|(_, deadline)| *deadline)
            .map_or((MetricEndpointId(0), None), |(endpoint, deadline)| {
                (endpoint, Some(deadline))
            })
    }

    fn endpoint_for_stream(&self, stream_id: StreamId) -> Option<MetricEndpointId> {
        self.endpoints
            .iter()
            .position(|endpoint| endpoint.stream_id == Some(stream_id))
            .map(MetricEndpointId)
    }

    async fn update_credentials(&mut self, key: &str) -> Result<(), GenericError> {
        let api_key = match parse_api_key(key) {
            Ok(api_key) => api_key,
            Err(error) => {
                warn!(%error, "Ignoring an invalid stateful metrics API key update.");
                return Ok(());
            }
        };
        if api_key == self.api_key {
            return Ok(());
        }
        self.api_key = api_key;
        for index in 0..self.endpoints.len() {
            self.endpoints[index].suspended = false;
            self.endpoints[index].backoff = INITIAL_BACKOFF;
            let effects = self.core.reset_destination_state(MetricEndpointId(index));
            self.apply(effects).await?;
        }
        Ok(())
    }

    async fn pump(&mut self) -> Result<(), GenericError> {
        if self.flush_deadline().is_some_and(|deadline| deadline <= Instant::now()) {
            self.flush().await?;
        }
        // Bound each turn even when many small batches coalesce; keep timers and input responsive.
        for _ in 0..MAX_INFLIGHT_BATCHES {
            let mut progressed = false;
            if self.core.has_send_capacity() {
                if let Some(attempt) = self.queue.pop_shared().await {
                    progressed = true;
                    let batch = match attempt {
                        PendingTransaction::HighPriority(batch) => batch,
                        PendingTransaction::LowPriority(batch) => {
                            self.telemetry.batches_retried.increment(1);
                            batch
                        }
                    };
                    let result = self.core.push_batch(batch.0, now());
                    self.buffered_deadline
                        .get_or_insert_with(|| Instant::now() + self.flush_timeout);
                    self.apply_result(result, None).await?;
                }
            }
            for endpoint in self.lanes().collect::<Vec<_>>() {
                if !self.core.endpoint_has_send_capacity(endpoint) {
                    continue;
                }
                let Some(batch) = self.queue.pop_lane(endpoint).await else {
                    continue;
                };
                progressed = true;
                self.telemetry.batches_retried.increment(1);
                let result = self.core.send_batch_to(endpoint, batch.0, now());
                self.apply_result(result, Some(endpoint)).await?;
            }
            if !progressed {
                break;
            }
        }
        if self.flush_deadline().is_some_and(|deadline| deadline <= Instant::now()) {
            self.flush().await?;
        }
        Ok(())
    }

    async fn flush(&mut self) -> Result<(), GenericError> {
        let result = self.core.flush(now());
        self.apply_result(result, None).await
    }

    async fn fail(
        &mut self, endpoint: MetricEndpointId, kind: MetricStreamFailureKind, message: &str,
    ) -> Result<(), GenericError> {
        if let Some(stream_id) = self.core.current_stream_id(endpoint) {
            let effects = self
                .core
                .handle_stream_error(stream_id, MetricStreamFailure::new(kind, message));
            self.apply(effects).await?;
        }
        Ok(())
    }

    async fn on_transport(&mut self, event: TransportEvent) -> Result<(), GenericError> {
        let TransportEvent {
            endpoint,
            stream_id,
            kind,
        } = event;
        if self.core.current_stream_id(endpoint) != Some(stream_id) {
            return Ok(());
        }
        match kind {
            TransportEventKind::Opened(stream) => {
                if let Some(transport) = &mut self.transports[endpoint.get()] {
                    transport.state = TransportState::Open(stream);
                }
                self.schedule(endpoint, stream_id, TimerKind::RotateStream, self.stream_lifetime);
                let effects = self.core.handle_stream_opened(stream_id);
                self.apply(effects).await?;
            }
            TransportEventKind::Ack(ack) => {
                if ack.status != i32::from(batch_status::Status::Ok) {
                    return self
                        .fail(
                            endpoint,
                            MetricStreamFailureKind::InvalidArgument,
                            "invalid acknowledgement status",
                        )
                        .await;
                }
                let effects = self.core.handle_ack(stream_id, u64::from(ack.batch_id));
                let accepted = !effects.iter().any(|effect| {
                    matches!(
                        effect,
                        MetricClientEffect::StreamFailed { .. }
                            | MetricClientEffect::ReturnUnacknowledged { .. }
                            | MetricClientEffect::ReportError { .. }
                    )
                });
                if accepted {
                    let state = &mut self.endpoints[endpoint.get()];
                    state.backoff = INITIAL_BACKOFF;
                    state.unacknowledged = state.unacknowledged.saturating_sub(1);
                    state.ack_deadline = (state.unacknowledged > 0).then(|| Instant::now() + ACK_TIMEOUT);
                    self.telemetry.batches_acked.increment(1);
                }
                self.apply(effects).await?;
            }
            TransportEventKind::Failed(status) => {
                self.fail(endpoint, classify(status.code()), status.message()).await?
            }
        }
        Ok(())
    }

    fn schedule(&mut self, endpoint: MetricEndpointId, stream_id: StreamId, kind: TimerKind, delay: Duration) {
        self.endpoints[endpoint.get()].timers.push(
            async move {
                sleep(delay).await;
                (stream_id, kind)
            }
            .boxed(),
        );
    }

    /// Queues a returned batch for `endpoint` alone, or for every endpoint when `None`.
    async fn requeue(&mut self, endpoint: Option<MetricEndpointId>, batch: LogicalMetricBatch) {
        self.telemetry.track_drops(self.queue.push_retry(endpoint, batch).await);
    }

    async fn requeue_returned(
        &mut self, endpoint: MetricEndpointId, action: MetricFailureAction, batches: Vec<LogicalMetricBatch>,
    ) {
        for batch in batches {
            if action == MetricFailureAction::DoNotRetry {
                self.telemetry.batches_abandoned.increment(1);
                self.telemetry.points_dropped.increment(batch.point_count() as u64);
            } else {
                self.requeue(Some(endpoint), batch).await;
            }
        }
    }

    async fn apply_result(
        &mut self, result: Result<Vec<MetricClientEffect>, MetricClientError>, target: Option<MetricEndpointId>,
    ) -> Result<(), GenericError> {
        match result {
            Ok(effects) => self.apply(effects).await,
            Err(MetricClientError::Push(error)) => {
                self.requeue(target, error.into_batch()).await;
                if self.core.buffered_series_len() == 0 {
                    self.buffered_deadline = None;
                }
                Ok(())
            }
        }
    }

    async fn apply(&mut self, effects: Vec<MetricClientEffect>) -> Result<(), GenericError> {
        let mut effects: VecDeque<_> = effects.into();
        while let Some(effect) = effects.pop_front() {
            match effect {
                MetricClientEffect::OpenStream { endpoint, stream_id } => {
                    let state = &mut self.endpoints[endpoint.get()];
                    state.stream_closed();
                    state.stream_id = Some(stream_id);
                    self.transports[endpoint.get()] = Some(Transport::open(
                        endpoint,
                        stream_id,
                        state.address.clone(),
                        self.api_key.clone(),
                    ));
                }
                MetricClientEffect::SendPayload { stream_id, payload } => {
                    let Some(endpoint) = self.endpoint_for_stream(stream_id) else {
                        continue;
                    };
                    // A send failure below closes the stream; skip its remaining payloads.
                    if self.core.current_stream_id(endpoint) != Some(stream_id) {
                        continue;
                    }
                    let sent = match &mut self.transports[endpoint.get()] {
                        Some(transport) if transport.stream_id == stream_id => transport.send(payload),
                        _ => Err(Status::unavailable("outbound stream missing")),
                    };
                    if let Err(status) = sent {
                        let recovery = self.core.handle_stream_error(
                            stream_id,
                            MetricStreamFailure::new(MetricStreamFailureKind::Unavailable, status.message()),
                        );
                        for effect in recovery.into_iter().rev() {
                            effects.push_front(effect);
                        }
                        continue;
                    }
                    let state = &mut self.endpoints[endpoint.get()];
                    state.unacknowledged += 1;
                    state.ack_deadline.get_or_insert_with(|| Instant::now() + ACK_TIMEOUT);
                }
                MetricClientEffect::CloseStream { stream_id } => {
                    if let Some(endpoint) = self.endpoint_for_stream(stream_id) {
                        self.transports[endpoint.get()] = None;
                        self.endpoints[endpoint.get()].stream_closed();
                    }
                }
                MetricClientEffect::ReturnUnacknowledged {
                    endpoint,
                    action,
                    batches,
                } => self.requeue_returned(endpoint, action, batches).await,
                MetricClientEffect::ReturnBuffered { batch } => self.requeue(None, batch).await,
                MetricClientEffect::StreamFailed {
                    endpoint,
                    failure,
                    action,
                    unacknowledged,
                } => {
                    warn!(endpoint = endpoint.get(), kind = ?failure.kind(), ?action, batches = unacknowledged.len(), "Stateful metrics stream failed.");
                    self.telemetry.stream_failed(failure.kind());
                    self.requeue_returned(endpoint, action, unacknowledged).await;
                    // Configuration alone selects delivery. Unsupported stateful intake suspends retries.
                    self.endpoints[endpoint.get()].suspended = action != MetricFailureAction::RetryWithBackoff;
                }
                MetricClientEffect::ScheduleReconnect { stream_id } => {
                    if let Some(endpoint) = self.endpoint_for_stream(stream_id) {
                        let backoff = self.endpoints[endpoint.get()].backoff;
                        self.schedule(endpoint, stream_id, TimerKind::Reconnect, backoff);
                        self.endpoints[endpoint.get()].backoff = (backoff * 2).min(MAX_BACKOFF);
                    }
                }
                MetricClientEffect::ScheduleTimer { stream_id, timer } => {
                    if let Some(endpoint) = self.endpoint_for_stream(stream_id) {
                        self.schedule(endpoint, stream_id, timer.kind, timer.after);
                    }
                }
                MetricClientEffect::ReportError { error } => {
                    if matches!(
                        error,
                        MetricStreamError::AckMismatch { .. } | MetricStreamError::AckWithoutInflightBatch
                    ) {
                        warn!(
                            ?error,
                            "Stateful protocol rejected acknowledgement; endpoint suspended."
                        );
                    } else {
                        debug!(?error, "Stateful protocol event rejected.");
                    }
                }
            }
        }
        if self.core.buffered_series_len() == 0 {
            self.buffered_deadline = None;
        }
        Ok(())
    }

    async fn shutdown(mut self) -> Result<(), GenericError> {
        // Shutdown transfers all logical work out of the core and opens no replacement streams.
        for effect in self.core.shutdown() {
            match effect {
                MetricClientEffect::ReturnUnacknowledged {
                    endpoint,
                    action,
                    batches,
                } => self.requeue_returned(endpoint, action, batches).await,
                MetricClientEffect::ReturnBuffered { batch } => self.requeue(None, batch).await,
                _ => {}
            }
        }
        self.transports.clear();
        self.telemetry.track_drops(self.queue.flush().await?);
        Ok(())
    }
}

fn now() -> std::time::Instant {
    Instant::now().into_std()
}

/// Waits for the next timer on any endpoint. Cancel-safe: it holds no state between polls.
async fn next_timer(endpoints: &mut [EndpointState]) -> (StreamId, TimerKind) {
    poll_fn(|cx| {
        for endpoint in endpoints.iter_mut() {
            if let Poll::Ready(Some(fired)) = endpoint.timers.poll_next_unpin(cx) {
                return Poll::Ready(fired);
            }
        }
        Poll::Pending
    })
    .await
}

async fn wait_deadline(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => sleep_until(deadline).await,
        None => pending().await,
    }
}

fn parse_api_key(key: &str) -> Result<MetadataValue<Ascii>, GenericError> {
    let key = key.trim();
    if key.is_empty() {
        return Err(generic_error!("API key must not be blank"));
    }
    let mut value: MetadataValue<Ascii> = key.parse().error_context("API key is not valid gRPC metadata")?;
    value.set_sensitive(true);
    Ok(value)
}

fn classify(code: Code) -> MetricStreamFailureKind {
    match code {
        Code::DeadlineExceeded => MetricStreamFailureKind::DeadlineExceeded,
        Code::ResourceExhausted => MetricStreamFailureKind::ResourceExhausted,
        Code::InvalidArgument | Code::DataLoss | Code::OutOfRange => MetricStreamFailureKind::InvalidArgument,
        Code::Unauthenticated | Code::PermissionDenied => MetricStreamFailureKind::Unauthenticated,
        Code::FailedPrecondition | Code::Unimplemented => MetricStreamFailureKind::FailedPrecondition,
        _ => MetricStreamFailureKind::Unavailable,
    }
}
