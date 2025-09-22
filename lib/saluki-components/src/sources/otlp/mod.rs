use std::sync::LazyLock;
use std::time::Duration;

use ::metrics::Counter;
use async_trait::async_trait;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::post;
use axum::Router;
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder, MemoryLimiter};
use otlp_protos::opentelemetry::proto::collector::metrics::v1::metrics_service_server::{
    MetricsService, MetricsServiceServer,
};
use otlp_protos::opentelemetry::proto::collector::metrics::v1::{
    ExportMetricsServiceRequest, ExportMetricsServiceResponse,
};
use otlp_protos::opentelemetry::proto::metrics::v1::ResourceMetrics as OtlpResourceMetrics;
use prost::Message;
use saluki_common::task::HandleExt as _;
use saluki_config::GenericConfiguration;
use saluki_context::{ContextResolver, ContextResolverBuilder};
use saluki_core::observability::ComponentMetricsExt;
use saluki_core::topology::interconnect::EventBufferManager;
use saluki_core::topology::shutdown::{DynamicShutdownCoordinator, DynamicShutdownHandle};
use saluki_core::{
    components::{
        sources::{Source, SourceBuilder, SourceContext},
        ComponentContext,
    },
    data_model::event::EventType,
    topology::{EventsBuffer, OutputDefinition},
};
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
mod metrics;

use self::metrics::translator::OtlpTranslator;

/// Configuration for the OTLP source.
#[derive(Deserialize, Debug, Default)]
pub struct OtlpConfiguration {
    #[serde(default)]
    receiver: Receiver,
    #[serde(default)]
    metrics: MetricsConfig,
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
}

pub struct Metrics {
    metrics_received: Counter,
}

impl Metrics {
    fn metrics_received(&self) -> &Counter {
        &self.metrics_received
    }
}

fn build_metrics(component_context: &ComponentContext) -> Metrics {
    let builder = MetricsBuilder::from_component_context(component_context);

    Metrics {
        metrics_received: builder
            .register_debug_counter_with_tags("component_events_received_total", [("message_type", "otlp_metrics")]),
    }
}

#[async_trait]
impl SourceBuilder for OtlpConfiguration {
    fn outputs(&self) -> &[OutputDefinition] {
        static OUTPUTS: LazyLock<Vec<OutputDefinition>> =
            LazyLock::new(|| vec![OutputDefinition::named_output("metrics", EventType::Metric)]);

        &OUTPUTS
    }

    async fn build(&self, context: ComponentContext) -> Result<Box<dyn Source + Send>, GenericError> {
        if !self.metrics.enabled {
            return Err(generic_error!("OTLP metrics support is disabled."));
        }

        let grpc_listen_str = format!(
            "{}://{}",
            self.receiver.protocols.grpc.transport, self.receiver.protocols.grpc.endpoint
        );
        let grpc_endpoint = ListenAddress::try_from(grpc_listen_str.as_str())
            .map_err(|e| generic_error!("Invalid gRPC endpoint address '{}': {}", grpc_listen_str, e))?;

        // Enforce the current limitation that we only support TCP for gRPC.
        if !matches!(grpc_endpoint, ListenAddress::Tcp(_)) {
            return Err(generic_error!("Only 'tcp' transport is supported for OTLP gRPC"));
        }

        let http_socket_addr = self.receiver.protocols.http.endpoint.parse().map_err(|e| {
            generic_error!(
                "Invalid HTTP endpoint address '{}': {}",
                self.receiver.protocols.http.endpoint,
                e
            )
        })?;

        let context_resolver = ContextResolverBuilder::from_name(format!("{}/otlp", context.component_id()))?.build();
        let translator_config = metrics::config::OtlpTranslatorConfig::default().with_remapping(true);
        let grpc_max_recv_msg_size_bytes = self.receiver.protocols.grpc.max_recv_msg_size_mib as usize * 1024 * 1024;
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
    metrics: Metrics,
}

#[async_trait]
impl Source for Otlp {
    async fn run(self: Box<Self>, mut context: SourceContext) -> Result<(), GenericError> {
        let mut global_shutdown = context.take_shutdown_handle();
        let mut health = context.take_health_handle();
        let memory_limiter = context.topology_context().memory_limiter();

        // Create the internal channel for decoupling the servers from the converter.
        let (tx, rx) = mpsc::channel(1024);
        let mut converter_shutdown_coordinator = DynamicShutdownCoordinator::default();

        let translator = OtlpTranslator::new(self.translator_config, self.context_resolver);

        let thread_pool_handle = context.topology_context().global_thread_pool().clone();

        // Spawn the converter task. This task is shared by both servers.
        thread_pool_handle.spawn_traced_named(
            "otlp-metric-converter",
            run_converter(
                rx,
                context.clone(),
                converter_shutdown_coordinator.register(),
                translator,
                self.metrics,
            ),
        );

        // Create and spawn the gRPC server.
        let grpc_service = GrpcService::new(tx.clone(), memory_limiter.clone());
        let grpc_server =
            MetricsServiceServer::new(grpc_service).max_decoding_message_size(self.grpc_max_recv_msg_size_bytes);
        let grpc_server = Server::builder().add_service(grpc_server);

        let grpc_socket_addr = self
            .grpc_endpoint
            .as_local_connect_addr()
            .ok_or_else(|| generic_error!("OTLP gRPC endpoint is not a local TCP address."))?;
        thread_pool_handle.spawn_traced_named("otlp-grpc-server", grpc_server.serve(grpc_socket_addr));
        debug!(endpoint = %self.grpc_endpoint, "OTLP gRPC server started.");

        // Create and spawn the HTTP server.
        let service = TowerToHyperService::new(
            Router::new()
                .route("/v1/metrics", post(http_handler))
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

async fn http_handler(
    State((tx, memory_limiter)): State<(mpsc::Sender<OtlpResourceMetrics>, MemoryLimiter)>, body: Bytes,
) -> (StatusCode, &'static str) {
    memory_limiter.wait_for_capacity().await;
    match ExportMetricsServiceRequest::decode(body) {
        Ok(request) => {
            for resource_metrics in request.resource_metrics {
                if tx.send(resource_metrics).await.is_err() {
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
    sender: mpsc::Sender<OtlpResourceMetrics>,
    memory_limiter: MemoryLimiter,
}

impl GrpcService {
    fn new(sender: mpsc::Sender<OtlpResourceMetrics>, memory_limiter: MemoryLimiter) -> Self {
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
            if self.sender.send(resource_metrics).await.is_err() {
                error!("Failed to send resource metrics to converter; channel is closed.");
                return Err(Status::internal("Internal processing channel closed."));
            }
        }

        Ok(Response::new(ExportMetricsServiceResponse { partial_success: None }))
    }
}

async fn dispatch_events(events: EventsBuffer, source_context: &SourceContext) {
    if events.is_empty() {
        return;
    }

    let len = events.len();
    if let Err(e) = source_context.dispatcher().dispatch_named("metrics", events).await {
        error!(error = %e, "Failed to dispatch metric events.");
    } else {
        debug!(events_len = len, "Dispatched metric events.");
    }
}

async fn run_converter(
    mut receiver: mpsc::Receiver<OtlpResourceMetrics>, source_context: SourceContext,
    shutdown_handle: DynamicShutdownHandle, mut translator: OtlpTranslator, metrics: Metrics,
) {
    tokio::pin!(shutdown_handle);
    debug!("OTLP metric converter task started.");

    // Set a buffer flush interval of 100ms, which will ensure we always flush buffered events at least every 100ms if
    // we're otherwise idle and not receiving packets from the client.
    let mut buffer_flush = interval(Duration::from_millis(100));
    buffer_flush.set_missed_tick_behavior(MissedTickBehavior::Delay);

    let mut event_buffer_manager = EventBufferManager::default();

    loop {
        select! {
            Some(resource_metrics) = receiver.recv() => {
                match translator.map_metrics(resource_metrics, &metrics) {
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

    debug!("OTLP metric converter task stopped.");
}
