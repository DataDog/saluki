//! Common OTLP server infrastructure.
//!
//! Provides shared server setup code for both OTLP receiver (proxy mode) and OTLP source (translation mode).

pub mod config;
pub mod util;

use std::sync::Arc;

use ::metrics::Counter;
use async_trait::async_trait;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::post;
use axum::Router;
use memory_accounting::MemoryLimiter;
use otlp_protos::opentelemetry::proto::collector::logs::v1::logs_service_server::{LogsService, LogsServiceServer};
use otlp_protos::opentelemetry::proto::collector::logs::v1::{ExportLogsServiceRequest, ExportLogsServiceResponse};
use otlp_protos::opentelemetry::proto::collector::metrics::v1::metrics_service_server::{
    MetricsService, MetricsServiceServer,
};
use otlp_protos::opentelemetry::proto::collector::metrics::v1::{
    ExportMetricsServiceRequest, ExportMetricsServiceResponse,
};
use otlp_protos::opentelemetry::proto::collector::trace::v1::trace_service_server::{TraceService, TraceServiceServer};
use otlp_protos::opentelemetry::proto::collector::trace::v1::{ExportTraceServiceRequest, ExportTraceServiceResponse};
use prost::Message;
use saluki_common::task::HandleExt as _;
use saluki_core::components::ComponentContext;
use saluki_core::observability::ComponentMetricsExt;
use saluki_error::{generic_error, GenericError};
use saluki_io::net::listener::ConnectionOrientedListener;
use saluki_io::net::server::http::{ErrorHandle, HttpServer, ShutdownHandle};
use saluki_io::net::util::hyper::TowerToHyperService;
use saluki_io::net::ListenAddress;
use saluki_metrics::MetricsBuilder;
use stringtheory::MetaString;
use tokio::runtime::Handle;
use tonic::transport::Server;
use tonic::{Request as TonicRequest, Response, Status};
use tracing::error;

pub const OTLP_METRICS_GRPC_SERVICE_PATH: MetaString =
    MetaString::from_static("/opentelemetry.proto.collector.metrics.v1.MetricsService/Export");
pub const OTLP_LOGS_GRPC_SERVICE_PATH: MetaString =
    MetaString::from_static("/opentelemetry.proto.collector.logs.v1.LogsService/Export");
pub const OTLP_TRACES_GRPC_SERVICE_PATH: MetaString =
    MetaString::from_static("/opentelemetry.proto.collector.trace.v1.TraceService/Export");

#[derive(Clone)]
pub struct Metrics {
    metrics_received: Counter,
    logs_received: Counter,
    bytes_received: Counter,
    spans_received: Counter,
}

impl Metrics {
    pub fn metrics_received(&self) -> &Counter {
        &self.metrics_received
    }

    pub fn logs_received(&self) -> &Counter {
        &self.logs_received
    }

    pub fn spans_received(&self) -> &Counter {
        &self.spans_received
    }

    pub fn bytes_received(&self) -> &Counter {
        &self.bytes_received
    }

    /// Test-only helper to construct a `Metrics` instance.
    #[cfg(test)]
    pub fn for_tests() -> Self {
        Metrics {
            metrics_received: Counter::noop(),
            logs_received: Counter::noop(),
            bytes_received: Counter::noop(),
            spans_received: Counter::noop(),
        }
    }
}

/// Builds the metrics for the OTLP server.
pub fn build_metrics(component_context: &ComponentContext) -> Metrics {
    let builder = MetricsBuilder::from_component_context(component_context);

    Metrics {
        metrics_received: builder
            .register_debug_counter_with_tags("component_events_received_total", [("message_type", "otlp_metrics")]),
        logs_received: builder
            .register_debug_counter_with_tags("component_events_received_total", [("message_type", "otlp_logs")]),
        bytes_received: builder.register_counter_with_tags("component_bytes_received_total", [("source", "otlp")]),
        spans_received: builder
            .register_debug_counter_with_tags("component_events_received_total", [("message_type", "otlp_spans")]),
    }
}

/// Handler for OTLP data.
#[async_trait]
pub trait OtlpHandler: Send + Sync + 'static {
    async fn handle_metrics(&self, body: Bytes) -> Result<(), GenericError>;
    async fn handle_logs(&self, body: Bytes) -> Result<(), GenericError>;
    async fn handle_traces(&self, body: Bytes) -> Result<(), GenericError>;
}

/// OTLP server configuration and setup.
pub struct OtlpServerBuilder {
    http_endpoint: ListenAddress,
    grpc_endpoint: ListenAddress,
    grpc_max_recv_msg_size_bytes: usize,
}

impl OtlpServerBuilder {
    /// Creates a new OTLP server builder.
    pub fn new(
        http_endpoint: ListenAddress, grpc_endpoint: ListenAddress, grpc_max_recv_msg_size_bytes: usize,
    ) -> Self {
        Self {
            http_endpoint,
            grpc_endpoint,
            grpc_max_recv_msg_size_bytes,
        }
    }

    /// Builds and starts the OTLP servers (HTTP and gRPC).
    ///
    /// Returns the HTTP server shutdown handle and error handle.
    pub async fn build<H: OtlpHandler>(
        self, handler: H, memory_limiter: MemoryLimiter, thread_pool_handle: Handle, metrics: Metrics,
    ) -> Result<(ShutdownHandle, ErrorHandle), GenericError> {
        let otlp_handler = Arc::new(handler);
        let metrics = Arc::new(metrics);

        // Create and spawn the gRPC server.
        //
        // TODO: Properly update the `bytes_received` metric for gRPC payloads.
        let grpc_metrics_server =
            MetricsServiceServer::new(GrpcServiceImpl::new(otlp_handler.clone(), memory_limiter.clone()))
                .max_decoding_message_size(self.grpc_max_recv_msg_size_bytes);

        let grpc_logs_server =
            LogsServiceServer::new(GrpcServiceImpl::new(otlp_handler.clone(), memory_limiter.clone()))
                .max_decoding_message_size(self.grpc_max_recv_msg_size_bytes);

        let grpc_traces_server =
            TraceServiceServer::new(GrpcServiceImpl::new(otlp_handler.clone(), memory_limiter.clone()))
                .max_decoding_message_size(self.grpc_max_recv_msg_size_bytes);

        let grpc_server = Server::builder()
            .add_service(grpc_metrics_server)
            .add_service(grpc_logs_server)
            .add_service(grpc_traces_server);

        let grpc_socket_addr = match self.grpc_endpoint {
            ListenAddress::Tcp(addr) => addr,
            _ => return Err(generic_error!("OTLP gRPC endpoint must be a TCP address.")),
        };
        thread_pool_handle.spawn_traced_named("otlp-grpc-server", grpc_server.serve(grpc_socket_addr));

        // Create and spawn the HTTP server.
        let service = TowerToHyperService::new(
            Router::new()
                .route("/v1/metrics", post(http_metrics_handler::<H>))
                .route("/v1/logs", post(http_logs_handler::<H>))
                .route("/v1/traces", post(http_traces_handler::<H>))
                .with_state((otlp_handler, memory_limiter, metrics)),
        );

        let http_listener = ConnectionOrientedListener::from_listen_address(self.http_endpoint)
            .await
            .map_err(|e| generic_error!("Failed to create OTLP HTTP listener: {}", e))?;

        let (http_shutdown, http_error) = HttpServer::from_listener(http_listener, service)
            .with_executor(thread_pool_handle)
            .listen();

        Ok((http_shutdown, http_error))
    }
}

/// HTTP handler for OTLP metrics requests.
async fn http_metrics_handler<H: OtlpHandler>(
    State((handler, memory_limiter, metrics)): State<(Arc<H>, MemoryLimiter, Arc<Metrics>)>, body: Bytes,
) -> (StatusCode, &'static str) {
    memory_limiter.wait_for_capacity().await;

    metrics.bytes_received().increment(body.len() as u64);

    match handler.handle_metrics(body).await {
        Ok(()) => (StatusCode::OK, "OK"),
        Err(e) => {
            error!(error = %e, "Failed to handle OTLP metrics.");
            (StatusCode::INTERNAL_SERVER_ERROR, "Internal processing error")
        }
    }
}

/// HTTP handler for OTLP logs requests.
async fn http_logs_handler<H: OtlpHandler>(
    State((handler, memory_limiter, metrics)): State<(Arc<H>, MemoryLimiter, Arc<Metrics>)>, body: Bytes,
) -> (StatusCode, &'static str) {
    memory_limiter.wait_for_capacity().await;

    metrics.bytes_received().increment(body.len() as u64);

    match handler.handle_logs(body).await {
        Ok(()) => (StatusCode::OK, "OK"),
        Err(e) => {
            error!(error = %e, "Failed to handle OTLP logs.");
            (StatusCode::INTERNAL_SERVER_ERROR, "Internal processing error")
        }
    }
}

/// HTTP handler for OTLP traces requests.
async fn http_traces_handler<H: OtlpHandler>(
    State((handler, memory_limiter, metrics)): State<(Arc<H>, MemoryLimiter, Arc<Metrics>)>, body: Bytes,
) -> (StatusCode, &'static str) {
    memory_limiter.wait_for_capacity().await;

    metrics.bytes_received().increment(body.len() as u64);

    match handler.handle_traces(body).await {
        Ok(()) => (StatusCode::OK, "OK"),
        Err(e) => {
            error!(error = %e, "Failed to handle OTLP traces.");
            (StatusCode::INTERNAL_SERVER_ERROR, "Internal processing error")
        }
    }
}

/// gRPC service implementation that delegates to the handler.
struct GrpcServiceImpl<H> {
    handler: Arc<H>,
    memory_limiter: MemoryLimiter,
}

impl<H> GrpcServiceImpl<H> {
    fn new(handler: Arc<H>, memory_limiter: MemoryLimiter) -> Self {
        Self {
            handler,
            memory_limiter,
        }
    }
}

impl<H> Clone for GrpcServiceImpl<H> {
    fn clone(&self) -> Self {
        Self {
            handler: self.handler.clone(),
            memory_limiter: self.memory_limiter.clone(),
        }
    }
}

#[async_trait]
impl<H: OtlpHandler> MetricsService for GrpcServiceImpl<H> {
    async fn export(
        &self, request: TonicRequest<ExportMetricsServiceRequest>,
    ) -> Result<Response<ExportMetricsServiceResponse>, Status> {
        self.memory_limiter.wait_for_capacity().await;

        let raw_bytes = request.into_inner().encode_to_vec();

        match self.handler.handle_metrics(Bytes::from(raw_bytes)).await {
            Ok(()) => Ok(Response::new(ExportMetricsServiceResponse { partial_success: None })),
            Err(e) => {
                error!(error = %e, "Failed to handle OTLP metrics.");
                Err(Status::internal("Internal processing error"))
            }
        }
    }
}

#[async_trait]
impl<H: OtlpHandler> LogsService for GrpcServiceImpl<H> {
    async fn export(
        &self, request: TonicRequest<ExportLogsServiceRequest>,
    ) -> Result<Response<ExportLogsServiceResponse>, Status> {
        self.memory_limiter.wait_for_capacity().await;

        let raw_bytes = request.into_inner().encode_to_vec();

        match self.handler.handle_logs(Bytes::from(raw_bytes)).await {
            Ok(()) => Ok(Response::new(ExportLogsServiceResponse { partial_success: None })),
            Err(e) => {
                error!(error = %e, "Failed to handle OTLP logs.");
                Err(Status::internal("Internal processing error"))
            }
        }
    }
}

#[async_trait]
impl<H: OtlpHandler> TraceService for GrpcServiceImpl<H> {
    async fn export(
        &self, request: TonicRequest<ExportTraceServiceRequest>,
    ) -> Result<Response<ExportTraceServiceResponse>, Status> {
        self.memory_limiter.wait_for_capacity().await;

        let raw_bytes = request.into_inner().encode_to_vec();

        match self.handler.handle_traces(Bytes::from(raw_bytes)).await {
            Ok(()) => Ok(Response::new(ExportTraceServiceResponse { partial_success: None })),
            Err(e) => {
                error!(error = %e, "Failed to handle OTLP traces.");
                Err(Status::internal("Internal processing error"))
            }
        }
    }
}
