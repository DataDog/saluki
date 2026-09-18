//! Experimental ADP destination for configuration-selected Foldspace series delivery.
//!
//! Each sender worker owns one sans-I/O core, gRPC stream, and ADP priority queue. Logical
//! batches enter the low-priority retry queue on recovery and can spill to disk. Retried data
//! is encoded against the current core state; original ADP metrics are not retained.
//! The initial topology starts one worker; future sharding can route to additional workers.
//!
//! # Missing
//!
//! - TODO: Add TLS/proxy support before using this path beyond plaintext integration tests.
//! - TODO: Add a byte budget for core inflight metrics and dictionary state.
//! - TODO: Support additional destinations; this experiment supports one stateful destination.

use std::{collections::VecDeque, future::pending, time::Duration};

use agent_data_plane_config::Live;
use async_trait::async_trait;
use foldspace_core::{
    proto::stateful::batch_status, CoreConfig, LogicalMetricBatch, MetricClientEffect, MetricClientError,
    MetricFailureAction, MetricStreamError, MetricStreamFailure, MetricStreamFailureKind, SenderConfig,
    StatefulMetricsClient, StreamId, TimerKind, ZstdBatchCompressor,
};
use futures::{future::BoxFuture, stream::FuturesUnordered, FutureExt as _, StreamExt as _};
use saluki_components::forwarders::queue::{DeliveryQueueConfiguration, PendingTransaction, PendingTransactions};
use saluki_core::{
    accounting::{MemoryBounds, MemoryBoundsBuilder},
    components::{
        destinations::{Destination, DestinationBuilder, DestinationContext},
        BuildContext,
    },
    data_model::event::{Event, EventType},
    observability::ComponentMetricsExt as _,
};
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use saluki_metrics::MetricsBuilder;
use stringtheory::MetaString;
use tokio::{
    select,
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
mod telemetry;
#[cfg(test)]
mod tests;
mod transport;

pub use self::router::StatefulMetricsRouterConfiguration;
use self::{
    retry::{build_queue, RetryBatch},
    telemetry::Telemetry,
    transport::{next_transport_event, Transport, TransportEvent, TransportEventKind, TransportState},
};

const MIN_FLUSH_TIMEOUT: Duration = Duration::from_millis(10);
const MAX_INFLIGHT_BATCHES: usize = 8;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const ACK_TIMEOUT: Duration = Duration::from_secs(30);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(30);
const INITIAL_BACKOFF: Duration = Duration::from_millis(250);
const MAX_BACKOFF: Duration = Duration::from_secs(30);

/// Opt-in destination for stateful series; delivery never switches automatically to HTTP.
pub struct StatefulMetricsConfiguration {
    /// Explicit plaintext test intake origin. There is no default endpoint.
    pub endpoint: MetaString,
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
        let endpoint = Endpoint::from_shared(self.endpoint.to_string())?.connect_timeout(CONNECT_TIMEOUT);
        let builder = MetricsBuilder::from_component_context(context.component_context());
        let queue = build_queue(&self.queue, &self.endpoint, 0, &builder).await?;
        let client = StatefulMetricsWorker::new(
            endpoint,
            parse_api_key(&self.api_key)?,
            self.compression_level,
            self.flush_timeout,
            self.batch_capacity,
            queue,
            builder,
        );
        Ok(Box::new(StatefulMetrics {
            client,
            api_key: self.api_key.clone(),
            delivery_shutdown_timeout: (self.stop_timeout / 2).min(SHUTDOWN_TIMEOUT),
        }))
    }
}

impl MemoryBounds for StatefulMetricsConfiguration {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        builder
            .minimum()
            .with_single_value::<StatefulMetrics>("component struct");
    }
}

struct StatefulMetrics {
    client: StatefulMetricsWorker,
    api_key: Live<String>,
    delivery_shutdown_timeout: Duration,
}

#[async_trait]
impl Destination for StatefulMetrics {
    async fn run(mut self: Box<Self>, mut context: DestinationContext) -> Result<(), GenericError> {
        let mut health = context.take_health_handle();
        let effects = self.client.core.start();
        self.client.apply(effects).await?;
        let mut input_closed = false;
        let mut shutdown_deadline = None;
        health.mark_ready();

        loop {
            self.client.pump().await?;
            if input_closed {
                if self.client.core.has_send_capacity() {
                    self.client.flush().await?;
                }
                if self.client.is_empty() || self.client.suspended {
                    break;
                }
            }
            let flush_deadline = self.client.flush_deadline();
            select! {
                _ = health.live() => {},
                _ = tokio::task::yield_now(), if !self.client.suspended && self.client.core.has_send_capacity() && !self.client.queue.is_empty() => {},
                key = self.api_key.changed() => { self.client.update_credentials(&key).await?; },
                event = next_transport_event(&mut self.client.transport) => { self.client.on_transport(event).await?; },
                Some((stream_id, kind)) = self.client.timers.next(), if !self.client.timers.is_empty() => {
                    let effects = self.client.core.handle_timer(stream_id, kind);
                    self.client.apply(effects).await?;
                },
                _ = wait_deadline(flush_deadline) => { self.client.flush().await?; },
                _ = wait_deadline(self.client.ack_deadline) => {
                    self.client.fail(MetricStreamFailureKind::DeadlineExceeded, "acknowledgement timed out").await?;
                },
                _ = wait_deadline(shutdown_deadline) => break,
                events = context.events().next(), if !input_closed => {
                    match events {
                        Some(events) => self.client.accept(events).await,
                        None => { input_closed = true; shutdown_deadline = Some(Instant::now() + self.delivery_shutdown_timeout); },
                    }
                },
            }
        }
        self.client.shutdown().await
    }
}

struct StatefulMetricsWorker {
    core: StatefulMetricsClient<ZstdBatchCompressor>,
    telemetry: Telemetry,
    endpoint: Endpoint,
    api_key: MetadataValue<Ascii>,
    transport: Option<Transport>,
    timers: FuturesUnordered<BoxFuture<'static, (StreamId, TimerKind)>>,
    queue: PendingTransactions<RetryBatch>,
    flush_timeout: Duration,
    buffered_deadline: Option<Instant>,
    suspended: bool,
    ack_deadline: Option<Instant>,
    backoff: Duration,
    stream_lifetime: Duration,
}

impl StatefulMetricsWorker {
    fn new(
        endpoint: Endpoint, api_key: MetadataValue<Ascii>, compression_level: i32, flush_timeout: Duration,
        batch_capacity: usize, queue: PendingTransactions<RetryBatch>, builder: MetricsBuilder,
    ) -> Self {
        let config = CoreConfig {
            batch_capacity,
            sender: SenderConfig {
                max_inflight_payloads: MAX_INFLIGHT_BATCHES,
                ..SenderConfig::default()
            },
        };
        let stream_lifetime = config.sender.stream_lifetime;
        Self {
            core: StatefulMetricsClient::new(config, ZstdBatchCompressor::new(compression_level)),
            telemetry: Telemetry::new(builder),
            endpoint,
            api_key,
            transport: None,
            timers: FuturesUnordered::new(),
            queue,
            flush_timeout: if flush_timeout.is_zero() {
                MIN_FLUSH_TIMEOUT
            } else {
                flush_timeout
            },
            buffered_deadline: None,
            suspended: false,
            ack_deadline: None,
            backoff: INITIAL_BACKOFF,
            stream_lifetime,
        }
    }

    async fn accept(&mut self, events: impl IntoIterator<Item = Event>) {
        let series = events
            .into_iter()
            .filter_map(|event| match event {
                Event::Metric(metric) => conversion::convert(&metric).filter(|series| !series.name().is_empty()),
                _ => None,
            })
            .collect();
        let batch = LogicalMetricBatch::new(series);
        if !batch.is_empty() {
            let points = batch.point_count() as u64;
            self.telemetry
                .track_enqueue(self.queue.push_high_priority(RetryBatch(batch)).await, points);
        }
    }

    fn is_empty(&self) -> bool {
        self.queue.is_empty() && self.core.inflight_len() == 0 && self.core.buffered_series_len() == 0
    }

    fn flush_deadline(&self) -> Option<Instant> {
        (!self.suspended && self.core.has_send_capacity())
            .then_some(self.buffered_deadline)
            .flatten()
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
        self.suspended = false;
        self.backoff = INITIAL_BACKOFF;
        let effects = self.core.reset_destination_state();
        self.apply(effects).await
    }

    async fn pump(&mut self) -> Result<(), GenericError> {
        if self.flush_deadline().is_some_and(|deadline| deadline <= Instant::now()) {
            self.flush().await?;
        }
        // Bound each turn even when many small batches coalesce; keep timers and input responsive.
        for _ in 0..MAX_INFLIGHT_BATCHES {
            if self.suspended || !self.core.has_send_capacity() {
                break;
            }
            let Some(attempt) = self.queue.pop().await else { break };
            let batch = match attempt {
                PendingTransaction::HighPriority(batch) => batch,
                PendingTransaction::LowPriority(batch) => {
                    self.telemetry.batches_retried.increment(1);
                    batch
                }
            };
            let result = self.core.push_batch(batch.0);
            self.buffered_deadline
                .get_or_insert_with(|| Instant::now() + self.flush_timeout);
            self.apply(result).await?;
        }
        if self.flush_deadline().is_some_and(|deadline| deadline <= Instant::now()) {
            self.flush().await?;
        }
        Ok(())
    }

    async fn flush(&mut self) -> Result<(), GenericError> {
        let result = self.core.flush();
        self.apply(result).await
    }

    async fn fail(&mut self, kind: MetricStreamFailureKind, message: &str) -> Result<(), GenericError> {
        if let Some(stream_id) = self.core.current_stream_id() {
            let effects = self
                .core
                .handle_stream_error(stream_id, MetricStreamFailure::new(kind, message));
            self.apply(effects).await?;
        }
        Ok(())
    }

    async fn on_transport(&mut self, event: TransportEvent) -> Result<(), GenericError> {
        let stream_id = event.stream_id;
        if self.core.current_stream_id() != Some(stream_id) {
            return Ok(());
        }
        match event.kind {
            TransportEventKind::Opened(stream) => {
                if let Some(transport) = &mut self.transport {
                    transport.state = TransportState::Open(stream);
                }
                self.schedule(stream_id, TimerKind::RotateStream, self.stream_lifetime);
                let effects = self.core.handle_stream_opened(stream_id);
                self.apply(effects).await?;
            }
            TransportEventKind::Ack(ack) => {
                if ack.status != i32::from(batch_status::Status::Ok) {
                    return self
                        .fail(
                            MetricStreamFailureKind::InvalidArgument,
                            "invalid acknowledgement status",
                        )
                        .await;
                }
                let before = self.core.inflight_len();
                let effects = self.core.handle_ack(stream_id, u64::from(ack.batch_id));
                let accepted = effects.as_ref().is_ok_and(|effects| {
                    !effects.iter().any(|effect| {
                        matches!(
                            effect,
                            MetricClientEffect::StreamFailed { .. } | MetricClientEffect::ReturnUnacknowledged { .. }
                        )
                    })
                }) && self.core.inflight_len() < before;
                if accepted {
                    self.backoff = INITIAL_BACKOFF;
                    self.telemetry.batches_acked.increment(1);
                }
                self.apply(effects).await?;
                if self.core.inflight_len() == 0 {
                    self.ack_deadline = None;
                } else if accepted {
                    self.ack_deadline = Some(Instant::now() + ACK_TIMEOUT);
                }
            }
            TransportEventKind::Failed(status) => self.fail(classify(status.code()), status.message()).await?,
        }
        Ok(())
    }

    fn schedule(&mut self, stream_id: StreamId, kind: TimerKind, delay: Duration) {
        self.timers.push(
            async move {
                sleep(delay).await;
                (stream_id, kind)
            }
            .boxed(),
        );
    }

    async fn requeue(&mut self, batch: LogicalMetricBatch) {
        let points = batch.point_count() as u64;
        self.telemetry
            .track_enqueue(self.queue.push_low_priority(RetryBatch(batch)).await, points);
    }

    async fn apply(&mut self, result: Result<Vec<MetricClientEffect>, MetricClientError>) -> Result<(), GenericError> {
        let mut effects: VecDeque<_> = match result {
            Ok(effects) => effects.into(),
            Err(MetricClientError::Push(error)) => {
                self.requeue(error.into_batch()).await;
                if self.core.buffered_series_len() == 0 {
                    self.buffered_deadline = None;
                }
                return Ok(());
            }
            Err(MetricClientError::Encode { stream_id, .. }) => self
                .core
                .handle_stream_error(
                    stream_id,
                    MetricStreamFailure::new(
                        MetricStreamFailureKind::InvalidArgument,
                        "local stateful encoding failed",
                    ),
                )
                .map_err(|error| generic_error!("stateful recovery failed: {error:?}"))?
                .into(),
        };
        while let Some(effect) = effects.pop_front() {
            match effect {
                MetricClientEffect::OpenStream { stream_id } => {
                    self.timers.clear();
                    self.transport = Some(Transport::open(stream_id, self.endpoint.clone(), self.api_key.clone()));
                }
                MetricClientEffect::SendPayload { stream_id, payload } => {
                    let sent = match &mut self.transport {
                        Some(transport) if transport.stream_id == stream_id => transport.send(payload),
                        _ => Err(Status::unavailable("outbound stream missing")),
                    };
                    if let Err(status) = sent {
                        effects = self
                            .core
                            .handle_stream_error(
                                stream_id,
                                MetricStreamFailure::new(MetricStreamFailureKind::Unavailable, status.message()),
                            )
                            .map_err(|error| generic_error!("stateful recovery failed: {error:?}"))?
                            .into();
                        continue;
                    }
                    self.ack_deadline.get_or_insert_with(|| Instant::now() + ACK_TIMEOUT);
                }
                MetricClientEffect::CloseStream { .. } => {
                    self.transport = None;
                    self.timers.clear();
                    self.ack_deadline = None;
                }
                MetricClientEffect::ReturnUnacknowledged { batches } => {
                    for batch in batches {
                        self.requeue(batch).await;
                    }
                }
                MetricClientEffect::ReturnBuffered { batch } => self.requeue(batch).await,
                MetricClientEffect::StreamFailed {
                    failure,
                    action,
                    unacknowledged,
                } => {
                    warn!(kind = ?failure.kind(), ?action, batches = unacknowledged.len(), "Stateful metrics stream failed.");
                    self.telemetry.stream_failed(failure.kind());
                    for batch in unacknowledged {
                        if action == MetricFailureAction::DoNotRetry {
                            self.telemetry.batches_abandoned.increment(1);
                            self.telemetry.points_dropped.increment(batch.point_count() as u64);
                        } else {
                            self.requeue(batch).await;
                        }
                    }
                    // Configuration alone selects delivery. Unsupported stateful intake suspends retries.
                    self.suspended = action != MetricFailureAction::RetryWithBackoff;
                }
                MetricClientEffect::ScheduleReconnect { stream_id } => {
                    self.schedule(stream_id, TimerKind::Reconnect, self.backoff);
                    self.backoff = (self.backoff * 2).min(MAX_BACKOFF);
                }
                MetricClientEffect::ScheduleTimer { stream_id, timer } => {
                    self.schedule(stream_id, timer.kind, timer.after)
                }
                MetricClientEffect::ReportError { error } => {
                    if matches!(
                        error,
                        MetricStreamError::AckMismatch { .. } | MetricStreamError::AckWithoutInflightBatch
                    ) {
                        self.suspended = true;
                        warn!(
                            ?error,
                            "Stateful protocol rejected acknowledgement; delivery suspended."
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
        // Reset transfers all logical work out of the core. Do not open its replacement stream.
        let effects = self
            .core
            .reset_destination_state()
            .map_err(|error| generic_error!("stateful shutdown failed: {error:?}"))?;
        for effect in effects {
            match effect {
                MetricClientEffect::ReturnUnacknowledged { batches } => {
                    for batch in batches {
                        self.requeue(batch).await;
                    }
                }
                MetricClientEffect::ReturnBuffered { batch } => self.requeue(batch).await,
                _ => {}
            }
        }
        self.transport = None;
        self.telemetry.track_drops(self.queue.flush().await?);
        Ok(())
    }
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
