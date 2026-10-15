//! APM relay.
//!
//! Listens on TCP and/or a Unix domain socket where the trace-agent normally listens and forwards every request
//! to the relocated trace-agent.

use std::sync::LazyLock;

use agent_data_plane_config::domains;
use async_trait::async_trait;
use axum::extract::{Request as AxumRequest, State};
use axum::response::IntoResponse;
use axum::Router;
use saluki_api::APIHandler;
use saluki_core::accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_core::components::relays::{Relay, RelayBuilder, RelayContext};
use saluki_core::components::BuildContext;
use saluki_core::data_model::payload::PayloadType;
use saluki_core::runtime;
use saluki_core::topology::OutputDefinition;
use saluki_error::{generic_error, GenericError};
use saluki_io::net::server::http::HttpServer;
use saluki_io::net::ListenAddress;
use tokio::{pin, select};
use tracing::debug;

mod proxy;
use self::proxy::TraceAgentProxy;

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

    /// URL of the trace-agent that every request other than `POST /v1.0/traces` is proxied to: `http://host:port` or
    /// `unix:///path/to/socket`.
    pub proxy_destination: String,
}

impl ApmRelayConfiguration {
    /// Creates a new `ApmRelayConfiguration` from the resolved configuration.
    pub fn from_configuration(config: &domains::apm::Domain) -> Self {
        // receiver_port `0` disables the TCP listener
        let receiver_endpoint = if config.receiver_port == 0 {
            String::new()
        } else {
            let listen_address = if config.non_local_traffic {
                "0.0.0.0"
            } else {
                // TODO: resolve `bind_host` instead of hardcoding
                "127.0.0.1"
            };
            format!("{}:{}", listen_address, config.receiver_port)
        };
        Self {
            receiver_endpoint,
            receiver_socket: config.receiver_socket.clone().unwrap_or_default(),
            proxy_destination: config.proxy_destination.to_owned(),
        }
    }

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
            proxy,
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
}

#[async_trait]
impl Relay for ApmRelay {
    async fn run(self: Box<Self>, mut context: RelayContext) -> Result<(), GenericError> {
        let Self {
            tcp_endpoint,
            uds_endpoint,
            proxy,
        } = *self;

        let global_shutdown = context.take_shutdown_handle();
        pin!(global_shutdown);

        let mut health = context.take_health_handle();
        let worker_pool = context.topology_context().global_thread_pool().clone();
        let api_handler = ApmRelayAPIHandler::new(RelayState { proxy });
        let router = api_handler
            .generate_routes()
            .with_state(api_handler.generate_initial_state());

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
                .with_routes(router.clone())
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
}

struct ApmRelayAPIHandler {
    state: RelayState,
}

impl ApmRelayAPIHandler {
    fn new(state: RelayState) -> Self {
        Self { state }
    }

    async fn proxy_request(State(state): State<RelayState>, request: AxumRequest) -> impl IntoResponse {
        state.proxy.forward(request).await
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
    use std::time::Duration;

    use http::{Request, StatusCode};
    use saluki_core::components::test_util::TestComponentSupervisor;
    use saluki_core::runtime::state::{DataspaceUpdate, IdentifierFilter};
    use saluki_io::net::BoundListenAddress;
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
        })
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
        let supervisor = TestComponentSupervisor::start("apm-relay-uds-test").await;

        let dir = tempdir().expect("temp dir should be created");
        let socket_path = dir.path().join("apm.sock");
        let uds_endpoint = ListenAddress::try_from(format!("unix://{}", socket_path.display()).as_str())
            .expect("Unix APM endpoint should parse");

        let router = test_router();
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
            proxy_destination: "http://127.0.0.1:8127".to_string(),
        };
        assert_eq!(
            config.tcp_listen_address().unwrap().unwrap().to_string(),
            "tcp://0.0.0.0:8127"
        );
    }

    #[cfg(unix)]
    #[test]
    fn uds_listen_address_parses_path() {
        let config = ApmRelayConfiguration {
            receiver_endpoint: String::new(),
            receiver_socket: "/tmp/apm.sock".to_string(),
            proxy_destination: "http://127.0.0.1:8127".to_string(),
        };
        assert_eq!(
            config.uds_listen_address().unwrap().unwrap().to_string(),
            "unix:///tmp/apm.sock"
        );
    }
}
