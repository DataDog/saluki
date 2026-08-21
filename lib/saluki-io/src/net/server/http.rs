//! HTTP servers.
//!
//! [`HttpServer`] is the supervised server, and is what new code should use: it runs as a child of whatever supervisor
//! it is added to, binds its listener during initialization, and drains in-flight connections before it reports being
//! done. [`UnsupervisedHttpServer`] is the older, self-spawning form, kept only until its remaining callers move over.

use std::{
    future::Future,
    pin::Pin,
    sync::Arc,
    task::{ready, Context, Poll},
    time::Duration,
};

use async_trait::async_trait;
use http::{Request, Response};
use http_body::Body;
use hyper::{
    body::Incoming,
    rt::{Read, Write},
    service::Service,
};
use hyper_util::{
    rt::{TokioExecutor, TokioIo, TokioTimer},
    server::conn::auto::Builder,
};
use rustls::ServerConfig;
use saluki_common::{
    sync::shutdown::{ShutdownCoordinator, ShutdownHandle},
    task::{spawn_traced_named, HandleExt as _},
};
use saluki_core::runtime::{InitializationError, ShutdownStrategy, Supervisable, SupervisorFuture};
use saluki_error::{ErrorContext as _, GenericError};
use saluki_tls::ensure_server_config_fips_compliant;
use tokio::{pin, runtime::Handle, select, sync::oneshot, time::timeout};
use tokio_rustls::TlsAcceptor;
use tracing::{debug, error, info, warn};

use crate::net::{listener::ConnectionOrientedListener, ListenAddress};

fn build_conn_builder() -> Builder<TokioExecutor> {
    let mut builder = Builder::new(TokioExecutor::new());
    builder
        .http1()
        .timer(TokioTimer::new())
        .header_read_timeout(Duration::from_secs(10));
    builder
}

/// An HTTP server.
///
/// Serves a single [`Service`] over a connection-oriented listener, optionally with TLS. The server can't be run
/// directly: it is only usable by adding it to a supervisor.
///
/// # Supervision
///
/// The listen address is bound during initialization, so a failure to bind is raised before the supervised worker
/// starts running, and a restart rebinds.
///
/// The server will attempt to gracefully shutdown existing connections when the parent supervisor signals shutdown.
/// This will cause the worker to utilize the maximum allowable grace period during shutdown: it will attempt to take as
/// long as necessary to gracefully shutdown existing connections, bounded only by the parent supervisor.
pub struct HttpServer<S> {
    listen_address: ListenAddress,
    tls_config: Option<ServerConfig>,
    conn_builder: Builder<TokioExecutor>,
    graceful_shutdown_timeout: Option<Duration>,
    service: S,
}

impl<S> HttpServer<S> {
    /// Creates a server that will listen on the given address.
    pub fn from_listen_address(listen_address: ListenAddress, service: S) -> Self {
        Self {
            listen_address,
            tls_config: None,
            conn_builder: build_conn_builder(),
            graceful_shutdown_timeout: None,
            service,
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

    /// Sets the TLS configuration for the server.
    ///
    /// This enables TLS, after which the server only accepts connections that are encrypted with TLS.
    ///
    /// Defaults to TLS being disabled.
    pub fn with_tls_config(mut self, config: ServerConfig) -> Self {
        self.tls_config = Some(config);
        self
    }
}

#[async_trait]
impl<S, B> Supervisable for HttpServer<S>
where
    S: Service<Request<Incoming>, Response = Response<B>> + Send + Sync + Clone + 'static,
    S::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
    S::Future: Send + 'static,
    B: Body + Send + 'static,
    B::Data: Send,
    B::Error: std::error::Error + Send + Sync,
{
    fn name(&self) -> &str {
        "http_server"
    }

    fn shutdown_strategy(&self) -> ShutdownStrategy {
        // Utilize the maximum allowable grace period to give connections a chance to gracefully shutdown.
        ShutdownStrategy::Graceful(Duration::MAX)
    }

    async fn initialize(&self, process_shutdown: ShutdownHandle) -> Result<SupervisorFuture, InitializationError> {
        // Try binding our listener during initialization to surface issues earlier.
        let listener = ConnectionOrientedListener::from_listen_address(self.listen_address.clone())
            .await
            .with_error_context(|| format!("Failed to bind listener for HTTP server ({}).", self.listen_address))?;

        let conn_builder = self.conn_builder.clone();
        let service = self.service.clone();
        let tls_config = self.tls_config.clone();
        let maybe_shutdown_timeout = self.graceful_shutdown_timeout;

        // Connection handlers land on whichever runtime the supervisor placed this worker on, so nothing needs to be
        // threaded through: where the server runs is decided at the point it's spawned.
        //
        // TODO: Create our own custom `Executor` impl that can be used to bridge to a given supervisor such that we
        // spawn dynamic/temporary child workers instead of directly on the underlying Tokio runtime.
        let executor = Handle::current();

        Ok(Box::pin(run_accept_loop(
            listener,
            conn_builder,
            service,
            tls_config,
            executor,
            process_shutdown,
            maybe_shutdown_timeout,
        )))
    }
}

/// An HTTP server that spawns and manages itself.
///
/// # Deprecated
///
/// Callers should generally prefer to use [`HttpServer`], as it is designed to run under supervision and play nicely
/// with supervision trees: graceful shutdown, spawning of connection handlers in the right place, etc.
pub struct UnsupervisedHttpServer<S> {
    listener: ConnectionOrientedListener,
    tls_config: Option<ServerConfig>,
    conn_builder: Builder<TokioExecutor>,
    executor: Handle,
    service: S,
}

impl<S> UnsupervisedHttpServer<S> {
    /// Creates a new `UnsupervisedHttpServer` from the given listener and service.
    ///
    /// # Panics
    ///
    /// This will panic if called outside the context of a Tokio runtime.
    pub fn from_listener(listener: ConnectionOrientedListener, service: S) -> Self {
        Self {
            listener,
            tls_config: None,
            conn_builder: build_conn_builder(),
            executor: Handle::current(),
            service,
        }
    }

    /// Sets the TLS configuration for the server.
    ///
    /// This will enable TLS for the server, and the server will only accept connections that are encrypted with TLS.
    ///
    /// Defaults to TLS being disabled.
    pub fn with_tls_config(mut self, config: ServerConfig) -> Self {
        self.tls_config = Some(config);
        self
    }

    /// Sets the executor for the server.
    ///
    /// This executor will be used for spawning tasks to handle incoming connections, but _not_ for the spawn that accepts
    /// new connections.
    ///
    /// Defaults to the current Tokio runtime at the time [`from_listener`][Self::from_listener] is called.
    pub fn with_executor(mut self, executor: Handle) -> Self {
        self.executor = executor;
        self
    }
}

impl<S, B> UnsupervisedHttpServer<S>
where
    S: Service<Request<Incoming>, Response = Response<B>> + Send + Clone + 'static,
    S::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
    S::Future: Send + 'static,
    B: Body + Send + 'static,
    B::Data: Send,
    B::Error: std::error::Error + Send + Sync,
{
    /// Starts the server and listens for incoming connections.
    ///
    /// Returns two handles: one for shutting down the server, and one for receiving any errors that occur while the
    /// server is running.
    pub fn listen(self) -> (ShutdownCoordinator, ErrorHandle) {
        let (shutdown_coordinator, shutdown) = ShutdownHandle::paired();
        let (error_tx, error_rx) = oneshot::channel();

        let Self {
            executor,
            listener,
            conn_builder,
            service,
            tls_config,
        } = self;

        spawn_traced_named("http-server-acceptor", async move {
            if let Err(e) = run_accept_loop(listener, conn_builder, service, tls_config, executor, shutdown, None).await
            {
                let _ = error_tx.send(e);
            }
        });

        (shutdown_coordinator, ErrorHandle(error_rx))
    }
}

/// Accepts connections until shutdown is signalled or the listener fails.
///
/// Returns once every connection it accepted has finished, so a caller that awaits this can be sure no request is still
/// being served.
async fn run_accept_loop<S, B>(
    mut listener: ConnectionOrientedListener, conn_builder: Builder<TokioExecutor>, service: S,
    tls_config: Option<ServerConfig>, executor: Handle, shutdown: ShutdownHandle,
    maybe_shutdown_timeout: Option<Duration>,
) -> Result<(), GenericError>
where
    S: Service<Request<Incoming>, Response = Response<B>> + Send + Clone + 'static,
    S::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
    S::Future: Send + 'static,
    B: Body + Send + 'static,
    B::Data: Send,
    B::Error: std::error::Error + Send + Sync,
{
    let maybe_tls_acceptor = match tls_config {
        Some(mut config) => {
            // Allow for HTTP/1.1 and HTTP/2.
            config.alpn_protocols.push(b"h2".to_vec());
            config.alpn_protocols.push(b"http/1.1".to_vec());

            ensure_server_config_fips_compliant(&mut config)?;

            Some(TlsAcceptor::from(Arc::new(config)))
        }
        None => None,
    };
    let tls_enabled = maybe_tls_acceptor.is_some();
    let listen_addr = listener.listen_address().clone();

    info!(%listen_addr, tls_enabled, "HTTP server started.");

    // Every connection handler holds a handle from this coordinator, which is what lets us wait for in-flight requests
    // below instead of abandoning them.
    let mut conn_shutdown_coordinator = ShutdownCoordinator::default();

    pin!(shutdown);

    let result = loop {
        select! {
            result = listener.accept() => match result {
                Ok(stream) => {
                    let conn_builder = conn_builder.clone();
                    let service = service.clone();
                    let listen_addr = listen_addr.clone();
                    let conn_shutdown = conn_shutdown_coordinator.register();

                    match &maybe_tls_acceptor {
                        Some(acceptor) => {
                            let tls_stream = match acceptor.accept(stream).await {
                                Ok(stream) => stream,
                                Err(e) => {
                                    error!(%listen_addr, error = %e, "Failed to complete TLS handshake.");
                                    continue
                                },
                            };

                            executor.spawn_traced_named("http_server_tls_conn", drive_connection(
                                conn_builder, TokioIo::new(tls_stream), service, listen_addr, conn_shutdown, maybe_shutdown_timeout
                            ));
                        },
                        None => {
                            executor.spawn_traced_named("http_server_conn", drive_connection(
                                conn_builder, TokioIo::new(stream), service, listen_addr, conn_shutdown, maybe_shutdown_timeout
                            ));
                        },
                    }
                },
                Err(e) => break Err(GenericError::from(e)),
            },

            _ = &mut shutdown => {
                debug!(%listen_addr, "Received shutdown signal.");
                break Ok(());
            }
        }
    };

    // We've stopped accepting; now let anything still being served finish before we report being done.
    debug!(%listen_addr, "Waiting for in-flight HTTP connections to finish...");
    conn_shutdown_coordinator.shutdown_and_wait().await;

    info!(%listen_addr, "HTTP server stopped.");

    result
}

/// Serves a single connection, finishing what it has started if asked to shut down.
///
/// When shutdown is triggered, the connection is gracefully shutdown: new requests aren't allowed, but any pending
/// or in-flight reads/writes will be completed prior to closing the connection.
async fn drive_connection<I, S, B>(
    conn_builder: Builder<TokioExecutor>, io: I, service: S, listen_addr: ListenAddress, shutdown: ShutdownHandle,
    maybe_shutdown_timeout: Option<Duration>,
) where
    I: Read + Write + Unpin + Send + 'static,
    S: Service<Request<Incoming>, Response = Response<B>> + 'static,
    S::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
    S::Future: Send + 'static,
    B: Body + Send + 'static,
    B::Data: Send,
    B::Error: std::error::Error + Send + Sync,
{
    let conn = conn_builder.serve_connection(io, service);
    pin!(conn, shutdown);

    select! {
        result = conn.as_mut() => if let Err(e) = result {
            error!(%listen_addr, error = %e, "Failed to serve HTTP connection.");
        },

        _ = &mut shutdown => {
            debug!(%listen_addr, "Draining HTTP connection.");

            conn.as_mut().graceful_shutdown();

            let shutdown_timeout = maybe_shutdown_timeout.unwrap_or(Duration::MAX);
            match timeout(shutdown_timeout, conn.as_mut()).await {
                Ok(Ok(())) => {},
                Ok(Err(e)) => warn!(%listen_addr, error = %e, "Failed to drain HTTP connection."),
                Err(_) => warn!(%listen_addr, "Failed to gracefully drain HTTP connection after {:?}.", shutdown_timeout)
            }
        },
    }
}

/// A future that resolves when [`UnsupervisedHttpServer`] encounters an unrecoverable error.
pub struct ErrorHandle(oneshot::Receiver<GenericError>);

impl Future for ErrorHandle {
    type Output = Option<GenericError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match ready!(Pin::new(&mut self.0).poll(cx)) {
            Ok(err) => Poll::Ready(Some(err)),
            Err(_) => Poll::Ready(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::net::{SocketAddr, TcpListener as StdTcpListener};
    use std::sync::atomic::{AtomicBool, Ordering};

    use http_body_util::Full;
    use hyper::service::service_fn;
    use saluki_common::sync::shutdown::ShutdownCoordinator;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    use tokio::net::TcpStream;
    use tokio::time::timeout;

    use super::*;

    /// Bound on any server await in these tests, so a hang fails rather than stalling the suite.
    const TEST_TIMEOUT: Duration = Duration::from_secs(5);

    /// Reserves a loopback port and releases it, yielding an address a server can bind.
    ///
    /// Inherently racy against anything else on the host, but the window is small and there is no way to hand an
    /// already-bound listener to the supervised server.
    fn free_local_addr() -> SocketAddr {
        let listener = StdTcpListener::bind("127.0.0.1:0").expect("should bind an ephemeral port");
        let addr = listener.local_addr().expect("should have a local address");
        drop(listener);
        addr
    }

    /// Builds a server whose handler runs `f` for every request.
    fn server_with<F, Fut>(
        addr: SocketAddr, f: F,
    ) -> HttpServer<
        impl Service<
                Request<Incoming>,
                Response = Response<Full<bytes::Bytes>>,
                Error = std::convert::Infallible,
                Future = Fut,
            > + Send
            + Sync
            + Clone
            + 'static,
    >
    where
        F: Fn() -> Fut + Send + Sync + Clone + 'static,
        Fut: Future<Output = Result<Response<Full<bytes::Bytes>>, std::convert::Infallible>> + Send + 'static,
    {
        let service = service_fn(move |_req: Request<Incoming>| f());
        HttpServer::from_listen_address(ListenAddress::Tcp(addr), service)
    }

    /// A handler that responds immediately.
    async fn ok_response() -> Result<Response<Full<bytes::Bytes>>, std::convert::Infallible> {
        Ok(Response::new(Full::new(bytes::Bytes::from_static(b"ok"))))
    }

    #[tokio::test]
    async fn binds_during_initialization() {
        // The listener is bound by `initialize`, not by the worker future, so the port is already taken before anything
        // starts serving. That is what makes a bind failure a non-restartable initialization error.
        let addr = free_local_addr();
        let server = server_with(addr, ok_response);

        let run = server
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
        // Hold the address so the server can't have it. An initialization error is non-restartable, which is the point:
        // an unusable listen address should fail the child rather than being retried forever.
        let addr = free_local_addr();
        let _held = StdTcpListener::bind(addr).expect("should hold the address");

        let server = server_with(addr, ok_response);
        match server.initialize(ShutdownHandle::noop()).await {
            Ok(_) => panic!("initialization should have failed to bind {addr}"),
            Err(e) => {
                let error = e.to_string();
                assert!(error.contains("Failed to bind listener"), "unexpected error: {error}");
            }
        }
    }

    #[tokio::test]
    async fn releases_its_port_once_the_worker_finishes() {
        // The whole reason for supervising the server: when its worker stops, the socket is gone. Previously the
        // acceptor was a detached task that outlived whatever spawned it.
        let addr = free_local_addr();
        let server = server_with(addr, ok_response);

        let mut coordinator = ShutdownCoordinator::default();
        let run = server
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
    async fn a_half_sent_request_does_not_wedge_the_drain() {
        // A peer that writes a partial request head and stalls keeps its connection permanently non-idle, so
        // `graceful_shutdown` alone never closes it. Before the connection builder had a timer and the drain had a
        // deadline, one such socket stalled shutdown indefinitely -- for the OTLP receivers that meant every ADP
        // shutdown hanging until the component budget forced an abort.
        let addr = free_local_addr();
        let server = server_with(addr, ok_response).with_graceful_shutdown_timeout(Duration::from_secs(1));

        let mut coordinator = ShutdownCoordinator::default();
        let run = server
            .initialize(coordinator.register())
            .await
            .expect("should initialize");
        let run = tokio::spawn(run);

        let mut stream = TcpStream::connect(addr).await.expect("should connect");
        stream
            .write_all(b"GET / HTTP/1.1\r\nHost: localhost")
            .await
            .expect("should write a partial request head");
        stream.flush().await.expect("should flush");

        // Let the server read what there is before signalling, so the connection is genuinely mid-parse.
        tokio::time::sleep(Duration::from_millis(100)).await;
        coordinator.shutdown();

        timeout(TEST_TIMEOUT, run)
            .await
            .expect("server should finish draining rather than waiting on a half-sent request")
            .expect("server task should not panic")
            .expect("server should stop cleanly");
    }

    #[tokio::test]
    async fn does_not_finish_until_in_flight_requests_do() {
        // Shutdown stops the server accepting, but a request already being served has to complete first. Without the
        // connection drain, the worker future would return immediately and the response would be lost.
        let addr = free_local_addr();

        let handler_started = Arc::new(AtomicBool::new(false));
        let started = Arc::clone(&handler_started);
        let server = server_with(addr, move || {
            let started = Arc::clone(&started);
            async move {
                started.store(true, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(300)).await;
                ok_response().await
            }
        });

        let mut coordinator = ShutdownCoordinator::default();
        let run = server
            .initialize(coordinator.register())
            .await
            .expect("should initialize");
        let mut run = tokio::spawn(run);

        // Issue a request by hand rather than pulling in a client: all we need is for the handler to be running.
        let mut stream = TcpStream::connect(addr).await.expect("should connect");
        stream
            .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .await
            .expect("should write request");

        while !handler_started.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        // Signal shutdown mid-request.
        coordinator.shutdown();

        // The handler is still working, so the worker must not be finished yet. This ordering is the actual assertion:
        // without the drain the worker returns here and the response is abandoned to a detached task.
        assert!(
            timeout(Duration::from_millis(50), &mut run).await.is_err(),
            "server should not finish while a request is still being served"
        );

        let mut response = Vec::new();
        timeout(TEST_TIMEOUT, stream.read_to_end(&mut response))
            .await
            .expect("response should arrive")
            .expect("should read response");
        assert!(
            response.ends_with(b"ok"),
            "expected the in-flight response to complete, got {:?}",
            String::from_utf8_lossy(&response)
        );

        timeout(TEST_TIMEOUT, &mut run)
            .await
            .expect("server should finish after draining")
            .expect("server task should not panic")
            .expect("server should stop cleanly");
    }
}
