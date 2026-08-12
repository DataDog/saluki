//! Endpoint-scoped Foldspace transport for logical metric series.

use std::{collections::VecDeque, path::PathBuf, sync::Arc, time::Duration};

use foldspace_core::{
    proto::stateful::{
        batch_status, stateful_intake_client::StatefulIntakeClient, BatchStatus, StatefulBatch as ProtoStatefulBatch,
    },
    CoreConfig, LogicalMetricBatch, MetricBatchEncoder, MetricEffect, SenderConfig, StatefulMetricsCore, StreamError,
    StreamId, TimerKind, ZstdBatchCompressor,
};
use metrics::Counter;
use rand::RngExt as _;
use saluki_core::{
    components::ComponentContext,
    data_model::payload::{MetricSeriesPayload, PayloadMetadata},
};
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use saluki_io::net::util::retry::{DiskUsageRetrieverImpl, EventContainer, PersistedQueueArgs, RetryQueue, Retryable};
use saluki_metrics::MetricsBuilder;
use serde::{Deserialize, Serialize};
use stringtheory::MetaString;
use tokio::sync::{mpsc, oneshot};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{metadata::MetadataValue, transport::Channel, Request as TonicRequest};
use tracing::{error, warn};

use super::{
    config::ForwarderConfiguration,
    endpoints::{EndpointRoute, EndpointV3Settings, ResolvedEndpoint, V3EndpointConfig},
    io::{generate_retry_queue_id, should_route_to_endpoint, track_queue_drops},
    protocol::MetricsPayloadInfo,
    telemetry::ComponentTelemetry,
    transaction::Metadata,
};

const STATE_REQUEST_BYTES: u64 = 5 * 1024 * 1024;
const LOGICAL_METRIC_BATCH_VERSION: u16 = 1;
const LOGICAL_METRIC_COMPRESSION_LEVEL: i32 = 3;
const FRESH_BURST_LIMIT: usize = 8;
const RETRIES_BEFORE_DISK: usize = 4;
const RECONNECT_JITTER_FRACTION: f64 = 0.2;
const MAX_RECONNECT_BACKOFF: Duration = Duration::from_secs(30);

/// Builder for a stateful metrics forwarder.
pub(crate) struct StatefulMetricsForwarder {
    context: ComponentContext,
    config: ForwarderConfiguration,
    live_config: Option<saluki_config::GenericConfiguration>,
    telemetry: ComponentTelemetry,
    metrics_builder: MetricsBuilder,
}

impl StatefulMetricsForwarder {
    pub(crate) fn from_config(
        context: ComponentContext, config: ForwarderConfiguration,
        live_config: Option<saluki_config::GenericConfiguration>, telemetry: ComponentTelemetry,
        metrics_builder: MetricsBuilder,
    ) -> Option<Self> {
        config.stateful_metrics_enabled().then_some(Self {
            context,
            config,
            live_config,
            telemetry,
            metrics_builder,
        })
    }

    pub(crate) async fn spawn(self) -> Result<StatefulMetricsHandle, GenericError> {
        let endpoints = self.config.build_routable_endpoints(self.live_config.clone())?;
        let has_metrics_primary = endpoints
            .iter()
            .any(|endpoint| endpoint.route() == EndpointRoute::MetricsPrimary);
        let (payloads_tx, mut payloads_rx) = mpsc::channel::<MetricSeriesPayload>(8);
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let mut endpoint_senders = Vec::new();
        let mut endpoint_tasks = Vec::new();

        for routable in endpoints {
            let (route, endpoint) = routable.into_parts();
            if !should_route_to_endpoint(true, has_metrics_primary, route)
                || !endpoint_uses_stateful_series(&self.config, route, &endpoint)
            {
                continue;
            }

            let (endpoint_tx, endpoint_rx) = mpsc::channel(self.config.endpoint_buffer_size());
            let sender = StatefulMetricsEndpoint::new(
                self.context.clone(),
                self.config.clone(),
                endpoint,
                self.telemetry.clone(),
                &self.metrics_builder,
            )
            .await?;
            endpoint_senders.push(endpoint_tx);
            endpoint_tasks.push(tokio::spawn(sender.run(endpoint_rx)));
        }

        let telemetry = self.telemetry;
        tokio::spawn(async move {
            while let Some(payload) = payloads_rx.recv().await {
                endpoint_senders.retain(|sender| !sender.is_closed());
                if endpoint_senders.is_empty() {
                    let metadata = metadata_from_payload(payload.metadata());
                    telemetry.track_permanently_failed_transaction(&metadata, None, "stateful-metrics");
                    continue;
                }
                for sender in &endpoint_senders {
                    if sender.send(payload.clone()).await.is_err() {
                        let metadata = metadata_from_payload(payload.metadata());
                        telemetry.track_permanently_failed_transaction(&metadata, None, "stateful-metrics");
                    }
                }
            }
            drop(endpoint_senders);
            for task in endpoint_tasks {
                if let Err(error) = task.await {
                    error!(%error, "Stateful metrics endpoint task panicked.");
                }
            }
            let _ = shutdown_tx.send(());
        });

        Ok(StatefulMetricsHandle {
            payloads_tx,
            shutdown_rx,
        })
    }
}

/// Handle for sending logical series and awaiting graceful persistence.
pub(crate) struct StatefulMetricsHandle {
    payloads_tx: mpsc::Sender<MetricSeriesPayload>,
    shutdown_rx: oneshot::Receiver<()>,
}

impl StatefulMetricsHandle {
    pub(crate) async fn send(&self, payload: MetricSeriesPayload) -> Result<(), GenericError> {
        self.payloads_tx
            .send(payload)
            .await
            .error_context("Stateful metrics forwarder stopped before accepting a logical batch.")
    }

    pub(crate) async fn shutdown(self) {
        drop(self.payloads_tx);
        let _ = self.shutdown_rx.await;
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct CompressedLogicalMetricBatch {
    version: u16,
    event_count: usize,
    data_point_count: usize,
    #[serde(with = "compressed_bytes")]
    compressed_data: Vec<u8>,
}

impl CompressedLogicalMetricBatch {
    fn from_payload(payload: MetricSeriesPayload) -> Self {
        let (version, metadata, compressed_data) = payload.into_parts();
        Self {
            version,
            event_count: metadata.event_count(),
            data_point_count: metadata.data_point_count(),
            compressed_data: compressed_data.to_vec(),
        }
    }

    fn from_logical(logical: &LogicalMetricBatch) -> Result<Self, GenericError> {
        let serialized =
            rmp_serde::to_vec_named(logical).error_context("Failed to serialize a logical metrics retry batch.")?;
        let compressed_data = zstd::stream::encode_all(serialized.as_slice(), LOGICAL_METRIC_COMPRESSION_LEVEL)
            .error_context("Failed to compress a logical metrics retry batch.")?;
        Ok(Self {
            version: LOGICAL_METRIC_BATCH_VERSION,
            event_count: logical.series().len(),
            data_point_count: logical.point_count(),
            compressed_data,
        })
    }

    fn decode(&self) -> Result<LogicalMetricBatch, GenericError> {
        if self.version != LOGICAL_METRIC_BATCH_VERSION {
            return Err(generic_error!(
                "Unsupported logical metric batch version {}.",
                self.version
            ));
        }
        let serialized = zstd::stream::decode_all(self.compressed_data.as_slice())
            .error_context("Failed to decompress a logical metric batch.")?;
        rmp_serde::from_slice(&serialized).error_context("Failed to deserialize a logical metric batch.")
    }

    fn metadata(&self) -> Metadata {
        Metadata::from_event_and_data_point_count(self.event_count, self.data_point_count)
    }
}

impl EventContainer for CompressedLogicalMetricBatch {
    fn event_count(&self) -> u64 {
        self.event_count as u64
    }

    fn data_point_count(&self) -> u64 {
        self.data_point_count as u64
    }
}

impl Retryable for CompressedLogicalMetricBatch {
    fn size_bytes(&self) -> u64 {
        (size_of::<Self>() + self.compressed_data.len()) as u64
    }
}

struct ActiveStream {
    stream_id: StreamId,
    outbound: mpsc::Sender<ProtoStatefulBatch>,
}

enum StatefulEvent {
    Opened {
        stream_id: StreamId,
        outbound: mpsc::Sender<ProtoStatefulBatch>,
    },
    Failed {
        stream_id: StreamId,
        error: MetaString,
    },
    Ack {
        stream_id: StreamId,
        batch_id: u64,
    },
    Timer(TimerKind),
}

struct StatefulMetricsTelemetry {
    encoded: Counter,
    retried: Counter,
    reconnects: Counter,
    shutdown_flush_errors: Counter,
}

impl StatefulMetricsTelemetry {
    fn new(builder: &MetricsBuilder, domain: &str) -> Self {
        let domain_tag = [("domain", domain.to_string())];
        Self {
            encoded: builder
                .register_counter_with_tags("stateful_metrics_logical_batches_encoded_total", domain_tag.clone()),
            retried: builder
                .register_counter_with_tags("stateful_metrics_logical_batches_requeued_total", domain_tag.clone()),
            reconnects: builder.register_counter_with_tags("stateful_metrics_reconnects_total", domain_tag.clone()),
            shutdown_flush_errors: builder
                .register_counter_with_tags("stateful_metrics_shutdown_flush_errors_total", domain_tag),
        }
    }
}

struct StatefulMetricsEndpoint {
    endpoint: ResolvedEndpoint,
    api_key: MetaString,
    endpoint_domain: String,
    client: StatefulIntakeClient<Channel>,
    core: StatefulMetricsCore,
    encoder: MetricBatchEncoder<ZstdBatchCompressor>,
    outbound_capacity: usize,
    active: Option<ActiveStream>,
    events_tx: mpsc::UnboundedSender<StatefulEvent>,
    events_rx: mpsc::UnboundedReceiver<StatefulEvent>,
    fresh: VecDeque<CompressedLogicalMetricBatch>,
    max_fresh: usize,
    retry: RetryQueue<CompressedLogicalMetricBatch>,
    fresh_since_retry: usize,
    retries_since_disk: usize,
    reconnect_failures: u32,
    delivery_metadata: VecDeque<(u64, Metadata)>,
    telemetry: ComponentTelemetry,
    stateful_telemetry: StatefulMetricsTelemetry,
}

impl StatefulMetricsEndpoint {
    async fn new(
        context: ComponentContext, config: ForwarderConfiguration, endpoint: ResolvedEndpoint,
        telemetry: ComponentTelemetry, metrics_builder: &MetricsBuilder,
    ) -> Result<Self, GenericError> {
        let mut endpoint = endpoint;
        let api_key = MetaString::from(endpoint.api_key());
        let authority = endpoint.endpoint().authority();
        let grpc_endpoint = format!("{}://{}", endpoint.endpoint().scheme(), authority);
        let channel = Channel::from_shared(grpc_endpoint)
            .error_context("Failed to build Foldspace metrics endpoint.")?
            .connect_timeout(config.request_timeout())
            .connect_lazy();
        let endpoint_domain = endpoint.endpoint().origin().ascii_serialization();
        let queue_id = format!("{}-stateful-metrics", generate_retry_queue_id(context, &endpoint));
        let mut retry = RetryQueue::new(queue_id.clone(), config.retry().queue_max_size_bytes())
            .with_flush_to_disk_mem_ratio(config.retry().flush_to_disk_mem_ratio());
        if config.retry().storage_max_size_bytes() > 0 {
            retry = retry
                .with_disk_persistence(PersistedQueueArgs {
                    root_path: PathBuf::from(config.retry().storage_path()),
                    max_on_disk_bytes: config.retry().storage_max_size_bytes(),
                    storage_max_disk_ratio: config.retry().storage_max_disk_ratio(),
                    disk_usage_retriever: Arc::new(DiskUsageRetrieverImpl::new(PathBuf::from(
                        config.retry().storage_path(),
                    ))),
                    max_age_days: config.retry().outdated_file_in_days(),
                })
                .await
                .unwrap_or_else(|error| {
                    warn!(%error, "Failed to initialize stateful metrics disk retry queue.");
                    RetryQueue::new(queue_id, config.retry().queue_max_size_bytes())
                        .with_flush_to_disk_mem_ratio(config.retry().flush_to_disk_mem_ratio())
                });
        }

        let (events_tx, events_rx) = mpsc::unbounded_channel();
        let max_inflight_payloads = config.stateful_metrics_max_inflight_payloads().max(1);
        let core = StatefulMetricsCore::new(CoreConfig {
            sender: SenderConfig {
                max_inflight_payloads,
                ..SenderConfig::default()
            },
            ..CoreConfig::default()
        });
        Ok(Self {
            endpoint,
            api_key,
            endpoint_domain: endpoint_domain.clone(),
            client: StatefulIntakeClient::new(channel),
            core,
            encoder: MetricBatchEncoder::new(ZstdBatchCompressor::default()),
            outbound_capacity: max_inflight_payloads.saturating_add(1),
            active: None,
            events_tx,
            events_rx,
            fresh: VecDeque::with_capacity(config.endpoint_buffer_size()),
            max_fresh: config.endpoint_buffer_size(),
            retry,
            fresh_since_retry: 0,
            retries_since_disk: 0,
            reconnect_failures: 0,
            delivery_metadata: VecDeque::new(),
            telemetry,
            stateful_telemetry: StatefulMetricsTelemetry::new(metrics_builder, &endpoint_domain),
        })
    }

    async fn run(mut self, mut payloads_rx: mpsc::Receiver<MetricSeriesPayload>) {
        let effects = self.core.start();
        self.execute(effects).await;

        loop {
            self.schedule_ready().await;
            tokio::select! {
                biased;

                Some(event) = self.events_rx.recv() => self.handle_event(event).await,
                payload = payloads_rx.recv() => match payload {
                    Some(payload) => self.enqueue_fresh(CompressedLogicalMetricBatch::from_payload(payload)).await,
                    None => break,
                },
            }
        }

        self.preserve_shutdown().await;
    }

    async fn enqueue_fresh(&mut self, batch: CompressedLogicalMetricBatch) {
        if self.max_fresh == 0 {
            self.enqueue_retry(batch).await;
            return;
        }
        if self.fresh.len() == self.max_fresh {
            let oldest = self.fresh.pop_front().expect("fresh queue is at capacity");
            self.enqueue_retry(oldest).await;
        }
        self.fresh.push_back(batch);
    }

    async fn schedule_ready(&mut self) {
        self.refresh_destination().await;
        while self.core.has_send_capacity() {
            let retry_available = !self.retry.is_empty();
            let choice = choose_queue(!self.fresh.is_empty(), retry_available, self.fresh_since_retry);
            let Some(choice) = choice else {
                return;
            };
            let queued = match choice {
                QueueChoice::Fresh => {
                    self.fresh_since_retry += 1;
                    self.fresh.pop_front()
                }
                QueueChoice::Retry => {
                    self.fresh_since_retry = 0;
                    match self.pop_retry().await {
                        Ok(queued) => queued,
                        Err(error) => {
                            error!(%error, "Failed to dequeue a logical metrics retry batch.");
                            return;
                        }
                    }
                }
            };
            let Some(queued) = queued else {
                continue;
            };
            let metadata = queued.metadata();
            let logical = match queued.decode() {
                Ok(logical) => logical,
                Err(error) => {
                    self.telemetry
                        .track_permanently_failed_transaction(&metadata, None, &self.endpoint_domain);
                    error!(%error, "Dropping unreadable logical metric retry batch.");
                    continue;
                }
            };
            let effects = match self.core.push_batch(logical) {
                Ok(effects) => effects,
                Err(error) => {
                    self.enqueue_retry(
                        CompressedLogicalMetricBatch::from_logical(&error.into_batch()).unwrap_or(queued),
                    )
                    .await;
                    return;
                }
            };
            if let Some(batch_id) = effects.iter().find_map(|effect| match effect {
                MetricEffect::SendBatch { batch } if batch.batch_id != 0 => Some(batch.batch_id),
                _ => None,
            }) {
                self.delivery_metadata.push_back((batch_id, metadata));
                self.stateful_telemetry.encoded.increment(1);
            }
            self.execute(effects).await;
        }
    }

    async fn refresh_destination(&mut self) {
        let api_key = MetaString::from(self.endpoint.api_key());
        if api_key == self.api_key {
            return;
        }

        self.api_key = api_key;
        self.active = None;
        self.delivery_metadata.clear();
        let effects = self.core.reset_destination_state();
        self.execute(effects).await;
    }

    async fn pop_retry(&mut self) -> Result<Option<CompressedLogicalMetricBatch>, GenericError> {
        let force_disk = self.retry.has_persisted_entries() && self.retries_since_disk >= RETRIES_BEFORE_DISK;
        let result = if force_disk {
            self.retries_since_disk = 0;
            self.retry.pop_persisted().await
        } else {
            self.retries_since_disk += 1;
            self.retry.pop().await
        };
        result
    }

    async fn enqueue_retry(&mut self, batch: CompressedLogicalMetricBatch) {
        let metadata = batch.metadata();
        match self.retry.push(batch).await {
            Ok(result) => {
                self.stateful_telemetry.retried.increment(1);
                track_queue_drops(&self.telemetry, &self.endpoint_domain, result);
            }
            Err(error) => {
                self.telemetry
                    .track_permanently_failed_transaction(&metadata, None, &self.endpoint_domain);
                error!(%error, "Logical metric batch exceeded retry limits and was dropped.");
            }
        }
    }

    async fn handle_event(&mut self, event: StatefulEvent) {
        match event {
            StatefulEvent::Opened { stream_id, outbound } => {
                self.reconnect_failures = 0;
                self.active = Some(ActiveStream { stream_id, outbound });
                let effects = self.core.handle_stream_opened(stream_id);
                self.execute(effects).await;
            }
            StatefulEvent::Ack { stream_id, batch_id } => {
                let effects = self.core.handle_ack(stream_id, batch_id);
                if effects.is_empty() && batch_id != 0 {
                    if let Some((expected, metadata)) = self.delivery_metadata.pop_front() {
                        if expected == batch_id {
                            self.telemetry
                                .track_successful_transaction(&metadata, &self.endpoint_domain);
                        }
                    }
                }
                self.execute(effects).await;
            }
            StatefulEvent::Failed { stream_id, error } => {
                if self.core.current_stream_id() != Some(stream_id) {
                    return;
                }
                self.reconnect_failures = self.reconnect_failures.saturating_add(1);
                self.stateful_telemetry.reconnects.increment(1);
                let effects = self.fail_stream(stream_id, StreamError::new(error.as_ref()));
                self.execute(effects).await;
            }
            StatefulEvent::Timer(kind) => {
                let effects = self.core.handle_timer(kind);
                self.execute(effects).await;
            }
        }
    }

    async fn execute(&mut self, effects: Vec<MetricEffect>) {
        let mut effects = VecDeque::from(effects);
        while let Some(effect) = effects.pop_front() {
            match effect {
                MetricEffect::OpenStream { stream_id } => {
                    if let Err(error) = self.open_stream(stream_id) {
                        effects.extend(self.fail_stream(stream_id, StreamError::new(error.to_string())));
                    }
                }
                MetricEffect::SendBatch { batch } => {
                    let stream_id = batch.stream;
                    let encoded = match self.encoder.encode(batch) {
                        Ok(encoded) => encoded,
                        Err(error) => {
                            effects.extend(self.fail_stream(
                                stream_id,
                                StreamError::new(format!("failed to encode Foldspace metric batch: {error:?}")),
                            ));
                            continue;
                        }
                    };
                    let Some(active) = self.active.as_mut().filter(|active| active.stream_id == stream_id) else {
                        effects.extend(
                            self.fail_stream(stream_id, StreamError::new("Foldspace metric stream is unavailable")),
                        );
                        continue;
                    };
                    self.telemetry.bytes_sent().increment(encoded.data.len() as u64);
                    if let Err(error) = active.outbound.send(encoded).await {
                        effects.extend(self.fail_stream(
                            stream_id,
                            StreamError::new(format!("failed to transmit Foldspace metric batch: {error}")),
                        ));
                    }
                }
                MetricEffect::CloseStream { stream_id } => {
                    if self.active.as_ref().is_some_and(|active| active.stream_id == stream_id) {
                        self.active = None;
                    }
                }
                MetricEffect::ReturnUnacknowledged { batches } => {
                    for logical in batches {
                        match CompressedLogicalMetricBatch::from_logical(&logical) {
                            Ok(batch) => self.enqueue_retry(batch).await,
                            Err(error) => {
                                let metadata = Metadata::from_event_and_data_point_count(
                                    logical.series().len(),
                                    logical.point_count(),
                                );
                                self.telemetry.track_permanently_failed_transaction(
                                    &metadata,
                                    None,
                                    &self.endpoint_domain,
                                );
                                error!(%error, "Failed to preserve an unacknowledged logical metric batch.");
                            }
                        }
                    }
                }
                MetricEffect::ScheduleTimer { timer } => {
                    let events_tx = self.events_tx.clone();
                    let random = rand::rng().random::<f64>();
                    let delay = jittered_backoff(timer.after, self.reconnect_failures, random);
                    tokio::spawn(async move {
                        tokio::time::sleep(delay).await;
                        let _ = events_tx.send(StatefulEvent::Timer(timer.kind));
                    });
                }
                MetricEffect::ReportError { error } => warn!(?error, "Foldspace metrics core reported an error."),
            }
        }
    }

    fn fail_stream(&mut self, stream_id: StreamId, error: StreamError) -> Vec<MetricEffect> {
        self.active = None;
        self.delivery_metadata.clear();
        self.core.handle_stream_error(stream_id, error)
    }

    fn open_stream(&mut self, stream_id: StreamId) -> Result<(), GenericError> {
        let (outbound, receiver) = mpsc::channel(self.outbound_capacity);
        let mut request = TonicRequest::new(ReceiverStream::new(receiver));
        let api_key = MetadataValue::try_from(self.api_key.as_ref())
            .error_context("Foldspace metrics API key is not valid gRPC metadata.")?;
        request.metadata_mut().insert("dd-api-key", api_key);
        request
            .metadata_mut()
            .insert("dd-content-encoding", MetadataValue::from_static("zstd"));
        request.metadata_mut().insert(
            "dd-state-request-bytes",
            MetadataValue::try_from(STATE_REQUEST_BYTES.to_string())
                .error_context("Foldspace metric state request limit is invalid gRPC metadata.")?,
        );
        let mut client = self.client.clone();
        let events_tx = self.events_tx.clone();
        tokio::spawn(async move {
            match client.stateful_stream(request).await {
                Ok(response) => {
                    if events_tx.send(StatefulEvent::Opened { stream_id, outbound }).is_ok() {
                        spawn_response_reader(stream_id, response.into_inner(), events_tx);
                    }
                }
                Err(error) => {
                    let _ = events_tx.send(StatefulEvent::Failed {
                        stream_id,
                        error: MetaString::from(format!("failed to open Foldspace metrics stream: {error}")),
                    });
                }
            }
        });
        Ok(())
    }

    async fn preserve_shutdown(&mut self) {
        if let Some(stream_id) = self.core.current_stream_id() {
            let effects = self
                .core
                .handle_stream_error(stream_id, StreamError::new("graceful shutdown"));
            for effect in effects {
                if let MetricEffect::ReturnUnacknowledged { batches } = effect {
                    for logical in batches {
                        match CompressedLogicalMetricBatch::from_logical(&logical) {
                            Ok(batch) => self.enqueue_retry(batch).await,
                            Err(error) => {
                                let metadata = Metadata::from_event_and_data_point_count(
                                    logical.series().len(),
                                    logical.point_count(),
                                );
                                self.telemetry.track_permanently_failed_transaction(
                                    &metadata,
                                    None,
                                    &self.endpoint_domain,
                                );
                                error!(%error, "Failed to preserve a logical metric batch during shutdown.");
                            }
                        }
                    }
                }
            }
        }
        while let Some(batch) = self.fresh.pop_front() {
            self.enqueue_retry(batch).await;
        }
        match std::mem::replace(&mut self.retry, RetryQueue::new("shutdown".to_string(), 0))
            .flush()
            .await
        {
            Ok(result) => track_queue_drops(&self.telemetry, &self.endpoint_domain, result),
            Err(error) => {
                self.stateful_telemetry.shutdown_flush_errors.increment(1);
                error!(%error, "Failed to flush logical metric retries during shutdown.");
            }
        }
    }
}

fn endpoint_uses_stateful_series(
    config: &ForwarderConfiguration, route: EndpointRoute, endpoint: &ResolvedEndpoint,
) -> bool {
    if config.compressor_disables_metrics_v3() {
        return false;
    }
    let metrics_primary_v3_override = (route == EndpointRoute::MetricsPrimary)
        .then(|| config.opw_metrics_v3_series_override())
        .flatten();
    let serializer_v3_configured_endpoint =
        (route == EndpointRoute::MetricsPrimary).then(|| config.primary_configured_endpoint());
    EndpointV3Settings::from_v3_config(V3EndpointConfig {
        configured_endpoint: endpoint.configured_endpoint(),
        resolved_endpoint: endpoint.endpoint(),
        serializer_v3_configured_endpoint: serializer_v3_configured_endpoint.as_deref(),
        series_config: config.use_v3_api_series(),
        metrics_primary_v3_override,
        serializer_v3_series_endpoints: &config.v3_api().series.endpoints,
        serializer_v3_sketches_endpoints: &config.v3_api().sketches.endpoints,
        series_validate: config.v3_api().series.validate,
        sketches_validate: config.v3_api().sketches.validate,
        series_shadow_sites: &config.v3_api().series.shadow_sites,
    })
    .should_receive_payload(Some(MetricsPayloadInfo::v3_series()))
}

fn metadata_from_payload(metadata: &PayloadMetadata) -> Metadata {
    Metadata::from_event_and_data_point_count(metadata.event_count(), metadata.data_point_count())
}

fn spawn_response_reader(
    stream_id: StreamId, mut responses: tonic::Streaming<BatchStatus>, events_tx: mpsc::UnboundedSender<StatefulEvent>,
) {
    tokio::spawn(async move {
        loop {
            match responses.message().await {
                Ok(Some(status)) if status.status == i32::from(batch_status::Status::Ok) => {
                    if events_tx
                        .send(StatefulEvent::Ack {
                            stream_id,
                            batch_id: u64::from(status.batch_id),
                        })
                        .is_err()
                    {
                        return;
                    }
                }
                Ok(Some(status)) => {
                    let _ = events_tx.send(StatefulEvent::Failed {
                        stream_id,
                        error: MetaString::from(format!(
                            "Foldspace intake rejected metric batch {} with status {}",
                            status.batch_id, status.status
                        )),
                    });
                    return;
                }
                Ok(None) => {
                    let _ = events_tx.send(StatefulEvent::Failed {
                        stream_id,
                        error: MetaString::from_static("Foldspace metrics response stream closed"),
                    });
                    return;
                }
                Err(error) => {
                    let _ = events_tx.send(StatefulEvent::Failed {
                        stream_id,
                        error: MetaString::from(format!("Foldspace metrics acknowledgement failed: {error}")),
                    });
                    return;
                }
            }
        }
    });
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum QueueChoice {
    Fresh,
    Retry,
}

fn choose_queue(fresh: bool, retry: bool, fresh_since_retry: usize) -> Option<QueueChoice> {
    match (fresh, retry) {
        (false, false) => None,
        (true, false) => Some(QueueChoice::Fresh),
        (false, true) => Some(QueueChoice::Retry),
        (true, true) if fresh_since_retry >= FRESH_BURST_LIMIT => Some(QueueChoice::Retry),
        (true, true) => Some(QueueChoice::Fresh),
    }
}

fn jittered_backoff(base: Duration, failures: u32, random: f64) -> Duration {
    let exponent = failures.saturating_sub(1).min(16);
    let exponential = base.saturating_mul(1_u32 << exponent).min(MAX_RECONNECT_BACKOFF);
    let factor = (1.0 - RECONNECT_JITTER_FRACTION) + random.clamp(0.0, 1.0) * (RECONNECT_JITTER_FRACTION * 2.0);
    exponential.mul_f64(factor).min(MAX_RECONNECT_BACKOFF)
}

mod compressed_bytes {
    use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
    use serde::{Deserialize as _, Deserializer, Serializer};

    pub(super) fn serialize<S>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&BASE64.encode(bytes))
    }

    pub(super) fn deserialize<'de, D>(deserializer: D) -> Result<Vec<u8>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let encoded = String::deserialize(deserializer)?;
        BASE64.decode(encoded).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use foldspace_core::{LogicalMetricSeries, MetricPoint, MetricSeriesType};
    use tempfile::tempdir;

    use super::*;

    fn logical_batch() -> LogicalMetricBatch {
        LogicalMetricBatch::new(vec![LogicalMetricSeries::new(
            "requests",
            MetricSeriesType::Count,
            vec![MetricPoint::new(10, 2.0)],
        )])
    }

    #[test]
    fn logical_retry_round_trip_does_not_require_foldspace_history() {
        let logical = logical_batch();
        let compressed = CompressedLogicalMetricBatch::from_logical(&logical).unwrap();

        assert_eq!(compressed.decode().unwrap(), logical);
    }

    #[tokio::test]
    async fn logical_retry_memory_limit_drops_oldest_with_counts() {
        let first = CompressedLogicalMetricBatch::from_logical(&logical_batch()).unwrap();
        let second = first.clone();
        let mut retry = RetryQueue::new("metrics".to_string(), first.size_bytes());

        assert!(!retry.push(first).await.unwrap().had_drops());
        let result = retry.push(second).await.unwrap();

        assert_eq!(result.items_dropped, 1);
        assert_eq!(result.events_dropped, 1);
        assert_eq!(result.data_points_dropped, 1);
        assert_eq!(retry.len(), 1);
    }

    #[test]
    fn unsupported_logical_retry_version_is_rejected() {
        let mut compressed = CompressedLogicalMetricBatch::from_logical(&logical_batch()).unwrap();
        compressed.version += 1;

        assert!(compressed.decode().is_err());
    }

    #[tokio::test]
    async fn disk_restart_recovers_versioned_logical_batch() {
        let root = tempdir().unwrap();
        let args = PersistedQueueArgs {
            root_path: root.path().to_path_buf(),
            max_on_disk_bytes: 64 * 1024,
            storage_max_disk_ratio: 1.0,
            disk_usage_retriever: Arc::new(DiskUsageRetrieverImpl::new(root.path().to_path_buf())),
            max_age_days: 1,
        };
        let mut retry: RetryQueue<CompressedLogicalMetricBatch> = RetryQueue::new("metrics".to_string(), 64 * 1024)
            .with_disk_persistence(args.clone())
            .await
            .unwrap();
        let batch = CompressedLogicalMetricBatch::from_logical(&logical_batch()).unwrap();
        assert!(!retry.push(batch).await.unwrap().had_drops());
        assert!(!retry.flush().await.unwrap().had_drops());

        let mut restarted: RetryQueue<CompressedLogicalMetricBatch> = RetryQueue::new("metrics".to_string(), 64 * 1024)
            .with_disk_persistence(args)
            .await
            .unwrap();
        let newer = LogicalMetricBatch::new(vec![LogicalMetricSeries::new(
            "newer",
            MetricSeriesType::Gauge,
            vec![MetricPoint::new(11, 3.0)],
        )]);
        assert!(!restarted
            .push(CompressedLogicalMetricBatch::from_logical(&newer).unwrap())
            .await
            .unwrap()
            .had_drops());
        let recovered = restarted.pop_persisted().await.unwrap().unwrap();

        assert_eq!(recovered.decode().unwrap(), logical_batch());
        assert_eq!(restarted.pop().await.unwrap().unwrap().decode().unwrap(), newer);
    }

    #[test]
    fn fresh_batches_are_prioritized_but_retry_makes_progress() {
        for sent in 0..FRESH_BURST_LIMIT {
            assert_eq!(choose_queue(true, true, sent), Some(QueueChoice::Fresh));
        }
        assert_eq!(choose_queue(true, true, FRESH_BURST_LIMIT), Some(QueueChoice::Retry));
    }

    #[test]
    fn reconnect_backoff_is_exponential_jittered_and_bounded() {
        let base = Duration::from_millis(250);
        assert_eq!(jittered_backoff(base, 1, 0.5), base);
        assert_eq!(jittered_backoff(base, 2, 0.5), Duration::from_millis(500));
        assert!(jittered_backoff(base, 32, 1.0) <= MAX_RECONNECT_BACKOFF);
    }
}
