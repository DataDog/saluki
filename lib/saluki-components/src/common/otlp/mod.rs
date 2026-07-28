//! Common OTLP server infrastructure.
//!
//! Provides shared server setup code for both OTLP receiver (proxy mode) and OTLP source (translation mode).

pub mod attributes;
pub mod config;
pub mod origin;
pub mod semantics;
pub mod traces;
pub mod util;

use std::{io, sync::Arc, time::Duration};

use ::metrics::Counter;
use agent_data_plane_config::domains::otlp::TlsConfig;
use async_trait::async_trait;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::post;
use axum::Router;
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
use rustls::{
    pki_types::{pem::PemObject as _, CertificateDer, PrivateKeyDer},
    server::WebPkiClientVerifier,
    RootCertStore, ServerConfig,
};
use saluki_common::sync::shutdown::ShutdownCoordinator;
use saluki_common::task::HandleExt as _;
use saluki_core::accounting::MemoryLimiter;
use saluki_core::components::ComponentContext;
use saluki_core::observability::ComponentMetricsExt;
use saluki_error::{generic_error, GenericError};
use saluki_io::net::listener::ConnectionOrientedListener;
use saluki_io::net::server::http::{ErrorHandle, HttpServer};
use saluki_io::net::util::hyper::TowerToHyperService;
use saluki_io::net::ListenAddress;
use saluki_metrics::MetricsBuilder;
use saluki_tls::ensure_server_config_fips_compliant;
use stringtheory::MetaString;
use tokio::runtime::Handle;
use tokio::sync::mpsc;
use tokio::time::timeout;
use tokio_rustls::TlsAcceptor;
use tonic::transport::Server;
use tonic::{Request as TonicRequest, Response, Status};
use tracing::error;

const OTLP_GRPC_TLS_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

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
            .register_counter_with_tags("component_events_received_total", [("message_type", "otlp_metrics")]),
        logs_received: builder
            .register_counter_with_tags("component_events_received_total", [("message_type", "otlp_logs")]),
        bytes_received: builder.register_counter_with_tags("component_bytes_received_total", [("source", "otlp")]),
        spans_received: builder
            .register_counter_with_tags("component_events_received_total", [("message_type", "otlp_spans")]),
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
    http_tls_config: Option<ServerConfig>,
    grpc_tls_config: Option<ServerConfig>,
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
            http_tls_config: None,
            grpc_tls_config: None,
            grpc_max_recv_msg_size_bytes,
        }
    }

    /// Configures TLS for the OTLP HTTP and gRPC servers.
    pub fn with_tls_configs(
        mut self, http_tls_config: Option<ServerConfig>, grpc_tls_config: Option<ServerConfig>,
    ) -> Self {
        self.http_tls_config = http_tls_config;
        self.grpc_tls_config = grpc_tls_config;
        self
    }

    /// Builds and starts the OTLP servers (HTTP and gRPC).
    ///
    /// Returns the HTTP server shutdown handle and error handle.
    pub async fn build<H: OtlpHandler>(
        self, handler: H, memory_limiter: MemoryLimiter, thread_pool_handle: Handle, metrics: Metrics,
    ) -> Result<(ShutdownCoordinator, ErrorHandle), GenericError> {
        let otlp_handler = Arc::new(handler);
        let metrics = Arc::new(metrics);

        // Create and spawn the gRPC server.
        let grpc_metrics_server = MetricsServiceServer::new(GrpcServiceImpl::new(
            otlp_handler.clone(),
            memory_limiter.clone(),
            metrics.clone(),
        ))
        .max_decoding_message_size(self.grpc_max_recv_msg_size_bytes);

        let grpc_logs_server = LogsServiceServer::new(GrpcServiceImpl::new(
            otlp_handler.clone(),
            memory_limiter.clone(),
            metrics.clone(),
        ))
        .max_decoding_message_size(self.grpc_max_recv_msg_size_bytes);

        let grpc_traces_server = TraceServiceServer::new(GrpcServiceImpl::new(
            otlp_handler.clone(),
            memory_limiter.clone(),
            metrics.clone(),
        ))
        .max_decoding_message_size(self.grpc_max_recv_msg_size_bytes);

        let grpc_server = Server::builder()
            .add_service(grpc_metrics_server)
            .add_service(grpc_logs_server)
            .add_service(grpc_traces_server);

        let grpc_socket_addr = match self.grpc_endpoint {
            ListenAddress::Tcp(addr) => addr,
            _ => return Err(generic_error!("OTLP gRPC endpoint must be a TCP address.")),
        };

        let grpc_listener = tokio::net::TcpListener::bind(grpc_socket_addr)
            .await
            .map_err(|e| generic_error!("Failed to bind OTLP gRPC listener on '{}': {}", grpc_socket_addr, e))?;
        match self.grpc_tls_config {
            Some(mut tls_config) => {
                // gRPC over TLS requires ALPN negotiation of HTTP/2. Tonic configures this when it owns TLS setup;
                // configure it explicitly because this server accepts Rustls streams directly.
                tls_config.alpn_protocols = vec![b"h2".to_vec()];
                let tls_acceptor = TlsAcceptor::from(Arc::new(tls_config));
                let (incoming_tx, incoming_rx) = mpsc::channel(1024);
                let handshake_executor = thread_pool_handle.clone();

                thread_pool_handle.spawn_traced_named("otlp-grpc-tls-acceptor", async move {
                    loop {
                        let (stream, _) = match grpc_listener.accept().await {
                            Ok(stream) => stream,
                            Err(error) => {
                                let _ = incoming_tx.send(Err(error)).await;
                                break;
                            }
                        };

                        let acceptor = tls_acceptor.clone();
                        let incoming_tx = incoming_tx.clone();
                        handshake_executor.spawn_traced_named("otlp-grpc-tls-handshake", async move {
                            match timeout(OTLP_GRPC_TLS_HANDSHAKE_TIMEOUT, acceptor.accept(stream)).await {
                                Ok(Ok(stream)) => {
                                    let _ = incoming_tx.send(Ok::<_, io::Error>(stream)).await;
                                }
                                Ok(Err(error)) => error!(%error, "Failed to complete OTLP gRPC TLS handshake."),
                                Err(_) => error!("Timed out completing OTLP gRPC TLS handshake."),
                            }
                        });
                    }
                });

                let grpc_incoming = futures::stream::unfold(incoming_rx, |mut rx| async {
                    rx.recv().await.map(|stream| (stream, rx))
                });
                thread_pool_handle
                    .spawn_traced_named("otlp-grpc-server", grpc_server.serve_with_incoming(grpc_incoming));
            }
            None => {
                let grpc_incoming = tonic::transport::server::TcpIncoming::from(grpc_listener);
                thread_pool_handle
                    .spawn_traced_named("otlp-grpc-server", grpc_server.serve_with_incoming(grpc_incoming));
            }
        }

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

        let mut http_server = HttpServer::from_listener(http_listener, service).with_executor(thread_pool_handle);
        if let Some(tls_config) = self.http_tls_config {
            http_server = http_server.with_tls_config(tls_config);
        }
        let (http_shutdown_coordinator, http_error) = http_server.listen();

        Ok((http_shutdown_coordinator, http_error))
    }
}

/// Builds a server TLS configuration from an OTLP receiver's TLS settings.
///
/// An empty configuration leaves TLS disabled. A configured certificate and key enable TLS; an optional client CA file
/// additionally enables mutual TLS by requiring and verifying client certificates.
pub async fn build_server_config(tls: &TlsConfig) -> Result<Option<ServerConfig>, GenericError> {
    if !tls.is_configured()? {
        return Ok(None);
    }

    let certificate_pem = tokio::fs::read(&tls.cert_file).await.map_err(|error| {
        generic_error!(
            "Failed to read OTLP TLS certificate file '{}': {}",
            tls.cert_file,
            error
        )
    })?;
    let private_key_pem = tokio::fs::read(&tls.key_file)
        .await
        .map_err(|error| generic_error!("Failed to read OTLP TLS private key file '{}': {}", tls.key_file, error))?;

    let certificate_chain = CertificateDer::pem_slice_iter(&certificate_pem)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| {
            generic_error!(
                "Failed to parse OTLP TLS certificate file '{}': {}",
                tls.cert_file,
                error
            )
        })?;
    if certificate_chain.is_empty() {
        return Err(generic_error!(
            "OTLP TLS certificate file '{}' contains no certificates.",
            tls.cert_file
        ));
    }
    let private_key = PrivateKeyDer::from_pem_slice(&private_key_pem)
        .map_err(|error| {
            generic_error!(
                "Failed to parse OTLP TLS private key file '{}': {}",
                tls.key_file,
                error
            )
        })?
        .clone_key();

    // The Collector loads `ca_file` into `tls.Config.RootCAs`. A pure inbound receiver does not consult those roots,
    // but loading and validating the file preserves the Collector's configuration behavior.
    if !tls.ca_file.is_empty() {
        let ca_pem = tokio::fs::read(&tls.ca_file)
            .await
            .map_err(|error| generic_error!("Failed to read OTLP TLS CA file '{}': {}", tls.ca_file, error))?;
        let ca_certificates = CertificateDer::pem_slice_iter(&ca_pem)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| generic_error!("Failed to parse OTLP TLS CA file '{}': {}", tls.ca_file, error))?;
        let mut roots = RootCertStore::empty();
        for certificate in ca_certificates {
            roots.add(certificate).map_err(|error| {
                generic_error!(
                    "Failed to add a certificate from OTLP TLS CA file '{}': {}",
                    tls.ca_file,
                    error
                )
            })?;
        }
        if roots.is_empty() {
            return Err(generic_error!(
                "OTLP TLS CA file '{}' contains no certificates.",
                tls.ca_file
            ));
        }
    }

    let mut config = if tls.client_ca_file.is_empty() {
        ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(certificate_chain, private_key)
            .map_err(|error| generic_error!("Failed to build OTLP TLS server configuration: {}", error))?
    } else {
        let client_ca_pem = tokio::fs::read(&tls.client_ca_file).await.map_err(|error| {
            generic_error!(
                "Failed to read OTLP TLS client CA file '{}': {}",
                tls.client_ca_file,
                error
            )
        })?;
        let client_certificates = CertificateDer::pem_slice_iter(&client_ca_pem)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| {
                generic_error!(
                    "Failed to parse OTLP TLS client CA file '{}': {}",
                    tls.client_ca_file,
                    error
                )
            })?;
        let mut client_roots = RootCertStore::empty();
        for certificate in client_certificates {
            client_roots.add(certificate).map_err(|error| {
                generic_error!(
                    "Failed to add a certificate from OTLP TLS client CA file '{}': {}",
                    tls.client_ca_file,
                    error
                )
            })?;
        }
        if client_roots.is_empty() {
            return Err(generic_error!(
                "OTLP TLS client CA file '{}' contains no certificates.",
                tls.client_ca_file
            ));
        }
        let client_verifier = WebPkiClientVerifier::builder(Arc::new(client_roots))
            .build()
            .map_err(|error| generic_error!("Failed to build OTLP TLS client verifier: {}", error))?;
        ServerConfig::builder()
            .with_client_cert_verifier(client_verifier)
            .with_single_cert(certificate_chain, private_key)
            .map_err(|error| generic_error!("Failed to build OTLP mutual TLS server configuration: {}", error))?
    };

    ensure_server_config_fips_compliant(&mut config)?;
    Ok(Some(config))
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
    metrics: Arc<Metrics>,
}

impl<H> GrpcServiceImpl<H> {
    fn new(handler: Arc<H>, memory_limiter: MemoryLimiter, metrics: Arc<Metrics>) -> Self {
        Self {
            handler,
            memory_limiter,
            metrics,
        }
    }
}

impl<H> Clone for GrpcServiceImpl<H> {
    fn clone(&self) -> Self {
        Self {
            handler: self.handler.clone(),
            memory_limiter: self.memory_limiter.clone(),
            metrics: self.metrics.clone(),
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
        self.metrics.bytes_received().increment(raw_bytes.len() as u64);

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
        self.metrics.bytes_received().increment(raw_bytes.len() as u64);

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
        self.metrics.bytes_received().increment(raw_bytes.len() as u64);

        match self.handler.handle_traces(Bytes::from(raw_bytes)).await {
            Ok(()) => Ok(Response::new(ExportTraceServiceResponse { partial_success: None })),
            Err(e) => {
                error!(error = %e, "Failed to handle OTLP traces.");
                Err(Status::internal("Internal processing error"))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use saluki_core::{accounting::MemoryLimiter, components::ComponentContext};
    use saluki_metrics::test::TestRecorder;

    use super::*;

    struct NoopHandler;

    #[async_trait]
    impl OtlpHandler for NoopHandler {
        async fn handle_metrics(&self, _body: Bytes) -> Result<(), GenericError> {
            Ok(())
        }

        async fn handle_logs(&self, _body: Bytes) -> Result<(), GenericError> {
            Ok(())
        }

        async fn handle_traces(&self, _body: Bytes) -> Result<(), GenericError> {
            Ok(())
        }
    }

    fn assert_bytes_received(recorder: &TestRecorder, expected_size: u64) {
        assert_eq!(
            recorder.counter((
                "component_bytes_received_total",
                &[
                    ("component_id", "otlp_test"),
                    ("component_type", "source"),
                    ("source", "otlp"),
                ]
            )),
            Some(expected_size)
        );
    }

    fn test_component_context() -> ComponentContext {
        ComponentContext::test_source("otlp_test")
    }

    #[tokio::test]
    async fn grpc_metrics_export_updates_bytes_received() {
        let recorder = TestRecorder::default();
        let _local = metrics::set_default_local_recorder(&recorder);

        let metrics = Arc::new(build_metrics(&test_component_context()));
        let service = GrpcServiceImpl::new(Arc::new(NoopHandler), MemoryLimiter::noop(), metrics);
        let request = ExportMetricsServiceRequest {
            resource_metrics: vec![otlp_protos::opentelemetry::proto::metrics::v1::ResourceMetrics::default()],
        };
        let expected_size = request.encode_to_vec().len() as u64;

        MetricsService::export(&service, TonicRequest::new(request))
            .await
            .unwrap();

        assert_bytes_received(&recorder, expected_size);
    }

    #[tokio::test]
    async fn grpc_logs_export_updates_bytes_received() {
        let recorder = TestRecorder::default();
        let _local = metrics::set_default_local_recorder(&recorder);

        let metrics = Arc::new(build_metrics(&test_component_context()));
        let service = GrpcServiceImpl::new(Arc::new(NoopHandler), MemoryLimiter::noop(), metrics);
        let request = ExportLogsServiceRequest {
            resource_logs: vec![otlp_protos::opentelemetry::proto::logs::v1::ResourceLogs::default()],
        };
        let expected_size = request.encode_to_vec().len() as u64;

        LogsService::export(&service, TonicRequest::new(request)).await.unwrap();

        assert_bytes_received(&recorder, expected_size);
    }

    #[tokio::test]
    async fn grpc_traces_export_updates_bytes_received() {
        let recorder = TestRecorder::default();
        let _local = metrics::set_default_local_recorder(&recorder);

        let metrics = Arc::new(build_metrics(&test_component_context()));
        let service = GrpcServiceImpl::new(Arc::new(NoopHandler), MemoryLimiter::noop(), metrics);
        let request = ExportTraceServiceRequest {
            resource_spans: vec![otlp_protos::opentelemetry::proto::trace::v1::ResourceSpans::default()],
        };
        let expected_size = request.encode_to_vec().len() as u64;

        TraceService::export(&service, TonicRequest::new(request))
            .await
            .unwrap();

        assert_bytes_received(&recorder, expected_size);
    }
}
