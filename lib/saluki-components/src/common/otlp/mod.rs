//! Common OTLP server infrastructure.
//!
//! Provides shared server setup code for both OTLP receiver (proxy mode) and OTLP source (translation mode).

pub mod attributes;
pub mod origin;
pub mod semantics;
pub mod traces;
pub mod util;

use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;

use ::metrics::Counter;
use async_trait::async_trait;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderName, Method, StatusCode};
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
use saluki_core::accounting::MemoryLimiter;
use saluki_core::components::{ComponentContext, ComponentSpawner};
use saluki_core::observability::ComponentMetricsExt;
use saluki_error::{ErrorContext as _, GenericError};
use saluki_io::net::server::{grpc::GrpcServer, http::HttpServer};
use saluki_io::net::util::hyper::TowerToHyperService;
use saluki_io::net::ListenAddress;
use saluki_metrics::MetricsBuilder;
use saluki_tls::ServerTLSConfigBuilder;
use stringtheory::MetaString;
use tonic::{Request as TonicRequest, Response, Status};
use tower_http::cors::{AllowOrigin, Any, CorsLayer};
use tracing::error;

pub const OTLP_METRICS_GRPC_SERVICE_PATH: MetaString =
    MetaString::from_static("/opentelemetry.proto.collector.metrics.v1.MetricsService/Export");
pub const OTLP_LOGS_GRPC_SERVICE_PATH: MetaString =
    MetaString::from_static("/opentelemetry.proto.collector.logs.v1.LogsService/Export");
pub const OTLP_TRACES_GRPC_SERVICE_PATH: MetaString =
    MetaString::from_static("/opentelemetry.proto.collector.trace.v1.TraceService/Export");
const IMPLICIT_HEADERS: [HeaderName; 4] = [
    HeaderName::from_static("accept"),
    HeaderName::from_static("accept-language"),
    HeaderName::from_static("content-type"),
    HeaderName::from_static("content-language"),
];

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

/// CORS settings for an OTLP HTTP server.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CorsConfiguration {
    /// Origins allowed to make cross-origin requests.
    pub allowed_origins: Vec<String>,
    /// Request headers allowed in cross-origin requests.
    pub allowed_headers: Vec<String>,
    /// Response headers exposed to browser clients.
    pub exposed_headers: Vec<String>,
    /// Seconds browsers may cache a preflight response.
    pub max_age: u64,
}

/// TLS configuration for an OTLP receiver, loaded from file paths on disk.
///
/// Both `cert_file` and `key_file` are required to enable TLS. When `ca_file` is set, the server requests client
/// certificates and verifies them if presented, but does not require them (optional verification).
#[derive(Clone, Debug)]
pub struct OtlpTlsConfiguration {
    cert_file: PathBuf,
    key_file: PathBuf,
    ca_file: Option<PathBuf>,
}

impl OtlpTlsConfiguration {
    /// Creates a new TLS configuration with the given certificate and key file paths.
    pub fn new(cert_file: PathBuf, key_file: PathBuf) -> Self {
        Self {
            cert_file,
            key_file,
            ca_file: None,
        }
    }

    /// Sets the CA certificate file for optional client certificate verification.
    pub fn with_ca_file(mut self, ca_file: PathBuf) -> Self {
        self.ca_file = Some(ca_file);
        self
    }

    /// Builds a `rustls::ServerConfig` from the configured file paths.
    ///
    /// # Errors
    ///
    /// If the certificate or key files cannot be read or parsed, or if the resulting configuration isn't FIPS
    /// compliant, an error will be returned.
    fn build(self) -> Result<rustls::ServerConfig, GenericError> {
        let mut builder = ServerTLSConfigBuilder::new()
            .with_cert_file(self.cert_file)
            .with_key_file(self.key_file);

        if let Some(ca_file) = self.ca_file {
            builder = builder.with_ca_file(ca_file);
        }

        builder.build()
    }
}

/// OTLP server configuration.
///
/// Holds the raw inputs needed to construct and start the OTLP HTTP and gRPC servers. Call [`build`][Self::build] to
/// validate the configuration and spawn the servers.
pub struct OtlpServerConfiguration {
    http_endpoint: ListenAddress,
    grpc_endpoint: ListenAddress,
    grpc_max_recv_msg_size_bytes: usize,
    cors: CorsConfiguration,
    http_tls: Option<OtlpTlsConfiguration>,
    grpc_tls: Option<OtlpTlsConfiguration>,
}

impl OtlpServerConfiguration {
    /// Creates a new OTLP server configuration.
    pub fn new(
        http_endpoint: ListenAddress, grpc_endpoint: ListenAddress, grpc_max_recv_msg_size_bytes: usize,
    ) -> Self {
        Self {
            http_endpoint,
            grpc_endpoint,
            grpc_max_recv_msg_size_bytes,
            cors: CorsConfiguration::default(),
            http_tls: None,
            grpc_tls: None,
        }
    }

    /// Sets the CORS configuration for the HTTP receiver.
    pub fn with_cors(mut self, cors: CorsConfiguration) -> Self {
        self.cors = cors;
        self
    }

    /// Sets the TLS configuration for the HTTP receiver.
    ///
    /// When set, the HTTP receiver only accepts encrypted TLS connections.
    pub fn with_http_tls(mut self, tls: OtlpTlsConfiguration) -> Self {
        self.http_tls = Some(tls);
        self
    }

    /// Sets the TLS configuration for the gRPC receiver.
    ///
    /// When set, the gRPC receiver only accepts encrypted TLS connections.
    pub fn with_grpc_tls(mut self, tls: OtlpTlsConfiguration) -> Self {
        self.grpc_tls = Some(tls);
        self
    }

    /// Builds and starts the OTLP servers (HTTP and gRPC).
    ///
    /// Both servers run on the shared worker pool, since request handling shouldn't contend with the runtime driving
    /// the topology, and decoding can be compute-heavy for large requests.
    ///
    /// # Errors
    ///
    /// If the gRPC endpoint isn't a TCP or Unix address, the listen addresses can't be bound, the TLS configuration
    /// is invalid, or either server can't be spawned, an error is returned.
    pub async fn build<H: OtlpHandler>(
        self, handler: H, memory_limiter: MemoryLimiter, metrics: Metrics, spawner: &ComponentSpawner,
    ) -> Result<(), GenericError> {
        let otlp_handler = Arc::new(handler);
        let metrics = Arc::new(metrics);

        // Validate TLS configurations before spawning.
        let http_tls_config = match self.http_tls {
            Some(tls) => Some(tls.build()?),
            None => None,
        };
        let grpc_tls_config = match self.grpc_tls {
            Some(tls) => Some(tls.build()?),
            None => None,
        };

        // Create and spawn the gRPC server.
        let inner_grpc = GrpcServiceImpl::new(otlp_handler.clone(), memory_limiter.clone(), metrics.clone());

        let grpc_metrics_server =
            MetricsServiceServer::new(inner_grpc.clone()).max_decoding_message_size(self.grpc_max_recv_msg_size_bytes);

        let grpc_logs_server =
            LogsServiceServer::new(inner_grpc.clone()).max_decoding_message_size(self.grpc_max_recv_msg_size_bytes);

        let grpc_traces_server =
            TraceServiceServer::new(inner_grpc).max_decoding_message_size(self.grpc_max_recv_msg_size_bytes);

        let mut grpc_server = GrpcServer::new(self.grpc_endpoint.clone())
            .add_service(grpc_metrics_server)
            .add_service(grpc_logs_server)
            .add_service(grpc_traces_server);

        if let Some(tls_config) = grpc_tls_config {
            grpc_server = grpc_server.with_tls_config(tls_config);
        }

        spawner
            .supervisable(grpc_server)
            .on_worker_pool()
            .spawn()
            .await
            .error_context("Failed to spawn OTLP gRPC server.")?;

        // Create and spawn the HTTP server.
        let router = Router::new()
            .route("/v1/metrics", post(http_metrics_handler::<H>))
            .route("/v1/logs", post(http_logs_handler::<H>))
            .route("/v1/traces", post(http_traces_handler::<H>))
            .with_state((otlp_handler, memory_limiter, metrics));

        // Apply CORS middleware when origins are configured.
        let router = if !self.cors.allowed_origins.is_empty() {
            router.layer(build_cors_layer(&self.cors))
        } else {
            router
        };

        let service = TowerToHyperService::new(router);

        let mut http_server = HttpServer::from_listen_address(self.http_endpoint, service);

        if let Some(tls_config) = http_tls_config {
            http_server = http_server.with_tls_config(tls_config);
        }

        spawner
            .supervisable(http_server)
            .on_worker_pool()
            .spawn()
            .await
            .error_context("Failed to spawn OTLP HTTP server.")?;

        Ok(())
    }
}

/// Builds a CORS layer from the resolved CORS configuration.
///
/// A bare `*` in the list of allowed origins enables allow-all; otherwise the first `*` in an origin is a partial wildcard
/// (for example, `http://*.domain.com` matches `http://foo.domain.com`).
fn build_cors_layer(cors: &CorsConfiguration) -> CorsLayer {
    let mut layer = CorsLayer::new();
    let allowed_origins = cors
        .allowed_origins
        .iter()
        .map(|origin| origin.to_ascii_lowercase())
        .collect::<Vec<_>>();

    let allows_any_origin = allowed_origins.iter().any(|origin| origin == "*");
    if allows_any_origin {
        layer = layer.allow_origin(Any);
    } else {
        layer = layer.allow_origin(AllowOrigin::predicate(move |origin, _request_parts| {
            let Ok(origin) = origin.to_str() else {
                return false;
            };
            let origin = origin.to_ascii_lowercase();

            allowed_origins.iter().any(|pattern| origin_matches(pattern, &origin))
        }));
    }

    // Preflight must permit the methods browser exporters use; without this the browser blocks
    // the actual request even when the origin is allowed.
    layer = layer.allow_methods([Method::GET, Method::POST, Method::HEAD]);

    if cors.allowed_headers.iter().any(|h| h == "*") {
        layer = layer.allow_headers(Any);
    } else {
        let mut headers: Vec<HeaderName> = cors
            .allowed_headers
            .iter()
            .filter_map(|h| HeaderName::from_str(h).ok())
            .collect();
        headers.extend_from_slice(&IMPLICIT_HEADERS);
        if cors.allowed_headers.is_empty() {
            headers.push(HeaderName::from_static("x-requested-with"));
        }
        layer = layer.allow_headers(headers);
    }

    // Exposed headers.
    if !cors.exposed_headers.is_empty() {
        let headers: Vec<HeaderName> = cors
            .exposed_headers
            .iter()
            .filter_map(|h| HeaderName::from_str(h).ok())
            .collect();
        layer = layer.expose_headers(headers);
    }

    // Max age.
    if cors.max_age > 0 {
        layer = layer.max_age(std::time::Duration::from_secs(cors.max_age));
    }

    // Wildcard CORS responses cannot permit browser credentials.
    if !allows_any_origin
        && !cors.allowed_headers.iter().any(|header| header == "*")
        && !cors.exposed_headers.iter().any(|header| header == "*")
    {
        layer = layer.allow_credentials(true);
    }

    layer
}

/// Matches an Origin header against an allowed-origin pattern with first-`*` wildcard semantics.
fn origin_matches(pattern: &str, origin: &str) -> bool {
    let mut parts = pattern.splitn(2, '*');
    let prefix = parts.next().unwrap_or("");
    let Some(suffix) = parts.next() else {
        return pattern == origin;
    };

    origin.starts_with(prefix) && origin.ends_with(suffix) && origin.len() >= prefix.len() + suffix.len()
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
    #[cfg(unix)]
    use std::time::Duration;

    use axum::{
        body::Body,
        http::{header, Request},
        routing::post,
    };
    use rustls::pki_types::ServerName;
    #[cfg(unix)]
    use saluki_core::components::ComponentSpawner;
    #[cfg(unix)]
    use saluki_core::runtime::Supervisor;
    use saluki_core::{
        accounting::MemoryLimiter,
        components::{test_util::TestComponentSupervisor, ComponentContext},
    };
    use saluki_metrics::test::TestRecorder;
    use saluki_tls::test_util::SelfSignedCert;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio_rustls::TlsConnector;
    use tower::ServiceExt;

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

    #[test]
    fn origin_matcher_matches_rs_cors_patterns() {
        assert!(origin_matches("http://*.example.com", "http://foo.example.com"));
        assert!(origin_matches("http://*.example.com", "http://.example.com"));
        assert!(!origin_matches(
            "http://*.example.com",
            "http://foo.example.com.evil.com"
        ));
        assert!(origin_matches("http://*.example.com/*", "http://foo.example.com/*"));
        assert!(!origin_matches("http://*.example.com/*", "http://foo.example.com/bar"));
        assert!(origin_matches("http://example.com", "http://example.com"));
        assert!(!origin_matches("http://example.com", "http://other.com"));
    }

    #[tokio::test]
    async fn cors_layer_matches_partial_wildcard_origin() {
        let app = Router::new()
            .route("/", post(|| async { StatusCode::NO_CONTENT }))
            .layer(build_cors_layer(&CorsConfiguration {
                allowed_origins: vec!["HTTP://*.EXAMPLE.COM".to_string()],
                ..Default::default()
            }));

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/")
                    .header(header::ORIGIN, "http://api.example.com")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(
            response
                .headers()
                .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
                .and_then(|value| value.to_str().ok()),
            Some("http://api.example.com")
        );
        assert_eq!(
            response
                .headers()
                .get(header::ACCESS_CONTROL_ALLOW_CREDENTIALS)
                .and_then(|value| value.to_str().ok()),
            Some("true")
        );
    }

    #[tokio::test]
    async fn cors_layer_rejects_unrelated_partial_wildcard_origin() {
        let app = Router::new()
            .route("/", post(|| async { StatusCode::NO_CONTENT }))
            .layer(build_cors_layer(&CorsConfiguration {
                allowed_origins: vec!["http://*.example.com".to_string()],
                ..Default::default()
            }));

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/")
                    .header(header::ORIGIN, "https://evil.example")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert!(response.headers().get(header::ACCESS_CONTROL_ALLOW_ORIGIN).is_none());
    }

    #[tokio::test]
    async fn cors_layer_allows_any_origin_for_bare_wildcard() {
        let app = Router::new()
            .route("/", post(|| async { StatusCode::NO_CONTENT }))
            .layer(build_cors_layer(&CorsConfiguration {
                allowed_origins: vec!["*".to_string()],
                ..Default::default()
            }));

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/")
                    .header(header::ORIGIN, "https://evil.example")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(
            response
                .headers()
                .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
                .and_then(|value| value.to_str().ok()),
            Some("*")
        );
        assert!(response
            .headers()
            .get(header::ACCESS_CONTROL_ALLOW_CREDENTIALS)
            .is_none());
    }

    #[tokio::test]
    async fn cors_preflight_allows_post_method() {
        let app = Router::new()
            .route("/", post(|| async { StatusCode::NO_CONTENT }))
            .layer(build_cors_layer(&CorsConfiguration {
                allowed_origins: vec!["*".to_string()],
                ..Default::default()
            }));

        let response = app
            .oneshot(
                Request::builder()
                    .method("OPTIONS")
                    .uri("/")
                    .header(header::ORIGIN, "http://example.com")
                    .header(header::ACCESS_CONTROL_REQUEST_METHOD, "POST")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        let allowed_methods = response
            .headers()
            .get(header::ACCESS_CONTROL_ALLOW_METHODS)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("");
        assert!(allowed_methods.contains("POST"));
    }

    /// Waits for a Unix socket file to appear, retrying briefly to avoid races with async
    /// server startup.
    #[cfg(unix)]
    async fn wait_for_socket(path: &std::path::Path) {
        for _ in 0..100 {
            if path.exists() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        panic!("Unix socket at {} did not appear within 500ms", path.display());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn build_succeeds_with_unix_grpc_endpoint() {
        let dir = tempfile::tempdir().expect("temp dir should be created");
        let grpc_socket = dir.path().join("grpc.sock");
        let grpc_endpoint = ListenAddress::try_from(format!("unix://{}", grpc_socket.display()).as_str())
            .expect("Unix gRPC endpoint should parse");
        // Use an ephemeral TCP port for HTTP since we are only testing the gRPC path here.
        let http_endpoint = ListenAddress::Tcp("127.0.0.1:0".parse().expect("addr should parse"));

        let mut supervisor = Supervisor::new("otlp-test")
            .expect("test supervisor name should be valid")
            .with_shutdown_budget(Duration::from_secs(5));
        let spawner = ComponentSpawner::new(supervisor.handle(), tokio::runtime::Handle::current());

        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
        let supervisor_task = tokio::spawn(async move {
            supervisor
                .run_with_shutdown(async {
                    let _ = shutdown_rx.await;
                })
                .await
        });

        // Give the supervisor time to start.
        tokio::task::yield_now().await;

        let result = OtlpServerConfiguration::new(http_endpoint, grpc_endpoint, 4 * 1024 * 1024)
            .build(NoopHandler, MemoryLimiter::noop(), Metrics::for_tests(), &spawner)
            .await;

        assert!(
            result.is_ok(),
            "build should succeed with a Unix gRPC endpoint, got: {:?}",
            result.err()
        );

        // Shut down cleanly.
        let _ = shutdown_tx.send(());
        let _ = supervisor_task.await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn grpc_export_works_over_unix_socket() {
        use otlp_protos::opentelemetry::proto::collector::metrics::v1::metrics_service_client::MetricsServiceClient;

        let dir = tempfile::tempdir().expect("temp dir should be created");
        let grpc_socket = dir.path().join("grpc.sock");
        let grpc_endpoint = ListenAddress::try_from(format!("unix://{}", grpc_socket.display()).as_str())
            .expect("Unix gRPC endpoint should parse");
        let http_endpoint = ListenAddress::Tcp("127.0.0.1:0".parse().expect("addr should parse"));

        let mut supervisor = Supervisor::new("otlp-test")
            .expect("test supervisor name should be valid")
            .with_shutdown_budget(Duration::from_secs(5));
        let spawner = ComponentSpawner::new(supervisor.handle(), tokio::runtime::Handle::current());

        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
        let supervisor_task = tokio::spawn(async move {
            supervisor
                .run_with_shutdown(async {
                    let _ = shutdown_rx.await;
                })
                .await
        });

        // Give the supervisor time to start.
        tokio::task::yield_now().await;

        OtlpServerConfiguration::new(http_endpoint, grpc_endpoint, 4 * 1024 * 1024)
            .build(NoopHandler, MemoryLimiter::noop(), Metrics::for_tests(), &spawner)
            .await
            .expect("build should succeed");

        wait_for_socket(&grpc_socket).await;

        // Connect a tonic gRPC client over the Unix socket and send a metrics export request.
        let endpoint_str = format!("unix://{}", grpc_socket.display());
        let channel = tonic::transport::Endpoint::from_shared(endpoint_str)
            .expect("endpoint should parse")
            .connect()
            .await
            .expect("should connect to gRPC server over Unix socket");

        let mut client = MetricsServiceClient::new(channel);
        let request = ExportMetricsServiceRequest {
            resource_metrics: vec![otlp_protos::opentelemetry::proto::metrics::v1::ResourceMetrics::default()],
        };
        let response = client.export(request).await.expect("export should succeed");

        // A successful response has an empty partial_success (None).
        assert_eq!(response.into_inner().partial_success, None);

        let _ = shutdown_tx.send(());
        let _ = supervisor_task.await;
    }

    /// Builds a `rustls::ClientConfig` that trusts the self-signed certificate.
    fn self_signed_client_config(cert: &SelfSignedCert) -> rustls::ClientConfig {
        let mut root_store = rustls::RootCertStore::empty();
        root_store.add(cert.cert_chain().into_iter().next().unwrap()).unwrap();
        rustls::ClientConfig::builder()
            .with_root_certificates(root_store)
            .with_no_client_auth()
    }

    /// Starts an `OtlpServerConfiguration` with TLS enabled on both HTTP and gRPC, returning the bound ports and
    /// the self-signed certificate (so callers can build a matching client config).
    async fn start_otlp_server_with_tls() -> (u16, u16, SelfSignedCert, TestComponentSupervisor) {
        let _ = saluki_tls::initialize_default_crypto_provider();
        let supervisor = TestComponentSupervisor::start("otlp-tls-test").await;
        let spawner = supervisor.spawner();

        // Bind listeners ourselves first to get ephemeral ports, then drop them and let the builder re-bind.
        let http_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let grpc_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let http_port = http_listener.local_addr().unwrap().port();
        let grpc_port = grpc_listener.local_addr().unwrap().port();
        drop(http_listener);
        drop(grpc_listener);

        let cert = SelfSignedCert::localhost();
        let http_endpoint = ListenAddress::Tcp(format!("127.0.0.1:{}", http_port).parse().unwrap());
        let grpc_endpoint = ListenAddress::Tcp(format!("127.0.0.1:{}", grpc_port).parse().unwrap());

        // Write the cert and key to temp files so OtlpTlsConfiguration can load them.
        let tempdir = tempfile::tempdir().expect("temp dir should be created");
        let cert_path = tempdir.path().join("cert.pem");
        let key_path = tempdir.path().join("key.pem");
        cert.write_cert_pem(&cert_path);
        cert.write_key_pem(&key_path);

        let tls_config = OtlpTlsConfiguration::new(cert_path, key_path);
        let server_config = OtlpServerConfiguration::new(http_endpoint, grpc_endpoint, 4 * 1024 * 1024)
            .with_http_tls(tls_config.clone())
            .with_grpc_tls(tls_config);

        server_config
            .build(NoopHandler, MemoryLimiter::noop(), Metrics::for_tests(), &spawner)
            .await
            .expect("OTLP server with TLS should start");

        (http_port, grpc_port, cert, supervisor)
    }

    #[tokio::test]
    async fn http_tls_handshake_succeeds_with_trusted_cert() {
        let (http_port, _grpc_port, cert, supervisor) = start_otlp_server_with_tls().await;

        // Connect a TLS client to the HTTP port and verify the handshake completes.
        let connector = TlsConnector::from(Arc::new(self_signed_client_config(&cert)));
        let stream = tokio::net::TcpStream::connect(format!("127.0.0.1:{}", http_port))
            .await
            .expect("should connect to HTTP port");
        let server_name = ServerName::try_from("localhost").unwrap();
        let mut tls_stream = connector
            .connect(server_name, stream)
            .await
            .expect("TLS handshake should succeed with trusted self-signed cert");

        // Send a minimal HTTP request to confirm the server is serving over TLS.
        tls_stream
            .write_all(b"GET /v1/traces HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .await
            .expect("should write HTTP request over TLS");

        // Read the response — any HTTP response means TLS is working.
        let mut buf = [0u8; 1024];
        let n = tls_stream
            .read(&mut buf)
            .await
            .expect("should read HTTP response over TLS");
        let response = String::from_utf8_lossy(&buf[..n]);
        assert!(
            response.starts_with("HTTP/"),
            "expected an HTTP response, got: {response}"
        );

        // Close the connection before shutting down the supervisor so the server can drain cleanly.
        let _ = tls_stream.shutdown().await;

        supervisor
            .shutdown()
            .await
            .expect("supervisor should shut down cleanly");
    }

    #[tokio::test]
    async fn grpc_tls_handshake_succeeds_with_trusted_cert() {
        let (_http_port, grpc_port, cert, supervisor) = start_otlp_server_with_tls().await;

        // Connect a TLS client to the gRPC port and verify the handshake completes.
        // We use a raw TLS connection rather than a full gRPC client to keep the test simple — completing the
        // handshake proves the server is serving TLS.
        let connector = TlsConnector::from(Arc::new(self_signed_client_config(&cert)));
        let stream = tokio::net::TcpStream::connect(format!("127.0.0.1:{}", grpc_port))
            .await
            .expect("should connect to gRPC port");
        let server_name = ServerName::try_from("localhost").unwrap();
        let mut tls_stream = connector
            .connect(server_name, stream)
            .await
            .expect("TLS handshake should succeed with trusted self-signed cert");

        // Send an HTTP/2 connection preface to confirm the gRPC server is speaking HTTP/2 over TLS.
        tls_stream
            .write_all(b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n")
            .await
            .expect("should write HTTP/2 preface over TLS");

        // Read any response — the server may send a settings frame or close. Either way, the handshake succeeded.
        let mut buf = [0u8; 1024];
        let _ = tls_stream.read(&mut buf).await;

        // Close the connection before shutting down the supervisor so the server can drain cleanly.
        let _ = tls_stream.shutdown().await;

        supervisor
            .shutdown()
            .await
            .expect("supervisor should shut down cleanly");
    }

    #[tokio::test]
    async fn http_tls_handshake_fails_without_trusted_cert() {
        let (http_port, _grpc_port, _cert, supervisor) = start_otlp_server_with_tls().await;

        // Connect a TLS client with an empty root store — the handshake should fail because the self-signed cert
        // is not trusted.
        let client_config = rustls::ClientConfig::builder()
            .with_root_certificates(rustls::RootCertStore::empty())
            .with_no_client_auth();
        let connector = TlsConnector::from(Arc::new(client_config));
        let stream = tokio::net::TcpStream::connect(format!("127.0.0.1:{}", http_port))
            .await
            .expect("should connect to HTTP port");
        let server_name = ServerName::try_from("localhost").unwrap();
        let result = connector.connect(server_name, stream).await;

        assert!(result.is_err(), "TLS handshake should fail without trusted cert");

        supervisor
            .shutdown()
            .await
            .expect("supervisor should shut down cleanly");
    }
}
