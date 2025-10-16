use std::sync::Arc;
use std::sync::LazyLock;
use std::time::Duration;

use ::metrics::Counter;
use async_trait::async_trait;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::post;
use axum::Router;
use bytesize::ByteSize;
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder, MemoryLimiter};
use otlp_protos::opentelemetry::proto::collector::logs::v1::logs_service_server::{LogsService, LogsServiceServer};
use otlp_protos::opentelemetry::proto::collector::logs::v1::{ExportLogsServiceRequest, ExportLogsServiceResponse};
use otlp_protos::opentelemetry::proto::collector::metrics::v1::metrics_service_server::{
    MetricsService, MetricsServiceServer,
};
use otlp_protos::opentelemetry::proto::collector::metrics::v1::{
    ExportMetricsServiceRequest, ExportMetricsServiceResponse,
};
use otlp_protos::opentelemetry::proto::logs::v1::ResourceLogs as OtlpResourceLogs;
use otlp_protos::opentelemetry::proto::metrics::v1::ResourceMetrics as OtlpResourceMetrics;
use prost::Message;
use saluki_common::task::HandleExt as _;
use saluki_config::GenericConfiguration;
use saluki_context::ContextResolver;
use saluki_core::observability::ComponentMetricsExt;
use saluki_core::topology::interconnect::EventBufferManager;
use saluki_core::topology::shutdown::{DynamicShutdownCoordinator, DynamicShutdownHandle};
use saluki_core::{
    components::{
        sources::{Source, SourceBuilder, SourceContext},
        ComponentContext,
    },
    data_model::event::{Event, EventType},
    topology::{EventsBuffer, OutputDefinition},
};
use saluki_env::WorkloadProvider;
use saluki_error::{generic_error, GenericError};
use saluki_io::net::listener::ConnectionOrientedListener;
use saluki_io::net::server::http::HttpServer;
use saluki_io::net::util::hyper::TowerToHyperService;
use saluki_io::net::ListenAddress;
use saluki_metrics::MetricsBuilder;
use serde::Deserialize;
use tokio::select;
use tokio::sync::mpsc;
use tokio::time::{interval, MissedTickBehavior};
use tonic::transport::Server;
use tonic::{Request, Response, Status};
use tracing::{debug, error};

mod attributes;
mod logs;
mod metrics;
mod origin;
mod resolver;
use self::logs::translator::OtlpLogsTranslator;
use self::metrics::translator::OtlpMetricsTranslator;
use self::origin::OtlpOriginTagResolver;
use self::resolver::build_context_resolver;

const fn default_context_string_interner_size() -> ByteSize {
    ByteSize::mib(2)
}

const fn default_cached_contexts_limit() -> usize {
    500_000
}

const fn default_cached_tagsets_limit() -> usize {
    500_000
}

const fn default_allow_context_heap_allocations() -> bool {
    true
}

/// Configuration for the OTLP source.
#[derive(Deserialize, Default)]
pub struct OtlpConfiguration {
    otlp_config: OtlpConfig,

    /// Total size of the string interner used for contexts.
    ///
    /// This controls the amount of memory that can be used to intern metric names and tags. If the interner is full,
    /// metrics with contexts that have not already been resolved may or may not be dropped, depending on the value of
    /// `allow_context_heap_allocations`.
    #[serde(
        rename = "otlp_string_interner_size",
        default = "default_context_string_interner_size"
    )]
    context_string_interner_bytes: ByteSize,

    /// The maximum number of cached contexts to allow.
    ///
    /// This is the maximum number of resolved contexts that can be cached at any given time. This limit does not affect
    /// the total number of contexts that can be _alive_ at any given time, which is dependent on the interner capacity
    /// and whether or not heap allocations are allowed.
    ///
    /// Defaults to 500,000.
    #[serde(rename = "otlp_cached_contexts_limit", default = "default_cached_contexts_limit")]
    cached_contexts_limit: usize,

    /// The maximum number of cached tagsets to allow.
    ///
    /// This is the maximum number of resolved tagsets that can be cached at any given time. This limit does not affect
    /// the total number of tagsets that can be _alive_ at any given time, which is dependent on the interner capacity
    /// and whether or not heap allocations are allowed.
    ///
    /// Defaults to 500,000.
    #[serde(rename = "otlp_cached_tagsets_limit", default = "default_cached_tagsets_limit")]
    cached_tagsets_limit: usize,

    /// Whether or not to allow heap allocations when resolving contexts.
    ///
    /// When resolving contexts during parsing, the metric name and tags are interned to reduce memory usage. The
    /// interner has a fixed size, however, which means some strings can fail to be interned if the interner is full.
    /// When set to `true`, we allow these strings to be allocated on the heap like normal, but this can lead to
    /// increased (unbounded) memory usage. When set to `false`, if the metric name and all of its tags cannot be
    /// interned, the metric is skipped.
    ///
    /// Defaults to `true`.
    #[serde(
        rename = "otlp_allow_context_heap_allocs",
        default = "default_allow_context_heap_allocations"
    )]
    allow_context_heap_allocations: bool,

    /// Workload provider to utilize for origin detection/enrichment.
    #[serde(skip)]
    workload_provider: Option<Arc<dyn WorkloadProvider + Send + Sync>>,
}

#[derive(Deserialize, Debug, Default)]
pub struct OtlpConfig {
    #[serde(default)]
    receiver: Receiver,
    #[serde(default)]
    metrics: MetricsConfig,
    #[serde(default)]
    logs: LogsConfig,
}

#[derive(Deserialize, Debug, Default)]
pub struct Receiver {
    #[serde(default)]
    protocols: Protocols,
}

#[derive(Deserialize, Debug, Default)]
struct Protocols {
    #[serde(default)]
    grpc: GrpcConfig,

    #[serde(default)]
    http: HttpConfig,
}

#[derive(Deserialize, Debug)]
pub struct GrpcConfig {
    /// The endpoint to bind the OTLP/gRPC server to.
    ///
    /// Defaults to 0.0.0.0:4317.
    #[serde(default = "default_grpc_endpoint")]
    pub endpoint: String,

    /// The transport to use for the OTLP/gRPC server.
    ///
    /// Defaults to "tcp".
    #[serde(default = "default_transport")]
    pub transport: String,

    /// The maximum size (in MiB) of messages accepted by the OTLP/gRPC endpoint.
    ///
    /// Defaults to 4 MiB.
    #[serde(default = "default_max_recv_msg_size_mib")]
    #[serde(rename = "max_recv_msg_size_mib")]
    pub max_recv_msg_size_mib: u64,
}

#[derive(Deserialize, Debug)]
pub struct HttpConfig {
    /// The OTLP/HTTP listener endpoint.
    ///
    /// Defaults to 0.0.0.0:4318.
    #[serde(default = "default_http_endpoint")]
    pub endpoint: String,
}

impl Default for GrpcConfig {
    fn default() -> Self {
        Self {
            endpoint: default_grpc_endpoint(),
            transport: default_transport(),
            max_recv_msg_size_mib: default_max_recv_msg_size_mib(),
        }
    }
}

impl Default for HttpConfig {
    fn default() -> Self {
        Self {
            endpoint: default_http_endpoint(),
        }
    }
}

fn default_grpc_endpoint() -> String {
    "0.0.0.0:4317".to_string()
}

fn default_http_endpoint() -> String {
    "0.0.0.0:4318".to_string()
}

fn default_transport() -> String {
    "tcp".to_string()
}

fn default_max_recv_msg_size_mib() -> u64 {
    4
}

enum OtlpResource {
    Metrics(OtlpResourceMetrics),
    Logs(OtlpResourceLogs),
}

#[derive(Deserialize, Debug)]
pub struct LogsConfig {
    /// Whether to enable OTLP logs support.
    ///
    /// Defaults to true.
    #[serde(default = "default_logs_enabled")]
    pub enabled: bool,
}

fn default_logs_enabled() -> bool {
    true
}

impl Default for LogsConfig {
    fn default() -> Self {
        Self {
            enabled: default_logs_enabled(),
        }
    }
}

#[derive(Deserialize, Debug)]
pub struct MetricsConfig {
    /// Whether to enable OTLP metrics support.
    ///
    /// Defaults to true.
    #[serde(default = "default_metrics_enabled")]
    pub enabled: bool,
}

fn default_metrics_enabled() -> bool {
    true
}

impl Default for MetricsConfig {
    fn default() -> Self {
        Self {
            enabled: default_metrics_enabled(),
        }
    }
}

impl OtlpConfiguration {
    /// Creates a new `OTLPConfiguration` from the given configuration.
    pub fn from_configuration(config: &GenericConfiguration) -> Result<Self, GenericError> {
        Ok(config.as_typed()?)
    }

    /// Sets the workload provider to use for configuring origin detection/enrichment.
    ///
    /// A workload provider must be set otherwise origin detection/enrichment will not be enabled.
    ///
    /// Defaults to unset.
    pub fn with_workload_provider<W>(mut self, workload_provider: W) -> Self
    where
        W: WorkloadProvider + Send + Sync + 'static,
    {
        self.workload_provider = Some(Arc::new(workload_provider));
        self
    }
}

pub struct Metrics {
    metrics_received: Counter,
    logs_received: Counter,
}

impl Metrics {
    fn metrics_received(&self) -> &Counter {
        &self.metrics_received
    }

    fn logs_received(&self) -> &Counter {
        &self.logs_received
    }

    /// Test-only helper to construct a `Metrics` instance.
    #[cfg(test)]
    pub fn for_tests() -> Self {
        Metrics {
            metrics_received: Counter::noop(),
            logs_received: Counter::noop(),
        }
    }
}

fn build_metrics(component_context: &ComponentContext) -> Metrics {
    let builder = MetricsBuilder::from_component_context(component_context);

    Metrics {
        metrics_received: builder
            .register_debug_counter_with_tags("component_events_received_total", [("message_type", "otlp_metrics")]),
        logs_received: builder
            .register_debug_counter_with_tags("component_events_received_total", [("message_type", "otlp_logs")]),
    }
}

#[async_trait]
impl SourceBuilder for OtlpConfiguration {
    fn outputs(&self) -> &[OutputDefinition] {
        static OUTPUTS: LazyLock<Vec<OutputDefinition>> = LazyLock::new(|| {
            vec![
                OutputDefinition::named_output("metrics", EventType::Metric),
                OutputDefinition::named_output("logs", EventType::Log),
            ]
        });

        &OUTPUTS
    }

    async fn build(&self, context: ComponentContext) -> Result<Box<dyn Source + Send>, GenericError> {
        if !self.otlp_config.metrics.enabled && !self.otlp_config.logs.enabled {
            return Err(generic_error!(
                "OTLP metrics and logs support is disabled. Please enable at least one of them."
            ));
        }

        let grpc_listen_str = format!(
            "{}://{}",
            self.otlp_config.receiver.protocols.grpc.transport, self.otlp_config.receiver.protocols.grpc.endpoint
        );
        let grpc_endpoint = ListenAddress::try_from(grpc_listen_str.as_str())
            .map_err(|e| generic_error!("Invalid gRPC endpoint address '{}': {}", grpc_listen_str, e))?;

        // Enforce the current limitation that we only support TCP for gRPC.
        if !matches!(grpc_endpoint, ListenAddress::Tcp(_)) {
            return Err(generic_error!("Only 'tcp' transport is supported for OTLP gRPC"));
        }

        let http_socket_addr = self.otlp_config.receiver.protocols.http.endpoint.parse().map_err(|e| {
            generic_error!(
                "Invalid HTTP endpoint address '{}': {}",
                self.otlp_config.receiver.protocols.http.endpoint,
                e
            )
        })?;

        let maybe_origin_tags_resolver = self.workload_provider.clone().map(OtlpOriginTagResolver::new);

        let context_resolver = build_context_resolver(self, &context, maybe_origin_tags_resolver)?;
        let translator_config = metrics::config::OtlpTranslatorConfig::default().with_remapping(true);
        let grpc_max_recv_msg_size_bytes =
            self.otlp_config.receiver.protocols.grpc.max_recv_msg_size_mib as usize * 1024 * 1024;
        let metrics = build_metrics(&context);

        Ok(Box::new(Otlp {
            context_resolver,
            grpc_endpoint,
            http_endpoint: ListenAddress::Tcp(http_socket_addr),
            grpc_max_recv_msg_size_bytes,
            translator_config,
            metrics,
        }))
    }
}

impl MemoryBounds for OtlpConfiguration {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        builder
            .minimum()
            .with_single_value::<Otlp>("source struct")
            .with_single_value::<GrpcService>("gRPC service");
    }
}

pub struct Otlp {
    context_resolver: ContextResolver,
    grpc_endpoint: ListenAddress,
    http_endpoint: ListenAddress,
    grpc_max_recv_msg_size_bytes: usize,
    translator_config: metrics::config::OtlpTranslatorConfig,
    metrics: Metrics, // Telemetry metrics, not DD native metrics.
}

#[async_trait]
impl Source for Otlp {
    async fn run(self: Box<Self>, mut context: SourceContext) -> Result<(), GenericError> {
        let mut global_shutdown = context.take_shutdown_handle();
        let mut health = context.take_health_handle();
        let memory_limiter = context.topology_context().memory_limiter();

        // Create the internal channel for decoupling the servers from the converter.
        let (tx, rx) = mpsc::channel::<OtlpResource>(1024);

        let mut converter_shutdown_coordinator = DynamicShutdownCoordinator::default();

        let metrics_translator = OtlpMetricsTranslator::new(self.translator_config, self.context_resolver);

        let thread_pool_handle = context.topology_context().global_thread_pool().clone();

        // Spawn the converter task. This task is shared by both servers.
        thread_pool_handle.spawn_traced_named(
            "otlp-resource-converter",
            run_converter(
                rx,
                context.clone(),
                converter_shutdown_coordinator.register(),
                metrics_translator,
                self.metrics,
            ),
        );

        // Create and spawn the gRPC server.
        let grpc_metrics_server = MetricsServiceServer::new(GrpcService::new(tx.clone(), memory_limiter.clone()))
            .max_decoding_message_size(self.grpc_max_recv_msg_size_bytes);
        let grpc_logs_server = LogsServiceServer::new(GrpcService::new(tx.clone(), memory_limiter.clone()))
            .max_decoding_message_size(self.grpc_max_recv_msg_size_bytes);
        let grpc_server = Server::builder()
            .add_service(grpc_metrics_server)
            .add_service(grpc_logs_server);

        let grpc_socket_addr = self
            .grpc_endpoint
            .as_local_connect_addr()
            .ok_or_else(|| generic_error!("OTLP gRPC endpoint is not a local TCP address."))?;
        thread_pool_handle.spawn_traced_named("otlp-grpc-server", grpc_server.serve(grpc_socket_addr));
        debug!(endpoint = %self.grpc_endpoint, "OTLP gRPC server started.");

        // Create and spawn the HTTP server.
        let service = TowerToHyperService::new(
            Router::new()
                .route("/v1/metrics", post(http_metric_handler))
                .route("/v1/logs", post(http_logs_handler))
                .with_state((tx, memory_limiter.clone())),
        );
        let http_listener = ConnectionOrientedListener::from_listen_address(self.http_endpoint)
            .await
            .map_err(|e| generic_error!("Failed to create OTLP HTTP listener: {}", e))?;
        let http_server = HttpServer::from_listener(http_listener, service);
        let (http_shutdown, mut http_error) = http_server.listen();

        health.mark_ready();
        debug!("OTLP source started.");

        // Wait for the global shutdown signal, then notify converter to shutdown.
        loop {
            select! {
                _ = &mut global_shutdown => {
                    debug!("Received shutdown signal.");
                    break
                },
                error = &mut http_error => {
                    if let Some(error) = error {
                        debug!(%error, "HTTP server error.");
                    }
                    break;
                },
                _ = health.live() => continue,
            }
        }

        debug!("Stopping OTLP source...");

        http_shutdown.shutdown();
        converter_shutdown_coordinator.shutdown().await;

        debug!("OTLP source stopped.");

        Ok(())
    }
}

async fn http_logs_handler(
    State((tx, memory_limiter)): State<(mpsc::Sender<OtlpResource>, MemoryLimiter)>, body: Bytes,
) -> (StatusCode, &'static str) {
    memory_limiter.wait_for_capacity().await;
    match ExportLogsServiceRequest::decode(body) {
        Ok(request) => {
            for resource_logs in request.resource_logs {
                if tx.send(OtlpResource::Logs(resource_logs)).await.is_err() {
                    error!("Failed to send resource logs to converter; channel is closed.");
                    return (StatusCode::INTERNAL_SERVER_ERROR, "Internal processing channel closed.");
                }
            }
            (StatusCode::OK, "OK")
        }
        Err(e) => {
            error!(error = %e, "Failed to decode OTLP protobuf request.");
            (StatusCode::BAD_REQUEST, "Bad Request: Invalid protobuf.")
        }
    }
}

async fn http_metric_handler(
    State((tx, memory_limiter)): State<(mpsc::Sender<OtlpResource>, MemoryLimiter)>, body: Bytes,
) -> (StatusCode, &'static str) {
    memory_limiter.wait_for_capacity().await;
    match ExportMetricsServiceRequest::decode(body) {
        Ok(request) => {
            for resource_metrics in request.resource_metrics {
                if tx.send(OtlpResource::Metrics(resource_metrics)).await.is_err() {
                    error!("Failed to send resource metrics to converter; channel is closed.");
                    return (StatusCode::INTERNAL_SERVER_ERROR, "Internal processing channel closed.");
                }
            }
            (StatusCode::OK, "OK")
        }
        Err(e) => {
            error!(error = %e, "Failed to decode OTLP protobuf request.");
            (StatusCode::BAD_REQUEST, "Bad Request: Invalid protobuf.")
        }
    }
}

struct GrpcService {
    sender: mpsc::Sender<OtlpResource>,
    memory_limiter: MemoryLimiter,
}

impl GrpcService {
    fn new(sender: mpsc::Sender<OtlpResource>, memory_limiter: MemoryLimiter) -> Self {
        Self { sender, memory_limiter }
    }
}

#[async_trait]
impl MetricsService for GrpcService {
    async fn export(
        &self, request: Request<ExportMetricsServiceRequest>,
    ) -> Result<Response<ExportMetricsServiceResponse>, Status> {
        self.memory_limiter.wait_for_capacity().await;
        let request = request.into_inner();

        for resource_metrics in request.resource_metrics {
            if self.sender.send(OtlpResource::Metrics(resource_metrics)).await.is_err() {
                error!("Failed to send resource metrics to converter; channel is closed.");
                return Err(Status::internal("Internal processing channel closed."));
            }
        }

        Ok(Response::new(ExportMetricsServiceResponse { partial_success: None }))
    }
}

#[async_trait]
impl LogsService for GrpcService {
    async fn export(
        &self, request: Request<ExportLogsServiceRequest>,
    ) -> Result<Response<ExportLogsServiceResponse>, Status> {
        self.memory_limiter.wait_for_capacity().await;
        let request = request.into_inner();

        for resource_logs in request.resource_logs {
            if self.sender.send(OtlpResource::Logs(resource_logs)).await.is_err() {
                error!("Failed to send resource logs to converter; channel is closed.");
                return Err(Status::internal("Internal processing channel closed."));
            }
        }

        Ok(Response::new(ExportLogsServiceResponse { partial_success: None }))
    }
}

async fn dispatch_events(mut events: EventsBuffer, source_context: &SourceContext) {
    if events.is_empty() {
        return;
    }

    if events.has_event_type(EventType::Log) {
        let mut buffered_dispatcher = source_context
            .dispatcher()
            .buffered_named("logs")
            .expect("logs output should exist");

        for log_event in events.extract(Event::is_log) {
            if let Err(e) = buffered_dispatcher.push(log_event).await {
                error!(error = %e, "Failed to dispatch log(s).");
            }
        }

        if let Err(e) = buffered_dispatcher.flush().await {
            error!(error = %e, "Failed to flush log(s).");
        }
    }

    let len = events.len();
    if let Err(e) = source_context.dispatcher().dispatch_named("metrics", events).await {
        error!(error = %e, "Failed to dispatch metric events.");
    } else {
        debug!(events_len = len, "Dispatched metric events.");
    }
}

async fn run_converter(
    mut receiver: mpsc::Receiver<OtlpResource>, source_context: SourceContext, shutdown_handle: DynamicShutdownHandle,
    mut metrics_translator: OtlpMetricsTranslator, metrics: Metrics,
) {
    tokio::pin!(shutdown_handle);
    debug!("OTLP resource converter task started.");

    // Set a buffer flush interval of 100ms, which will ensure we always flush buffered events at least every 100ms if
    // we're otherwise idle and not receiving packets from the client.
    let mut buffer_flush = interval(Duration::from_millis(100));
    buffer_flush.set_missed_tick_behavior(MissedTickBehavior::Delay);

    let mut event_buffer_manager = EventBufferManager::default();

    loop {
        select! {
            Some(otlp_resource) = receiver.recv() => {
                match otlp_resource {
                    OtlpResource::Metrics(resource_metrics) => {
                        match metrics_translator.map_metrics(resource_metrics, &metrics) {
                            Ok(events) => {
                                for event in events {
                                    if let Some(event_buffer) = event_buffer_manager.try_push(event) {
                                        dispatch_events(event_buffer, &source_context).await;
                                    }
                                }
                            }
                            Err(e) => {
                                error!(error = %e, "Failed to handle resource metrics.");
                            }
                        }
                    }
                    OtlpResource::Logs(resource_logs) => {
                        let translator = OtlpLogsTranslator::from_resource_logs(resource_logs);
                        for log_event in translator {
                            metrics.logs_received().increment(1);

                            if let Some(event_buffer) = event_buffer_manager.try_push(log_event) {
                                dispatch_events(event_buffer, &source_context).await;
                            }
                        }
                    }
                }
            },
            _ = buffer_flush.tick() => {
                if let Some(event_buffer) = event_buffer_manager.consume() {
                    dispatch_events(event_buffer, &source_context).await;
                }
            },
            _ = &mut shutdown_handle => {
                debug!("Converter task received shutdown signal.");
                break;
            }
        }
    }

    if let Some(event_buffer) = event_buffer_manager.consume() {
        dispatch_events(event_buffer, &source_context).await;
    }

    debug!("OTLP resource converter task stopped.");
}
