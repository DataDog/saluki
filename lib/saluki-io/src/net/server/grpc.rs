use std::{
    convert::Infallible,
    io,
    net::SocketAddr,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};

use async_trait::async_trait;
use http::Request;
use rustls::ServerConfig;
use saluki_common::sync::shutdown::ShutdownHandle;
use saluki_core::runtime::{
    state::{DataspaceRegistry, Identifier},
    InitializationError, ShutdownStrategy, Supervisable, SupervisorFuture,
};
use saluki_error::ErrorContext as _;
use saluki_tls::ensure_server_config_fips_compliant;
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    pin, select,
    sync::{mpsc, oneshot},
    time::timeout,
};
use tokio_rustls::server::TlsStream as TokioTlsStream;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{
    body::Body,
    server::NamedService,
    service::Routes,
    transport::{
        server::{Connected, TcpIncoming},
        Server,
    },
};
use tower::Service;
use tracing::{debug, warn};

#[cfg(unix)]
use crate::net::unix::{ensure_unix_socket_free, set_unix_socket_write_only};
use crate::net::{server::BoundServerAddress, ListenAddress};

/// Resolved keepalive parameters for a gRPC server.
///
/// Defaults are 2 h interval, 20 s timeout, and no connection age limit.
#[derive(Clone, Debug)]
pub struct GrpcKeepalive {
    /// Interval between HTTP/2 keepalive PING frames.
    pub http2_keepalive_interval: Duration,

    /// Timeout for receiving a PONG after a keepalive PING before closing the connection.
    pub http2_keepalive_timeout: Duration,

    /// Maximum duration a connection may exist before the server sends GOAWAY. A zero duration
    /// means no limit.
    pub max_connection_age: Duration,

    /// Grace period after `max_connection_age` before the connection is forcibly closed. A zero
    /// duration means no limit.
    pub max_connection_age_grace: Duration,
}

impl Default for GrpcKeepalive {
    fn default() -> Self {
        Self {
            http2_keepalive_interval: Duration::from_secs(2 * 60 * 60),
            http2_keepalive_timeout: Duration::from_secs(20),
            max_connection_age: Duration::ZERO,
            max_connection_age_grace: Duration::ZERO,
        }
    }
}

/// A gRPC server.
///
/// Allows serving multiple gRPC services from a single endpoint.
///
/// This type is a thin wrapper over helper types from `tonic` and `axum`, and principally is meant to provide an opaque
/// gRPC server implementation that operates correctly when run under supervision. As such, this type can't be manually
/// served: it is only usable by adding it to a supervisor.
///
/// # Supervision
///
/// The listen address is bound during initialization, so a failure to bind is raised before the supervised worker
/// starts running, and a restart rebinds.
///
/// The server will attempt to gracefully shutdown existing connections when the parent supervisor signals shutdown.
/// This will cause the worker to utilize the maximum allowable grace period during shutdown: it will attempt to take as
/// long as necessary to gracefully shutdown existing connections, bounded only by the parent supervisor.
pub struct GrpcServer {
    listen_addr: ListenAddress,
    routes: Option<Routes>,
    graceful_shutdown_timeout: Option<Duration>,
    keepalive: GrpcKeepalive,
    tls_config: Option<ServerConfig>,
    bound_address_id: Option<Identifier>,
    max_concurrent_streams: u32,
}

impl GrpcServer {
    /// Creates an empty server with no attached services, configured to listen on the given address.
    pub fn new(listen_addr: ListenAddress) -> Self {
        Self {
            listen_addr,
            routes: None,
            graceful_shutdown_timeout: None,
            keepalive: GrpcKeepalive::default(),
            tls_config: None,
            bound_address_id: None,
            max_concurrent_streams: 0,
        }
    }

    /// Sets the identifier used to publish the bound TCP address.
    ///
    /// Defaults to not publishing the address. Unix listen addresses never publish an address.
    pub fn with_bound_address_id(mut self, id: impl Into<Identifier>) -> Self {
        self.bound_address_id = Some(id.into());
        self
    }

    /// Sets the graceful shutdown timeout for this server.
    ///
    /// During shutdown, the server will for all in-flight connections to complete before ultimately completing itself.
    /// When no timeout is specified, this will lead to the worker taking the maximum allowable time to shutdown if
    /// connections are blocked or otherwise "stuck." Setting an explicit graceful shutdown timeout will cause the
    /// worker to bound how long it waits for in-flight connections to shutdown before forcefully completing and moving
    /// on.
    ///
    /// Defaults to no timeout (wait as long as allowed).
    pub fn with_graceful_shutdown_timeout(mut self, timeout: Duration) -> Self {
        self.graceful_shutdown_timeout = Some(timeout);
        self
    }

    /// Sets the TLS configuration for the server.
    ///
    /// This enables TLS, after which the server only accepts connections that are encrypted with TLS.
    ///
    /// Defaults to TLS being disabled.
    pub fn with_tls_config(mut self, config: ServerConfig) -> Self {
        self.tls_config = Some(config);
        self
    }

    fn publish_bound_address<F>(&self, local_addr: F, configured_addr: &SocketAddr) -> Result<(), InitializationError>
    where
        F: FnOnce() -> io::Result<SocketAddr>,
    {
        let Some(id) = self.bound_address_id.clone() else {
            return Ok(());
        };

        let bound_address = local_addr()
            .with_error_context(|| format!("Failed to query bound address for gRPC server ({}).", configured_addr))?;
        let dataspace = DataspaceRegistry::try_current()
            .ok_or_else(|| saluki_error::generic_error!("Dataspace not available for gRPC server."))?;
        dataspace.assert(BoundServerAddress(bound_address), id);

        Ok(())
    }

    /// Adds a new service to this server.
    pub fn add_service<S>(mut self, svc: S) -> Self
    where
        S: Service<Request<Body>, Error = Infallible> + NamedService + Clone + Send + Sync + 'static,
        S::Response: axum::response::IntoResponse,
        S::Future: Send + 'static,
    {
        let routes = self.routes.take().unwrap_or_default().add_service(svc);

        Self {
            routes: Some(routes),
            ..self
        }
    }

    /// Sets the keepalive parameters for this server.
    pub fn with_keepalive(mut self, keepalive: GrpcKeepalive) -> Self {
        self.keepalive = keepalive;
        self
    }

    /// Sets the HTTP/2 maximum concurrent streams per connection.
    ///
    /// A value of `0` (the default) means no limit. A positive value sets the
    /// `SETTINGS_MAX_CONCURRENT_STREAMS` HTTP/2 setting, bounding how many concurrent streams a
    /// single connection may have.
    pub fn with_max_concurrent_streams(mut self, max_concurrent_streams: u32) -> Self {
        self.max_concurrent_streams = max_concurrent_streams;
        self
    }
}

#[async_trait]
impl Supervisable for GrpcServer {
    fn name(&self) -> &str {
        "grpc_server"
    }

    fn shutdown_strategy(&self) -> ShutdownStrategy {
        // Utilize the maximum allowable grace period to give connections a chance to gracefully shutdown.
        ShutdownStrategy::Graceful(Duration::MAX)
    }

    async fn initialize(&self, process_shutdown: ShutdownHandle) -> Result<SupervisorFuture, InitializationError> {
        let routes = self.routes.clone().unwrap_or_default();
        let shutdown_timeout = self.graceful_shutdown_timeout.unwrap_or(Duration::MAX);

        // Build the tonic server with keepalive settings applied.
        let ka = &self.keepalive;
        let mut server = Server::default()
            .http2_keepalive_interval(Some(ka.http2_keepalive_interval))
            .http2_keepalive_timeout(Some(ka.http2_keepalive_timeout));
        if ka.max_connection_age != Duration::ZERO {
            server = server.max_connection_age(ka.max_connection_age);
        }
        if ka.max_connection_age_grace != Duration::ZERO {
            server = server.max_connection_age_grace(ka.max_connection_age_grace);
        }
        if self.max_concurrent_streams > 0 {
            server = server.max_concurrent_streams(self.max_concurrent_streams);
        }

        // Prepare TLS config once: both TCP and Unix paths use the same acceptor.
        let tls_acceptor = if let Some(mut config) = self.tls_config.clone() {
            ensure_server_config_fips_compliant(&mut config)
                .error_context("Failed to configure TLS for gRPC server")?;

            // ALPN: advertise h2 so TLS clients negotiate HTTP/2 directly.
            config.alpn_protocols.push(b"h2".to_vec());

            Some(tokio_rustls::TlsAcceptor::from(Arc::new(config)))
        } else {
            None
        };

        match &self.listen_addr {
            ListenAddress::Tcp(addr) => {
                if let Some(acceptor) = tls_acceptor {
                    let listener = tokio::net::TcpListener::bind(*addr)
                        .await
                        .with_error_context(|| format!("Failed to bind listener for gRPC server ({}).", addr))?;
                    self.publish_bound_address(|| listener.local_addr(), addr)?;

                    // Spawn each TLS handshake concurrently so a stalled client cannot block the accept loop.
                    let incoming = spawn_tls_handshake_loop(listener, acceptor);

                    Ok(Box::pin(async move {
                        let (drain_tx, drain_rx) = oneshot::channel();
                        let serve = server.serve_with_incoming_shutdown(routes, incoming, async move {
                            let _ = drain_rx.await;
                        });

                        pin!(serve, process_shutdown);

                        select! {
                            result = &mut serve => result.error_context("Failed to serve gRPC server."),

                            _ = &mut process_shutdown => {
                                let _ = drain_tx.send(());

                                match timeout(shutdown_timeout, serve).await {
                                    Ok(Ok(())) => Ok(()),
                                    Ok(Err(e)) => Err(e).error_context("Failed to serve gRPC server."),
                                    Err(_) => {
                                        warn!("Failed to gracefully drain gRPC connections.");
                                        Ok(())
                                    },
                                }
                            },
                        }
                    }))
                } else {
                    let listener = TcpIncoming::bind(*addr)
                        .with_error_context(|| format!("Failed to bind listener for gRPC server ({}).", addr))?;
                    self.publish_bound_address(|| listener.local_addr(), addr)?;

                    Ok(Box::pin(async move {
                        let (drain_tx, drain_rx) = oneshot::channel();
                        let serve = server.serve_with_incoming_shutdown(routes, listener, async move {
                            let _ = drain_rx.await;
                        });

                        pin!(serve, process_shutdown);

                        select! {
                            result = &mut serve => result.error_context("Failed to serve gRPC server."),

                            _ = &mut process_shutdown => {
                                let _ = drain_tx.send(());

                                match timeout(shutdown_timeout, serve).await {
                                    Ok(Ok(())) => Ok(()),
                                    Ok(Err(e)) => Err(e).error_context("Failed to serve gRPC server."),
                                    Err(_) => {
                                        warn!("Failed to gracefully drain gRPC connections.");
                                        Ok(())
                                    },
                                }
                            },
                        }
                    }))
                }
            }
            #[cfg(unix)]
            ListenAddress::Unix(path) => {
                let path = path.clone();
                ensure_unix_socket_free(&path)
                    .await
                    .with_error_context(|| format!("Failed to clear gRPC Unix socket '{}'.", path.display()))?;
                let listener = tokio::net::UnixListener::bind(&path)
                    .with_error_context(|| format!("Failed to bind gRPC Unix listener on '{}'.", path.display()))?;
                set_unix_socket_write_only(&path).await.with_error_context(|| {
                    format!("Failed to set permissions on gRPC Unix socket '{}'.", path.display())
                })?;

                if let Some(acceptor) = tls_acceptor {
                    // TLS over Unix sockets: same concurrent handshake pattern as TCP.
                    let incoming = spawn_tls_handshake_loop(listener, acceptor);

                    Ok(Box::pin(async move {
                        let (drain_tx, drain_rx) = oneshot::channel();
                        let serve = server.serve_with_incoming_shutdown(routes, incoming, async move {
                            let _ = drain_rx.await;
                        });

                        pin!(serve, process_shutdown);

                        select! {
                            result = &mut serve => result.error_context("Failed to serve gRPC server."),

                            _ = &mut process_shutdown => {
                                let _ = drain_tx.send(());

                                match timeout(shutdown_timeout, serve).await {
                                    Ok(Ok(())) => Ok(()),
                                    Ok(Err(e)) => Err(e).error_context("Failed to serve gRPC server."),
                                    Err(_) => {
                                        warn!("Failed to gracefully drain gRPC connections.");
                                        Ok(())
                                    },
                                }
                            },
                        }
                    }))
                } else {
                    let incoming = tokio_stream::wrappers::UnixListenerStream::new(listener);

                    Ok(Box::pin(async move {
                        let (drain_tx, drain_rx) = oneshot::channel();
                        let serve = server.serve_with_incoming_shutdown(routes, incoming, async move {
                            let _ = drain_rx.await;
                        });

                        pin!(serve, process_shutdown);

                        select! {
                            result = &mut serve => result.error_context("Failed to serve gRPC server."),

                            _ = &mut process_shutdown => {
                                let _ = drain_tx.send(());

                                match timeout(shutdown_timeout, serve).await {
                                    Ok(Ok(())) => Ok(()),
                                    Ok(Err(e)) => Err(e).error_context("Failed to serve gRPC server."),
                                    Err(_) => {
                                        warn!("Failed to gracefully drain gRPC connections.");
                                        Ok(())
                                    },
                                }
                            },
                        }
                    }))
                }
            }
            _ => Err(InitializationError::Failed {
                source: saluki_error::generic_error!("gRPC endpoint must be a TCP or Unix address."),
            }),
        }
    }
}

/// Accepts connections from a listener and performs TLS handshakes concurrently.
///
/// Each accepted connection gets its own spawned task for the TLS handshake, so a stalled client that connects
/// without sending a TLS ClientHello cannot block subsequent connections from being accepted.
fn spawn_tls_handshake_loop<L, S>(
    listener: L, acceptor: tokio_rustls::TlsAcceptor,
) -> ReceiverStream<Result<TlsServerStream<S>, std::io::Error>>
where
    L: AcceptConnection<S> + Send + 'static,
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (tx, rx) = mpsc::channel(128);

    tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok(stream) => {
                    let acceptor = acceptor.clone();
                    let tx = tx.clone();

                    tokio::spawn(async move {
                        match acceptor.accept(stream).await {
                            Ok(tls_stream) => {
                                let _ = tx.send(Ok(TlsServerStream(tls_stream))).await;
                            }
                            Err(e) => {
                                debug!(error = %e, "gRPC TLS handshake failed; skipping connection.");
                            }
                        }
                    });
                }
                Err(e) => {
                    let _ = tx.send(Err(e)).await;
                    break;
                }
            }
        }
    });

    ReceiverStream::new(rx)
}

/// Trait abstracting over `TcpListener` and `UnixListener` accept loops.
trait AcceptConnection<S> {
    fn accept(&self) -> impl std::future::Future<Output = std::io::Result<S>> + Send;
}

#[allow(clippy::manual_async_fn)]
impl AcceptConnection<tokio::net::TcpStream> for tokio::net::TcpListener {
    fn accept(&self) -> impl std::future::Future<Output = std::io::Result<tokio::net::TcpStream>> + Send {
        async { self.accept().await.map(|(stream, _)| stream) }
    }
}

#[cfg(unix)]
#[allow(clippy::manual_async_fn)]
impl AcceptConnection<tokio::net::UnixStream> for tokio::net::UnixListener {
    fn accept(&self) -> impl std::future::Future<Output = std::io::Result<tokio::net::UnixStream>> + Send {
        async { self.accept().await.map(|(stream, _)| stream) }
    }
}

/// A TLS-wrapped stream that implements tonic's `Connected` trait.
///
/// This wrapper is needed because `tokio_rustls::server::TlsStream` only implements tonic's `Connected` trait when
/// the `tls-connect-info` feature is enabled, which we do not use. We provide a minimal `Connected` implementation
/// with `ConnectInfo = ()` since the gRPC server does not need connection-level metadata.
struct TlsServerStream<S>(TokioTlsStream<S>);

impl<S: AsyncRead + AsyncWrite + Unpin> AsyncRead for TlsServerStream<S> {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_read(cx, buf)
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> AsyncWrite for TlsServerStream<S> {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.0).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_shutdown(cx)
    }
}

impl<S> Connected for TlsServerStream<S> {
    type ConnectInfo = ();

    fn connect_info(&self) -> Self::ConnectInfo {}
}

#[cfg(test)]
mod tests {
    use std::net::{SocketAddr, TcpListener as StdTcpListener};

    use saluki_common::sync::shutdown::ShutdownCoordinator;
    #[cfg(unix)]
    use saluki_core::runtime::state::IdentifierFilter;
    use saluki_tls::test_util::SelfSignedCert;
    #[cfg(unix)]
    use tokio::io::AsyncReadExt as _;
    use tokio::io::AsyncWriteExt as _;
    use tokio::net::TcpStream;
    use tokio::time::timeout;

    use super::*;
    use crate::net::server::test_util::ServerTestHarness;
    #[cfg(unix)]
    use crate::net::server::{test_util::connect_unix, BoundServerAddress};

    /// Bound on any server await in these tests, so a hang fails rather than stalling the suite.
    const TEST_TIMEOUT: Duration = Duration::from_secs(10);

    /// Reserves a loopback port and releases it, yielding an address a server can bind.
    fn free_local_addr() -> SocketAddr {
        let listener = StdTcpListener::bind("127.0.0.1:0").expect("should bind an ephemeral port");
        let addr = listener.local_addr().expect("should have a local address");
        drop(listener);
        addr
    }

    #[tokio::test]
    async fn publishes_bound_tcp_address() {
        let harness = ServerTestHarness::start("grpc-bound-address", |supervisor| {
            let listen_address = ListenAddress::Tcp("127.0.0.1:0".parse().expect("address should parse"));
            let server = GrpcServer::new(listen_address).with_bound_address_id("grpc-bound-address");
            supervisor.add_worker(server);
        })
        .await;

        let address = harness.bound_address("grpc-bound-address").await;
        assert_ne!(address.port(), 0);
        let stream = TcpStream::connect(address)
            .await
            .expect("should connect to published address");
        drop(stream);

        harness.shutdown().await;
    }

    #[tokio::test]
    async fn tls_server_publishes_bound_tcp_address() {
        let _ = saluki_tls::initialize_default_crypto_provider();
        let cert = SelfSignedCert::localhost();
        let tls_config = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(cert.cert_chain(), cert.private_key())
            .expect("should build TLS config");
        let harness = ServerTestHarness::start("grpc-tls-bound-address", move |supervisor| {
            let listen_address = ListenAddress::Tcp("127.0.0.1:0".parse().expect("address should parse"));
            let server = GrpcServer::new(listen_address)
                .with_tls_config(tls_config)
                .with_bound_address_id("grpc-tls-bound-address");
            supervisor.add_worker(server);
        })
        .await;

        let address = harness.bound_address("grpc-tls-bound-address").await;
        assert_ne!(address.port(), 0);
        let stream = TcpStream::connect(address)
            .await
            .expect("should connect to published address");
        drop(stream);

        harness.shutdown().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unix_listener_does_not_publish_bound_address() {
        let tempdir = tempfile::tempdir().expect("should create temp dir");
        let socket_path = tempdir.path().join("grpc.sock");
        let listen_address = ListenAddress::Unix(socket_path.clone());
        let harness = ServerTestHarness::start("grpc-unix-bound-address", move |supervisor| {
            let server = GrpcServer::new(listen_address).with_bound_address_id("grpc-unix-bound-address");
            supervisor.add_worker(server);
        })
        .await;

        let mut stream = connect_unix(&socket_path).await;
        stream
            .write_all(b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n")
            .await
            .expect("should write HTTP/2 preface");
        let mut response = [0; 1];
        timeout(TEST_TIMEOUT, stream.read_exact(&mut response))
            .await
            .expect("server should respond over Unix listener")
            .expect("should read server response");
        assert!(
            harness
                .dataspace
                .current_values::<BoundServerAddress>(IdentifierFilter::all())
                .is_empty(),
            "Unix listener should not publish a bound address"
        );
        drop(stream);

        harness.shutdown().await;
    }

    #[tokio::test]
    async fn binds_during_initialization() {
        // The listener is bound by `initialize`, so the port is taken before anything serves. That is what makes a bind
        // failure a non-restartable initialization error rather than a runtime one.
        let addr = free_local_addr();
        let run = GrpcServer::new(ListenAddress::Tcp(addr))
            .initialize(ShutdownHandle::noop())
            .await
            .expect("should initialize");

        assert!(
            StdTcpListener::bind(addr).is_err(),
            "initialization should have bound {addr} before the worker future ran"
        );

        drop(run);
    }

    #[tokio::test]
    async fn bind_failure_is_an_initialization_error() {
        let addr = free_local_addr();
        let _held = StdTcpListener::bind(addr).expect("should hold the address");

        match GrpcServer::new(ListenAddress::Tcp(addr))
            .initialize(ShutdownHandle::noop())
            .await
        {
            Ok(_) => panic!("initialization should have failed to bind {addr}"),
            Err(e) => {
                let error = e.to_string();
                assert!(error.contains("Failed to bind listener"), "unexpected error: {error}");
            }
        }
    }

    #[tokio::test]
    async fn releases_its_port_once_the_worker_finishes() {
        let addr = free_local_addr();
        let mut coordinator = ShutdownCoordinator::default();
        let run = GrpcServer::new(ListenAddress::Tcp(addr))
            .initialize(coordinator.register())
            .await
            .expect("should initialize");

        coordinator.shutdown();
        timeout(TEST_TIMEOUT, run)
            .await
            .expect("server should stop on shutdown")
            .expect("server should stop cleanly");

        assert!(
            StdTcpListener::bind(addr).is_ok(),
            "the server should have released {addr} when its worker finished"
        );
    }

    #[tokio::test]
    async fn an_idle_peer_does_not_wedge_the_drain() {
        // `tonic` waits for every connection to close and imposes no bound of its own, so a peer that connects and then
        // does nothing would otherwise hold shutdown open indefinitely.
        let addr = free_local_addr();
        let mut coordinator = ShutdownCoordinator::default();
        let run = GrpcServer::new(ListenAddress::Tcp(addr))
            .with_graceful_shutdown_timeout(Duration::from_secs(1))
            .initialize(coordinator.register())
            .await
            .expect("should initialize");
        let run = tokio::spawn(run);

        let mut stream = TcpStream::connect(addr).await.expect("should connect");
        stream.write_all(b"PRI * HTTP/2.0\r\n").await.expect("should write");
        stream.flush().await.expect("should flush");
        tokio::time::sleep(Duration::from_millis(100)).await;

        coordinator.shutdown();
        timeout(TEST_TIMEOUT, run)
            .await
            .expect("server should finish draining rather than waiting on an idle peer")
            .expect("server task should not panic")
            .expect("server should stop cleanly");
    }
}
