//! APM relay.
//!
//! Sits in front of the trace-agent, listening on TCP and/or a Unix domain socket where the trace-agent normally
//! would. v1.0 (ETP) tracer payloads sent with `POST /v1.0/traces` are dispatched as a `Payload::Http` for downstream
//! decoding: headers are preserved verbatim and the body is handed off unmodified, so tracer metadata and the raw
//! msgpack payload both survive to the decoder untouched. Every other request is proxied to the trace-agent.

use std::sync::{Arc, LazyLock};

use async_trait::async_trait;
use axum::body::to_bytes;
use axum::extract::{Request as AxumRequest, State};
use axum::response::Response;
use axum::routing::post;
use axum::Router;
use http::{Request, StatusCode};
use saluki_common::buf::FrozenChunkedBytesBuffer;
use saluki_core::accounting::{MemoryBounds, MemoryBoundsBuilder, MemoryLimiter};
use saluki_core::components::relays::{Relay, RelayBuilder, RelayContext};
use saluki_core::components::BuildContext;
use saluki_core::data_model::payload::{HttpPayload, Payload, PayloadMetadata, PayloadType};
use saluki_core::runtime;
use saluki_core::topology::OutputDefinition;
use saluki_error::{generic_error, GenericError};
use saluki_io::net::server::http::HttpServer;
use saluki_io::net::ListenAddress;
use tokio::sync::mpsc;
use tokio::{pin, select};
use tracing::{debug, error};

mod proxy;
use self::proxy::TraceAgentProxy;

/// Path served by the v1.0 trace receiver.
const TRACES_PATH: &str = "/v1.0/traces";

/// Configuration for the APM relay.
///
/// Holds plain values, populated by the binary that assembles the topology.
///
/// Fields are plain, public `String`s rather than `Option<String>` for two reasons: it collapses the redundant
/// `None` vs. `Some("")` "disabled" encoding down to a single empty-string representation, and, because
/// `receiver_endpoint` and `receiver_socket` are otherwise identically typed, it makes constructing this struct with
/// the two swapped a matter of getting a field name wrong (a compiler-obvious mistake) rather than transposing two
/// positional constructor arguments (a silent one).
#[derive(Clone, Debug)]
pub struct ApmRelayConfiguration {
    /// TCP address the relay listens on, for example `0.0.0.0:8126`.
    ///
    /// Empty disables the TCP listener.
    pub receiver_endpoint: String,

    /// Unix domain socket path the relay listens on.
    ///
    /// Empty disables the Unix domain socket listener.
    pub receiver_socket: String,

    /// Maximum accepted `/v1.0/traces` request body size, in bytes.
    ///
    /// Requests whose body exceeds this size are rejected with `413 Payload Too Large` before any decoding is
    /// attempted.
    ///
    /// There is no default here: the effective default lives in the configuration layer, so this struct cannot state a
    /// second one. The caller supplies every field.
    pub max_payload_size: usize,

    /// URL of the trace-agent that every request other than `POST /v1.0/traces` is proxied to: `http://host:port` or
    /// `unix:///path/to/socket`.
    pub proxy_destination: String,
}

impl ApmRelayConfiguration {
    fn tcp_listen_address(&self) -> Result<Option<ListenAddress>, GenericError> {
        if self.receiver_endpoint.is_empty() {
            return Ok(None);
        }

        ListenAddress::try_from(format!("tcp://{}", self.receiver_endpoint))
            .map(Some)
            .map_err(|e| {
                generic_error!(
                    "Invalid APM relay TCP receiver endpoint `{}`: {e}",
                    self.receiver_endpoint
                )
            })
    }

    fn uds_listen_address(&self) -> Result<Option<ListenAddress>, GenericError> {
        if self.receiver_socket.is_empty() {
            return Ok(None);
        }

        ListenAddress::try_from(format!("unix://{}", self.receiver_socket))
            .map(Some)
            .map_err(|e| {
                generic_error!(
                    "Invalid APM relay Unix domain socket path `{}`: {e}",
                    self.receiver_socket
                )
            })
    }
}

impl MemoryBounds for ApmRelayConfiguration {
    fn specify_bounds(&self, _builder: &mut MemoryBoundsBuilder) {}
}

#[async_trait]
impl RelayBuilder for ApmRelayConfiguration {
    fn outputs(&self) -> &[OutputDefinition<PayloadType>] {
        static OUTPUTS: LazyLock<Vec<OutputDefinition<PayloadType>>> =
            LazyLock::new(|| vec![OutputDefinition::named_output("traces", PayloadType::Http)]);
        &OUTPUTS
    }

    async fn build(&self, _context: BuildContext) -> Result<Box<dyn Relay + Send>, GenericError> {
        let tcp_endpoint = self.tcp_listen_address()?;
        let uds_endpoint = self.uds_listen_address()?;

        if tcp_endpoint.is_none() && uds_endpoint.is_none() {
            return Err(generic_error!(
                "APM relay requires at least one of `receiver_endpoint` or `receiver_socket` to be configured."
            ));
        }

        let proxy = TraceAgentProxy::from_destination(&self.proxy_destination)?;

        Ok(Box::new(ApmRelay {
            tcp_endpoint,
            uds_endpoint,
            max_payload_size: self.max_payload_size,
            proxy,
        }))
    }
}

/// APM relay.
///
/// Receives tracer requests via HTTP, on TCP and/or a Unix domain socket. Dispatches v1.0 trace payloads for downstream
/// decoding and proxies everything else to the trace-agent.
pub struct ApmRelay {
    tcp_endpoint: Option<ListenAddress>,
    uds_endpoint: Option<ListenAddress>,
    max_payload_size: usize,
    proxy: TraceAgentProxy,
}

#[async_trait]
impl Relay for ApmRelay {
    async fn run(self: Box<Self>, mut context: RelayContext) -> Result<(), GenericError> {
        let Self {
            tcp_endpoint,
            uds_endpoint,
            max_payload_size,
            proxy,
        } = *self;

        let global_shutdown = context.take_shutdown_handle();
        pin!(global_shutdown);

        let mut health = context.take_health_handle();
        let worker_pool = context.topology_context().global_thread_pool().clone();
        let memory_limiter = context.topology_context().memory_limiter().clone();

        let (payload_tx, mut payload_rx) = mpsc::channel(1024);
        let router = build_router(RelayState::new(payload_tx, max_payload_size, memory_limiter, proxy));

        if let Some(endpoint) = tcp_endpoint {
            debug!(%endpoint, "Binding APM relay TCP listener.");
            let server = HttpServer::from_listen_address(endpoint)
                .with_routes(router.clone())
                .with_worker_pool(worker_pool.clone());
            runtime::nested_supervisor(server.into_supervisor()).spawn();
        }
        if let Some(endpoint) = uds_endpoint {
            debug!(%endpoint, "Binding APM relay Unix domain socket listener.");
            let server = HttpServer::from_listen_address(endpoint)
                .with_routes(router)
                .with_worker_pool(worker_pool);
            runtime::nested_supervisor(server.into_supervisor()).spawn();
        }

        health.mark_ready();
        debug!("APM relay started.");

        loop {
            select! {
                _ = &mut global_shutdown => {
                    debug!("Received shutdown signal.");
                    break
                },
                Some(payload) = payload_rx.recv() => {
                    let http_payload = HttpPayload::new(payload.metadata, payload.request);
                    if let Err(e) = context.dispatcher().dispatch_named("traces", Payload::Http(http_payload)).await {
                        error!(error = %e, "Failed to dispatch APM payload.");
                    }
                },
                _ = health.live() => continue,
            }
        }

        debug!("Stopping APM relay...");
        debug!("APM relay stopped.");

        Ok(())
    }
}

/// Shared HTTP handler state.
struct RelayState {
    tx: mpsc::Sender<ApmPayload>,
    max_payload_size: usize,
    memory_limiter: MemoryLimiter,
    proxy: TraceAgentProxy,
}

impl RelayState {
    fn new(
        tx: mpsc::Sender<ApmPayload>, max_payload_size: usize, memory_limiter: MemoryLimiter, proxy: TraceAgentProxy,
    ) -> Self {
        Self {
            tx,
            max_payload_size,
            memory_limiter,
            proxy,
        }
    }
}

/// A dispatched APM payload: the request body plus its original headers.
struct ApmPayload {
    metadata: PayloadMetadata,
    request: Request<FrozenChunkedBytesBuffer>,
}

/// Builds the router: `POST` on `TRACES_PATH` is handled here, and every other path or method is proxied to the
/// trace-agent, which answers it exactly as it would without the relay in front.
fn build_router(state: RelayState) -> Router {
    Router::new()
        .route(TRACES_PATH, post(handle_traces).fallback(proxy_request))
        .fallback(proxy_request)
        .with_state(Arc::new(state))
}

async fn proxy_request(State(state): State<Arc<RelayState>>, request: AxumRequest) -> Response {
    state.proxy.forward(request).await
}

async fn handle_traces(State(state): State<Arc<RelayState>>, request: AxumRequest) -> StatusCode {
    state.memory_limiter.wait_for_capacity().await;

    let (parts, body) = request.into_parts();

    let body_bytes = match to_bytes(body, state.max_payload_size).await {
        Ok(bytes) => bytes,
        Err(err) => return body_size_error_status(&err),
    };

    let apm_request = Request::from_parts(parts, FrozenChunkedBytesBuffer::from(body_bytes));
    let payload = ApmPayload {
        metadata: PayloadMetadata::from_event_count(1),
        request: apm_request,
    };

    match state.tx.send(payload).await {
        Ok(()) => StatusCode::OK,
        Err(_) => {
            error!("Failed to send APM payload to relay dispatcher: channel closed.");
            StatusCode::SERVICE_UNAVAILABLE
        }
    }
}

/// Maps a body-collection error to a status code, distinguishing an over-limit body (`413`) from any other error
/// while reading the body (`400`).
fn body_size_error_status(err: &axum::Error) -> StatusCode {
    let too_large =
        std::error::Error::source(err).is_some_and(|source| source.is::<http_body_util::LengthLimitError>());

    if too_large {
        StatusCode::PAYLOAD_TOO_LARGE
    } else {
        StatusCode::BAD_REQUEST
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use saluki_core::components::test_util::TestComponentSupervisor;
    use saluki_core::runtime::state::{DataspaceUpdate, IdentifierFilter};
    use saluki_io::net::BoundListenAddress;
    use tempfile::tempdir;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;
    #[cfg(unix)]
    use tokio::net::UnixStream;
    use tower::ServiceExt;

    use super::*;

    /// A proxy whose trace-agent is not listening, so a proxied request is answered `502 Bad Gateway`. That makes
    /// "was this request proxied?" observable without running a trace-agent.
    fn unreachable_proxy() -> TraceAgentProxy {
        TraceAgentProxy::from_destination("http://127.0.0.1:1").expect("destination should parse")
    }

    fn test_router(max_payload_size: usize) -> (Router, mpsc::Receiver<ApmPayload>) {
        let (tx, rx) = mpsc::channel(4);
        (
            build_router(RelayState::new(
                tx,
                max_payload_size,
                MemoryLimiter::noop(),
                unreachable_proxy(),
            )),
            rx,
        )
    }

    #[tokio::test]
    async fn proxies_other_paths_to_the_trace_agent() {
        let (router, mut rx) = test_router(1024);

        let response = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/not-a-real-path")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        assert!(rx.try_recv().is_err(), "a proxied request must not be dispatched");
    }

    #[tokio::test]
    async fn proxies_other_methods_on_the_traces_path() {
        let (router, mut rx) = test_router(1024);

        let response = router
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri(TRACES_PATH)
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        assert!(rx.try_recv().is_err(), "a proxied request must not be dispatched");
    }

    #[tokio::test]
    async fn rejects_oversized_body_with_413() {
        let (router, _rx) = test_router(4);

        let response = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(TRACES_PATH)
                    .body(axum::body::Body::from(vec![0u8; 64]))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[tokio::test]
    async fn accepts_body_within_limit_and_dispatches_it() {
        let (router, mut rx) = test_router(1024);

        let body = b"raw msgpack payload".to_vec();
        let response = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(TRACES_PATH)
                    .header("datadog-meta-lang", "go")
                    .header("datadog-meta-tracer-version", "1.2.3")
                    .header("x-datadog-trace-count", "42")
                    .body(axum::body::Body::from(body.clone()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);

        let payload = rx.recv().await.expect("payload should be dispatched");
        let (parts, buffer) = payload.request.into_parts();
        assert_eq!(buffer.into_bytes().as_ref(), body.as_slice());
        assert_eq!(parts.headers.get("datadog-meta-lang").unwrap(), "go");
        assert_eq!(parts.headers.get("datadog-meta-tracer-version").unwrap(), "1.2.3");
        assert_eq!(parts.headers.get("x-datadog-trace-count").unwrap(), "42");
    }

    #[tokio::test]
    async fn returns_503_when_dispatch_channel_is_closed() {
        let (tx, rx) = mpsc::channel(4);
        // Drop the receiver immediately so the channel is closed before the handler sends into it.
        drop(rx);

        let router = build_router(RelayState::new(tx, 1024, MemoryLimiter::noop(), unreachable_proxy()));

        let response = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(TRACES_PATH)
                    .body(axum::body::Body::from(b"raw msgpack payload".to_vec()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn binds_and_serves_over_tcp() {
        const BOUND_ADDRESS_ID: &str = "apm-relay-tcp-test-bound-address";

        let supervisor = TestComponentSupervisor::start("apm-relay-tcp-test").await;

        // Bind to an OS-assigned ephemeral port, and discover it via the published `BoundListenAddress`.
        let tcp_endpoint = ListenAddress::try_from("tcp://127.0.0.1:0").expect("TCP APM endpoint should parse");

        let (tx, mut rx) = mpsc::channel(4);
        let router = build_router(RelayState::new(
            tx,
            1024 * 1024,
            MemoryLimiter::noop(),
            unreachable_proxy(),
        ));
        let server = HttpServer::from_listen_address(tcp_endpoint)
            .with_routes(router)
            .with_server_id(BOUND_ADDRESS_ID)
            .with_worker_pool(tokio::runtime::Handle::current());

        let mut bound_address_subscription = supervisor
            .dataspace()
            .subscribe::<BoundListenAddress>(IdentifierFilter::exact(format!("http-server-{BOUND_ADDRESS_ID}")));

        supervisor
            .scope(async {
                runtime::nested_supervisor(server.into_supervisor()).spawn();
            })
            .await;

        let bound_addr = match tokio::time::timeout(Duration::from_secs(5), bound_address_subscription.recv()).await {
            Ok(Some(DataspaceUpdate::Asserted(_, BoundListenAddress::Tcp(address)))) => address,
            update => panic!("expected a bound address assertion for `{BOUND_ADDRESS_ID}`, got {update:?}"),
        };

        let mut stream = tokio::time::timeout(Duration::from_secs(5), TcpStream::connect(bound_addr))
            .await
            .expect("should connect to TCP listener within the timeout")
            .expect("should connect to TCP listener");

        let body = b"raw msgpack payload over tcp";
        let request = format!(
            "POST {TRACES_PATH} HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        stream
            .write_all(request.as_bytes())
            .await
            .expect("should write request head");
        stream.write_all(body).await.expect("should write request body");

        let mut response = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut response))
            .await
            .expect("server should respond over TCP")
            .expect("should read response");

        assert!(
            response.starts_with(b"HTTP/1.1 200"),
            "expected a 200 OK response, got: {}",
            String::from_utf8_lossy(&response)
        );

        let payload = rx.recv().await.expect("payload should be dispatched");
        let (_, buffer) = payload.request.into_parts();
        assert_eq!(buffer.into_bytes().as_ref(), body);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn binds_and_serves_over_unix_domain_socket() {
        let supervisor = TestComponentSupervisor::start("apm-relay-uds-test").await;

        let dir = tempdir().expect("temp dir should be created");
        let socket_path = dir.path().join("apm.sock");
        let uds_endpoint = ListenAddress::try_from(format!("unix://{}", socket_path.display()).as_str())
            .expect("Unix APM endpoint should parse");

        let (tx, mut rx) = mpsc::channel(4);
        let router = build_router(RelayState::new(
            tx,
            1024 * 1024,
            MemoryLimiter::noop(),
            unreachable_proxy(),
        ));
        let server = HttpServer::from_listen_address(uds_endpoint)
            .with_routes(router)
            .with_worker_pool(tokio::runtime::Handle::current());

        supervisor
            .scope(async {
                runtime::nested_supervisor(server.into_supervisor()).spawn();
            })
            .await;

        wait_for_socket(&socket_path).await;

        let mut stream = UnixStream::connect(&socket_path)
            .await
            .expect("should connect to Unix domain socket");

        let body = b"raw msgpack payload over uds";
        let request = format!(
            "POST {TRACES_PATH} HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        stream
            .write_all(request.as_bytes())
            .await
            .expect("should write request head");
        stream.write_all(body).await.expect("should write request body");

        let mut response = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut response))
            .await
            .expect("server should respond over the Unix domain socket")
            .expect("should read response");

        assert!(
            response.starts_with(b"HTTP/1.1 200"),
            "expected a 200 OK response, got: {}",
            String::from_utf8_lossy(&response)
        );

        let payload = rx.recv().await.expect("payload should be dispatched");
        let (_, buffer) = payload.request.into_parts();
        assert_eq!(buffer.into_bytes().as_ref(), body);
    }

    #[cfg(unix)]
    async fn wait_for_socket(path: &std::path::Path) {
        for _ in 0..100 {
            if path.exists() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("Unix domain socket at {} did not appear within 500ms", path.display());
    }

    #[tokio::test]
    async fn build_fails_without_any_configured_endpoint() {
        // Listening nowhere is a configuration error, not a silent no-op: `build` must reject it.
        let config = ApmRelayConfiguration {
            receiver_endpoint: String::new(),
            receiver_socket: String::new(),
            max_payload_size: 1024,
            proxy_destination: "http://127.0.0.1:8127".to_string(),
        };

        let error = match config.build(BuildContext::test_relay("apm_relay_no_endpoints")).await {
            Ok(_) => panic!("build should fail when no endpoint is configured"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("at least one"));
    }

    #[tokio::test]
    async fn build_succeeds_with_only_tcp_endpoint_configured() {
        let config = ApmRelayConfiguration {
            receiver_endpoint: "0.0.0.0:8127".to_string(),
            receiver_socket: String::new(),
            max_payload_size: 1024,
            proxy_destination: "http://127.0.0.1:8127".to_string(),
        };

        assert!(config
            .build(BuildContext::test_relay("apm_relay_tcp_only"))
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn build_fails_with_invalid_proxy_destination() {
        let config = ApmRelayConfiguration {
            receiver_endpoint: "127.0.0.1:8126".to_string(),
            receiver_socket: String::new(),
            max_payload_size: 1024,
            proxy_destination: "ftp://127.0.0.1:8127".to_string(),
        };

        assert!(config
            .build(BuildContext::test_relay("apm_relay_bad_destination"))
            .await
            .is_err());
    }

    #[tokio::test]
    async fn build_fails_with_invalid_endpoint_string() {
        let config = ApmRelayConfiguration {
            receiver_endpoint: "not a valid host:port".to_string(),
            receiver_socket: String::new(),
            max_payload_size: 1024,
            proxy_destination: "http://127.0.0.1:8127".to_string(),
        };

        let error = match config
            .build(BuildContext::test_relay("apm_relay_invalid_endpoint"))
            .await
        {
            Ok(_) => panic!("build should fail for an invalid TCP receiver endpoint"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("Invalid APM relay TCP receiver endpoint"));
    }

    #[test]
    fn tcp_listen_address_parses_host_port() {
        let config = ApmRelayConfiguration {
            receiver_endpoint: "0.0.0.0:8127".to_string(),
            receiver_socket: String::new(),
            max_payload_size: 1024,
            proxy_destination: "http://127.0.0.1:8127".to_string(),
        };
        assert_eq!(
            config.tcp_listen_address().unwrap().unwrap().to_string(),
            "tcp://0.0.0.0:8127"
        );
    }

    #[test]
    fn uds_listen_address_parses_path() {
        let config = ApmRelayConfiguration {
            receiver_endpoint: String::new(),
            receiver_socket: "/tmp/apm.sock".to_string(),
            max_payload_size: 1024,
            proxy_destination: "http://127.0.0.1:8127".to_string(),
        };
        assert_eq!(
            config.uds_listen_address().unwrap().unwrap().to_string(),
            "unix:///tmp/apm.sock"
        );
    }
}
