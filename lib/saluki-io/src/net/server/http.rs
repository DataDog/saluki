//! HTTP servers.
//!
//! [`HttpServer`] is the supervised server, and is what new code should use: it runs as a child of whatever supervisor
//! it is added to, binds its listener during initialization, and drains in-flight connections before it reports being
//! done. [`UnsupervisedHttpServer`] is the older, self-spawning form, kept only until its remaining callers move over.
//!
//! Both forms speak HTTP/1.1 and HTTP/2, chosen per connection, which is what allows a single server to serve gRPC
//! alongside REST-ful routes. See [`grpc`][crate::net::server::grpc] for the routing helpers that make that work, and
//! [`Http2Config`] for the HTTP/2 knobs that gRPC deployments typically care about.

use std::{
    convert::Infallible,
    future::Future,
    pin::Pin,
    sync::Arc,
    task::{ready, Context, Poll},
    time::Duration,
};

use async_trait::async_trait;
use axum::{response::IntoResponse, Router};
use http::{Request, Response};
use http_body::Body;
use hyper::{
    body::Incoming,
    rt::{Read, Write},
};
use hyper_util::{
    rt::{TokioExecutor, TokioIo, TokioTimer},
    server::conn::auto::Builder,
};
use pin_project_lite::pin_project;
use rustls::ServerConfig;
use saluki_common::{
    sync::shutdown::{ShutdownCoordinator, ShutdownHandle},
    task::{spawn_traced_named, HandleExt as _},
};
use saluki_core::runtime::{
    state::{DataspaceRegistry, Identifier},
    InitializationError, ShutdownStrategy, Supervisable, SupervisorFuture,
};
use saluki_error::{ErrorContext as _, GenericError};
use saluki_tls::ensure_server_config_fips_compliant;
use stringtheory::MetaString;
use tokio::{
    pin,
    runtime::Handle,
    select,
    sync::oneshot,
    time::{sleep, timeout, Sleep},
};
use tokio_rustls::TlsAcceptor;
use tonic::{body::Body as GrpcBody, server::NamedService, service::Routes};
use tower::{util::Oneshot, Service, ServiceExt as _};
use tracing::{debug, error, info, warn};

use crate::net::{listener::ConnectionOrientedListener, server::grpc::merge_grpc_routes, ListenAddress};

/// Conventional gRPC keepalive interval: how long a connection sits idle before the server sends a PING.
const DEFAULT_GRPC_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(2 * 60 * 60);

/// Conventional gRPC keepalive timeout: how long the server waits for a PONG before closing the connection.
const DEFAULT_GRPC_KEEPALIVE_TIMEOUT: Duration = Duration::from_secs(20);

/// HTTP/2 connection settings.
///
/// These cover the HTTP/2 keepalive mechanism, the maximum lifetime of an individual connection, and how many
/// concurrent streams a connection may carry. They apply only to connections actually served over HTTP/2; a connection
/// negotiated as HTTP/1.1 ignores them.
///
/// Defaults to no keepalive, no connection age limit, and no stream limit, matching the behavior of a server that only
/// expects short HTTP/1.1 request/response exchanges. Long-lived HTTP/2 clients -- gRPC clients in particular --
/// generally want at least a keepalive interval configured so that dead connections are detected rather than
/// lingering.
#[derive(Clone, Copy, Debug, Default)]
pub struct Http2Config {
    keepalive_interval: Option<Duration>,
    keepalive_timeout: Option<Duration>,
    max_connection_age: Option<Duration>,
    max_connection_age_grace: Option<Duration>,
    max_concurrent_streams: Option<u32>,
}

impl Http2Config {
    /// Creates a configuration carrying the keepalive defaults conventionally used by gRPC servers.
    ///
    /// This is a two hour keepalive interval with a twenty second timeout, which is what gRPC implementations generally
    /// settle on: long enough that idle connections cost almost nothing, short enough that a connection silently
    /// dropped by a NAT or load balancer is eventually noticed.
    ///
    /// Deployments that need dead connections reclaimed faster should configure their own interval via
    /// [`with_keepalive`][Self::with_keepalive] rather than starting from this.
    pub fn grpc_defaults() -> Self {
        Self::default().with_keepalive(DEFAULT_GRPC_KEEPALIVE_INTERVAL, DEFAULT_GRPC_KEEPALIVE_TIMEOUT)
    }

    /// Sets the HTTP/2 keepalive parameters.
    ///
    /// After a connection has been idle for `interval`, the server sends a keepalive PING frame. If no PONG arrives
    /// within `timeout`, the connection is closed.
    ///
    /// Defaults to keepalive being disabled. Shorter intervals detect dead peers faster at the cost of more PING
    /// traffic on otherwise idle connections; the right value depends on how many idle connections a deployment carries
    /// and how quickly it needs to reclaim them.
    pub fn with_keepalive(mut self, interval: Duration, timeout: Duration) -> Self {
        self.keepalive_interval = Some(interval);
        self.keepalive_timeout = Some(timeout);
        self
    }

    /// Sets the maximum age of a connection, and the grace period that follows it.
    ///
    /// Once a connection has existed for `max_age`, the server sends GOAWAY so the peer stops issuing new requests, and
    /// lets in-flight requests finish. If `grace` is `Some`, the connection is forcibly closed once that period
    /// elapses; if it is `None`, the server waits for the connection to close on its own.
    ///
    /// Defaults to no limit, meaning connections live until either side closes them. Setting a limit is how a
    /// deployment behind a load balancer spreads load back out periodically, since HTTP/2 clients otherwise pin
    /// themselves to whichever backend they first reached.
    pub fn with_max_connection_age(mut self, max_age: Duration, grace: Option<Duration>) -> Self {
        self.max_connection_age = Some(max_age);
        self.max_connection_age_grace = grace;
        self
    }

    /// Sets the maximum number of concurrent streams allowed on a single connection.
    ///
    /// This is sent to the peer as the `SETTINGS_MAX_CONCURRENT_STREAMS` HTTP/2 setting, which bounds how many
    /// requests a client may have in flight on one connection: a client that reaches the limit waits for a stream to
    /// complete before opening another.
    ///
    /// Defaults to no limit, meaning a client is bounded only by what the connection can carry. Setting a limit caps
    /// the work a single connection can queue up, at the cost of a client having to open more connections -- or wait
    /// -- to exceed it.
    pub fn with_max_concurrent_streams(mut self, max_concurrent_streams: u32) -> Self {
        self.max_concurrent_streams = Some(max_concurrent_streams);
        self
    }
}

/// An HTTP server.
///
/// Serves a set of routes over a connection-oriented listener, optionally with TLS. The server can't be run directly:
/// it is only usable by adding it to a supervisor.
///
/// # Routes
///
/// Routes are accumulated on the server itself: HTTP routes via [`add_routes`][Self::add_routes], gRPC services via
/// [`add_grpc_service`][Self::add_grpc_service]. Both can be called as many times as needed, and both feed the same
/// router, because a gRPC service is a route set like any other -- one whose paths follow the gRPC naming convention.
/// The final router is built once, during initialization.
///
/// The server will respond accordingly depending on whether or not at least one gRPC service was configured. For
/// example, when an unknown gRPC service/operation is called, it will receive a gRPC-specific response indicating as
/// such, rather than a generic HTTP "404 Not Found" response.
///
/// A caller that has already built the exact router it wants can hand it over with [`with_routes`][Self::with_routes],
/// which bypasses all of the above.
///
/// # Supervision
///
/// The listen address is bound during initialization, so a failure to bind is raised before the supervised worker
/// starts running, and a restart rebinds.
///
/// The server will attempt to gracefully shutdown existing connections when the parent supervisor signals shutdown.
/// This will cause the worker to utilize the maximum allowable grace period during shutdown: it will attempt to take as
/// long as necessary to gracefully shutdown existing connections, bounded only by the parent supervisor.
///
/// # Assertions
///
/// `HttpServer` can optionally assert particular information at runtime when the server identifier is set (see
/// [`with_server_id`][Self::with_server_id]):
///
/// - the bound listen address (`BoundListenAddress`, with an identifier of `http-server-<server ID>`)
pub struct HttpServer {
    listen_address: ListenAddress,
    tls_config: Option<ServerConfig>,
    http2_config: Http2Config,
    http2_only: bool,
    graceful_shutdown_timeout: Option<Duration>,
    server_id: Option<MetaString>,
    http_routes: Router,
    grpc_routes: Option<Routes>,
    router_override: Option<Router>,
}

impl HttpServer {
    /// Creates a server that will listen on the given address, with no routes attached.
    pub fn from_listen_address(listen_address: ListenAddress) -> Self {
        Self {
            listen_address,
            tls_config: None,
            http2_config: Http2Config::default(),
            http2_only: false,
            graceful_shutdown_timeout: None,
            server_id: None,
            http_routes: Router::new(),
            grpc_routes: None,
            router_override: None,
        }
    }

    /// Adds HTTP routes to this server.
    ///
    /// Can be called more than once, in which case the route sets are merged.
    ///
    /// # Panics
    ///
    /// Panics if `routes` defines a path that another route set already defines, which is [`Router::merge`]'s behavior.
    pub fn add_routes(mut self, routes: Router) -> Self {
        self.http_routes = self.http_routes.merge(routes);
        self
    }

    /// Adds a gRPC service to this server.
    ///
    /// The service's routes are served from the same listener, and alongside the same HTTP routes, as everything else
    /// attached to this server.
    ///
    /// Can be called more than once to attach several services.
    pub fn add_grpc_service<S>(mut self, service: S) -> Self
    where
        S: Service<Request<GrpcBody>, Error = Infallible> + NamedService + Clone + Send + Sync + 'static,
        S::Response: IntoResponse,
        S::Future: Send + 'static,
    {
        self.grpc_routes = Some(self.grpc_routes.take().unwrap_or_default().add_service(service));
        self
    }

    /// Serves the given router, ignoring any routes otherwise attached to this server.
    ///
    /// Any existing routes, whether HTTP or gRPC, will be ignored entirely.
    pub fn with_routes(mut self, routes: Router) -> Self {
        self.router_override = Some(routes);
        self
    }

    /// Builds the router this server will serve.
    fn build_router(&self) -> Router {
        if let Some(router) = &self.router_override {
            return router.clone();
        }

        let mut router = self.http_routes.clone();
        if let Some(grpc_routes) = self.grpc_routes.clone() {
            router = merge_grpc_routes(router, grpc_routes);
        }

        router
    }

    /// Sets the HTTP/2 settings for the server.
    ///
    /// Defaults to [`Http2Config::default()`], which enables neither keepalive nor a connection age limit.
    pub fn with_http2_config(mut self, config: Http2Config) -> Self {
        self.http2_config = config;
        self
    }

    /// Restricts the server to HTTP/2.
    ///
    /// By default, the protocol is detected per connection: a client that opens with the HTTP/2 connection preface is
    /// served over HTTP/2, and anything else is served over HTTP/1.1. Restricting the server to HTTP/2 skips that
    /// detection, so an HTTP/1.1 client is rejected at the protocol level rather than being routed and answered.
    ///
    /// This is worth setting on an endpoint that only ever serves gRPC, where an HTTP/1.1 request is a client error
    /// worth surfacing as one. Leave it off for any endpoint that also serves REST-ful routes.
    ///
    /// Defaults to accepting both HTTP/1.1 and HTTP/2.
    pub fn with_http2_only(mut self) -> Self {
        self.http2_only = true;
        self
    }

    /// Sets the server identifier to use when asserting any facts for this server.
    ///
    /// If no identifier is set, no assertions will be made at runtime.
    pub fn with_server_id(mut self, id: impl Into<MetaString>) -> Self {
        self.server_id = Some(id.into());
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

    fn get_server_id(&self) -> Option<Identifier> {
        self.server_id.clone().map(|sid| get_bound_address_id(&sid))
    }
}

#[async_trait]
impl Supervisable for HttpServer {
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

        // Assert our bound listen address if we have a configured server ID.
        if let Some(server_id) = self.get_server_id() {
            let dataspace = DataspaceRegistry::try_current()
                .ok_or_else(|| saluki_error::generic_error!("Dataspace not available for HTTP server."))?;

            dataspace.assert(listener.bound_listen_address(), server_id);
        }

        let conn_builder = build_conn_builder(self.http2_config, self.http2_only);
        let service = self.build_router();
        let tls_config = self.tls_config.clone();
        let maybe_shutdown_timeout = self.graceful_shutdown_timeout;
        let http2_config = self.http2_config;

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
            http2_config,
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
            conn_builder: build_conn_builder(Http2Config::default(), false),
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
            let result = run_accept_loop(
                listener,
                conn_builder,
                service,
                tls_config,
                executor,
                shutdown,
                None,
                Http2Config::default(),
            )
            .await;
            if let Err(e) = result {
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
#[allow(clippy::too_many_arguments)]
async fn run_accept_loop<S, B>(
    mut listener: ConnectionOrientedListener, conn_builder: Builder<TokioExecutor>, service: S,
    tls_config: Option<ServerConfig>, executor: Handle, shutdown: ShutdownHandle,
    maybe_shutdown_timeout: Option<Duration>, http2_config: Http2Config,
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
                                conn_builder, TokioIo::new(tls_stream), service, listen_addr, conn_shutdown, maybe_shutdown_timeout, http2_config
                            ));
                        },
                        None => {
                            executor.spawn_traced_named("http_server_conn", drive_connection(
                                conn_builder, TokioIo::new(stream), service, listen_addr, conn_shutdown, maybe_shutdown_timeout, http2_config
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
///
/// A connection that outlives the configured maximum age is retired the same way, independently of server shutdown.
#[allow(clippy::too_many_arguments)]
async fn drive_connection<I, S, B>(
    conn_builder: Builder<TokioExecutor>, io: I, service: S, listen_addr: ListenAddress, shutdown: ShutdownHandle,
    maybe_shutdown_timeout: Option<Duration>, http2_config: Http2Config,
) where
    I: Read + Write + Unpin + Send + 'static,
    S: Service<Request<Incoming>, Response = Response<B>> + Send + Clone + 'static,
    S::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
    S::Future: Send + 'static,
    B: Body + Send + 'static,
    B::Data: Send,
    B::Error: std::error::Error + Send + Sync,
{
    let service = TowerToHyperService::new(service);
    let conn = conn_builder.serve_connection(io, service);
    let mut connection_age = ConnectionAge::new(&http2_config);
    pin!(conn, shutdown);

    loop {
        select! {
            result = conn.as_mut() => {
                if let Err(e) = result {
                    error!(%listen_addr, error = %e, "Failed to serve HTTP connection.");
                }

                return;
            },

            action = connection_age.next_action() => match action {
                ConnectionAgeAction::Retire => {
                    debug!(%listen_addr, "Retiring HTTP connection that reached its maximum age.");

                    conn.as_mut().graceful_shutdown();
                },
                ConnectionAgeAction::Close => {
                    warn!(%listen_addr, "Forcibly closing HTTP connection that did not retire within its grace period.");

                    return;
                },
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

                return;
            },
        }
    }
}

/// What to do with a connection that has reached an age-based deadline.
enum ConnectionAgeAction {
    /// Stop accepting new requests on the connection, and let in-flight ones finish.
    Retire,

    /// Close the connection, abandoning anything still in flight.
    Close,
}

/// Where a connection sits relative to its age-based deadlines.
enum ConnectionAgePhase {
    /// Waiting for the maximum age to elapse.
    Aging(Pin<Box<Sleep>>),

    /// Retired, and waiting for the grace period to elapse before being closed.
    Grace(Pin<Box<Sleep>>),

    /// No deadline left to wait on, either because none was configured or because all of them have passed.
    Expired,
}

/// Tracks the age-based deadlines of a single connection.
///
/// Yields at most two actions over the life of a connection -- [`Retire`][ConnectionAgeAction::Retire] once the maximum
/// age elapses, then [`Close`][ConnectionAgeAction::Close] once the grace period does -- and never resolves again
/// afterwards, so it is safe to keep selecting on in a loop.
struct ConnectionAge {
    phase: ConnectionAgePhase,
    grace: Option<Duration>,
}

impl ConnectionAge {
    fn new(http2_config: &Http2Config) -> Self {
        Self {
            phase: match http2_config.max_connection_age {
                Some(max_age) => ConnectionAgePhase::Aging(Box::pin(sleep(max_age))),
                None => ConnectionAgePhase::Expired,
            },
            grace: http2_config.max_connection_age_grace,
        }
    }

    async fn next_action(&mut self) -> ConnectionAgeAction {
        match &mut self.phase {
            ConnectionAgePhase::Aging(deadline) => {
                deadline.as_mut().await;

                // Retiring the connection is the last thing we do to it unless a grace period was configured, in which
                // case we come back around and close it out if it hasn't finished by then.
                self.phase = match self.grace {
                    Some(grace) => ConnectionAgePhase::Grace(Box::pin(sleep(grace))),
                    None => ConnectionAgePhase::Expired,
                };

                ConnectionAgeAction::Retire
            }

            ConnectionAgePhase::Grace(deadline) => {
                deadline.as_mut().await;
                self.phase = ConnectionAgePhase::Expired;

                ConnectionAgeAction::Close
            }

            ConnectionAgePhase::Expired => std::future::pending().await,
        }
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

/// A Tower service converted into a Hyper service.
#[derive(Debug, Copy, Clone)]
struct TowerToHyperService<S> {
    service: S,
}

impl<S> TowerToHyperService<S> {
    fn new(tower_service: S) -> Self {
        Self { service: tower_service }
    }
}

impl<S, R> hyper::service::Service<R> for TowerToHyperService<S>
where
    S: tower::Service<R> + Clone,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = TowerToHyperServiceFuture<S, R>;

    fn call(&self, req: R) -> Self::Future {
        TowerToHyperServiceFuture {
            future: self.service.clone().oneshot(req),
        }
    }
}

pin_project! {
    /// Response future for [`TowerToHyperService`].
    struct TowerToHyperServiceFuture<S, R>
    where
        S: tower::Service<R>,
    {
        #[pin]
        future: Oneshot<S, R>,
    }
}

impl<S, R> Future for TowerToHyperServiceFuture<S, R>
where
    S: tower::Service<R>,
{
    type Output = Result<S::Response, S::Error>;

    #[inline]
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.project().future.poll(cx)
    }
}

fn build_conn_builder(http2_config: Http2Config, http2_only: bool) -> Builder<TokioExecutor> {
    let mut builder = Builder::new(TokioExecutor::new());
    builder
        .http1()
        .timer(TokioTimer::new())
        .header_read_timeout(Duration::from_secs(10));

    builder
        .http2()
        .timer(TokioTimer::new())
        .keep_alive_interval(http2_config.keepalive_interval);

    if let Some(keepalive_timeout) = http2_config.keepalive_timeout {
        builder.http2().keep_alive_timeout(keepalive_timeout);
    }

    if let Some(max_concurrent_streams) = http2_config.max_concurrent_streams {
        builder.http2().max_concurrent_streams(max_concurrent_streams);
    }

    if http2_only {
        builder = builder.http2_only();
    }

    builder
}

fn get_bound_address_id(server_id: &str) -> Identifier {
    Identifier::from(format!("http-server-{}", server_id))
}

#[cfg(test)]
mod tests {
    use std::net::{SocketAddr, TcpListener as StdTcpListener};
    use std::sync::atomic::{AtomicBool, Ordering};

    use http::{StatusCode, Version};
    use http_body_util::{Empty, Full};
    use hyper_util::client::legacy::Client;
    use saluki_common::sync::shutdown::ShutdownCoordinator;
    use saluki_tls::test_util::SelfSignedCert;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    use tokio::net::TcpStream;
    use tokio::time::{timeout, Instant};
    use tower::util::service_fn;

    use super::*;
    use crate::net::addr::BoundListenAddress;
    use crate::net::server::test_util::connect_tcp;
    #[cfg(unix)]
    use crate::net::server::test_util::{connect_unix, ServerTestHarness};

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
    fn server_with<F, Fut>(listen_address: ListenAddress, f: F) -> HttpServer
    where
        F: Fn() -> Fut + Send + Sync + Clone + 'static,
        Fut: Future<Output = Result<Response<Full<bytes::Bytes>>, Infallible>> + Send + 'static,
    {
        HttpServer::from_listen_address(listen_address).add_routes(routes_answering_with(f))
    }

    /// Builds a router whose every route runs `f`.
    fn routes_answering_with<F, Fut>(f: F) -> Router
    where
        F: Fn() -> Fut + Send + Sync + Clone + 'static,
        Fut: Future<Output = Result<Response<Full<bytes::Bytes>>, Infallible>> + Send + 'static,
    {
        Router::new().fallback_service(service_fn(move |_req: axum::extract::Request| f()))
    }

    /// A handler that responds immediately.
    async fn ok_response() -> Result<Response<Full<bytes::Bytes>>, Infallible> {
        Ok(Response::new(Full::new(bytes::Bytes::from_static(b"ok"))))
    }

    /// An ephemeral loopback address, for servers that are never actually bound.
    fn loopback() -> ListenAddress {
        ListenAddress::Tcp("127.0.0.1:0".parse().expect("address should parse"))
    }

    /// Drives a request through a router without going near a socket.
    async fn route_request(router: Router, uri: &str, content_type: Option<&str>) -> Response<axum::body::Body> {
        let mut builder = Request::builder().uri(uri);
        if let Some(content_type) = content_type {
            builder = builder.header(http::header::CONTENT_TYPE, content_type);
        }

        let request = builder
            .body(axum::body::Body::empty())
            .expect("should build the request");

        router.oneshot(request).await.expect("router should answer")
    }

    /// A gRPC service that answers immediately, for attaching gRPC routes to a server under test.
    #[derive(Clone)]
    struct EmptyGrpcService;

    impl NamedService for EmptyGrpcService {
        const NAME: &'static str = "test.EmptyService";
    }

    impl Service<Request<GrpcBody>> for EmptyGrpcService {
        type Response = Response<axum::body::Body>;
        type Error = Infallible;
        type Future = std::future::Ready<Result<Self::Response, Infallible>>;

        fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, _req: Request<GrpcBody>) -> Self::Future {
            std::future::ready(Ok(Response::new(axum::body::Body::empty())))
        }
    }

    #[tokio::test]
    async fn accumulated_http_and_grpc_routes_are_served_together() {
        let router = HttpServer::from_listen_address(loopback())
            .add_routes(Router::new().route("/first", axum::routing::get(|| async { "first" })))
            .add_routes(Router::new().route("/second", axum::routing::get(|| async { "second" })))
            .add_grpc_service(EmptyGrpcService)
            .build_router();

        // Both route sets survive being merged, as do the gRPC service's own routes.
        for path in ["/first", "/second"] {
            let response = route_request(router.clone(), path, None).await;
            assert_eq!(response.status(), StatusCode::OK, "{path} should be served");
        }

        let grpc_response = route_request(router.clone(), "/test.EmptyService/Method", Some("application/grpc")).await;
        assert_eq!(grpc_response.status(), StatusCode::OK);

        // Attaching a gRPC service is what brings in the protocol-aware fallback, so an unmatched request is now
        // answered in whichever protocol it arrived in.
        let unmatched_grpc = route_request(router.clone(), "/test.Missing/Method", Some("application/grpc")).await;
        assert_eq!(
            unmatched_grpc
                .headers()
                .get("grpc-status")
                .and_then(|v| v.to_str().ok()),
            Some("12")
        );

        let unmatched_http = route_request(router, "/nowhere", None).await;
        assert_eq!(unmatched_http.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn an_http_only_server_keeps_its_own_fallback() {
        // With no gRPC service attached there is no protocol-aware fallback to install, so the caller's own fallback
        // is left alone rather than being displaced by one it never asked for.
        let router = HttpServer::from_listen_address(loopback())
            .add_routes(Router::new().fallback(|| async { StatusCode::IM_A_TEAPOT }))
            .build_router();

        let response = route_request(router, "/anything", None).await;
        assert_eq!(response.status(), StatusCode::IM_A_TEAPOT);
    }

    #[tokio::test]
    async fn overriding_the_router_discards_accumulated_routes() {
        let router = HttpServer::from_listen_address(loopback())
            .add_routes(Router::new().route("/added", axum::routing::get(|| async { "added" })))
            .add_grpc_service(EmptyGrpcService)
            .with_routes(Router::new().route("/override", axum::routing::get(|| async { "override" })))
            .build_router();

        let overridden = route_request(router.clone(), "/override", None).await;
        assert_eq!(overridden.status(), StatusCode::OK);

        // Everything that was added beforehand is gone, gRPC routes included.
        let added = route_request(router.clone(), "/added", None).await;
        assert_eq!(added.status(), StatusCode::NOT_FOUND);

        let grpc = route_request(router, "/test.EmptyService/Method", Some("application/grpc")).await;
        assert_eq!(grpc.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn publishes_bound_tcp_address() {
        let harness = ServerTestHarness::start("http-tcp-bound-address", |supervisor, server_id| {
            let server = server_with(ListenAddress::tcp_loopback(0), ok_response).with_server_id(server_id);
            supervisor.add_worker(server);
        })
        .await;

        let local_tcp_address = match harness.bound_address().await {
            BoundListenAddress::Tcp(addr) => addr,
            other_addr => panic!("expected TCP address, got {:?}", other_addr),
        };

        assert_ne!(local_tcp_address.port(), 0);

        // Try and connect.
        //
        // Panics if the connection fails or we timeout trying to connect.. so we don't assert anything
        // here since no panic means "it worked."
        let stream = connect_tcp(local_tcp_address).await;
        drop(stream);

        harness.shutdown().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn publishes_bound_unix_address() {
        let tempdir = tempfile::tempdir().expect("should create temp dir");
        let socket_path = tempdir.path().join("http.sock");
        let listen_address = ListenAddress::Unix(socket_path.clone());

        let harness = ServerTestHarness::start("http-unix-bound-address", move |supervisor, server_id| {
            let server = server_with(listen_address, ok_response).with_server_id(server_id);
            supervisor.add_worker(server);
        })
        .await;

        let local_unix_address = match harness.bound_address().await {
            BoundListenAddress::Unix(addr) => addr,
            other_addr => panic!("expected UDS address, got {:?}", other_addr),
        };

        assert_eq!(socket_path, local_unix_address);

        // Try and connect.
        //
        // Panics if the connection fails or we timeout trying to connect.. so we don't assert anything
        // here since no panic means "it worked."
        let stream = connect_unix(&socket_path).await;
        drop(stream);

        harness.shutdown().await;
    }

    #[tokio::test]
    async fn publishes_bound_tcp_address_with_tls() {
        let _ = saluki_tls::initialize_default_crypto_provider();
        let cert = SelfSignedCert::localhost();
        let tls_config = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(cert.cert_chain(), cert.private_key())
            .expect("should build TLS config");

        let harness = ServerTestHarness::start("http-tcp-tls-bound-address", move |supervisor, server_id| {
            let server = server_with(ListenAddress::tcp_loopback(0), ok_response)
                .with_tls_config(tls_config)
                .with_server_id(server_id);
            supervisor.add_worker(server);
        })
        .await;

        let local_tcp_address = match harness.bound_address().await {
            BoundListenAddress::Tcp(addr) => addr,
            other_addr => panic!("expected TCP address, got {:?}", other_addr),
        };

        assert_ne!(local_tcp_address.port(), 0);

        // Try and connect.
        //
        // Panics if the connection fails or we timeout trying to connect.. so we don't assert anything
        // here since no panic means "it worked."
        let stream = connect_tcp(local_tcp_address).await;
        drop(stream);

        harness.shutdown().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn publishes_bound_unix_address_with_tls() {
        let _ = saluki_tls::initialize_default_crypto_provider();
        let cert = SelfSignedCert::localhost();
        let tls_config = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(cert.cert_chain(), cert.private_key())
            .expect("should build TLS config");

        let tempdir = tempfile::tempdir().expect("should create temp dir");
        let socket_path = tempdir.path().join("http.sock");
        let listen_address = ListenAddress::Unix(socket_path.clone());

        let harness = ServerTestHarness::start("http-unix-tls-bound-address", move |supervisor, server_id| {
            let server = server_with(listen_address, ok_response)
                .with_tls_config(tls_config)
                .with_server_id(server_id);
            supervisor.add_worker(server);
        })
        .await;

        let local_unix_address = match harness.bound_address().await {
            BoundListenAddress::Unix(addr) => addr,
            other_addr => panic!("expected UDS address, got {:?}", other_addr),
        };

        assert_eq!(socket_path, local_unix_address);

        // Try and connect.
        //
        // Panics if the connection fails or we timeout trying to connect.. so we don't assert anything
        // here since no panic means "it worked."
        let stream = connect_unix(&socket_path).await;
        drop(stream);

        harness.shutdown().await;
    }

    #[tokio::test]
    async fn binds_during_initialization() {
        // The listener is bound by `initialize`, not by the worker future, so the port is already taken before anything
        // starts serving. That is what makes a bind failure a non-restartable initialization error.
        let addr = free_local_addr();
        let server = server_with(ListenAddress::Tcp(addr), ok_response);

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

        let server = server_with(ListenAddress::Tcp(addr), ok_response);
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
        let server = server_with(ListenAddress::Tcp(addr), ok_response);

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
        let server =
            server_with(ListenAddress::Tcp(addr), ok_response).with_graceful_shutdown_timeout(Duration::from_secs(1));

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
        let server = server_with(ListenAddress::Tcp(addr), move || {
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

    #[tokio::test]
    async fn serves_http2_requests() {
        // The protocol is chosen from the bytes the client opens with rather than from ALPN, so a cleartext client that
        // knows to speak HTTP/2 gets HTTP/2. That is what lets one server carry both REST-ful routes and gRPC services,
        // since gRPC is HTTP/2 and nothing else.
        let harness = ServerTestHarness::start("http2-request", |supervisor, server_id| {
            let server = server_with(ListenAddress::tcp_loopback(0), ok_response).with_server_id(server_id);
            supervisor.add_worker(server);
        })
        .await;

        let address = harness.bound_tcp_address().await;
        let client = Client::builder(TokioExecutor::new())
            .http2_only(true)
            .build_http::<Empty<bytes::Bytes>>();
        let uri = format!("http://{address}/").parse().expect("should be a valid URI");
        let response = timeout(TEST_TIMEOUT, client.get(uri))
            .await
            .expect("server should answer an HTTP/2 request")
            .expect("request should succeed");

        assert_eq!(response.version(), Version::HTTP_2);
        assert_eq!(response.status(), StatusCode::OK);

        harness.shutdown().await;
    }

    #[tokio::test]
    async fn http2_only_server_rejects_http1_requests() {
        // An endpoint that only serves gRPC has no use for HTTP/1.1, and rejecting it at the protocol level tells the
        // caller more than routing the request and answering with a 404 would.
        let addr = free_local_addr();
        let server = server_with(ListenAddress::Tcp(addr), ok_response).with_http2_only();

        let mut coordinator = ShutdownCoordinator::default();
        let run = server
            .initialize(coordinator.register())
            .await
            .expect("should initialize");
        let run = tokio::spawn(run);

        let mut stream = TcpStream::connect(addr).await.expect("should connect");
        stream
            .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .await
            .expect("should write request");

        // The connection is torn down rather than answered, which surfaces as either a clean EOF or a reset depending
        // on how far the peer got before the server gave up on it. Either is a rejection; what matters is that no
        // HTTP/1.1 response comes back.
        let mut response = Vec::new();
        let _ = timeout(TEST_TIMEOUT, stream.read_to_end(&mut response))
            .await
            .expect("server should close the connection rather than leaving the client hanging");
        assert!(
            !response.starts_with(b"HTTP/1.1"),
            "expected no HTTP/1.1 response, got {:?}",
            String::from_utf8_lossy(&response)
        );

        coordinator.shutdown();
        timeout(TEST_TIMEOUT, run)
            .await
            .expect("server should stop on shutdown")
            .expect("server task should not panic")
            .expect("server should stop cleanly");
    }

    #[tokio::test]
    async fn an_idle_http2_peer_does_not_wedge_the_drain() {
        // The HTTP/2 counterpart of the half-sent request case: a peer that writes part of the connection preface and
        // then stalls never becomes idle, so `graceful_shutdown` alone will not close it.
        let addr = free_local_addr();
        let server =
            server_with(ListenAddress::Tcp(addr), ok_response).with_graceful_shutdown_timeout(Duration::from_secs(1));

        let mut coordinator = ShutdownCoordinator::default();
        let run = server
            .initialize(coordinator.register())
            .await
            .expect("should initialize");
        let run = tokio::spawn(run);

        let mut stream = TcpStream::connect(addr).await.expect("should connect");
        stream
            .write_all(b"PRI * HTTP/2.0\r\n")
            .await
            .expect("should write a partial HTTP/2 preface");
        stream.flush().await.expect("should flush");
        tokio::time::sleep(Duration::from_millis(100)).await;

        coordinator.shutdown();
        timeout(TEST_TIMEOUT, run)
            .await
            .expect("server should finish draining rather than waiting on an idle peer")
            .expect("server task should not panic")
            .expect("server should stop cleanly");
    }

    #[tokio::test]
    async fn retires_connections_that_reach_their_maximum_age() {
        // Nothing in the connection builder knows about connection age, so the server enforces the deadline itself.
        // Without it, a long-lived HTTP/2 client pins itself to whichever backend it first reached and stays there.
        let addr = free_local_addr();
        let max_age = Duration::from_millis(300);
        let server = server_with(ListenAddress::Tcp(addr), ok_response)
            .with_http2_config(Http2Config::default().with_max_connection_age(max_age, None));

        let mut coordinator = ShutdownCoordinator::default();
        let run = server
            .initialize(coordinator.register())
            .await
            .expect("should initialize");
        let run = tokio::spawn(run);

        // Issue a request without asking for the connection to be closed, so what gets retired is a genuinely idle
        // keep-alive connection rather than one the client was finished with anyway.
        let started = Instant::now();
        let mut stream = TcpStream::connect(addr).await.expect("should connect");
        stream
            .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await
            .expect("should write request");

        let mut response = Vec::new();
        timeout(TEST_TIMEOUT, stream.read_to_end(&mut response))
            .await
            .expect("server should close the connection once it reaches its maximum age")
            .expect("should read the response");

        assert!(
            response.ends_with(b"ok"),
            "expected the request to be served before the connection was retired, got {:?}",
            String::from_utf8_lossy(&response)
        );
        assert!(
            started.elapsed() >= max_age,
            "connection was closed after {:?}, before reaching its maximum age of {:?}",
            started.elapsed(),
            max_age
        );

        coordinator.shutdown();
        timeout(TEST_TIMEOUT, run)
            .await
            .expect("server should stop on shutdown")
            .expect("server task should not panic")
            .expect("server should stop cleanly");
    }

    #[tokio::test]
    async fn announces_its_maximum_concurrent_streams() {
        // A stream limit only bounds anything if the peer is told about it, and the only place it is stated is the
        // SETTINGS frame that opens the connection.
        const CLIENT_PREFACE: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";
        const FRAME_TYPE_SETTINGS: u8 = 0x4;
        const SETTINGS_MAX_CONCURRENT_STREAMS: u16 = 0x3;

        let addr = free_local_addr();
        let max_concurrent_streams = 42;
        let server = server_with(ListenAddress::Tcp(addr), ok_response)
            .with_http2_config(Http2Config::default().with_max_concurrent_streams(max_concurrent_streams));

        let mut coordinator = ShutdownCoordinator::default();
        let run = server
            .initialize(coordinator.register())
            .await
            .expect("should initialize");
        let run = tokio::spawn(run);

        // The server only starts speaking HTTP/2 once it has seen the client's preface, since the protocol is detected
        // per connection.
        let mut stream = TcpStream::connect(addr).await.expect("should connect");
        stream
            .write_all(CLIENT_PREFACE)
            .await
            .expect("should write the HTTP/2 preface");
        stream.flush().await.expect("should flush");

        // The server's half of the preface is a SETTINGS frame: a nine byte header (24-bit payload length, then type,
        // flags, and stream identifier) followed by six bytes per setting (16-bit identifier, 32-bit value).
        let mut header = [0u8; 9];
        timeout(TEST_TIMEOUT, stream.read_exact(&mut header))
            .await
            .expect("server should send its half of the connection preface")
            .expect("should read the frame header");
        assert_eq!(
            header[3], FRAME_TYPE_SETTINGS,
            "server's first frame should be SETTINGS, got frame type {:#x}",
            header[3]
        );

        let payload_len = u32::from_be_bytes([0, header[0], header[1], header[2]]) as usize;
        let mut payload = vec![0u8; payload_len];
        timeout(TEST_TIMEOUT, stream.read_exact(&mut payload))
            .await
            .expect("server should send the full SETTINGS payload")
            .expect("should read the frame payload");

        let advertised = payload.chunks_exact(6).find_map(|setting| {
            let identifier = u16::from_be_bytes([setting[0], setting[1]]);
            (identifier == SETTINGS_MAX_CONCURRENT_STREAMS)
                .then(|| u32::from_be_bytes([setting[2], setting[3], setting[4], setting[5]]))
        });

        assert_eq!(
            advertised,
            Some(max_concurrent_streams),
            "server should advertise its configured stream limit"
        );

        // The connection never issues a request, so it would otherwise sit through the entire drain.
        drop(stream);

        coordinator.shutdown();
        timeout(TEST_TIMEOUT, run)
            .await
            .expect("server should stop on shutdown")
            .expect("server task should not panic")
            .expect("server should stop cleanly");
    }
}
