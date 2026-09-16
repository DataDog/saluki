//! Experimental ADP client for Foldspace series delivery.
//!
//! Foldspace is a linked library: it owns encoding, dictionary state, and recovery decisions.
//! Each sender worker owns its own core, transport, timers, and bounded logical retries.
//! No dictionary, stream ID, or inflight state is shared between workers, even for the same endpoint.
//! The initial topology starts one worker; future sharding can route metrics to more worker instances.
//! Original metrics are moved into a parallel FIFO until ACK so stateless fallback can use the existing HTTP encoder unchanged.
//!
//! # Missing
//!
//! - TODO: Add TLS/proxy support before using this path beyond plaintext integration tests.
//! - TODO: Add durable retries and a byte budget for retained metrics and dictionary state.
//! - TODO: Support additional destinations; this experiment supports one stateful destination.

use std::{collections::VecDeque, future::pending, time::Duration};

use agent_data_plane_config::Live;
use async_trait::async_trait;
use foldspace_core::proto::stateful::{
    batch_status, stateful_intake_client::StatefulIntakeClient, BatchStatus, StatefulBatch,
};
use foldspace_core::{
    CoreConfig, LogicalMetricBatch, MetricClientEffect, MetricClientError, MetricFailureAction, MetricStreamError,
    MetricStreamFailure, MetricStreamFailureKind, SenderConfig, StatefulMetricsClient, StreamId, TimerKind,
    ZstdBatchCompressor,
};
use futures::{future::BoxFuture, stream::FuturesUnordered, FutureExt as _, StreamExt as _};
use metrics::counter;
use saluki_core::{
    accounting::{MemoryBounds, MemoryBoundsBuilder},
    components::{
        transforms::{Transform, TransformBuilder, TransformContext},
        BuildContext,
    },
    data_model::event::{metric::Metric, Event, EventType},
    topology::OutputDefinition,
};
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use stringtheory::MetaString;
use tokio::{
    select,
    sync::mpsc,
    time::{sleep, sleep_until, timeout, Instant},
};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{
    metadata::{Ascii, MetadataValue},
    transport::Endpoint,
    Code, Request, Status, Streaming,
};
use tracing::{debug, warn};

mod conversion;
#[cfg(test)]
mod tests;

const MAX_BUFFERED_BATCHES: usize = 32;
const MAX_INFLIGHT_BATCHES: usize = 8;
const REQUESTED_STATE_BYTES: &str = "5242880";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const ACK_TIMEOUT: Duration = Duration::from_secs(30);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(30);
const INITIAL_BACKOFF: Duration = Duration::from_millis(250);
const MAX_BACKOFF: Duration = Duration::from_secs(30);
const OUTPUTS: &[OutputDefinition<EventType>] = &[OutputDefinition::default_output(EventType::Metric)];

/// Opt-in stateful series client; its output feeds the existing HTTP metrics encoder.
pub struct StatefulMetricsConfiguration {
    /// Explicit plaintext test intake origin. There is no default endpoint.
    pub endpoint: MetaString,
    /// Live primary API key. A change clears dictionary state and resumes suspended delivery.
    pub api_key: Live<String>,
    /// Zstd level, supplied from the existing serializer configuration (default 3).
    pub compression_level: i32,
}

#[async_trait]
impl TransformBuilder for StatefulMetricsConfiguration {
    fn input_event_type(&self) -> EventType {
        EventType::Metric
    }
    fn outputs(&self) -> &[OutputDefinition<EventType>] {
        OUTPUTS
    }

    async fn build(&self, _context: BuildContext) -> Result<Box<dyn Transform + Send>, GenericError> {
        let endpoint = Endpoint::from_shared(self.endpoint.to_string())?.connect_timeout(CONNECT_TIMEOUT);
        let client = StatefulMetricsWorker::new(endpoint, parse_api_key(&self.api_key)?, self.compression_level);
        Ok(Box::new(StatefulMetrics {
            client,
            api_key: self.api_key.clone(),
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
}

#[async_trait]
impl Transform for StatefulMetrics {
    async fn run(mut self: Box<Self>, mut context: TransformContext) -> Result<(), GenericError> {
        let mut health = context.take_health_handle();
        let effects = self.client.core.start();
        self.client.apply(effects)?;
        let mut input_closed = false;
        let mut shutdown_deadline = None;
        health.mark_ready();

        loop {
            self.client.pump()?;
            if !self.client.fallback.is_empty() {
                context
                    .dispatcher()
                    .buffered()?
                    .send_all(self.client.fallback.drain(..).map(Event::Metric))
                    .await?;
            }
            if input_closed && self.client.buffered_batches() == 0 {
                break;
            }

            select! {
                _ = health.live() => {},
                key = self.api_key.changed() => {
                    self.client.update_credentials(&key)?;
                }
                event = next_transport_event(&mut self.client.transport) => {
                    self.client.on_transport(event)?;
                }
                Some((stream_id, kind)) = self.client.timers.next(), if !self.client.timers.is_empty() => {
                    let effects = self.client.core.handle_timer(stream_id, kind);
                    self.client.apply(effects)?;
                }
                _ = wait_deadline(self.client.ack_deadline) => {
                    self.client.fail(MetricStreamFailureKind::DeadlineExceeded, "acknowledgement timed out")?;
                }
                _ = wait_deadline(shutdown_deadline) => {
                    return Err(generic_error!("stateful metrics shutdown timed out with {} unacknowledged or queued batches", self.client.buffered_batches()));
                }
                events = context.events().next(), if !input_closed && self.client.buffered_batches() < MAX_BUFFERED_BATCHES => {
                    match events {
                        Some(events) => {
                            let passthrough = self.client.accept(events);
                            if !passthrough.is_empty() {
                                context.dispatcher().buffered()?.send_all(passthrough).await?;
                            }
                        }
                        None => {
                            input_closed = true;
                            shutdown_deadline = Some(Instant::now() + SHUTDOWN_TIMEOUT);
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DeliveryMode {
    Stateful,
    Suspended,
    Http,
}

struct PendingBatch {
    logical: LogicalMetricBatch,
    originals: Vec<Metric>,
}

struct StatefulMetricsWorker {
    core: StatefulMetricsClient<ZstdBatchCompressor>,
    endpoint: Endpoint,
    api_key: MetadataValue<Ascii>,
    transport: Option<Transport>,
    timers: FuturesUnordered<BoxFuture<'static, (StreamId, TimerKind)>>,
    pending: VecDeque<PendingBatch>,
    inflight: VecDeque<Vec<Metric>>,
    fallback: Vec<Metric>,
    mode: DeliveryMode,
    ack_deadline: Option<Instant>,
    backoff: Duration,
    stream_lifetime: Duration,
}

impl StatefulMetricsWorker {
    fn new(endpoint: Endpoint, api_key: MetadataValue<Ascii>, compression_level: i32) -> Self {
        let config = CoreConfig {
            sender: SenderConfig {
                max_inflight_payloads: MAX_INFLIGHT_BATCHES,
                ..SenderConfig::default()
            },
            ..CoreConfig::default()
        };
        let stream_lifetime = config.sender.stream_lifetime;
        Self {
            core: StatefulMetricsClient::new(config, ZstdBatchCompressor::new(compression_level)),
            endpoint,
            api_key,
            transport: None,
            timers: FuturesUnordered::new(),
            pending: VecDeque::new(),
            inflight: VecDeque::new(),
            fallback: Vec::new(),
            mode: DeliveryMode::Stateful,
            ack_deadline: None,
            backoff: INITIAL_BACKOFF,
            stream_lifetime,
        }
    }

    fn accept(&mut self, events: impl IntoIterator<Item = Event>) -> Vec<Event> {
        let mut passthrough = Vec::new();
        let mut originals = Vec::new();
        let mut series = Vec::new();
        for event in events {
            let Event::Metric(metric) = event else { continue };
            if self.mode != DeliveryMode::Http {
                if let Some(logical) = conversion::convert(&metric) {
                    if !logical.name().is_empty() && !logical.points().is_empty() {
                        originals.push(metric);
                        series.push(logical);
                    }
                    continue;
                }
            }
            passthrough.push(Event::Metric(metric));
        }
        if !series.is_empty() {
            self.pending.push_back(PendingBatch {
                logical: LogicalMetricBatch::new(series),
                originals,
            });
        }
        passthrough
    }

    fn buffered_batches(&self) -> usize {
        self.pending.len() + self.inflight.len()
    }

    fn update_credentials(&mut self, key: &str) -> Result<(), GenericError> {
        self.api_key = parse_api_key(key)?;
        self.mode = DeliveryMode::Stateful;
        self.backoff = INITIAL_BACKOFF;
        let effects = self.core.reset_destination_state();
        self.apply(effects)
    }

    fn pump(&mut self) -> Result<(), GenericError> {
        while self.mode == DeliveryMode::Stateful && self.core.has_send_capacity() {
            let Some(PendingBatch { logical, originals }) = self.pending.pop_front() else {
                break;
            };
            match self.core.push_batch(logical) {
                Err(MetricClientError::Push(error)) => {
                    self.pending.push_front(PendingBatch {
                        logical: error.into_batch(),
                        originals,
                    });
                    break;
                }
                result => {
                    self.inflight.push_back(originals);
                    self.apply(result)?;
                }
            }
        }
        Ok(())
    }

    fn fail(&mut self, kind: MetricStreamFailureKind, message: &str) -> Result<(), GenericError> {
        if let Some(stream_id) = self.core.current_stream_id() {
            let effects = self
                .core
                .handle_stream_error(stream_id, MetricStreamFailure::new(kind, message));
            self.apply(effects)?;
        }
        Ok(())
    }

    fn on_transport(&mut self, event: TransportEvent) -> Result<(), GenericError> {
        let Some(stream_id) = self.core.current_stream_id() else {
            return Ok(());
        };
        match event {
            TransportEvent::Opened(stream) => {
                if let Some(transport) = &mut self.transport {
                    transport.state = TransportState::Open(stream);
                }
                self.schedule(stream_id, TimerKind::RotateStream, self.stream_lifetime);
                let effects = self.core.handle_stream_opened(stream_id);
                self.apply(effects)?;
            }
            TransportEvent::Ack(ack) => {
                if ack.status != i32::from(batch_status::Status::Ok) {
                    return self.fail(
                        MetricStreamFailureKind::InvalidArgument,
                        "invalid acknowledgement status",
                    );
                }
                let before = self.core.inflight_len();
                let effects = self.core.handle_ack(stream_id, u64::from(ack.batch_id));
                if let Ok(effects) = &effects {
                    let returned = effects.iter().any(|effect| {
                        matches!(
                            effect,
                            MetricClientEffect::StreamFailed { .. } | MetricClientEffect::ReturnUnacknowledged { .. }
                        )
                    });
                    if !returned && self.core.inflight_len() < before {
                        self.inflight.pop_front();
                        self.backoff = INITIAL_BACKOFF;
                        counter!("stateful_metrics_batches_acked_total").increment(1);
                    }
                }
                self.apply(effects)?;
                if self.core.inflight_len() == 0 {
                    self.ack_deadline = None;
                } else if self.core.inflight_len() < before {
                    self.ack_deadline = Some(Instant::now() + ACK_TIMEOUT);
                }
            }
            TransportEvent::Failed(status) => self.fail(classify(status.code()), status.message())?,
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

    fn apply(&mut self, result: Result<Vec<MetricClientEffect>, MetricClientError>) -> Result<(), GenericError> {
        let effects = match result {
            Ok(effects) => effects,
            Err(MetricClientError::Encode { stream_id, .. }) => self
                .core
                .handle_stream_error(
                    stream_id,
                    MetricStreamFailure::new(
                        MetricStreamFailureKind::InvalidArgument,
                        "local stateful encoding failed",
                    ),
                )
                .map_err(|error| generic_error!("failed to recover stateful encoding: {error:?}"))?,
            Err(MetricClientError::Push(_)) => return Err(generic_error!("unexpected stateful push rejection")),
        };
        for effect in effects {
            match effect {
                MetricClientEffect::OpenStream { .. } => {
                    self.timers.clear();
                    let (sender, receiver) = mpsc::channel(MAX_INFLIGHT_BATCHES + 1);
                    let endpoint = self.endpoint.clone();
                    let mut request = Request::new(ReceiverStream::new(receiver));
                    request.metadata_mut().insert("dd-api-key", self.api_key.clone());
                    request
                        .metadata_mut()
                        .insert("dd-content-encoding", MetadataValue::from_static("zstd"));
                    request.metadata_mut().insert(
                        "dd-state-request-bytes",
                        MetadataValue::from_static(REQUESTED_STATE_BYTES),
                    );
                    let opening = async move {
                        timeout(CONNECT_TIMEOUT, async move {
                            let channel = endpoint
                                .connect()
                                .await
                                .map_err(|_| Status::unavailable("connection failed"))?;
                            StatefulIntakeClient::new(channel)
                                .stateful_stream(request)
                                .await
                                .map(|response| response.into_inner())
                        })
                        .await
                        .unwrap_or_else(|_| Err(Status::deadline_exceeded("opening stream timed out")))
                    }
                    .boxed();
                    self.transport = Some(Transport {
                        sender,
                        state: TransportState::Connecting(opening),
                    });
                }
                MetricClientEffect::SendPayload { stream_id, payload } => {
                    let sent = self
                        .transport
                        .as_ref()
                        .is_some_and(|transport| transport.sender.try_send(payload).is_ok());
                    if !sent {
                        let effects = self.core.handle_stream_error(
                            stream_id,
                            MetricStreamFailure::new(
                                MetricStreamFailureKind::Unavailable,
                                "outbound stream closed or full",
                            ),
                        );
                        self.apply(effects)?;
                        break;
                    }
                    self.ack_deadline.get_or_insert_with(|| Instant::now() + ACK_TIMEOUT);
                }
                MetricClientEffect::CloseStream { .. } => {
                    self.transport = None;
                    self.timers.clear();
                    self.ack_deadline = None;
                }
                MetricClientEffect::ReturnUnacknowledged { batches } => self.requeue(batches)?,
                MetricClientEffect::StreamFailed {
                    failure,
                    action,
                    unacknowledged,
                } => {
                    warn!(kind = ?failure.kind(), ?action, batches = unacknowledged.len(), "Stateful metrics stream failed.");
                    counter!("stateful_metrics_stream_failures_total", "kind" => format!("{:?}", failure.kind()))
                        .increment(1);
                    let returned_count = unacknowledged.len();
                    self.requeue(unacknowledged)?;
                    match action {
                        MetricFailureAction::RetryWithBackoff => {}
                        MetricFailureAction::WaitForCredentials => self.mode = DeliveryMode::Suspended,
                        MetricFailureAction::DoNotRetry => {
                            // The core returns every speculative batch. None can be retried unchanged on this stream.
                            self.mode = DeliveryMode::Suspended;
                            counter!("stateful_metrics_batches_abandoned_total").increment(returned_count as u64);
                            self.pending.drain(..returned_count);
                        }
                        MetricFailureAction::UseStatelessDelivery => {
                            self.mode = DeliveryMode::Http;
                            for batch in self.pending.drain(..) {
                                self.fallback.extend(batch.originals);
                            }
                        }
                    }
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
                        self.mode = DeliveryMode::Suspended;
                        warn!(?error, "Stateful metrics suspended after invalid acknowledgement.");
                    } else {
                        debug!(?error, "Stateful protocol event rejected.");
                    }
                }
            }
        }
        Ok(())
    }

    fn requeue(&mut self, batches: Vec<LogicalMetricBatch>) -> Result<(), GenericError> {
        if batches.len() != self.inflight.len() {
            return Err(generic_error!("stateful logical and original batch ownership diverged"));
        }
        let mut returned: VecDeque<_> = batches
            .into_iter()
            .zip(self.inflight.drain(..))
            .map(|(logical, originals)| PendingBatch { logical, originals })
            .collect();
        returned.append(&mut self.pending);
        self.pending = returned;
        Ok(())
    }
}

struct Transport {
    sender: mpsc::Sender<StatefulBatch>,
    state: TransportState,
}

enum TransportState {
    Connecting(BoxFuture<'static, Result<Streaming<BatchStatus>, Status>>),
    Open(Box<Streaming<BatchStatus>>),
}

enum TransportEvent {
    Opened(Box<Streaming<BatchStatus>>),
    Ack(BatchStatus),
    Failed(Status),
}

async fn next_transport_event(transport: &mut Option<Transport>) -> TransportEvent {
    match transport.as_mut().map(|transport| &mut transport.state) {
        Some(TransportState::Connecting(opening)) => match opening.await {
            Ok(stream) => TransportEvent::Opened(Box::new(stream)),
            Err(status) => TransportEvent::Failed(status),
        },
        Some(TransportState::Open(stream)) => match stream.message().await {
            Ok(Some(ack)) => TransportEvent::Ack(ack),
            Ok(None) => TransportEvent::Failed(Status::unavailable("stream ended")),
            Err(status) => TransportEvent::Failed(status),
        },
        None => pending().await,
    }
}

async fn wait_deadline(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => sleep_until(deadline).await,
        None => pending().await,
    }
}

fn parse_api_key(key: &str) -> Result<MetadataValue<Ascii>, GenericError> {
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
