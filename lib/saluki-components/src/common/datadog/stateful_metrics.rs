//! Endpoint-scoped Foldspace destination for metric series.

use std::{
    collections::{HashMap, VecDeque},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};

use agent_data_plane_config::{shared::SharedConfiguration, Live};
use async_trait::async_trait;
use foldspace_core::{
    proto::stateful::{
        batch_status, stateful_intake_client::StatefulIntakeClient, BatchStatus, StatefulBatch as ProtoStatefulBatch,
        StatelessRequest,
    },
    CoreConfig, LogicalMetricBatch, LogicalMetricSeries, MetricBatchEncoder, MetricEffect, MetricFailureAction,
    MetricOrigin as FoldspaceMetricOrigin, MetricPoint, MetricResource, MetricSeriesEncoder, MetricSeriesType,
    MetricStatefulBatch, MetricStreamFailure, MetricStreamFailureKind, MetricTagSet, SenderConfig, StatefulMetricsCore,
    StreamId, TimerKind, ZstdBatchCompressor,
};
use metrics::Counter;
use saluki_common::collections::FastHashSet;
use saluki_context::tags::SharedTagSet;
use saluki_core::{
    accounting::{MemoryBounds, MemoryBoundsBuilder, UsageExpr},
    components::{destinations::*, BuildContext, ComponentContext},
    data_model::event::{
        metric::{Metric, MetricOrigin, MetricValues},
        EventType,
    },
    observability::ComponentMetricsExt as _,
};
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use saluki_io::net::util::retry::{
    DiskUsageRetrieverImpl, EventContainer, ExponentialBackoff, PersistedQueueArgs, RetryQueue, Retryable,
};
use saluki_metrics::MetricsBuilder;
use serde::{Deserialize, Serialize};
use stringtheory::MetaString;
use tokio::{select, sync::mpsc, time::sleep};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{metadata::MetadataValue, transport::Channel, Code, Request as TonicRequest};
use tracing::{error, warn};

use super::{
    api_key::{ApiKeyChanges, ApiKeyRefresher, ApiKeyView, LiveApiKeys},
    config::ForwarderConfiguration,
    endpoints::{EndpointRoute, EndpointV3Settings, ResolvedEndpoint, SingleDestination, V3EndpointConfig},
    io::{generate_retry_queue_id, should_route_to_endpoint, track_queue_drops},
    metrics::{
        emittable_scalar_point, has_emittable_scalar_point, is_foldspace_series, is_v3_series_device_tag,
        is_v3_series_resource_tag,
    },
    protocol::MetricsPayloadInfo,
    telemetry::ComponentTelemetry,
    transaction::Metadata,
};
use crate::encoders::DatadogMetricsConfiguration;

const STATE_REQUEST_BYTES: u64 = 5 * 1024 * 1024;
const LOGICAL_METRIC_BATCH_VERSION: u16 = 1;
const LOGICAL_METRIC_COMPRESSION_LEVEL: i32 = 3;
const FRESH_BURST_LIMIT: usize = 8;
const RETRIES_BEFORE_DISK: usize = 4;

/// Configuration for the Foldspace metrics destination.
pub struct StatefulMetricsDestinationConfiguration {
    config: ForwarderConfiguration,
    api_keys: LiveApiKeys,
    batching: StatefulMetricsBatchingConfiguration,
}

#[derive(Clone)]
struct StatefulMetricsBatchingConfiguration {
    additional_tags: SharedTagSet,
    max_metrics_per_batch: usize,
    max_points_per_batch: usize,
    max_inflight_payloads: usize,
    flush_timeout: Duration,
}

impl StatefulMetricsDestinationConfiguration {
    /// Creates a Foldspace destination when authoritative V3 series use stateful transport.
    pub fn from_configuration(
        shared: &SharedConfiguration, metrics_config: &DatadogMetricsConfiguration, api_key: Live<String>,
        additional_endpoints: Live<HashMap<String, Vec<String>>>,
    ) -> Result<Option<Self>, GenericError> {
        if !metrics_config.stateful_metrics_enabled()? {
            return Ok(None);
        }

        Ok(Some(Self {
            config: ForwarderConfiguration::from_configuration(shared),
            api_keys: LiveApiKeys {
                primary: Some(ApiKeyView::Required(api_key)),
                additional: Some(additional_endpoints),
            },
            batching: StatefulMetricsBatchingConfiguration::new(shared, metrics_config),
        }))
    }

    /// Creates a Foldspace destination for one endpoint override.
    pub fn for_endpoint_override(
        shared: &SharedConfiguration, metrics_config: &DatadogMetricsConfiguration, dd_url: String, api_key: String,
        api_key_view: Live<Option<String>>,
    ) -> Result<Option<Self>, GenericError> {
        if !metrics_config.stateful_metrics_enabled()? {
            return Ok(None);
        }

        let destination = SingleDestination {
            url: dd_url,
            api_key,
            accepts_v3_series: true,
        };
        Ok(Some(Self {
            config: ForwarderConfiguration::for_single_destination(shared, &destination),
            api_keys: LiveApiKeys {
                primary: Some(ApiKeyView::Optional(api_key_view)),
                additional: None,
            },
            batching: StatefulMetricsBatchingConfiguration::new(shared, metrics_config),
        }))
    }
}

impl StatefulMetricsBatchingConfiguration {
    fn new(shared: &SharedConfiguration, metrics_config: &DatadogMetricsConfiguration) -> Self {
        Self {
            additional_tags: metrics_config.stateful_additional_tags(),
            max_metrics_per_batch: metrics_config.stateful_max_metrics_per_batch(),
            max_points_per_batch: metrics_config.stateful_max_points_per_batch(),
            max_inflight_payloads: shared.metrics_encoding.stateful.max_inflight_payloads.max(1),
            flush_timeout: metrics_config.stateful_flush_timeout(),
        }
    }
}

#[async_trait]
impl DestinationBuilder for StatefulMetricsDestinationConfiguration {
    fn input_event_type(&self) -> EventType {
        EventType::Metric
    }

    async fn build(&self, context: BuildContext) -> Result<Box<dyn Destination + Send>, GenericError> {
        Ok(Box::new(StatefulMetricsDestination {
            context: context.component_context().clone(),
            config: self.config.clone(),
            api_keys: self.api_keys.clone(),
            batching: self.batching.clone(),
        }))
    }
}

impl MemoryBounds for StatefulMetricsDestinationConfiguration {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        builder
            .minimum()
            .with_single_value::<StatefulMetricsDestination>("component struct")
            .with_array::<Metric>("logical metrics batch", self.batching.max_metrics_per_batch);
        builder.firm().with_expr(UsageExpr::config(
            "stateful metrics retry queue",
            self.config.retry().queue_max_size_bytes() as usize,
        ));
    }
}

struct StatefulMetricsDestination {
    context: ComponentContext,
    config: ForwarderConfiguration,
    api_keys: LiveApiKeys,
    batching: StatefulMetricsBatchingConfiguration,
}

#[async_trait]
impl Destination for StatefulMetricsDestination {
    async fn run(self: Box<Self>, mut context: DestinationContext) -> Result<(), GenericError> {
        let Self {
            context: component_context,
            config,
            api_keys,
            batching,
        } = *self;
        let metrics_builder = MetricsBuilder::from_component_context(&component_context);
        let telemetry = ComponentTelemetry::from_builder(&metrics_builder);
        let endpoints = config.build_routable_endpoints()?;
        let api_key_refresher = ApiKeyRefresher::new(&endpoints, &api_keys)
            .expect("stateful metrics destinations always provide a live primary API key");
        let api_key_changes = api_key_refresher.changes();
        api_key_refresher.spawn();
        let has_metrics_primary = endpoints
            .iter()
            .any(|endpoint| endpoint.route() == EndpointRoute::MetricsPrimary);
        let mut endpoint_senders = Vec::new();
        let mut endpoint_tasks = Vec::new();

        for routable in endpoints {
            let (route, endpoint) = routable.into_parts();
            if !should_route_to_endpoint(true, has_metrics_primary, route)
                || !endpoint_uses_stateful_series(&config, route, &endpoint)
            {
                continue;
            }

            let (endpoint_tx, endpoint_rx) = mpsc::channel(config.endpoint_buffer_size());
            let sender = StatefulMetricsEndpoint::new(
                component_context.clone(),
                config.clone(),
                endpoint,
                telemetry.clone(),
                &metrics_builder,
                batching.clone(),
                api_key_changes.clone(),
            )
            .await?;
            endpoint_senders.push(endpoint_tx);
            endpoint_tasks.push(tokio::spawn(sender.run(endpoint_rx)));
        }

        let mut health = context.take_health_handle();
        health.mark_ready();
        loop {
            select! {
                _ = health.live() => continue,
                maybe_events = context.events().next() => match maybe_events {
                    Some(events) => {
                        let mut metrics = Vec::new();
                        for metric in events.into_iter().filter_map(|event| event.try_into_metric()) {
                            if !is_foldspace_series(&metric) {
                                continue;
                            }
                            if metric.values().len() > batching.max_points_per_batch
                                || !has_emittable_scalar_point(&metric)
                            {
                                telemetry.events_dropped_encoder().increment(1);
                                continue;
                            }
                            metrics.push(metric);
                        }
                        if metrics.is_empty() {
                            continue;
                        }

                        let metadata = metadata_from_metrics(&metrics);
                        let metrics: Arc<[Metric]> = metrics.into();
                        endpoint_senders.retain(|sender| !sender.is_closed());
                        if endpoint_senders.is_empty() {
                            telemetry.track_permanently_failed_transaction(&metadata, None, "stateful-metrics");
                            continue;
                        }
                        for sender in &endpoint_senders {
                            if sender.send(Arc::clone(&metrics)).await.is_err() {
                                telemetry.track_permanently_failed_transaction(
                                    &metadata,
                                    None,
                                    "stateful-metrics",
                                );
                            }
                        }
                    }
                    None => break,
                },
            }
        }

        drop(endpoint_senders);
        for task in endpoint_tasks {
            if let Err(error) = task.await {
                error!(%error, "Stateful metrics endpoint task panicked.");
            }
        }
        Ok(())
    }
}

/// Serialized form used only by the bounded retry queue and disk persistence.
#[derive(Clone, Debug, Deserialize, Serialize)]
struct CompressedLogicalMetricBatch {
    version: u16,
    event_count: usize,
    data_point_count: usize,
    #[serde(with = "compressed_bytes")]
    compressed_data: Vec<u8>,
}

impl CompressedLogicalMetricBatch {
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

#[derive(Debug)]
enum StatefulEvent {
    Opened {
        stream_id: StreamId,
        outbound: mpsc::Sender<ProtoStatefulBatch>,
    },
    Failed {
        stream_id: StreamId,
        failure: MetricStreamFailure,
    },
    Ack {
        stream_id: StreamId,
        batch_id: u64,
    },
    Timer {
        kind: TimerKind,
        stream_id: Option<StreamId>,
    },
    RotateStream(StreamId),
    StatelessRetryReady,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DeliveryMode {
    Stateful,
    Stateless,
    WaitingForCredentials,
    Suspended,
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

struct LogicalMetricBatcher {
    additional_tags: SharedTagSet,
    max_metrics_per_batch: usize,
    max_points_per_batch: usize,
    pending_series: Vec<LogicalMetricSeries>,
    pending_points: usize,
}

impl LogicalMetricBatcher {
    fn new(additional_tags: SharedTagSet, max_metrics_per_batch: usize, max_points_per_batch: usize) -> Self {
        Self {
            additional_tags,
            max_metrics_per_batch,
            max_points_per_batch,
            pending_series: Vec::with_capacity(max_metrics_per_batch),
            pending_points: 0,
        }
    }

    fn push(&mut self, metric: &Metric) -> (Vec<LogicalMetricBatch>, bool) {
        let Some(series) = logical_series_from_metric(metric, &self.additional_tags) else {
            return (Vec::new(), false);
        };
        if series.points().is_empty() {
            return (Vec::new(), false);
        }

        let point_count = series.points().len();
        if point_count > self.max_points_per_batch {
            return (Vec::new(), false);
        }
        let metric_limit_reached = self.pending_series.len() >= self.max_metrics_per_batch;
        let point_limit_exceeded = self.pending_points.saturating_add(point_count) > self.max_points_per_batch;
        let mut batches = Vec::new();
        if !self.pending_series.is_empty() && (metric_limit_reached || point_limit_exceeded) {
            batches.extend(self.take());
        }

        let started_batch = self.pending_series.is_empty();
        self.pending_points = self.pending_points.saturating_add(point_count);
        self.pending_series.push(series);
        if self.pending_series.len() >= self.max_metrics_per_batch || self.pending_points >= self.max_points_per_batch {
            batches.extend(self.take());
            return (batches, false);
        }

        (batches, started_batch)
    }

    fn take(&mut self) -> Option<LogicalMetricBatch> {
        if self.pending_series.is_empty() {
            return None;
        }

        self.pending_points = 0;
        Some(LogicalMetricBatch::new(std::mem::take(&mut self.pending_series)))
    }

    fn has_pending(&self) -> bool {
        !self.pending_series.is_empty()
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
    api_key_changes: ApiKeyChanges,
    request_timeout: Duration,
    stream_lifetime: Duration,
    delivery_mode: DeliveryMode,
    stateless_retry_ready: bool,
    reconnect_backoff: ExponentialBackoff,
    flush_timeout: Duration,
    batcher: LogicalMetricBatcher,
    fresh: VecDeque<LogicalMetricBatch>,
    max_fresh: usize,
    retry: RetryQueue<CompressedLogicalMetricBatch>,
    fresh_since_retry: usize,
    retries_since_disk: usize,
    reconnect_failures: u32,
    recovery_error_decrease_factor: Option<u32>,
    delivery_metadata: VecDeque<(u64, Metadata)>,
    telemetry: ComponentTelemetry,
    stateful_telemetry: StatefulMetricsTelemetry,
}

impl StatefulMetricsEndpoint {
    async fn new(
        context: ComponentContext, config: ForwarderConfiguration, endpoint: ResolvedEndpoint,
        telemetry: ComponentTelemetry, metrics_builder: &MetricsBuilder,
        batching: StatefulMetricsBatchingConfiguration, api_key_changes: ApiKeyChanges,
    ) -> Result<Self, GenericError> {
        let api_key = endpoint.api_key();
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
        let max_inflight_payloads = batching.max_inflight_payloads;
        let reconnect_backoff_base = config.retry().backoff_base();
        let reconnect_backoff = config.retry().to_exponential_backoff();
        let sender_config = SenderConfig {
            max_inflight_payloads,
            connection_timeout: config.request_timeout(),
            ..SenderConfig::default()
        };
        let stream_lifetime = sender_config.stream_lifetime;
        let core = StatefulMetricsCore::new(CoreConfig {
            sender: sender_config,
            reconnect_delay: reconnect_backoff_base,
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
            api_key_changes,
            request_timeout: config.request_timeout(),
            stream_lifetime,
            delivery_mode: DeliveryMode::Stateful,
            stateless_retry_ready: true,
            reconnect_backoff,
            flush_timeout: batching.flush_timeout,
            batcher: LogicalMetricBatcher::new(
                batching.additional_tags,
                batching.max_metrics_per_batch,
                batching.max_points_per_batch,
            ),
            fresh: VecDeque::with_capacity(config.endpoint_buffer_size()),
            max_fresh: config.endpoint_buffer_size(),
            retry,
            fresh_since_retry: 0,
            retries_since_disk: 0,
            reconnect_failures: 0,
            recovery_error_decrease_factor: config.retry().recovery_error_decrease_factor(),
            delivery_metadata: VecDeque::new(),
            telemetry,
            stateful_telemetry: StatefulMetricsTelemetry::new(metrics_builder, &endpoint_domain),
        })
    }

    async fn run(mut self, mut metrics_rx: mpsc::Receiver<Arc<[Metric]>>) {
        let effects = self.core.start();
        self.execute(effects).await;
        let flush_timeout = sleep(self.flush_timeout);
        tokio::pin!(flush_timeout);
        let mut flush_pending = false;

        loop {
            self.schedule_ready().await;
            select! {
                biased;

                Some(event) = self.events_rx.recv() => self.handle_event(event).await,
                _ = self.api_key_changes.changed() => self.refresh_destination().await,
                metrics = metrics_rx.recv() => match metrics {
                    Some(metrics) => {
                        let mut reset_flush_timeout = false;
                        for metric in metrics.iter() {
                            let (batches, started_batch) = self.batcher.push(metric);
                            reset_flush_timeout |= started_batch;
                            for batch in batches {
                                self.enqueue_fresh(batch).await;
                            }
                        }
                        flush_pending = self.batcher.has_pending();
                        if reset_flush_timeout && flush_pending {
                            flush_timeout.as_mut().reset(tokio::time::Instant::now() + self.flush_timeout);
                        }
                    }
                    None => break,
                },
                _ = &mut flush_timeout, if flush_pending => {
                    flush_pending = false;
                    if let Some(batch) = self.batcher.take() {
                        self.enqueue_fresh(batch).await;
                    }
                }
            }
        }

        self.preserve_shutdown().await;
    }

    async fn enqueue_fresh(&mut self, batch: LogicalMetricBatch) {
        if self.delivery_mode == DeliveryMode::Suspended {
            let metadata = metadata_from_logical(&batch);
            self.telemetry
                .track_permanently_failed_transaction(&metadata, None, &self.endpoint_domain);
            return;
        }
        if self.max_fresh == 0 {
            self.enqueue_logical_retry(batch).await;
            return;
        }
        if self.fresh.len() == self.max_fresh {
            let oldest = self.fresh.pop_front().expect("fresh queue is at capacity");
            self.enqueue_logical_retry(oldest).await;
        }
        self.fresh.push_back(batch);
    }

    async fn schedule_ready(&mut self) {
        self.refresh_destination().await;
        match self.delivery_mode {
            DeliveryMode::Stateful => self.schedule_stateful().await,
            DeliveryMode::Stateless if self.stateless_retry_ready => self.schedule_stateless().await,
            DeliveryMode::Stateless | DeliveryMode::WaitingForCredentials | DeliveryMode::Suspended => {}
        }
    }

    async fn schedule_stateful(&mut self) {
        while self.core.has_send_capacity() {
            let Some(logical) = self.next_logical().await else {
                return;
            };
            let metadata = metadata_from_logical(&logical);
            let effects = match self.core.push_batch(logical) {
                Ok(effects) => effects,
                Err(error) => {
                    self.enqueue_logical_retry(error.into_batch()).await;
                    return;
                }
            };
            if let Some(batch_id) = effects.iter().find_map(sent_batch_id) {
                self.delivery_metadata.push_back((batch_id, metadata));
                self.stateful_telemetry.encoded.increment(1);
            }
            self.execute(effects).await;
        }
    }

    async fn schedule_stateless(&mut self) {
        while self.delivery_mode == DeliveryMode::Stateless && self.stateless_retry_ready {
            let Some(logical) = self.next_logical().await else {
                return;
            };
            let metadata = metadata_from_logical(&logical);
            match self.send_stateless(&logical).await {
                Ok(bytes_sent) => {
                    self.record_delivery_success();
                    self.telemetry.bytes_sent().increment(bytes_sent as u64);
                    self.telemetry
                        .track_successful_transaction(&metadata, &self.endpoint_domain);
                }
                Err(failure) => {
                    let kind = failure.kind();
                    warn!(
                        ?kind,
                        error = failure.message(),
                        "Foldspace stateless metrics delivery failed."
                    );
                    match kind {
                        MetricStreamFailureKind::InvalidArgument | MetricStreamFailureKind::FailedPrecondition => {
                            self.telemetry
                                .track_permanently_failed_transaction(&metadata, None, &self.endpoint_domain);
                        }
                        MetricStreamFailureKind::Unauthenticated => {
                            self.delivery_mode = DeliveryMode::WaitingForCredentials;
                            self.enqueue_logical_retry(logical).await;
                        }
                        MetricStreamFailureKind::Unavailable
                        | MetricStreamFailureKind::DeadlineExceeded
                        | MetricStreamFailureKind::ResourceExhausted => {
                            self.enqueue_logical_retry(logical).await;
                            self.schedule_stateless_retry();
                        }
                    }
                    return;
                }
            }
        }
    }

    async fn next_logical(&mut self) -> Option<LogicalMetricBatch> {
        loop {
            let retry_available = !self.retry.is_empty();
            let choice = choose_queue(!self.fresh.is_empty(), retry_available, self.fresh_since_retry);
            let choice = choice?;
            let logical = match choice {
                QueueChoice::Fresh => {
                    self.fresh_since_retry += 1;
                    let Some(logical) = self.fresh.pop_front() else {
                        continue;
                    };
                    logical
                }
                QueueChoice::Retry => {
                    self.fresh_since_retry = 0;
                    match self.pop_retry().await {
                        Ok(Some(queued)) => {
                            let metadata = queued.metadata();
                            match queued.decode() {
                                Ok(logical) => logical,
                                Err(error) => {
                                    self.telemetry.track_permanently_failed_transaction(
                                        &metadata,
                                        None,
                                        &self.endpoint_domain,
                                    );
                                    error!(%error, "Dropping unreadable logical metric retry batch.");
                                    continue;
                                }
                            }
                        }
                        Ok(None) => continue,
                        Err(error) => {
                            error!(%error, "Failed to dequeue a logical metrics retry batch.");
                            return None;
                        }
                    }
                }
            };
            return Some(logical);
        }
    }

    async fn refresh_destination(&mut self) {
        let api_key = self.endpoint.api_key();
        if api_key == self.api_key {
            return;
        }

        self.api_key = api_key;
        self.delivery_mode = DeliveryMode::Stateful;
        self.stateless_retry_ready = true;
        self.reconnect_failures = 0;
        self.active = None;
        self.delivery_metadata.clear();
        let effects = self.core.reset_destination_state();
        self.execute(effects).await;
    }

    async fn send_stateless(&mut self, logical: &LogicalMetricBatch) -> Result<usize, MetricStreamFailure> {
        let mut series_encoder = MetricSeriesEncoder::default();
        let encoded = series_encoder.encode(logical);
        let definition_count = encoded.definitions().len();
        let (definitions, mut datums) = encoded.into_parts();
        let data_datums = datums.split_off(definition_count);
        let snapshot = self
            .encoder
            .encode(&MetricStatefulBatch {
                stream: (),
                batch_id: 0,
                datums: definitions,
            })
            .map_err(|error| {
                MetricStreamFailure::new(
                    MetricStreamFailureKind::InvalidArgument,
                    format!("failed to encode Foldspace metric snapshot: {error:?}"),
                )
            })?
            .data;
        let data = self
            .encoder
            .encode(&MetricStatefulBatch {
                stream: (),
                batch_id: 1,
                datums: data_datums,
            })
            .map_err(|error| {
                MetricStreamFailure::new(
                    MetricStreamFailureKind::InvalidArgument,
                    format!("failed to encode Foldspace metric data: {error:?}"),
                )
            })?
            .data;
        let encoded_bytes = snapshot.len().saturating_add(data.len());
        let mut request = TonicRequest::new(StatelessRequest { snapshot, data });
        add_request_metadata(&mut request, &self.api_key)?;

        let mut client = self.client.clone();
        if self.request_timeout.is_zero() {
            client
                .stateless(request)
                .await
                .map_err(metric_stream_failure_from_status)?;
        } else {
            tokio::time::timeout(self.request_timeout, client.stateless(request))
                .await
                .map_err(|_| {
                    MetricStreamFailure::new(
                        MetricStreamFailureKind::DeadlineExceeded,
                        "Foldspace stateless metrics request timed out",
                    )
                })?
                .map_err(metric_stream_failure_from_status)?;
        }
        Ok(encoded_bytes)
    }

    fn schedule_stateless_retry(&mut self) {
        self.stateless_retry_ready = false;
        self.reconnect_failures = self.reconnect_failures.saturating_add(1);
        let delay = self.reconnect_backoff.get_backoff_duration(self.reconnect_failures);
        let events_tx = self.events_tx.clone();
        tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            let _ = events_tx.send(StatefulEvent::StatelessRetryReady);
        });
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

    async fn enqueue_logical_retry(&mut self, logical: LogicalMetricBatch) {
        let metadata = metadata_from_logical(&logical);
        match CompressedLogicalMetricBatch::from_logical(&logical) {
            Ok(batch) => self.enqueue_retry(batch).await,
            Err(error) => {
                self.telemetry
                    .track_permanently_failed_transaction(&metadata, None, &self.endpoint_domain);
                error!(%error, "Failed to preserve a logical metric retry batch.");
            }
        }
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
                error!(%error, "Failed to queue logical metric batch; batch was dropped.");
            }
        }
    }

    fn record_delivery_success(&mut self) {
        self.reconnect_failures = self
            .recovery_error_decrease_factor
            .map_or(0, |decrease| self.reconnect_failures.saturating_sub(decrease));
    }

    async fn handle_event(&mut self, event: StatefulEvent) {
        match event {
            StatefulEvent::Opened { stream_id, outbound } => {
                self.active = Some(ActiveStream { stream_id, outbound });
                let events_tx = self.events_tx.clone();
                let stream_lifetime = self.stream_lifetime;
                tokio::spawn(async move {
                    tokio::time::sleep(stream_lifetime).await;
                    let _ = events_tx.send(StatefulEvent::RotateStream(stream_id));
                });
                let effects = self.core.handle_stream_opened(stream_id);
                self.execute(effects).await;
            }
            StatefulEvent::Ack { stream_id, batch_id } => {
                let inflight_before = self.core.inflight_len();
                let effects = self.core.handle_ack(stream_id, batch_id);
                if batch_id != 0 && self.core.inflight_len() < inflight_before {
                    self.record_delivery_success();
                    if let Some((expected, metadata)) = self.delivery_metadata.pop_front() {
                        if expected == batch_id {
                            self.telemetry
                                .track_successful_transaction(&metadata, &self.endpoint_domain);
                        }
                    }
                }
                self.execute(effects).await;
            }
            StatefulEvent::Failed { stream_id, failure } => {
                if self.core.current_stream_id() != Some(stream_id) {
                    return;
                }
                self.reconnect_failures = self.reconnect_failures.saturating_add(1);
                self.stateful_telemetry.reconnects.increment(1);
                let effects = self.fail_stream(stream_id, failure);
                self.execute(effects).await;
            }
            StatefulEvent::Timer { kind, stream_id } => {
                if self.core.current_stream_id() == stream_id {
                    let effects = self.core.handle_timer(kind);
                    self.execute(effects).await;
                }
            }
            StatefulEvent::RotateStream(stream_id) => {
                if self.core.current_stream_id() == Some(stream_id) {
                    let effects = self.core.handle_timer(TimerKind::RotateStream);
                    self.execute(effects).await;
                }
            }
            StatefulEvent::StatelessRetryReady => self.stateless_retry_ready = true,
        }
    }

    async fn execute(&mut self, effects: Vec<MetricEffect>) {
        let mut effects = VecDeque::from(effects);
        while let Some(effect) = effects.pop_front() {
            match effect {
                MetricEffect::OpenStream { stream_id } => {
                    if let Err(failure) = self.open_stream(stream_id) {
                        effects.extend(self.fail_stream(stream_id, failure));
                    }
                }
                MetricEffect::SendBatch { batch } => {
                    let stream_id = batch.stream;
                    let encoded = match self.encoder.encode(&batch) {
                        Ok(encoded) => encoded,
                        Err(error) => {
                            effects.extend(self.fail_stream(
                                stream_id,
                                MetricStreamFailure::new(
                                    MetricStreamFailureKind::InvalidArgument,
                                    format!("failed to encode Foldspace metric batch: {error:?}"),
                                ),
                            ));
                            continue;
                        }
                    };
                    let Some(active) = self.active.as_mut().filter(|active| active.stream_id == stream_id) else {
                        effects.extend(self.fail_stream(
                            stream_id,
                            MetricStreamFailure::new(
                                MetricStreamFailureKind::Unavailable,
                                "Foldspace metric stream is unavailable",
                            ),
                        ));
                        continue;
                    };
                    self.telemetry.bytes_sent().increment(encoded.data.len() as u64);
                    if let Err(error) = active.outbound.send(encoded).await {
                        effects.extend(self.fail_stream(
                            stream_id,
                            MetricStreamFailure::new(
                                MetricStreamFailureKind::Unavailable,
                                format!("failed to transmit Foldspace metric batch: {error}"),
                            ),
                        ));
                    }
                }
                MetricEffect::CloseStream { stream_id } => {
                    if self.active.as_ref().is_some_and(|active| active.stream_id == stream_id) {
                        self.active = None;
                    }
                }
                MetricEffect::StreamFailed {
                    failure,
                    action,
                    unacknowledged,
                } => {
                    warn!(
                        failure_kind = ?failure.kind(),
                        error = failure.message(),
                        ?action,
                        "Foldspace stateful metrics stream failed."
                    );
                    self.delivery_metadata.clear();
                    match action {
                        MetricFailureAction::RetryWithBackoff => {
                            self.delivery_mode = DeliveryMode::Stateful;
                            for logical in unacknowledged {
                                self.enqueue_logical_retry(logical).await;
                            }
                        }
                        MetricFailureAction::DoNotRetry => {
                            self.delivery_mode = DeliveryMode::Suspended;
                            for logical in unacknowledged {
                                let metadata = metadata_from_logical(&logical);
                                self.telemetry.track_permanently_failed_transaction(
                                    &metadata,
                                    None,
                                    &self.endpoint_domain,
                                );
                            }
                        }
                        MetricFailureAction::WaitForCredentials => {
                            self.delivery_mode = DeliveryMode::WaitingForCredentials;
                            for logical in unacknowledged {
                                self.enqueue_logical_retry(logical).await;
                            }
                        }
                        MetricFailureAction::UseStatelessDelivery => {
                            self.delivery_mode = DeliveryMode::Stateless;
                            self.stateless_retry_ready = true;
                            for logical in unacknowledged {
                                self.enqueue_logical_retry(logical).await;
                            }
                        }
                    }
                }
                MetricEffect::ReturnUnacknowledged { batches } => {
                    self.delivery_metadata.clear();
                    for logical in batches {
                        self.enqueue_logical_retry(logical).await;
                    }
                }
                MetricEffect::ScheduleTimer { timer } => {
                    let events_tx = self.events_tx.clone();
                    let stream_id = self.core.current_stream_id();
                    let delay = if timer.kind == TimerKind::Reconnect {
                        self.reconnect_backoff.get_backoff_duration(self.reconnect_failures)
                    } else {
                        timer.after
                    };
                    tokio::spawn(async move {
                        tokio::time::sleep(delay).await;
                        let _ = events_tx.send(StatefulEvent::Timer {
                            kind: timer.kind,
                            stream_id,
                        });
                    });
                }
                MetricEffect::ReportError { error } => warn!(?error, "Foldspace metrics core reported an error."),
            }
        }
    }

    fn fail_stream(&mut self, stream_id: StreamId, failure: MetricStreamFailure) -> Vec<MetricEffect> {
        self.active = None;
        self.core.handle_stream_error(stream_id, failure)
    }

    fn open_stream(&mut self, stream_id: StreamId) -> Result<(), MetricStreamFailure> {
        let (outbound, receiver) = mpsc::channel(self.outbound_capacity);
        let mut request = TonicRequest::new(ReceiverStream::new(receiver));
        add_request_metadata(&mut request, &self.api_key)?;
        request.metadata_mut().insert(
            "dd-state-request-bytes",
            MetadataValue::try_from(STATE_REQUEST_BYTES.to_string()).map_err(|error| {
                MetricStreamFailure::new(
                    MetricStreamFailureKind::InvalidArgument,
                    format!("Foldspace metric state request limit is invalid gRPC metadata: {error}"),
                )
            })?,
        );
        let mut client = self.client.clone();
        let events_tx = self.events_tx.clone();
        let request_timeout = self.request_timeout;
        tokio::spawn(async move {
            let response = if request_timeout.is_zero() {
                client.stateful_stream(request).await
            } else {
                match tokio::time::timeout(request_timeout, client.stateful_stream(request)).await {
                    Ok(response) => response,
                    Err(_) => {
                        let _ = events_tx.send(StatefulEvent::Failed {
                            stream_id,
                            failure: MetricStreamFailure::new(
                                MetricStreamFailureKind::DeadlineExceeded,
                                "Foldspace stateful metrics stream open timed out",
                            ),
                        });
                        return;
                    }
                }
            };
            match response {
                Ok(response) => {
                    if events_tx.send(StatefulEvent::Opened { stream_id, outbound }).is_ok() {
                        spawn_response_reader(stream_id, response.into_inner(), events_tx);
                    }
                }
                Err(error) => {
                    let _ = events_tx.send(StatefulEvent::Failed {
                        stream_id,
                        failure: metric_stream_failure_from_status(error),
                    });
                }
            }
        });
        Ok(())
    }

    async fn preserve_shutdown(&mut self) {
        if let Some(batch) = self.batcher.take() {
            self.enqueue_logical_retry(batch).await;
        }
        if let Some(stream_id) = self.core.current_stream_id() {
            let effects = self.core.handle_stream_error(
                stream_id,
                MetricStreamFailure::new(MetricStreamFailureKind::Unavailable, "graceful shutdown"),
            );
            for effect in effects {
                match effect {
                    MetricEffect::StreamFailed { unacknowledged, .. } => {
                        for logical in unacknowledged {
                            self.enqueue_logical_retry(logical).await;
                        }
                    }
                    MetricEffect::ReturnUnacknowledged { batches } => {
                        for logical in batches {
                            self.enqueue_logical_retry(logical).await;
                        }
                    }
                    _ => {}
                }
            }
        }
        while let Some(batch) = self.fresh.pop_front() {
            self.enqueue_logical_retry(batch).await;
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
        serializer_v3_configured_endpoint,
        series_config: config.use_v3_api_series(),
        metrics_primary_v3_override,
        serializer_v3_series_endpoints: &config.v3_api().series.endpoints,
        serializer_v3_sketches_endpoints: &config.v3_api().sketches.endpoints,
    })
    .should_receive_payload(Some(MetricsPayloadInfo::v3_series()))
}

fn metadata_from_metrics(metrics: &[Metric]) -> Metadata {
    Metadata::from_event_and_data_point_count(metrics.len(), metrics.iter().map(|metric| metric.values().len()).sum())
}

fn metadata_from_logical(logical: &LogicalMetricBatch) -> Metadata {
    Metadata::from_event_and_data_point_count(logical.series().len(), logical.point_count())
}

fn sent_batch_id(effect: &MetricEffect) -> Option<u64> {
    let MetricEffect::SendBatch { batch } = effect else {
        return None;
    };
    (batch.batch_id != 0).then_some(batch.batch_id)
}

fn logical_series_from_metric(metric: &Metric, additional_tags: &SharedTagSet) -> Option<LogicalMetricSeries> {
    if !is_foldspace_series(metric) {
        return None;
    }

    let metric_type = match metric.values() {
        MetricValues::Counter(..) => MetricSeriesType::Count,
        MetricValues::Rate(..) => MetricSeriesType::Rate,
        MetricValues::Gauge(..) => MetricSeriesType::Gauge,
        MetricValues::Set(..) | MetricValues::Histogram(..) | MetricValues::Distribution(..) => return None,
    };

    let mut seen_tags = FastHashSet::default();
    let prefix = additional_tags
        .into_iter()
        .filter(|tag| !is_v3_series_resource_tag(tag) && !is_v3_series_device_tag(tag))
        .filter_map(|tag| {
            let value = MetaString::from(tag.as_str());
            seen_tags.insert(value.clone()).then(|| value.to_string())
        })
        .collect();
    let values = metric
        .context()
        .tags()
        .into_iter()
        .chain(metric.context().origin_tags())
        .filter(|tag| !is_v3_series_resource_tag(tag) && !is_v3_series_device_tag(tag))
        .filter_map(|tag| {
            let value = MetaString::from(tag.as_str());
            seen_tags.insert(value.clone()).then(|| value.to_string())
        })
        .collect();

    let mut resources = Vec::new();
    if let Some(host) = metric.context().host().filter(|host| !host.is_empty()) {
        resources.push(MetricResource::new("host", host));
    }
    let mut device = None;
    let mut seen_resource_tags = FastHashSet::default();
    for tag in metric
        .context()
        .origin_tags()
        .into_iter()
        .chain(metric.context().tags())
        .chain(additional_tags)
    {
        let tag_value = MetaString::from(tag.as_str());
        if !seen_resource_tags.insert(tag_value) {
            continue;
        }
        if is_v3_series_device_tag(tag) {
            device = tag.value().filter(|value| !value.is_empty());
        } else if is_v3_series_resource_tag(tag) {
            if let Some((kind, name)) = tag.value().and_then(|value| value.split_once(':')) {
                if !kind.is_empty() && !name.is_empty() {
                    resources.push(MetricResource::new(kind, name));
                }
            }
        }
    }
    if let Some(device) = device {
        let index = usize::from(metric.context().host().is_some_and(|host| !host.is_empty()));
        resources.insert(index, MetricResource::new("device", device));
    }

    let (source_type_name, origin) = match metric.metadata().origin() {
        Some(MetricOrigin::SourceType(source_type)) => (Some(source_type.to_string()), None),
        Some(MetricOrigin::OriginMetadata {
            product,
            subproduct,
            product_detail,
        }) => (
            None,
            Some(FoldspaceMetricOrigin::new(
                *product as i32,
                *subproduct as i32,
                *product_detail as i32,
            )),
        ),
        None => (None, None),
    };

    let (interval, points) = match metric.values() {
        MetricValues::Counter(points) | MetricValues::Gauge(points) => (
            0,
            points
                .into_iter()
                .filter(|(_, value)| emittable_scalar_point(*value))
                .map(|(timestamp, value)| {
                    MetricPoint::new(timestamp.map_or(0, |timestamp| timestamp.get() as i64), value)
                })
                .collect(),
        ),
        MetricValues::Rate(points, interval) => (
            interval.as_secs(),
            points
                .into_iter()
                .map(|(timestamp, value)| {
                    let value = if interval.is_zero() {
                        value
                    } else {
                        value / interval.as_secs_f64()
                    };
                    (timestamp, value)
                })
                .filter(|(_, value)| emittable_scalar_point(*value))
                .map(|(timestamp, value)| {
                    MetricPoint::new(timestamp.map_or(0, |timestamp| timestamp.get() as i64), value)
                })
                .collect(),
        ),
        MetricValues::Set(..) | MetricValues::Histogram(..) | MetricValues::Distribution(..) => return None,
    };

    let mut series = LogicalMetricSeries::new(metric.context().name().to_string(), metric_type, points)
        .with_tags(MetricTagSet { prefix, values })
        .with_resources(resources)
        .with_interval(interval)
        .with_source_type_name(source_type_name)
        .with_origin(origin);
    if let Some(unit) = metric.metadata().unit() {
        series.set_unit(unit.to_string());
    }
    Some(series)
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
                        failure: MetricStreamFailure::new(
                            MetricStreamFailureKind::InvalidArgument,
                            format!(
                                "Foldspace intake rejected metric batch {} with status {}",
                                status.batch_id, status.status
                            ),
                        ),
                    });
                    return;
                }
                Ok(None) => {
                    let _ = events_tx.send(StatefulEvent::Failed {
                        stream_id,
                        failure: MetricStreamFailure::new(
                            MetricStreamFailureKind::Unavailable,
                            "Foldspace metrics response stream closed",
                        ),
                    });
                    return;
                }
                Err(error) => {
                    let _ = events_tx.send(StatefulEvent::Failed {
                        stream_id,
                        failure: metric_stream_failure_from_status(error),
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

fn add_request_metadata<T>(request: &mut TonicRequest<T>, api_key: &MetaString) -> Result<(), MetricStreamFailure> {
    let api_key = MetadataValue::try_from(api_key.as_ref()).map_err(|error| {
        MetricStreamFailure::new(
            MetricStreamFailureKind::Unauthenticated,
            format!("Foldspace metrics API key is not valid gRPC metadata: {error}"),
        )
    })?;
    request.metadata_mut().insert("dd-api-key", api_key);
    request
        .metadata_mut()
        .insert("dd-content-encoding", MetadataValue::from_static("zstd"));
    Ok(())
}

fn metric_stream_failure_from_status(status: tonic::Status) -> MetricStreamFailure {
    let kind = match status.code() {
        Code::Unavailable => MetricStreamFailureKind::Unavailable,
        Code::DeadlineExceeded => MetricStreamFailureKind::DeadlineExceeded,
        Code::ResourceExhausted => MetricStreamFailureKind::ResourceExhausted,
        Code::InvalidArgument => MetricStreamFailureKind::InvalidArgument,
        Code::Unauthenticated | Code::PermissionDenied => MetricStreamFailureKind::Unauthenticated,
        Code::FailedPrecondition => MetricStreamFailureKind::FailedPrecondition,
        _ => MetricStreamFailureKind::Unavailable,
    };
    MetricStreamFailure::new(kind, status.to_string())
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
    use std::sync::atomic::{AtomicUsize, Ordering};

    use foldspace_core::{
        proto::stateful::{
            metric_datum, stateful_intake_server::StatefulIntake, stateful_intake_server::StatefulIntakeServer,
            MetricDatumSequence, StatelessResponse,
        },
        LogicalMetricSeries, MetricPoint, MetricSeriesType,
    };
    use prost::Message as _;
    use saluki_context::{
        tags::{Tag, TagSet},
        Context,
    };
    use saluki_core::data_model::event::metric::MetricMetadata;
    use tempfile::tempdir;
    use tokio::sync::Notify;
    use tokio_stream::wrappers::ReceiverStream as TonicReceiverStream;
    use tonic::{
        transport::{server::TcpIncoming, Server},
        Response, Status,
    };

    use super::*;

    const TEST_RETRY_QUEUE_BYTES: u64 = 1024 * 1024;

    #[derive(Clone, Debug)]
    struct RestartingIntake {
        connection_count: Arc<AtomicUsize>,
        received: mpsc::UnboundedSender<(usize, ProtoStatefulBatch)>,
        shutdown: Arc<Notify>,
    }

    #[derive(Clone, Debug)]
    struct AckingIntake {
        received: mpsc::UnboundedSender<ProtoStatefulBatch>,
    }

    #[derive(Clone, Debug)]
    struct StatelessFallbackIntake {
        received: mpsc::UnboundedSender<StatelessRequest>,
    }

    #[async_trait]
    impl StatefulIntake for AckingIntake {
        type StatefulStreamStream = TonicReceiverStream<Result<BatchStatus, Status>>;

        async fn stateful_stream(
            &self, request: tonic::Request<tonic::Streaming<ProtoStatefulBatch>>,
        ) -> Result<Response<Self::StatefulStreamStream>, Status> {
            let mut inbound = request.into_inner();
            let received = self.received.clone();
            let (responses_tx, responses_rx) = mpsc::channel(4);
            tokio::spawn(async move {
                while let Ok(Some(batch)) = inbound.message().await {
                    let batch_id = batch.batch_id;
                    if received.send(batch).is_err()
                        || responses_tx
                            .send(Ok(BatchStatus {
                                batch_id,
                                status: i32::from(batch_status::Status::Ok),
                            }))
                            .await
                            .is_err()
                    {
                        return;
                    }
                }
            });
            Ok(Response::new(TonicReceiverStream::new(responses_rx)))
        }

        async fn stateless(
            &self, _request: tonic::Request<StatelessRequest>,
        ) -> Result<Response<StatelessResponse>, Status> {
            Ok(Response::new(StatelessResponse {}))
        }
    }

    #[async_trait]
    impl StatefulIntake for StatelessFallbackIntake {
        type StatefulStreamStream = TonicReceiverStream<Result<BatchStatus, Status>>;

        async fn stateful_stream(
            &self, _request: tonic::Request<tonic::Streaming<ProtoStatefulBatch>>,
        ) -> Result<Response<Self::StatefulStreamStream>, Status> {
            Err(Status::failed_precondition("stateful metrics are unavailable"))
        }

        async fn stateless(
            &self, request: tonic::Request<StatelessRequest>,
        ) -> Result<Response<StatelessResponse>, Status> {
            self.received
                .send(request.into_inner())
                .map_err(|_| Status::unavailable("test receiver closed"))?;
            Ok(Response::new(StatelessResponse {}))
        }
    }

    #[async_trait]
    impl StatefulIntake for RestartingIntake {
        type StatefulStreamStream = TonicReceiverStream<Result<BatchStatus, Status>>;

        async fn stateful_stream(
            &self, request: tonic::Request<tonic::Streaming<ProtoStatefulBatch>>,
        ) -> Result<Response<Self::StatefulStreamStream>, Status> {
            let connection = self.connection_count.fetch_add(1, Ordering::SeqCst);
            let mut inbound = request.into_inner();
            let received = self.received.clone();
            let shutdown = Arc::clone(&self.shutdown);
            let (responses_tx, responses_rx) = mpsc::channel(4);
            tokio::spawn(async move {
                match connection {
                    0 => {
                        let first = inbound.message().await.unwrap().unwrap();
                        received.send((connection, first)).unwrap();
                        responses_tx
                            .send(Err(Status::unavailable("test stream failure")))
                            .await
                            .unwrap();
                    }
                    1 => {
                        let retried = inbound.message().await.unwrap().unwrap();
                        let retried_batch_id = retried.batch_id;
                        received.send((connection, retried)).unwrap();
                        responses_tx
                            .send(Ok(BatchStatus {
                                batch_id: retried_batch_id,
                                status: i32::from(batch_status::Status::Ok),
                            }))
                            .await
                            .unwrap();
                        shutdown.notified().await;
                    }
                    _ => panic!("unexpected extra Foldspace connection"),
                }
            });
            Ok(Response::new(TonicReceiverStream::new(responses_rx)))
        }

        async fn stateless(
            &self, _request: tonic::Request<StatelessRequest>,
        ) -> Result<Response<StatelessResponse>, Status> {
            Ok(Response::new(StatelessResponse {}))
        }
    }

    fn test_endpoint(endpoint_url: &str, core: StatefulMetricsCore, queue_id: &str) -> StatefulMetricsEndpoint {
        let channel = Channel::from_shared(endpoint_url.to_string()).unwrap().connect_lazy();
        test_endpoint_with_channel(endpoint_url, channel, core, queue_id)
    }

    fn test_endpoint_with_channel(
        endpoint_url: &str, channel: Channel, core: StatefulMetricsCore, queue_id: &str,
    ) -> StatefulMetricsEndpoint {
        let endpoint = ResolvedEndpoint::from_raw_endpoint(endpoint_url, "test-api-key").unwrap();
        let api_key = endpoint.api_key();
        let (events_tx, events_rx) = mpsc::unbounded_channel();
        let metrics_builder = MetricsBuilder::default();
        StatefulMetricsEndpoint {
            endpoint,
            api_key,
            endpoint_domain: "test".to_string(),
            client: StatefulIntakeClient::new(channel),
            core,
            encoder: MetricBatchEncoder::new(ZstdBatchCompressor::default()),
            outbound_capacity: 4,
            active: None,
            events_tx,
            events_rx,
            api_key_changes: ApiKeyChanges::for_tests(),
            request_timeout: Duration::from_secs(1),
            stream_lifetime: Duration::from_secs(60),
            delivery_mode: DeliveryMode::Stateful,
            stateless_retry_ready: true,
            reconnect_backoff: ExponentialBackoff::with_jitter(Duration::from_millis(10), Duration::from_secs(1), 2.0),
            flush_timeout: Duration::from_millis(10),
            batcher: LogicalMetricBatcher::new(SharedTagSet::default(), 100, 100),
            fresh: VecDeque::new(),
            max_fresh: 4,
            retry: RetryQueue::new(queue_id.to_string(), TEST_RETRY_QUEUE_BYTES),
            fresh_since_retry: 0,
            retries_since_disk: 0,
            reconnect_failures: 0,
            recovery_error_decrease_factor: None,
            delivery_metadata: VecDeque::new(),
            telemetry: ComponentTelemetry::from_builder(&metrics_builder),
            stateful_telemetry: StatefulMetricsTelemetry::new(&metrics_builder, "test"),
        }
    }

    async fn next_event(endpoint: &mut StatefulMetricsEndpoint) -> StatefulEvent {
        tokio::time::timeout(Duration::from_secs(2), endpoint.events_rx.recv())
            .await
            .expect("timed out waiting for a Foldspace event")
            .expect("Foldspace event channel closed")
    }

    fn assert_metric_series_batch(batch: &ProtoStatefulBatch) {
        let decoded = zstd::stream::decode_all(batch.data.as_slice()).unwrap();
        let sequence = MetricDatumSequence::decode(decoded.as_slice()).unwrap();
        assert!(sequence
            .data
            .iter()
            .any(|datum| matches!(datum.data, Some(metric_datum::Data::MetricSeriesBatch(_)))));
    }

    fn logical_batch() -> LogicalMetricBatch {
        LogicalMetricBatch::new(vec![LogicalMetricSeries::new(
            "requests",
            MetricSeriesType::Count,
            vec![MetricPoint::new(10, 2.0)],
        )])
    }

    fn tag_set<const N: usize>(tags: [&'static str; N]) -> TagSet {
        tags.into_iter().map(Tag::from_static).collect()
    }

    #[test]
    fn logical_series_preserves_v3_fields() {
        let context = Context::from_static_parts(
            "stateful.complete",
            &["env:prod", "device:eth0", "dd.internal.resource:pod:pod-a"],
        )
        .with_origin_tags(tag_set(["origin:dogstatsd"]))
        .with_host(Some(MetaString::from_static("host-a")));
        let metadata = MetricMetadata::default()
            .with_origin(MetricOrigin::OriginMetadata {
                product: 10,
                subproduct: 11,
                product_detail: 12,
            })
            .with_unit(MetaString::from_static("request"));
        let metric = Metric::from_parts(
            context,
            MetricValues::rate([(123_u64, 20.0)], Duration::from_secs(10)),
            metadata,
        );
        let additional_tags = SharedTagSet::from(tag_set(["global:true"]));

        let series = logical_series_from_metric(&metric, &additional_tags).expect("series should translate");

        assert_eq!(series.name(), "stateful.complete");
        assert_eq!(series.metric_type(), MetricSeriesType::Rate);
        assert_eq!(series.interval(), 10);
        assert_eq!(series.unit(), Some("request"));
        assert_eq!(series.points(), [MetricPoint::new(123, 2.0)]);
        assert_eq!(series.tags().prefix, vec!["global:true"]);
        assert_eq!(series.tags().values, vec!["env:prod", "origin:dogstatsd"]);
        assert_eq!(
            series.resources(),
            [
                MetricResource::new("host", "host-a"),
                MetricResource::new("device", "eth0"),
                MetricResource::new("pod", "pod-a"),
            ]
        );
        assert_eq!(series.origin(), Some(FoldspaceMetricOrigin::new(10, 11, 12)));
        assert_eq!(series.source_type_name(), None);
    }

    #[test]
    fn logical_batcher_splits_on_point_limit() {
        let mut batcher = LogicalMetricBatcher::new(SharedTagSet::default(), 10, 3);

        let (batches, started_batch) = batcher.push(&Metric::counter("first", [(1, 1.0), (2, 2.0)]));
        assert!(batches.is_empty());
        assert!(started_batch);

        let (batches, started_batch) = batcher.push(&Metric::counter("second", [(3, 3.0), (4, 4.0)]));
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].series()[0].name(), "first");
        assert!(started_batch);

        let pending = batcher.take().expect("second metric should remain pending");
        assert_eq!(pending.series()[0].name(), "second");
    }

    #[test]
    fn logical_batcher_flushes_at_metric_limit() {
        let mut batcher = LogicalMetricBatcher::new(SharedTagSet::default(), 2, 10);

        assert!(batcher.push(&Metric::gauge("first", 1.0)).0.is_empty());
        let (batches, started_batch) = batcher.push(&Metric::gauge("second", 2.0));

        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].series().len(), 2);
        assert!(!started_batch);
        assert!(!batcher.has_pending());
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
        let restart_args = PersistedQueueArgs {
            root_path: root.path().to_path_buf(),
            max_on_disk_bytes: 64 * 1024,
            storage_max_disk_ratio: 1.0,
            disk_usage_retriever: Arc::new(DiskUsageRetrieverImpl::new(root.path().to_path_buf())),
            max_age_days: 1,
        };
        let mut retry: RetryQueue<CompressedLogicalMetricBatch> = RetryQueue::new("metrics".to_string(), 64 * 1024)
            .with_disk_persistence(args)
            .await
            .unwrap();
        let batch = CompressedLogicalMetricBatch::from_logical(&logical_batch()).unwrap();
        assert!(!retry.push(batch).await.unwrap().had_drops());
        assert!(!retry.flush().await.unwrap().had_drops());

        let mut restarted: RetryQueue<CompressedLogicalMetricBatch> = RetryQueue::new("metrics".to_string(), 64 * 1024)
            .with_disk_persistence(restart_args)
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
    fn grpc_statuses_map_to_foldspace_failure_classes() {
        for (code, expected) in [
            (Code::Unavailable, MetricStreamFailureKind::Unavailable),
            (Code::DeadlineExceeded, MetricStreamFailureKind::DeadlineExceeded),
            (Code::ResourceExhausted, MetricStreamFailureKind::ResourceExhausted),
            (Code::InvalidArgument, MetricStreamFailureKind::InvalidArgument),
            (Code::Unauthenticated, MetricStreamFailureKind::Unauthenticated),
            (Code::FailedPrecondition, MetricStreamFailureKind::FailedPrecondition),
        ] {
            let failure = metric_stream_failure_from_status(Status::new(code, "test"));
            assert_eq!(failure.kind(), expected);
        }
    }

    #[test]
    fn one_logical_batch_maps_to_one_payload_id() {
        let mut core = StatefulMetricsCore::new(CoreConfig {
            sender: SenderConfig {
                max_inflight_payloads: 4,
                ..SenderConfig::default()
            },
            ..CoreConfig::default()
        });
        let stream_id = match core.start().as_slice() {
            [MetricEffect::OpenStream { stream_id }] => stream_id.to_owned(),
            effects => panic!("expected one open-stream effect, got {effects:?}"),
        };
        assert!(core.handle_stream_opened(stream_id).is_empty());
        let effects = core
            .push_batch(LogicalMetricBatch::new(vec![LogicalMetricSeries::new(
                "requests",
                MetricSeriesType::Count,
                vec![MetricPoint::new(10, 2.0), MetricPoint::new(11, 3.0)],
            )]))
            .unwrap();

        assert_eq!(effects.iter().filter_map(sent_batch_id).collect::<Vec<_>>(), vec![1]);
    }

    #[test]
    fn repeated_metadata_reduces_foldspace_wire_bytes() {
        let mut core = StatefulMetricsCore::default();
        let stream_id = match core.start().as_slice() {
            [MetricEffect::OpenStream { stream_id }] => stream_id.to_owned(),
            effects => panic!("expected one open-stream effect, got {effects:?}"),
        };
        assert!(core.handle_stream_opened(stream_id).is_empty());
        let logical = LogicalMetricBatch::new(vec![LogicalMetricSeries::new(
            "requests.by.customer.and.availability_zone",
            MetricSeriesType::Count,
            vec![MetricPoint::new(10, 2.0)],
        )
        .with_tags(MetricTagSet {
            prefix: vec!["environment:production".to_string(), "service:payments-api".to_string()],
            values: vec!["availability-zone:us-east-1a".to_string()],
        })
        .with_resources(vec![MetricResource::new("host", "production-host-with-a-long-name")])]);
        let encoder = MetricBatchEncoder::new(ZstdBatchCompressor::default());

        let first = core.push_batch(logical.clone()).unwrap();
        let first_batch = match first.as_slice() {
            [MetricEffect::SendBatch { batch }] => batch,
            effects => panic!("expected one send-batch effect, got {effects:?}"),
        };
        let first_bytes = encoder.encode(first_batch).unwrap().data.len();
        assert!(core.handle_ack(stream_id, first_batch.batch_id).is_empty());

        let second = core.push_batch(logical).unwrap();
        let second_batch = match second.as_slice() {
            [MetricEffect::SendBatch { batch }] => batch,
            effects => panic!("expected one send-batch effect, got {effects:?}"),
        };
        let second_bytes = encoder.encode(second_batch).unwrap().data.len();

        assert!(
            second_bytes < first_bytes,
            "expected repeated metadata to shrink from {first_bytes} bytes, got {second_bytes} bytes"
        );
    }

    #[tokio::test]
    async fn long_connection_outage_keeps_retry_queued_without_reencoding() {
        let mut core = StatefulMetricsCore::default();
        assert!(matches!(core.start().as_slice(), [MetricEffect::OpenStream { .. }]));
        let mut endpoint = test_endpoint("http://127.0.0.1:9", core, "long-outage");
        let push_result = endpoint
            .retry
            .push(CompressedLogicalMetricBatch::from_logical(&logical_batch()).unwrap())
            .await
            .unwrap();
        assert!(!push_result.had_drops());

        for _ in 0..10_000 {
            endpoint.schedule_ready().await;
        }

        assert_eq!(endpoint.retry.len(), 1);
        assert_eq!(endpoint.retries_since_disk, 0);
        assert_eq!(endpoint.core.encoding_count(), 0);
        assert!(endpoint.delivery_metadata.is_empty());
    }

    #[tokio::test]
    async fn stateful_endpoint_sends_metrics_over_grpc() {
        let incoming = TcpIncoming::bind(([127, 0, 0, 1], 0).into()).unwrap();
        let endpoint_url = format!("http://{}", incoming.local_addr().unwrap());
        let (received_tx, mut received_rx) = mpsc::unbounded_channel();
        let server = tokio::spawn(
            Server::builder()
                .add_service(StatefulIntakeServer::new(AckingIntake { received: received_tx }))
                .serve_with_incoming(incoming),
        );
        let mut endpoint = test_endpoint(&endpoint_url, StatefulMetricsCore::default(), "grpc");

        let effects = endpoint.core.start();
        endpoint.execute(effects).await;
        let opened = next_event(&mut endpoint).await;
        assert!(matches!(opened, StatefulEvent::Opened { .. }));
        endpoint.handle_event(opened).await;

        endpoint.enqueue_fresh(logical_batch()).await;
        endpoint.schedule_ready().await;

        let batch = tokio::time::timeout(Duration::from_secs(2), received_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(batch.batch_id, 1);
        assert_metric_series_batch(&batch);
        let ack = next_event(&mut endpoint).await;
        assert!(matches!(ack, StatefulEvent::Ack { batch_id: 1, .. }));
        endpoint.handle_event(ack).await;
        assert_eq!(endpoint.core.inflight_len(), 0);
        assert!(endpoint.delivery_metadata.is_empty());

        server.abort();
        let _ = server.await;
    }

    #[tokio::test]
    async fn failed_precondition_falls_back_to_stateless_delivery() {
        let incoming = TcpIncoming::bind(([127, 0, 0, 1], 0).into()).unwrap();
        let endpoint_url = format!("http://{}", incoming.local_addr().unwrap());
        let (received_tx, mut received_rx) = mpsc::unbounded_channel();
        let server = tokio::spawn(
            Server::builder()
                .add_service(StatefulIntakeServer::new(StatelessFallbackIntake {
                    received: received_tx,
                }))
                .serve_with_incoming(incoming),
        );
        let mut endpoint = test_endpoint(&endpoint_url, StatefulMetricsCore::default(), "stateless-fallback");
        endpoint.enqueue_fresh(logical_batch()).await;

        let effects = endpoint.core.start();
        endpoint.execute(effects).await;
        let failure = next_event(&mut endpoint).await;
        assert!(matches!(failure, StatefulEvent::Failed { .. }));
        endpoint.handle_event(failure).await;
        assert_eq!(endpoint.delivery_mode, DeliveryMode::Stateless);

        endpoint.schedule_ready().await;
        let request = tokio::time::timeout(Duration::from_secs(2), received_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(!request.snapshot.is_empty());
        assert!(!request.data.is_empty());
        assert!(endpoint.fresh.is_empty());
        assert!(endpoint.retry.is_empty());

        server.abort();
        let _ = server.await;
    }

    #[tokio::test]
    async fn unacknowledged_batch_reconnects_and_resends_over_grpc() {
        let incoming = TcpIncoming::bind(([127, 0, 0, 1], 0).into()).unwrap();
        let endpoint_url = format!("http://{}", incoming.local_addr().unwrap());
        let (received_tx, mut received_rx) = mpsc::unbounded_channel();
        let shutdown = Arc::new(Notify::new());
        let service = RestartingIntake {
            connection_count: Arc::new(AtomicUsize::new(0)),
            received: received_tx,
            shutdown: Arc::clone(&shutdown),
        };
        let server = tokio::spawn(
            Server::builder()
                .add_service(StatefulIntakeServer::new(service))
                .serve_with_incoming(incoming),
        );
        let core = StatefulMetricsCore::new(CoreConfig {
            sender: SenderConfig {
                max_inflight_payloads: 4,
                ..SenderConfig::default()
            },
            reconnect_delay: Duration::from_millis(10),
            ..CoreConfig::default()
        });
        let mut endpoint = test_endpoint(&endpoint_url, core, "grpc-reconnect");

        let effects = endpoint.core.start();
        endpoint.execute(effects).await;
        let opened = next_event(&mut endpoint).await;
        assert!(matches!(opened, StatefulEvent::Opened { .. }));
        endpoint.handle_event(opened).await;

        let metric = Metric::counter("requests", [(10, 2.0), (11, 3.0)]);
        assert!(endpoint.batcher.push(&metric).0.is_empty());
        let logical = endpoint.batcher.take().expect("metric should be pending");
        endpoint.enqueue_fresh(logical).await;
        endpoint.schedule_ready().await;

        let failure = next_event(&mut endpoint).await;
        assert!(matches!(failure, StatefulEvent::Failed { .. }));
        endpoint.handle_event(failure).await;
        assert_eq!(endpoint.retry.len(), 1);

        let reconnect = next_event(&mut endpoint).await;
        assert!(matches!(
            reconnect,
            StatefulEvent::Timer {
                kind: TimerKind::Reconnect,
                ..
            }
        ));
        endpoint.handle_event(reconnect).await;
        let reopened = next_event(&mut endpoint).await;
        assert!(matches!(reopened, StatefulEvent::Opened { .. }));
        endpoint.handle_event(reopened).await;
        endpoint.schedule_ready().await;

        let retry_ack = next_event(&mut endpoint).await;
        assert!(matches!(retry_ack, StatefulEvent::Ack { batch_id: 1, .. }));
        endpoint.handle_event(retry_ack).await;

        let mut received = Vec::new();
        for _ in 0..2 {
            received.push(
                tokio::time::timeout(Duration::from_secs(2), received_rx.recv())
                    .await
                    .unwrap()
                    .unwrap(),
            );
        }
        assert_eq!(
            received
                .iter()
                .map(|(connection, batch)| (*connection, batch.batch_id))
                .collect::<Vec<_>>(),
            vec![(0, 1), (1, 1)]
        );
        assert_metric_series_batch(&received[0].1);
        assert_metric_series_batch(&received[1].1);
        assert!(endpoint.retry.is_empty());
        assert_eq!(endpoint.core.inflight_len(), 0);
        assert!(endpoint.delivery_metadata.is_empty());

        shutdown.notify_waiters();
        server.abort();
        let _ = server.await;
    }
}
