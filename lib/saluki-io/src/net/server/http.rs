//! HTTP server.
//!
//! [`HttpServer`] is a supervision subtree rather than a single worker.
//! [`into_supervisor`][HttpServer::into_supervisor] turns a configured server into a [`Supervisor`] whose sole static
//! child accepts connections, and whose dynamic children are the TLS handshakes, the connections themselves, and
//! whatever `hyper` asks to have executed. The subtree binds its listener during initialization, and drains in-flight
//! connections before it reports being done.
//!
//! It speaks HTTP/1.1 and HTTP/2, chosen per connection, which is what allows a single server to serve gRPC alongside
//! REST-ful routes. See [`grpc`][crate::net::server::grpc] for the routing helpers that make that work, and
//! [`Http2Config`] for the HTTP/2 knobs that gRPC deployments typically care about.

use std::{
    convert::Infallible,
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
    time::Duration,
};

use async_trait::async_trait;
use axum::{response::IntoResponse, Router};
use http::Request;
use hyper::rt::{Read, Write};
use hyper_util::{
    rt::{TokioIo, TokioTimer},
    server::conn::auto::Builder,
};
use pin_project_lite::pin_project;
use rustls::ServerConfig;
use saluki_common::sync::shutdown::ShutdownHandle;
use saluki_core::runtime::{
    self,
    state::{DataspaceRegistry, Identifier},
    BuilderState, ChildBuilder, InitializationError, ShutdownStrategy, Supervisable, Supervisor, SupervisorFuture,
    SupervisorHandle,
};
use saluki_error::{ErrorContext as _, GenericError};
use saluki_tls::ensure_server_config_fips_compliant;
use stringtheory::MetaString;
use tokio::{
    pin,
    runtime::Handle,
    select,
    time::{sleep, timeout, Sleep},
};
use tokio_rustls::TlsAcceptor;
use tonic::{body::Body as GrpcBody, server::NamedService, service::Routes};
use tower::{util::Oneshot, Service, ServiceExt as _};
use tracing::{debug, error, info, warn};

use crate::net::{
    listener::ConnectionOrientedListener, server::grpc::merge_grpc_routes, stream::Connection, ListenAddress,
};

/// Conventional gRPC keepalive interval: how long a connection sits idle before the server sends a PING.
const DEFAULT_GRPC_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(2 * 60 * 60);

/// Conventional gRPC keepalive timeout: how long the server waits for a PONG before closing the connection.
const DEFAULT_GRPC_KEEPALIVE_TIMEOUT: Duration = Duration::from_secs(20);

/// Name a server reports when it has no identifier of its own.
const DEFAULT_SERVER_NAME: &str = "http_server";

/// How long a connection is given to drain when no timeout is configured.
///
/// Matches the default shutdown timeout a topology gives its components, which is the deadline the rest of the process
/// is built around.
const DEFAULT_DRAIN_DEADLINE: Duration = Duration::from_secs(30);

/// How much longer than the drain deadline the subtree's shutdown budget runs.
///
/// A connection bounds its own drain, so under normal circumstances it finishes and exits cleanly before the budget is
/// anywhere near elapsing. The slack keeps the two from racing: without it a connection that took its full deadline
/// could be force-aborted at the very moment it was about to return, which would be reported as an unclean shutdown
/// all the way up the tree. The budget is the backstop for a connection that ignores its own deadline entirely.
const SHUTDOWN_BUDGET_SLACK: Duration = Duration::from_secs(1);

/// How long a TLS handshake is given to complete before the connection is abandoned.
///
/// Running handshakes concurrently is what keeps a slow one from holding up the listener, but it also means a peer
/// that connects and then says nothing no longer blocks anything -- and so nothing would ever reclaim it. This bounds
/// that. It matches both the HTTP/1.1 header read timeout here and the default handshake deadline on the client side
/// ([`with_tls_handshake_timeout`][crate::net::client::http::HttpClientBuilder::with_tls_handshake_timeout]), since all
/// three cover the same shape of problem: a peer that opens a connection and never finishes what it started.
const TLS_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// Process name for the child that accepts connections.
const ACCEPTOR_TASK_NAME: &str = "acceptor";

/// Process name for the child that performs a single TLS handshake.
const TLS_HANDSHAKE_TASK_NAME: &str = "tls_handshake";

/// Process name for the child that serves a single connection.
const CONNECTION_TASK_NAME: &str = "http_conn";

/// Process name for a background child spawned on `hyper`'s behalf.
const CONNECTION_BG_TASK_NAME: &str = "http_conn_task";

/// The connection builder this server uses, specialized to our own executor.
type ConnBuilder = Builder<SupervisedExecutor>;

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
/// [`into_supervisor`][Self::into_supervisor] turns it into the [`Supervisor`] that runs it, which is then added to
/// another supervisor like any other child.
///
/// # Routes
///
/// Routes are accumulated on the server itself: HTTP routes via [`add_routes`][Self::add_routes], gRPC services via
/// [`add_grpc_service`][Self::add_grpc_service]. Both can be called as many times as needed, and both feed the same
/// router, because a gRPC service is a route set like any other -- one whose paths follow the gRPC naming convention.
/// The final router is built once, when the server is converted into a supervisor.
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
/// The server runs as a subtree, not a single worker:
///
/// - the **acceptor** is the sole static child. It binds the listen address during initialization, so a failure to
///   bind is raised before anything starts serving, and a restart rebinds. That failure is an initialization error,
///   which propagates out of the subtree without being retried.
/// - a **TLS handshake** runs as its own dynamic child, so a slow or stalled handshake delays only itself rather than
///   holding up every connection queued behind it on the listener.
/// - a **connection** runs as its own dynamic child, and is drained rather than dropped when the subtree shuts down.
/// - anything **`hyper` asks to execute** -- which for HTTP/2 is one future per stream, meaning per request -- runs as
///   a dynamic child too. This is what makes per-request work visible in the process tree and in per-task metrics, at
///   the cost of putting the supervisor's spawn queue on the per-request path. A stream `hyper` starts after the
///   subtree has stopped accepting spawns, during a drain, is dropped rather than run.
///
/// # Shutdown
///
/// The subtree carries its own shutdown budget, because a nested supervisor is deliberately exempt from its parent's.
/// See [`with_graceful_shutdown_timeout`][Self::with_graceful_shutdown_timeout] for what sets it and what the default
/// is.
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
    name: MetaString,
    http_routes: Router,
    grpc_routes: Option<Routes>,
    router_override: Option<Router>,
    worker_pool: Option<Handle>,
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
            name: MetaString::from_static(DEFAULT_SERVER_NAME),
            http_routes: Router::new(),
            grpc_routes: None,
            router_override: None,
            worker_pool: None,
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
    /// The identifier also distinguishes this server from any other running under the same supervisor: it becomes part
    /// of the name the subtree reports, so that logs and per-worker task metrics can be attributed to a specific
    /// endpoint. Set it on any server that shares a supervisor with another, even when nothing consumes the
    /// assertions.
    ///
    /// If no identifier is set, no assertions will be made at runtime, and the subtree reports a bare
    /// `http_server`.
    pub fn with_server_id(mut self, id: impl Into<MetaString>) -> Self {
        let id = id.into();

        self.name = MetaString::from(format!("{}_{}", DEFAULT_SERVER_NAME, id));
        self.server_id = Some(id);
        self
    }

    /// Sets how long an individual connection is given to drain during shutdown.
    ///
    /// When shutdown is signalled, a connection stops accepting new requests and finishes what is already in flight.
    /// This bounds that: a connection that hasn't finished by then gives up and closes, logging a warning, so one
    /// wedged peer can't hold the subtree open.
    ///
    /// It also sets the subtree's shutdown budget, to this value plus a small amount of slack. That matters because a
    /// nested supervisor is exempt from its parent's budget, so without one of its own nothing would bound the subtree
    /// at all. Connections bound themselves, so the budget only comes into play for one that ignores its own deadline.
    ///
    /// Defaults to 30 seconds, matching the default shutdown timeout a topology gives its components. Lower it for an
    /// endpoint that should be abandoned quickly; raise it for one serving long-running requests that are worth
    /// waiting for.
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

    /// Runs this server's tasks on the given runtime.
    ///
    /// Every task the server runs is placed here: accepting connections, TLS handshakes, serving connections, and the
    /// futures `hyper` hands to the server's executor. Only the subtree's own supervisor loop stays on the runtime it
    /// was spawned on, since that is where children are registered rather than run.
    ///
    /// Use this to keep the server off the runtime that its owner runs on -- a topology component's server belongs on
    /// the shared worker pool rather than on the runtime driving the topology, since request handling and TLS
    /// handshake crypto are both compute-heavy enough to add scheduling latency to everything else there.
    ///
    /// Defaults to running on whichever runtime the subtree was spawned on.
    pub fn with_worker_pool(mut self, handle: Handle) -> Self {
        self.worker_pool = Some(handle);
        self
    }

    fn get_server_id(&self) -> Option<Identifier> {
        self.server_id.clone().map(|sid| get_bound_address_id(&sid))
    }

    /// Converts this server into the supervisor that runs it.
    ///
    /// The result is added to another supervisor like any other child, whether up front with
    /// [`Supervisor::add_worker`] or while that supervisor runs, via
    /// [`nested_supervisor`][saluki_core::runtime::nested_supervisor].
    ///
    /// # Panics
    ///
    /// Panics if the supervisor can't be created, which can only happen for an empty name. The name is derived from a
    /// non-empty constant, so this is unreachable.
    pub fn into_supervisor(self) -> Supervisor {
        let service = self.build_router();
        let bound_address_id = self.get_server_id();

        let drain_deadline = self.graceful_shutdown_timeout.unwrap_or(DEFAULT_DRAIN_DEADLINE);
        let mut supervisor = Supervisor::new(&*self.name)
            .expect("server name is derived from a non-empty constant")
            .with_shutdown_budget(drain_deadline.saturating_add(SHUTDOWN_BUDGET_SLACK));

        let acceptor = Acceptor {
            listen_address: self.listen_address,
            bound_address_id,
            tls_config: self.tls_config,
            http2_config: self.http2_config,
            http2_only: self.http2_only,
            drain_deadline,
            service,
            supervisor: supervisor.handle(),
            worker_pool: self.worker_pool.clone(),
        };

        // The acceptor is placed the same way every other child is, so a configured worker pool covers the whole
        // server rather than just the work it spawns. Keeping the socket's accept and its serving on one runtime is
        // also what avoids polling it from a runtime other than the one its readiness is registered with.
        supervisor.add_worker(place_on_pool(runtime::supervisable(acceptor), self.worker_pool.as_ref()).build());

        supervisor
    }
}

/// Applies the configured worker pool, if any, to a child builder.
fn place_on_pool<'a, S: BuilderState>(
    builder: ChildBuilder<'a, S>, worker_pool: Option<&Handle>,
) -> ChildBuilder<'a, S> {
    match worker_pool {
        Some(pool) => builder.on_runtime(pool.clone()),
        None => builder,
    }
}

/// Accepts connections for an [`HttpServer`], handing each one off to a child of its own.
struct Acceptor {
    listen_address: ListenAddress,
    bound_address_id: Option<Identifier>,
    tls_config: Option<ServerConfig>,
    http2_config: Http2Config,
    http2_only: bool,
    drain_deadline: Duration,
    service: Router,
    supervisor: SupervisorHandle,
    worker_pool: Option<Handle>,
}

#[async_trait]
impl Supervisable for Acceptor {
    fn name(&self) -> &str {
        ACCEPTOR_TASK_NAME
    }

    async fn initialize(&self, process_shutdown: ShutdownHandle) -> Result<SupervisorFuture, InitializationError> {
        // Try binding our listener during initialization to surface issues earlier.
        let listener = ConnectionOrientedListener::from_listen_address(self.listen_address.clone())
            .await
            .with_error_context(|| format!("Failed to bind listener for HTTP server ({}).", self.listen_address))?;

        // Assert our bound listen address if we have a configured server ID.
        if let Some(bound_address_id) = self.bound_address_id.clone() {
            let dataspace = DataspaceRegistry::try_current()
                .ok_or_else(|| saluki_error::generic_error!("Dataspace not available for HTTP server."))?;

            dataspace.assert(listener.bound_listen_address(), bound_address_id);
        }

        let conn_builder = build_conn_builder(
            SupervisedExecutor::new(self.supervisor.clone(), self.worker_pool.clone()),
            self.http2_config,
            self.http2_only,
        );

        // Resolve the TLS configuration here rather than in the accept loop: an ALPN mismatch or a non-compliant
        // cipher suite is a configuration error, and surfacing it as an initialization failure keeps it from being
        // retried as though it were transient.
        let maybe_tls_acceptor = match self.tls_config.clone() {
            Some(mut config) => {
                config.alpn_protocols = alpn_protocols(&conn_builder);
                ensure_server_config_fips_compliant(&mut config)?;

                Some(TlsAcceptor::from(Arc::new(config)))
            }
            None => None,
        };

        let context = ConnectionContext {
            conn_builder,
            service: self.service.clone(),
            listen_addr: listener.listen_address().clone(),
            drain_deadline: self.drain_deadline,
            http2_config: self.http2_config,
            supervisor: self.supervisor.clone(),
            worker_pool: self.worker_pool.clone(),
        };

        Ok(Box::pin(run_accept_loop(
            listener,
            context,
            maybe_tls_acceptor,
            process_shutdown,
        )))
    }
}

/// Everything needed to hand a freshly accepted connection off to a child of its own.
#[derive(Clone)]
struct ConnectionContext {
    conn_builder: ConnBuilder,
    service: Router,
    listen_addr: ListenAddress,
    drain_deadline: Duration,
    http2_config: Http2Config,
    supervisor: SupervisorHandle,
    worker_pool: Option<Handle>,
}

impl ConnectionContext {
    /// Spawns the child that serves `io`.
    fn spawn_connection<I>(&self, io: I)
    where
        I: Read + Write + Unpin + Send + 'static,
    {
        let connection = HttpConnection {
            conn_builder: self.conn_builder.clone(),
            io: Mutex::new(Some(io)),
            service: self.service.clone(),
            listen_addr: self.listen_addr.clone(),
            drain_deadline: self.drain_deadline,
            http2_config: self.http2_config,
        };

        let builder = self.supervisor.supervisable(connection).temporary();
        place_on_pool(builder, self.worker_pool.as_ref())
            .with_budget_bounded_shutdown()
            .spawn();
    }

    /// Spawns the child that performs the TLS handshake for `stream`, and serves it once it completes.
    fn spawn_handshake(&self, acceptor: TlsAcceptor, stream: Connection) {
        let handshake = TlsHandshake {
            acceptor,
            stream: Mutex::new(Some(stream)),
            context: self.clone(),
        };

        let builder = self.supervisor.supervisable(handshake).temporary();
        place_on_pool(builder, self.worker_pool.as_ref())
            .with_budget_bounded_shutdown()
            .spawn();
    }
}

/// Accepts connections until shutdown is signalled or the listener fails.
///
/// Every accepted connection becomes a child of the server's supervisor, so this returns as soon as it stops
/// accepting; waiting for those connections to finish is the supervisor's drain, not this loop's job.
async fn run_accept_loop(
    mut listener: ConnectionOrientedListener, context: ConnectionContext, maybe_tls_acceptor: Option<TlsAcceptor>,
    shutdown: ShutdownHandle,
) -> Result<(), GenericError> {
    let tls_enabled = maybe_tls_acceptor.is_some();
    let listen_addr = context.listen_addr.clone();

    info!(%listen_addr, tls_enabled, "HTTP server started.");

    pin!(shutdown);

    let result = loop {
        select! {
            result = listener.accept() => match result {
                // Neither arm awaits anything: the handshake is a child of its own precisely so that a slow one
                // can't hold up the connections queued behind it on the listener.
                Ok(stream) => match &maybe_tls_acceptor {
                    Some(acceptor) => context.spawn_handshake(acceptor.clone(), stream),
                    None => context.spawn_connection(TokioIo::new(stream)),
                },
                Err(e) => break Err(GenericError::from(e)),
            },

            _ = &mut shutdown => {
                debug!(%listen_addr, "Received shutdown signal.");
                break Ok(());
            }
        }
    };

    info!(%listen_addr, "HTTP server stopped accepting connections.");

    result
}

/// Performs the TLS handshake for a single accepted connection.
///
/// A handshake is abandoned if it doesn't complete within [`TLS_HANDSHAKE_TIMEOUT`], or as soon as the subtree starts
/// shutting down. Neither case has anything to preserve -- no connection exists yet -- and both are what stop a peer
/// that connects and then stalls from accumulating, or from holding the drain open.
struct TlsHandshake {
    acceptor: TlsAcceptor,
    stream: Mutex<Option<Connection>>,
    context: ConnectionContext,
}

#[async_trait]
impl Supervisable for TlsHandshake {
    fn name(&self) -> &str {
        TLS_HANDSHAKE_TASK_NAME
    }

    async fn initialize(&self, process_shutdown: ShutdownHandle) -> Result<SupervisorFuture, InitializationError> {
        let stream = take_once(&self.stream, "TLS handshake")?;
        let acceptor = self.acceptor.clone();
        let context = self.context.clone();

        Ok(Box::pin(async move {
            let listen_addr = context.listen_addr.clone();

            select! {
                result = timeout(TLS_HANDSHAKE_TIMEOUT, acceptor.accept(stream)) => match result {
                    Ok(Ok(stream)) => context.spawn_connection(TokioIo::new(stream)),
                    Ok(Err(e)) => error!(%listen_addr, error = %e, "Failed to complete TLS handshake."),
                    Err(_) => warn!(
                        %listen_addr,
                        "Abandoning TLS handshake that did not complete within {:?}.", TLS_HANDSHAKE_TIMEOUT
                    ),
                },

                _ = process_shutdown => debug!(%listen_addr, "Abandoning in-flight TLS handshake at shutdown."),
            }

            Ok(())
        }))
    }
}

/// Serves a single connection, finishing what it has started if asked to shut down.
///
/// When shutdown is triggered, the connection is gracefully shutdown: new requests aren't allowed, but any pending or
/// in-flight reads/writes will be completed prior to closing the connection.
///
/// A connection that outlives the configured maximum age is retired the same way, independently of server shutdown.
struct HttpConnection<I> {
    conn_builder: ConnBuilder,
    io: Mutex<Option<I>>,
    service: Router,
    listen_addr: ListenAddress,
    drain_deadline: Duration,
    http2_config: Http2Config,
}

#[async_trait]
impl<I> Supervisable for HttpConnection<I>
where
    I: Read + Write + Unpin + Send + 'static,
{
    fn name(&self) -> &str {
        CONNECTION_TASK_NAME
    }

    fn shutdown_strategy(&self) -> ShutdownStrategy {
        // Spawned with a budget-bounded shutdown, so this is only consulted if the subtree somehow has no budget. A
        // connection bounds its own drain either way, so there is nothing shorter worth imposing here.
        ShutdownStrategy::Graceful(Duration::MAX)
    }

    async fn initialize(&self, process_shutdown: ShutdownHandle) -> Result<SupervisorFuture, InitializationError> {
        let io = take_once(&self.io, "HTTP connection")?;
        let conn_builder = self.conn_builder.clone();
        let service = self.service.clone();
        let listen_addr = self.listen_addr.clone();
        let drain_deadline = self.drain_deadline;
        let http2_config = self.http2_config;

        Ok(Box::pin(async move {
            drive_connection(
                conn_builder,
                io,
                service,
                listen_addr,
                process_shutdown,
                drain_deadline,
                http2_config,
            )
            .await;

            Ok(())
        }))
    }
}

/// Takes the single-use value out of a worker, failing initialization if it has already been taken.
///
/// A connection and a handshake are both built around a value that can't be recreated -- a socket -- so they run
/// exactly once. Their restart policy says as much, and this is the backstop if that ever stops being true.
fn take_once<T>(slot: &Mutex<Option<T>>, what: &str) -> Result<T, InitializationError> {
    slot.lock()
        .expect("single-use worker mutex poisoned")
        .take()
        .ok_or_else(|| {
            InitializationError::from(saluki_error::generic_error!("{} can only be initialized once.", what))
        })
}

async fn drive_connection<I>(
    conn_builder: ConnBuilder, io: I, service: Router, listen_addr: ListenAddress, shutdown: ShutdownHandle,
    drain_deadline: Duration, http2_config: Http2Config,
) where
    I: Read + Write + Unpin + Send + 'static,
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

                match timeout(drain_deadline, conn.as_mut()).await {
                    Ok(Ok(())) => {},
                    Ok(Err(e)) => warn!(%listen_addr, error = %e, "Failed to drain HTTP connection."),
                    Err(_) => warn!(%listen_addr, "Failed to gracefully drain HTTP connection after {:?}.", drain_deadline)
                }

                return;
            },
        }
    }
}

/// An executor that runs `hyper`'s work as supervised children.
///
/// `hyper` needs somewhere to put the futures it can't drive from the connection itself. For HTTP/2 that is one future
/// per stream -- so one per request -- which is why this exists rather than a bare `tokio::spawn`: it is the
/// difference between per-request work being part of the process tree and being invisible to it.
///
/// Children are spawned against a concrete handle rather than the ambient supervisor, so this works wherever `hyper`
/// happens to call it from.
#[derive(Clone)]
struct SupervisedExecutor {
    supervisor: SupervisorHandle,
    worker_pool: Option<Handle>,
}

impl SupervisedExecutor {
    fn new(supervisor: SupervisorHandle, worker_pool: Option<Handle>) -> Self {
        Self {
            supervisor,
            worker_pool,
        }
    }
}

impl<F> hyper::rt::Executor<F> for SupervisedExecutor
where
    F: Future<Output = ()> + Send + 'static,
{
    fn execute(&self, fut: F) {
        let builder = self.supervisor.worker(CONNECTION_BG_TASK_NAME, fut);
        place_on_pool(builder, self.worker_pool.as_ref()).spawn();
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

fn build_conn_builder(executor: SupervisedExecutor, http2_config: Http2Config, http2_only: bool) -> ConnBuilder {
    let mut builder = Builder::new(executor);
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

/// ALPN protocols a server advertises, in preference order.
fn alpn_protocols(conn_builder: &ConnBuilder) -> Vec<Vec<u8>> {
    let mut protocols = vec![];

    if conn_builder.is_http2_available() {
        protocols.push(b"h2".to_vec());
    }

    if conn_builder.is_http1_available() {
        protocols.push(b"http/1.1".to_vec());
    }

    protocols
}

fn get_bound_address_id(server_id: &str) -> Identifier {
    Identifier::from(format!("http-server-{}", server_id))
}

#[cfg(test)]
mod tests {
    use std::net::{SocketAddr, TcpListener as StdTcpListener};
    use std::sync::atomic::{AtomicBool, Ordering};

    use http::{Response, StatusCode, Version};
    use http_body_util::{Empty, Full};
    use hyper_util::client::legacy::Client;
    use hyper_util::rt::TokioExecutor;
    use saluki_core::runtime::SupervisorError;
    use saluki_metrics::test::TestRecorder;
    use saluki_tls::test_util::SelfSignedCert;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    use tokio::net::TcpStream;
    use tokio::sync::oneshot;
    use tokio::task::JoinHandle;
    use tokio::time::{timeout, Instant};
    use tower::util::service_fn;

    use super::*;
    use crate::net::addr::BoundListenAddress;
    #[cfg(unix)]
    use crate::net::server::test_util::connect_unix;
    use crate::net::server::test_util::{connect_tcp, ServerTestHarness};

    /// Bound on any server await in these tests, so a hang fails rather than stalling the suite.
    const TEST_TIMEOUT: Duration = Duration::from_secs(5);

    /// A running server subtree, together with the trigger that stops it.
    struct RunningServer {
        shutdown_tx: Option<oneshot::Sender<()>>,
        task: JoinHandle<Result<(), SupervisorError>>,
    }

    impl RunningServer {
        /// Starts `server` on its own task.
        ///
        /// A server can't be driven by hand any more: its connections are children of its supervisor, so there has to
        /// be one running for anything to be served at all.
        fn start(server: HttpServer) -> Self {
            let mut supervisor = server.into_supervisor();
            let (shutdown_tx, shutdown_rx) = oneshot::channel();
            let task = tokio::spawn(async move { supervisor.run_with_shutdown(shutdown_rx).await });

            Self {
                shutdown_tx: Some(shutdown_tx),
                task,
            }
        }

        /// Signals shutdown without waiting for the subtree to finish.
        fn signal_shutdown(&mut self) {
            let _ = self.shutdown_tx.take().expect("should only shut down once").send(());
        }

        /// Awaits the subtree, returning whatever it reported.
        async fn join(self) -> Result<(), SupervisorError> {
            timeout(TEST_TIMEOUT, self.task)
                .await
                .expect("server should stop before timeout")
                .expect("server task should not panic")
        }

        /// Signals shutdown and asserts the subtree drained cleanly.
        async fn shutdown(mut self) {
            self.signal_shutdown();
            let result = self.join().await;
            assert!(result.is_ok(), "server should stop cleanly: {result:?}");
        }
    }

    /// Waits until `addr` is actually being served.
    ///
    /// Connecting is the only safe way to probe this. Trying to bind the address to see whether the server has taken
    /// it would race the server for that address and could win, which is a bind failure the test itself caused.
    async fn wait_until_listening(addr: SocketAddr) {
        drop(connect_tcp(addr).await);
    }

    /// Builds a connection builder for the tests that only inspect its configuration.
    fn test_conn_builder(http2_only: bool) -> ConnBuilder {
        let supervisor = Supervisor::new("alpn-test").expect("test supervisor name should be valid");

        build_conn_builder(
            SupervisedExecutor::new(supervisor.handle(), None),
            Http2Config::default(),
            http2_only,
        )
    }

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
        let router = HttpServer::from_listen_address(ListenAddress::tcp_loopback(0))
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

    #[test]
    fn the_subtree_name_reflects_the_server_id() {
        // Two servers sharing a supervisor have to be tellable apart in logs and per-worker task metrics. That is what
        // the identifier buys beyond namespacing assertions, and why an endpoint sets one even when nothing consumes
        // the assertions it enables. The name reaches those metrics as the subtree's supervisor ID, which every child
        // process name is scoped under.
        let unnamed = HttpServer::from_listen_address(ListenAddress::tcp_loopback(0)).into_supervisor();
        assert_eq!(unnamed.id(), DEFAULT_SERVER_NAME);

        let grpc = HttpServer::from_listen_address(ListenAddress::tcp_loopback(0))
            .with_server_id("otlp-grpc")
            .into_supervisor();
        let http = HttpServer::from_listen_address(ListenAddress::tcp_loopback(0))
            .with_server_id("otlp-http")
            .into_supervisor();
        assert_eq!(grpc.id(), "http_server_otlp-grpc");
        assert_eq!(http.id(), "http_server_otlp-http");
    }

    #[tokio::test]
    async fn an_http_only_server_keeps_its_own_fallback() {
        // With no gRPC service attached there is no protocol-aware fallback to install, so the caller's own fallback
        // is left alone rather than being displaced by one it never asked for.
        let router = HttpServer::from_listen_address(ListenAddress::tcp_loopback(0))
            .add_routes(Router::new().fallback(|| async { StatusCode::IM_A_TEAPOT }))
            .build_router();

        let response = route_request(router, "/anything", None).await;
        assert_eq!(response.status(), StatusCode::IM_A_TEAPOT);
    }

    #[tokio::test]
    async fn overriding_the_router_discards_accumulated_routes() {
        let router = HttpServer::from_listen_address(ListenAddress::tcp_loopback(0))
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
            supervisor.add_worker(server.into_supervisor());
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
            supervisor.add_worker(server.into_supervisor());
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

    /// Builds a TLS config for a server presenting `cert`.
    fn server_tls_config(cert: &SelfSignedCert) -> ServerConfig {
        ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(cert.cert_chain(), cert.private_key())
            .expect("should build TLS config")
    }

    /// Handshakes with a TLS server offering exactly `client_alpn`, returning the negotiated protocol.
    async fn negotiate_alpn(
        address: SocketAddr, cert_chain: Vec<rustls::pki_types::CertificateDer<'static>>, client_alpn: &[&[u8]],
    ) -> std::io::Result<Option<Vec<u8>>> {
        let mut roots = rustls::RootCertStore::empty();
        for cert in cert_chain {
            roots.add(cert).expect("should trust the self-signed cert");
        }

        let mut client_config = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        client_config.alpn_protocols = client_alpn.iter().map(|protocol| protocol.to_vec()).collect();

        let stream = TcpStream::connect(address).await.expect("should connect");
        let server_name = rustls::pki_types::ServerName::try_from("localhost").expect("should be a valid server name");
        let tls_stream = tokio_rustls::TlsConnector::from(Arc::new(client_config))
            .connect(server_name, stream)
            .await?;

        Ok(tls_stream.get_ref().1.alpn_protocol().map(ToOwned::to_owned))
    }

    /// Starts a TLS server on an ephemeral port, returning its address and the cert chain to trust.
    async fn start_tls_server(
        harness_id: &str, http2_only: bool,
    ) -> (
        ServerTestHarness,
        SocketAddr,
        Vec<rustls::pki_types::CertificateDer<'static>>,
    ) {
        let _ = saluki_tls::initialize_default_crypto_provider();
        let cert = SelfSignedCert::localhost();
        let cert_chain = cert.cert_chain();
        let tls_config = server_tls_config(&cert);

        let harness = ServerTestHarness::start(harness_id, move |supervisor, server_id| {
            let mut server = server_with(ListenAddress::tcp_loopback(0), ok_response)
                .with_tls_config(tls_config)
                .with_server_id(server_id);
            if http2_only {
                server = server.with_http2_only();
            }
            supervisor.add_worker(server.into_supervisor());
        })
        .await;

        let address = match harness.bound_address().await {
            BoundListenAddress::Tcp(addr) => addr,
            other_addr => panic!("expected TCP address, got {:?}", other_addr),
        };

        (harness, address, cert_chain)
    }

    #[test]
    fn advertised_alpn_protocols_track_the_protocol_restriction() {
        let conn_builder_both = test_conn_builder(false);
        assert_eq!(
            alpn_protocols(&conn_builder_both),
            vec![b"h2".to_vec(), b"http/1.1".to_vec()]
        );

        let conn_builder_http2_only = test_conn_builder(true);
        assert_eq!(alpn_protocols(&conn_builder_http2_only), vec![b"h2".to_vec()]);
    }

    #[tokio::test]
    async fn tls_server_advertises_both_protocols_by_default() {
        let (harness, address, cert_chain) = start_tls_server("http-alpn-auto", false).await;

        // A client that prefers HTTP/2 gets it, and one that only speaks HTTP/1.1 is still served.
        let negotiated = negotiate_alpn(address, cert_chain.clone(), &[b"h2", b"http/1.1"])
            .await
            .expect("handshake should succeed");
        assert_eq!(negotiated, Some(b"h2".to_vec()));

        let negotiated = negotiate_alpn(address, cert_chain, &[b"http/1.1"])
            .await
            .expect("handshake should succeed");
        assert_eq!(negotiated, Some(b"http/1.1".to_vec()));

        harness.shutdown().await;
    }

    #[tokio::test]
    async fn tls_server_advertises_only_http2_when_restricted() {
        let (harness, address, cert_chain) = start_tls_server("http-alpn-http2-only", true).await;

        let negotiated = negotiate_alpn(address, cert_chain.clone(), &[b"h2", b"http/1.1"])
            .await
            .expect("handshake should succeed");
        assert_eq!(negotiated, Some(b"h2".to_vec()));

        // The point of the restriction: an HTTP/1.1-only client cannot get a connection at all, instead of negotiating
        // a protocol the connection would immediately be torn down for. That the HTTP/2 client above succeeded against
        // the same certificate is what makes this failure attributable to ALPN rather than to trust.
        //
        // Which error the client sees is deliberately not pinned down: `rustls` rejects the handshake with a
        // `no_application_protocol` alert, but the accept loop drops the stream on handshake failure, so whether that
        // alert reaches the client before EOF is a race.
        negotiate_alpn(address, cert_chain, &[b"http/1.1"])
            .await
            .expect_err("handshake should fail when no offered protocol is served");

        harness.shutdown().await;
    }

    #[tokio::test]
    async fn a_stalled_tls_handshake_does_not_block_other_connections() {
        // The reason handshakes are children of their own. A peer that connects and then says nothing leaves its
        // handshake outstanding indefinitely; when handshakes were awaited inline in the accept loop, that one peer
        // was enough to stop the server accepting anything at all, and this second client would never be served.
        let (harness, address, cert_chain) = start_tls_server("http-tls-head-of-line", false).await;

        let _stalled = TcpStream::connect(address).await.expect("should connect");

        let negotiated = timeout(TEST_TIMEOUT, negotiate_alpn(address, cert_chain, &[b"h2"]))
            .await
            .expect("a stalled handshake must not delay another client's")
            .expect("handshake should succeed");
        assert_eq!(negotiated, Some(b"h2".to_vec()));

        // Shutting down cleanly is the other half: the stalled handshake is abandoned rather than held onto until the
        // subtree's budget elapses, which would take far longer than the harness allows.
        harness.shutdown().await;
    }

    /// Records the name of the thread a request handler ran on.
    fn thread_recording_server(listen_address: ListenAddress) -> (HttpServer, Arc<Mutex<Option<String>>>) {
        let thread_name = Arc::new(Mutex::new(None));
        let recorder = Arc::clone(&thread_name);

        let server = server_with(listen_address, move || {
            let recorder = Arc::clone(&recorder);
            async move {
                *recorder.lock().unwrap() = Some(std::thread::current().name().unwrap_or_default().to_string());
                ok_response().await
            }
        });

        (server, thread_name)
    }

    /// Builds a runtime whose threads are recognizable by name.
    fn named_pool(name: &'static str) -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .thread_name(name)
            .enable_all()
            .build()
            .expect("should build pool")
    }

    #[tokio::test]
    async fn the_worker_pool_runs_request_handling() {
        // What `with_worker_pool` is for, and the property the topology components depend on: request handling stays
        // off the runtime driving the topology, since decoding a large request is compute-heavy enough to add
        // scheduling latency to everything else there. The acceptor, handshakes, connections, and the futures `hyper`
        // executes are all placed through the same call, so covering the handler covers the arrangement.
        let pool = named_pool("http-pool-test");

        let addr = free_local_addr();
        let (server, handler_thread) = thread_recording_server(ListenAddress::Tcp(addr));
        let server = RunningServer::start(server.with_worker_pool(pool.handle().clone()));
        wait_until_listening(addr).await;

        let client = Client::builder(TokioExecutor::new()).build_http::<Empty<bytes::Bytes>>();
        let uri = format!("http://{addr}/").parse().expect("should be a valid URI");
        let response = timeout(TEST_TIMEOUT, client.get(uri))
            .await
            .expect("server should answer")
            .expect("request should succeed");
        assert_eq!(response.status(), StatusCode::OK);

        let handler_thread = handler_thread.lock().unwrap().clone().expect("handler should have run");
        assert!(
            handler_thread.starts_with("http-pool-test"),
            "request handling must run on the worker pool, but ran on thread {handler_thread:?}"
        );

        server.shutdown().await;
        pool.shutdown_background();
    }

    #[tokio::test]
    async fn the_worker_pool_runs_request_handling_behind_tls() {
        // The same, over TLS, which is the path that actually goes through a handshake child before a connection child
        // exists at all. Serving the request at all means the hand-off between the two survived being placed.
        let _ = saluki_tls::initialize_default_crypto_provider();
        let cert = SelfSignedCert::localhost();
        let tls_config = server_tls_config(&cert);
        let pool = named_pool("http-tls-pool-test");

        let addr = free_local_addr();
        let (server, handler_thread) = thread_recording_server(ListenAddress::Tcp(addr));
        let server = RunningServer::start(
            server
                .with_tls_config(tls_config)
                .with_worker_pool(pool.handle().clone()),
        );
        wait_until_listening(addr).await;

        let negotiated = timeout(TEST_TIMEOUT, negotiate_alpn(addr, cert.cert_chain(), &[b"http/1.1"]))
            .await
            .expect("handshake should complete")
            .expect("handshake should succeed");
        assert_eq!(negotiated, Some(b"http/1.1".to_vec()));

        // `negotiate_alpn` only handshakes, so drive an actual request through to reach the handler.
        let mut roots = rustls::RootCertStore::empty();
        for cert in cert.cert_chain() {
            roots.add(cert).expect("should trust the self-signed cert");
        }
        let mut client_config = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        client_config.alpn_protocols = vec![b"http/1.1".to_vec()];

        let stream = TcpStream::connect(addr).await.expect("should connect");
        let server_name = rustls::pki_types::ServerName::try_from("localhost").expect("should be a valid server name");
        let mut tls_stream = tokio_rustls::TlsConnector::from(Arc::new(client_config))
            .connect(server_name, stream)
            .await
            .expect("handshake should succeed");
        tls_stream
            .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .await
            .expect("should write request");

        let mut response = Vec::new();
        timeout(TEST_TIMEOUT, tls_stream.read_to_end(&mut response))
            .await
            .expect("response should arrive")
            .expect("should read response");
        assert!(
            response.ends_with(b"ok"),
            "expected the request to be served, got {:?}",
            String::from_utf8_lossy(&response)
        );

        let handler_thread = handler_thread.lock().unwrap().clone().expect("handler should have run");
        assert!(
            handler_thread.starts_with("http-tls-pool-test"),
            "request handling must run on the worker pool, but ran on thread {handler_thread:?}"
        );

        server.shutdown().await;
        pool.shutdown_background();
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
            supervisor.add_worker(server.into_supervisor());
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
            supervisor.add_worker(server.into_supervisor());
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
    async fn binds_its_listener_during_initialization() {
        // The listener is bound by the acceptor's `initialize`, not by its run future, which is what makes a bind
        // failure a non-restartable initialization error rather than something retried forever. Observed here by
        // connecting: probing with a bind of our own would race the server for the address and could win.
        let addr = free_local_addr();
        let server = RunningServer::start(server_with(ListenAddress::Tcp(addr), ok_response));

        wait_until_listening(addr).await;

        server.shutdown().await;
    }

    #[tokio::test]
    async fn bind_failure_is_an_initialization_error() {
        // Hold the address so the server can't have it. An initialization error is non-restartable, which is the point:
        // an unusable listen address should fail the subtree rather than being retried forever. That it propagates as
        // `FailedToInitialize` -- rather than as a runtime error the parent would restart -- is what carries that
        // property across the subtree boundary.
        let addr = free_local_addr();
        let _held = StdTcpListener::bind(addr).expect("should hold the address");

        let server = RunningServer::start(server_with(ListenAddress::Tcp(addr), ok_response));

        // Returning at all (before the timeout in `join`) is half the assertion: a retry loop would never get here.
        match server.join().await {
            Ok(()) => panic!("the subtree should have failed to bind {addr}"),
            Err(SupervisorError::FailedToInitialize { source, .. }) => {
                let error = source.to_string();
                assert!(error.contains("Failed to bind listener"), "unexpected error: {error}");
            }
            Err(e) => panic!("expected an initialization failure, got {e:?}"),
        }
    }

    #[tokio::test]
    async fn releases_its_port_once_the_subtree_finishes() {
        // The whole reason for supervising the server: when its subtree stops, the socket is gone. Previously the
        // acceptor was a detached task that outlived whatever spawned it.
        let addr = free_local_addr();
        let server = RunningServer::start(server_with(ListenAddress::Tcp(addr), ok_response));
        wait_until_listening(addr).await;

        server.shutdown().await;

        assert!(
            StdTcpListener::bind(addr).is_ok(),
            "the server should have released {addr} when its subtree finished"
        );
    }

    #[tokio::test]
    async fn a_half_sent_request_does_not_wedge_the_drain() {
        // A peer that writes a partial request head and stalls keeps its connection permanently non-idle, so
        // `graceful_shutdown` alone never closes it. Before the connection builder had a timer and the drain had a
        // deadline, one such socket stalled shutdown indefinitely -- for the OTLP receivers that meant every ADP
        // shutdown hanging until the component budget forced an abort.
        //
        // A clean result is the assertion that matters: the connection bounds its own drain and exits, rather than
        // being force-aborted by the subtree's budget, which would surface as `ShutdownTimedOut` all the way up.
        let addr = free_local_addr();
        let server = RunningServer::start(
            server_with(ListenAddress::Tcp(addr), ok_response).with_graceful_shutdown_timeout(Duration::from_secs(1)),
        );
        wait_until_listening(addr).await;

        let mut stream = TcpStream::connect(addr).await.expect("should connect");
        stream
            .write_all(b"GET / HTTP/1.1\r\nHost: localhost")
            .await
            .expect("should write a partial request head");
        stream.flush().await.expect("should flush");

        // Let the server read what there is before signalling, so the connection is genuinely mid-parse.
        tokio::time::sleep(Duration::from_millis(100)).await;

        server.shutdown().await;
    }

    #[tokio::test]
    async fn does_not_finish_until_in_flight_requests_do() {
        // Shutdown stops the server accepting, but a request already being served has to complete first. The
        // connection is a supervised child, so the subtree's drain is what waits for it; without that the subtree
        // would return as soon as the acceptor stopped and the response would be lost.
        let addr = free_local_addr();

        let handler_started = Arc::new(AtomicBool::new(false));
        let started = Arc::clone(&handler_started);
        let mut server = RunningServer::start(server_with(ListenAddress::Tcp(addr), move || {
            let started = Arc::clone(&started);
            async move {
                started.store(true, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(300)).await;
                ok_response().await
            }
        }));
        wait_until_listening(addr).await;

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
        server.signal_shutdown();

        // The handler is still working, so the subtree must not be finished yet. This ordering is the actual assertion:
        // without the drain the subtree returns here and the response is abandoned.
        assert!(
            timeout(Duration::from_millis(50), &mut server.task).await.is_err(),
            "subtree should not finish while a request is still being served"
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

        let result = server.join().await;
        assert!(result.is_ok(), "subtree should stop cleanly after draining: {result:?}");
    }

    #[tokio::test]
    async fn serves_http2_requests() {
        // The protocol is chosen from the bytes the client opens with rather than from ALPN, so a cleartext client that
        // knows to speak HTTP/2 gets HTTP/2. That is what lets one server carry both REST-ful routes and gRPC services,
        // since gRPC is HTTP/2 and nothing else.
        let harness = ServerTestHarness::start("http2-request", |supervisor, server_id| {
            let server = server_with(ListenAddress::tcp_loopback(0), ok_response).with_server_id(server_id);
            supervisor.add_worker(server.into_supervisor());
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
    async fn an_http2_request_runs_as_a_supervised_child() {
        // `hyper` hands us one future per HTTP/2 stream -- so one per request -- and our executor turns each into a
        // dynamic child rather than a detached task. That is what puts per-request work in the process tree, and the
        // per-task poll metrics are where it becomes observable. The tag is the child's fully qualified process name,
        // scoped under the subtree's own supervisor ID.
        let recorder = TestRecorder::default();
        let _guard = metrics::set_default_local_recorder(&recorder);

        // The recorder must be installed before anything is spawned: metric handles are resolved once, at spawn.
        let addr = free_local_addr();
        let server = RunningServer::start(
            server_with(ListenAddress::Tcp(addr), ok_response).with_server_id("supervised-stream"),
        );
        wait_until_listening(addr).await;

        let client = Client::builder(TokioExecutor::new())
            .http2_only(true)
            .build_http::<Empty<bytes::Bytes>>();
        let uri = format!("http://{addr}/").parse().expect("should be a valid URI");
        let response = timeout(TEST_TIMEOUT, client.get(uri))
            .await
            .expect("server should answer an HTTP/2 request")
            .expect("request should succeed");
        assert_eq!(response.status(), StatusCode::OK);

        server.shutdown().await;

        let polls = recorder.counter((
            "runtime_task_poll_count",
            &[("task_name", "http_server_supervised_stream.http_conn_task")],
        ));
        assert!(
            polls.is_some_and(|polls| polls > 0),
            "the stream `hyper` executed should have run as a supervised child, got {polls:?}"
        );
    }

    #[tokio::test]
    async fn http2_only_server_rejects_http1_requests() {
        // An endpoint that only serves gRPC has no use for HTTP/1.1, and rejecting it at the protocol level tells the
        // caller more than routing the request and answering with a 404 would.
        let addr = free_local_addr();
        let server = RunningServer::start(server_with(ListenAddress::Tcp(addr), ok_response).with_http2_only());
        wait_until_listening(addr).await;

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

        server.shutdown().await;
    }

    #[tokio::test]
    async fn an_idle_http2_peer_does_not_wedge_the_drain() {
        // The HTTP/2 counterpart of the half-sent request case: a peer that writes part of the connection preface and
        // then stalls never becomes idle, so `graceful_shutdown` alone will not close it.
        let addr = free_local_addr();
        let server = RunningServer::start(
            server_with(ListenAddress::Tcp(addr), ok_response).with_graceful_shutdown_timeout(Duration::from_secs(1)),
        );
        wait_until_listening(addr).await;

        let mut stream = TcpStream::connect(addr).await.expect("should connect");
        stream
            .write_all(b"PRI * HTTP/2.0\r\n")
            .await
            .expect("should write a partial HTTP/2 preface");
        stream.flush().await.expect("should flush");
        tokio::time::sleep(Duration::from_millis(100)).await;

        server.shutdown().await;
    }

    #[tokio::test]
    async fn retires_connections_that_reach_their_maximum_age() {
        // Nothing in the connection builder knows about connection age, so the server enforces the deadline itself.
        // Without it, a long-lived HTTP/2 client pins itself to whichever backend it first reached and stays there.
        let addr = free_local_addr();
        let max_age = Duration::from_millis(300);
        let server = RunningServer::start(
            server_with(ListenAddress::Tcp(addr), ok_response)
                .with_http2_config(Http2Config::default().with_max_connection_age(max_age, None)),
        );
        wait_until_listening(addr).await;

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

        server.shutdown().await;
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
