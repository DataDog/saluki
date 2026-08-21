use std::{convert::Infallible, time::Duration};

use async_trait::async_trait;
use http::Request;
use saluki_common::sync::shutdown::ShutdownHandle;
use saluki_core::runtime::{InitializationError, ShutdownStrategy, Supervisable, SupervisorFuture};
use saluki_error::ErrorContext as _;
use tokio::{pin, select, sync::oneshot, time::timeout};
use tonic::{
    body::Body,
    server::NamedService,
    service::Routes,
    transport::{server::TcpIncoming, Server},
};
use tower::Service;
use tracing::warn;

#[cfg(unix)]
use crate::net::unix::{ensure_unix_socket_free, set_unix_socket_write_only};
use crate::net::ListenAddress;

/// Resolved keepalive parameters for a gRPC server.
///
/// All fields are already resolved by the caller before constructing this struct. `None` means
/// "do not apply this setting." When constructed via `resolve_grpc_keepalive`, unset `time` and
/// `timeout` default to 2 hours and 20 seconds respectively; `max_connection_age` and
/// `max_connection_age_grace` default to no limit.
#[derive(Clone, Debug, Default)]
pub struct GrpcKeepalive {
    /// Interval between HTTP/2 keepalive PING frames. `None` disables server-initiated keepalive.
    pub http2_keepalive_interval: Option<Duration>,

    /// Timeout for receiving a PONG after a keepalive PING before closing the connection.
    pub http2_keepalive_timeout: Option<Duration>,

    /// Maximum duration a connection may exist before the server sends GOAWAY. `None` means no
    /// limit.
    pub max_connection_age: Option<Duration>,

    /// Grace period after `max_connection_age` before the connection is forcibly closed. `None` means
    /// no limit.
    pub max_connection_age_grace: Option<Duration>,
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
    keepalive: Option<GrpcKeepalive>,
}

impl GrpcServer {
    /// Creates an empty server with no attached services, configured to listen on the given address.
    pub fn new(listen_addr: ListenAddress) -> Self {
        Self {
            listen_addr,
            routes: None,
            graceful_shutdown_timeout: None,
            keepalive: None,
        }
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
        self.keepalive = Some(keepalive);
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
        let mut server = Server::default();
        if let Some(ref ka) = self.keepalive {
            server = server.http2_keepalive_interval(ka.http2_keepalive_interval);
            if let Some(timeout) = ka.http2_keepalive_timeout {
                server = server.http2_keepalive_timeout(Some(timeout));
            }
            if let Some(age) = ka.max_connection_age {
                server = server.max_connection_age(age);
            }
            if let Some(grace) = ka.max_connection_age_grace {
                server = server.max_connection_age_grace(grace);
            }
        }

        match &self.listen_addr {
            ListenAddress::Tcp(addr) => {
                let listener = TcpIncoming::bind(*addr)
                    .with_error_context(|| format!("Failed to bind listener for gRPC server ({}).", addr))?;

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
            _ => Err(InitializationError::Failed {
                source: saluki_error::generic_error!("gRPC endpoint must be a TCP or Unix address."),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::net::{SocketAddr, TcpListener as StdTcpListener};

    use saluki_common::sync::shutdown::ShutdownCoordinator;
    use tokio::io::AsyncWriteExt as _;
    use tokio::net::TcpStream;
    use tokio::time::timeout;

    use super::*;

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
