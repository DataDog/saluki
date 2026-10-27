//! APM relay.
//!
//! Listens on TCP and/or a Unix domain socket where the trace-agent normally listens and forwards every request
//! to the relocated trace-agent.

use std::sync::{Arc, LazyLock};

use agent_data_plane_config::SalukiConfiguration;
use async_trait::async_trait;
use axum::extract::{ConnectInfo, Request as AxumRequest, State};
use axum::response::IntoResponse;
use axum::Router;
use http::{HeaderName, HeaderValue};
use metrics::Counter;
use saluki_api::APIHandler;
use saluki_core::accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_core::components::relays::{Relay, RelayBuilder, RelayContext};
use saluki_core::components::BuildContext;
use saluki_core::data_model::payload::PayloadType;
use saluki_core::runtime;
use saluki_core::topology::OutputDefinition;
use saluki_env::workload::{CaptureEntityResolver, EntityId};
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use saluki_io::net::dns::resolve_host;
use saluki_io::net::server::http::HttpServer;
use saluki_io::net::{ConnectionAddress, ListenAddress};
use saluki_metrics::static_metrics;
use tokio::{pin, select};
use tracing::debug;

mod proxy;
use self::proxy::TraceAgentProxy;

/// Header a tracer sends its container ID in.
const CONTAINER_ID_HEADER: HeaderName = HeaderName::from_static("datadog-container-id");

/// Header a tracer sends its origin in (for example, `ci-<container ID>` or `in-<cgroup inode>`), which the trace-agent
/// prefers over `Datadog-Container-ID`.
const ENTITY_ID_HEADER: HeaderName = HeaderName::from_static("datadog-entity-id");

/// Configuration for the APM relay.
///
/// Holds plain values, populated by the binary that assembles the topology, plus an optional resolver for the
/// container of a tracer connected over the Unix domain socket.
///
/// `receiver_socket` is an empty `String` rather than an `Option<String>`, so that "disabled" has a single
/// representation.
#[derive(Clone)]
pub struct ApmRelayConfiguration {
    /// Unix domain socket path the relay listens on.
    ///
    /// Empty disables the Unix domain socket listener.
    receiver_socket: String,

    /// URL of the trace-agent that every request not handled by ADP is proxied to: `http://host:port` or
    /// `unix:///path/to/socket`.
    proxy_destination: String,

    /// Resolver for mapping the process ID of a tracer connected over the Unix domain socket to its container.
    ///
    /// The trace-agent finds the container of a tracer that sends no container headers from the peer credentials of
    /// its Unix domain socket connection, but behind the relay those credentials are ADP's. With a resolver, the relay
    /// resolves the container from the tracer's credentials itself, and passes it on as `Datadog-Container-ID`.
    capture_entity_resolver: Option<Arc<dyn CaptureEntityResolver + Send + Sync>>,

    /// The host address to bind the APM relay TCP listeners to.
    ///
    /// Accepts either an IP literal (for example, `192.168.1.50`, `::1`) or a hostname that resolves via DNS (for example, `agent.internal`).
    /// Ignored when `apm_non_local_traffic` is `true`, and binds to `127.0.0.1` when unset.
    bind_host: Option<String>,

    /// Whether the TCP listener accepts traffic from other hosts.
    ///
    /// When `true`, the listener binds every interface, overriding `bind_host`, as the trace-agent does.
    non_local_traffic: bool,

    /// TCP port the trace receiver listens on.
    ///
    /// `0` disables the TCP listener.
    receiver_port: u16,
}

#[static_metrics(prefix = component, labels(component_id, component_type))]
#[derive(Clone)]
struct Metrics {
    requests_relayed_total: Counter,
    requests_failed_total: Counter,
}

impl ApmRelayConfiguration {
    /// Creates a new `ApmRelayConfiguration` from the resolved configuration.
    pub fn from_configuration(config: &SalukiConfiguration) -> Self {
        // receiver_port `0` disables the TCP listener
        Self {
            receiver_socket: config.domains.apm.receiver_socket.clone().unwrap_or_default(),
            proxy_destination: config.domains.apm.proxy_destination.to_owned(),
            capture_entity_resolver: None,
            bind_host: config.shared.bind_host.clone(),
            non_local_traffic: config.domains.apm.non_local_traffic,
            receiver_port: config.domains.apm.receiver_port,
        }
    }

    /// Sets the resolver used to find the container of a tracer connected over the Unix domain socket.
    pub fn with_capture_entity_resolver(mut self, resolver: Arc<dyn CaptureEntityResolver + Send + Sync>) -> Self {
        self.capture_entity_resolver = Some(resolver);
        self
    }

    async fn tcp_listen_address(&self) -> Result<Option<ListenAddress>, GenericError> {
        if self.receiver_port == 0 {
            return Ok(None);
        };

        let listen_address = if self.non_local_traffic {
            [0, 0, 0, 0].into()
        } else if let Some(bind_host) = &self.bind_host {
            resolve_host(bind_host)
                .await
                .error_context("Could not resolve bind_host for the APM relay.")?
        } else {
            [127, 0, 0, 1].into()
        };

        Ok(Some(ListenAddress::Tcp(std::net::SocketAddr::new(
            listen_address,
            self.receiver_port,
        ))))
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
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        builder
            .minimum()
            .with_single_value::<ApmRelay>("component struct")
            .with_single_value::<RelayState>("component struct");
    }
}

#[async_trait]
impl RelayBuilder for ApmRelayConfiguration {
    fn outputs(&self) -> &[OutputDefinition<PayloadType>] {
        static OUTPUTS: LazyLock<Vec<OutputDefinition<PayloadType>>> =
            LazyLock::new(|| vec![OutputDefinition::named_output("traces", PayloadType::Http)]);
        &OUTPUTS
    }

    async fn build(&self, context: BuildContext) -> Result<Box<dyn Relay + Send>, GenericError> {
        let tcp_endpoint = self.tcp_listen_address().await?;
        let uds_endpoint = self.uds_listen_address()?;

        if tcp_endpoint.is_none() && uds_endpoint.is_none() {
            return Err(generic_error!(
                "APM relay requires at least one of `apm_config.receiver_port` or `apm_config.receiver_socket` to be configured."
            ));
        }

        let proxy = TraceAgentProxy::from_destination(&self.proxy_destination)?;

        Ok(Box::new(ApmRelay {
            tcp_endpoint,
            uds_endpoint,
            proxy,
            capture_entity_resolver: self.capture_entity_resolver.clone(),
            metrics: Metrics::new(context.component_id(), context.component_type().as_category_str()),
        }))
    }
}

/// APM relay.
///
/// Receives requests via HTTP, on TCP and/or a Unix domain socket, and proxies them to the trace-agent.
pub struct ApmRelay {
    tcp_endpoint: Option<ListenAddress>,
    uds_endpoint: Option<ListenAddress>,
    proxy: TraceAgentProxy,
    capture_entity_resolver: Option<Arc<dyn CaptureEntityResolver + Send + Sync>>,
    metrics: Metrics,
}

#[async_trait]
impl Relay for ApmRelay {
    async fn run(self: Box<Self>, mut context: RelayContext) -> Result<(), GenericError> {
        let Self {
            tcp_endpoint,
            uds_endpoint,
            proxy,
            capture_entity_resolver,
            metrics,
        } = *self;

        let global_shutdown = context.take_shutdown_handle();
        pin!(global_shutdown);

        let mut health = context.take_health_handle();
        let worker_pool = context.topology_context().global_thread_pool().clone();
        let api_handler = ApmRelayAPIHandler::new(RelayState {
            metrics,
            proxy,
            capture_entity_resolver,
        });
        let router = api_handler
            .generate_routes()
            .with_state(api_handler.generate_initial_state());

        // The listeners are needed: one that fails to bind, for example because the trace-agent still owns its address,
        // fails the relay rather than leaving it ready without serving that endpoint.
        if let Some(endpoint) = tcp_endpoint {
            debug!(%endpoint, "Binding APM relay TCP listener.");
            let server = HttpServer::from_listen_address(endpoint)
                .with_routes(router.clone())
                .with_worker_pool(worker_pool.clone());
            runtime::nested_supervisor(server.into_supervisor())
                .needed()
                .stops_with_owner()
                .spawn();
        }
        if let Some(endpoint) = uds_endpoint {
            debug!(%endpoint, "Binding APM relay Unix domain socket listener.");
            let server = HttpServer::from_listen_address(endpoint)
                .with_routes(router.clone())
                .with_worker_pool(worker_pool);
            runtime::nested_supervisor(server.into_supervisor())
                .needed()
                .stops_with_owner()
                .spawn();
        }

        health.mark_ready();
        debug!("APM relay started.");

        loop {
            select! {
                _ = &mut global_shutdown => {
                    debug!("Received shutdown signal.");
                    break
                },
                _ = health.live() => continue,
            }
        }

        debug!("APM relay stopped.");

        Ok(())
    }
}

/// Shared HTTP handler state.
#[derive(Clone)]
struct RelayState {
    proxy: TraceAgentProxy,
    capture_entity_resolver: Option<Arc<dyn CaptureEntityResolver + Send + Sync>>,
    metrics: Metrics,
}

struct ApmRelayAPIHandler {
    state: RelayState,
}

impl ApmRelayAPIHandler {
    fn new(state: RelayState) -> Self {
        Self { state }
    }

    async fn proxy_request(
        State(RelayState {
            proxy,
            capture_entity_resolver,
            metrics,
        }): State<RelayState>,
        mut request: AxumRequest,
    ) -> impl IntoResponse {
        if let Some(resolver) = capture_entity_resolver.as_deref() {
            add_peer_container_id(&mut request, resolver);
        }
        let result = proxy.forward(request).await;
        match &result {
            Ok(_) => metrics.requests_relayed_total().increment(1),
            Err(_) => metrics.requests_failed_total().increment(1),
        }
        result
    }
}

/// Sets `Datadog-Container-ID` to the container of the tracer that sent `request`, when the tracer is connected over the
/// Unix domain socket and sent no container headers of its own.
///
/// This stands in for the trace-agent's own lookup from the connection's peer credentials, which behind the relay
/// would find ADP rather than the tracer.
fn add_peer_container_id(request: &mut AxumRequest, resolver: &(dyn CaptureEntityResolver + Send + Sync)) {
    let headers = request.headers();
    if headers.contains_key(CONTAINER_ID_HEADER) || headers.contains_key(ENTITY_ID_HEADER) {
        return;
    }

    let Some(process_id) = request
        .extensions()
        .get::<ConnectInfo<ConnectionAddress>>()
        .and_then(|ConnectInfo(addr)| addr.process_credentials())
        .and_then(|creds| u32::try_from(creds.pid).ok())
    else {
        return;
    };

    match resolver.resolve_container_entity_for_live_pid(process_id) {
        Some(EntityId::Container(container_id)) => match HeaderValue::from_str(&container_id) {
            Ok(value) => {
                request.headers_mut().insert(CONTAINER_ID_HEADER, value);
            }
            Err(_) => debug!(process_id, %container_id, "Resolved container ID is not a valid header value."),
        },
        Some(entity_id) => debug!(process_id, %entity_id, "Tracer process resolved to a non-container entity."),
        None => debug!(process_id, "Could not resolve the container of the tracer process."),
    }
}

impl APIHandler for ApmRelayAPIHandler {
    type State = RelayState;

    fn generate_initial_state(&self) -> Self::State {
        self.state.clone()
    }

    /// Forwards every request to the trace-agent.
    fn generate_routes(&self) -> Router<Self::State> {
        Router::new().fallback(Self::proxy_request)
    }
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;
    use std::time::Duration;

    use http::{Request, StatusCode};
    use saluki_core::components::test_util::{TestComponentDriver, TestComponentSupervisor};
    use saluki_core::runtime::state::{DataspaceUpdate, IdentifierFilter};
    use saluki_io::net::{BoundListenAddress, ProcessCredentials};
    #[cfg(unix)]
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

    fn build_router(state: RelayState) -> Router {
        let api_handler = ApmRelayAPIHandler::new(state);
        api_handler
            .generate_routes()
            .with_state(api_handler.generate_initial_state())
    }

    fn test_router() -> Router {
        build_router(RelayState {
            proxy: unreachable_proxy(),
            capture_entity_resolver: None,
            metrics: Metrics::new("dd_traces_relay", "relays"),
        })
    }

    /// Resolves every process to the same container.
    struct FixedContainerResolver;

    impl CaptureEntityResolver for FixedContainerResolver {
        fn resolve_container_entity_for_live_pid(&self, _process_id: u32) -> Option<EntityId> {
            Some(EntityId::Container("tracer-container".into()))
        }
    }

    /// Builds a request as the relay's server hands it over: with the address of the peer that sent it.
    fn request_from(peer: ConnectionAddress, headers: &[(&str, &str)]) -> AxumRequest {
        let mut builder = Request::builder().method("POST").uri("/v0.4/traces");
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        let mut request = builder.body(axum::body::Body::empty()).unwrap();
        request.extensions_mut().insert(ConnectInfo(peer));
        request
    }

    fn unix_peer() -> ConnectionAddress {
        ProcessCredentials {
            pid: 1234,
            uid: 0,
            gid: 0,
        }
        .into()
    }

    #[test]
    fn sets_the_container_id_of_a_unix_socket_peer() {
        let mut request = request_from(unix_peer(), &[]);

        add_peer_container_id(&mut request, &FixedContainerResolver);

        assert_eq!(request.headers()[CONTAINER_ID_HEADER], "tracer-container");
    }

    #[test]
    fn keeps_the_container_headers_a_tracer_sent() {
        let mut request = request_from(unix_peer(), &[("Datadog-Container-ID", "from-tracer")]);
        add_peer_container_id(&mut request, &FixedContainerResolver);
        assert_eq!(request.headers()[CONTAINER_ID_HEADER], "from-tracer");

        let mut request = request_from(unix_peer(), &[("Datadog-Entity-ID", "in-42")]);
        add_peer_container_id(&mut request, &FixedContainerResolver);
        assert!(!request.headers().contains_key(CONTAINER_ID_HEADER));
    }

    #[test]
    fn leaves_tcp_requests_alone() {
        let mut request = request_from(ConnectionAddress::from(SocketAddr::from(([127, 0, 0, 1], 4242))), &[]);

        add_peer_container_id(&mut request, &FixedContainerResolver);

        assert!(!request.headers().contains_key(CONTAINER_ID_HEADER));
    }

    #[tokio::test]
    async fn counts_a_request_the_trace_agent_never_received_as_failed() {
        let recorder = saluki_metrics::test::TestRecorder::default();
        let _recorder_guard = metrics::set_default_local_recorder(&recorder);
        let router = test_router();

        let response = router
            .oneshot(
                Request::builder()
                    .uri("/v0.4/traces")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);

        let metric_key = |name| {
            metrics::Key::from_parts(
                name,
                vec![
                    metrics::Label::new("component_id", "dd_traces_relay"),
                    metrics::Label::new("component_type", "relays"),
                ],
            )
        };
        assert_eq!(recorder.counter(metric_key("component_requests_failed_total")), Some(1));
        assert_eq!(
            recorder.counter(metric_key("component_requests_relayed_total")),
            Some(0)
        );
    }

    #[tokio::test]
    async fn proxies_requests_to_the_trace_agent() {
        let router = test_router();

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
    }

    #[tokio::test]
    async fn binds_and_serves_over_tcp() {
        const BOUND_ADDRESS_ID: &str = "apm-relay-tcp-test-bound-address";

        let supervisor = TestComponentSupervisor::start("apm-relay-tcp-test").await;

        // Bind to an OS-assigned ephemeral port, and discover it via the published `BoundListenAddress`.
        let tcp_endpoint = ListenAddress::try_from("tcp://127.0.0.1:0").expect("TCP APM endpoint should parse");

        let router = test_router();
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

        let body = b"proxied request over tcp";
        let request = format!(
            "POST /info HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
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
            response.starts_with(b"HTTP/1.1 502"),
            "expected a 502 Bad Gateway response, got: {}",
            String::from_utf8_lossy(&response)
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn binds_and_serves_over_unix_domain_socket() {
        const BOUND_ADDRESS_ID: &str = "apm-relay-uds-test-bound-address";

        let supervisor = TestComponentSupervisor::start("apm-relay-uds-test").await;

        let dir = tempdir().expect("temp dir should be created");
        let socket_path = dir.path().join("apm.sock");
        let uds_endpoint = ListenAddress::try_from(format!("unix://{}", socket_path.display()).as_str())
            .expect("Unix APM endpoint should parse");

        let router = test_router();
        let server = HttpServer::from_listen_address(uds_endpoint)
            .with_routes(router)
            .with_server_id(BOUND_ADDRESS_ID)
            .with_worker_pool(tokio::runtime::Handle::current());

        // The bound address is published once the listener accepts connections.
        let mut bound_address_subscription = supervisor
            .dataspace()
            .subscribe::<BoundListenAddress>(IdentifierFilter::exact(format!("http-server-{BOUND_ADDRESS_ID}")));

        supervisor
            .scope(async {
                runtime::nested_supervisor(server.into_supervisor()).spawn();
            })
            .await;

        let bound_path = match tokio::time::timeout(Duration::from_secs(5), bound_address_subscription.recv()).await {
            Ok(Some(DataspaceUpdate::Asserted(_, BoundListenAddress::Unix(path)))) => path,
            update => panic!("expected a bound address assertion for `{BOUND_ADDRESS_ID}`, got {update:?}"),
        };
        assert_eq!(bound_path, socket_path);

        let mut stream = UnixStream::connect(&socket_path)
            .await
            .expect("should connect to Unix domain socket");

        let body = b"proxied request over uds";
        let request = format!(
            "POST /info HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
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
            response.starts_with(b"HTTP/1.1 502"),
            "expected a 502 Bad Gateway response, got: {}",
            String::from_utf8_lossy(&response)
        );
    }

    fn test_config(bind_host: Option<&str>, non_local_traffic: bool, receiver_port: u16) -> ApmRelayConfiguration {
        ApmRelayConfiguration {
            receiver_socket: String::new(),
            proxy_destination: "http://127.0.0.1:8127".to_string(),
            capture_entity_resolver: None,
            bind_host: bind_host.map(str::to_string),
            non_local_traffic,
            receiver_port,
        }
    }

    #[tokio::test]
    async fn a_listener_that_fails_to_bind_fails_the_relay() {
        // Like a trace-agent that still owns the receiver port.
        let occupied = std::net::TcpListener::bind("127.0.0.1:0").expect("should bind an ephemeral port");
        let port = occupied.local_addr().expect("should have a local address").port();

        let (control, _outputs) = TestComponentDriver::relay(test_config(None, false, port))
            .await
            .expect("the relay should build");

        let result = control.wait().await;
        assert!(result.is_err(), "the relay should fail when its listener can't bind");
    }

    #[tokio::test]
    async fn build_fails_without_any_configured_endpoint() {
        // Listening nowhere is a configuration error, not a silent no-op: `build` must reject it.
        let config = test_config(None, false, 0);

        let error = match config.build(BuildContext::test_relay("apm_relay_no_endpoints")).await {
            Ok(_) => panic!("build should fail when no endpoint is configured"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("at least one"));
    }

    #[tokio::test]
    async fn build_succeeds_with_only_tcp_endpoint_configured() {
        let config = test_config(None, true, 8127);

        assert!(config
            .build(BuildContext::test_relay("apm_relay_tcp_only"))
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn build_fails_with_invalid_proxy_destination() {
        let mut config = test_config(None, false, 8126);
        config.proxy_destination = "ftp://127.0.0.1:8127".to_string();

        assert!(config
            .build(BuildContext::test_relay("apm_relay_bad_destination"))
            .await
            .is_err());
    }

    #[tokio::test]
    async fn build_fails_with_unresolvable_bind_host() {
        let config = test_config(Some("not a valid host"), false, 8126);

        let error = match config
            .build(BuildContext::test_relay("apm_relay_unresolvable_bind_host"))
            .await
        {
            Ok(_) => panic!("build should fail for an unresolvable bind host"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("Could not resolve bind_host"));
    }

    #[tokio::test]
    async fn tcp_listen_address_defaults_to_loopback() {
        let config = test_config(None, false, 8127);
        assert_eq!(
            config.tcp_listen_address().await.unwrap().unwrap().to_string(),
            "tcp://127.0.0.1:8127"
        );
    }

    #[tokio::test]
    async fn tcp_listen_address_uses_bind_host() {
        let config = test_config(Some("192.168.1.50"), false, 8127);
        assert_eq!(
            config.tcp_listen_address().await.unwrap().unwrap().to_string(),
            "tcp://192.168.1.50:8127"
        );
    }

    #[tokio::test]
    async fn tcp_listen_address_non_local_traffic_overrides_bind_host() {
        let config = test_config(Some("192.168.1.50"), true, 8127);
        assert_eq!(
            config.tcp_listen_address().await.unwrap().unwrap().to_string(),
            "tcp://0.0.0.0:8127"
        );
    }

    #[cfg(unix)]
    #[test]
    fn uds_listen_address_parses_path() {
        let mut config = test_config(None, false, 0);
        config.receiver_socket = "/tmp/apm.sock".to_string();
        assert_eq!(
            config.uds_listen_address().unwrap().unwrap().to_string(),
            "unix:///tmp/apm.sock"
        );
    }
}
