//! Dynamic API server.
//!
//! Unlike [`APIBuilder`][crate::api::APIBuilder], which constructs its route set once at build time,
//! `DynamicAPIBuilder` subscribes to runtime notifications via the dataspace registry and dynamically registers and
//! unregisters routes as they're asserted and retracted.

use std::{
    convert::Infallible,
    error::Error,
    future::Future,
    net::SocketAddr,
    panic::{catch_unwind, AssertUnwindSafe},
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use arc_swap::ArcSwap;
use async_trait::async_trait;
use axum::{body::Body as AxumBody, Router};
use http::{Request, Response};
use rcgen::{generate_simple_self_signed, CertifiedKey};
use rustls::{pki_types::PrivateKeyDer, ServerConfig};
use rustls_pki_types::PrivatePkcs8KeyDer;
use saluki_api::{APIHandler, DynamicRoute, EndpointProtocol, EndpointType};
use saluki_common::collections::FastIndexMap;
use saluki_core::runtime::{
    state::{AssertionUpdate, DataspaceRegistry, Identifier, IdentifierFilter, Subscription},
    InitializationError, ProcessShutdown, Supervisable, SupervisorFuture,
};
use saluki_error::{generic_error, GenericError};
use saluki_io::net::{
    listener::ConnectionOrientedListener,
    server::{
        http::{ErrorHandle, HttpServer, ShutdownHandle},
        multiplex_service::MultiplexService,
    },
    util::hyper::TowerToHyperService,
    ListenAddress,
};
use tokio::{pin, select};
use tonic::{body::Body as GrpcBody, server::NamedService, service::RoutesBuilder};
use tower::Service;
use tracing::{debug, info, warn};

/// The actual bound listen address of a running dynamic API server.
///
/// Asserted by dynamic API servers to allow discovering the exact socket address the server is bound to.
#[derive(Clone, Debug)]
pub struct BoundApiAddress(pub SocketAddr);

/// A dynamic API server that can add and remove routes at runtime.
///
/// `DynamicAPIBuilder` serves HTTP and gRPC on a given address, multiplexing both protocols on a single port. Route
/// additions and removals are handled by subscribing to assertions/retractions of [`DynamicRoute`] in the
/// [`DataspaceRegistry`].
///
/// ## Adding and removing routes
///
/// Any process that wants to dynamically register API routes can simply assert a [`DynamicRoute`] in the
/// [`DataspaceRegistry`]. Retracting the assertion will remove the route, either when retracted manually or when the
/// process owning the route assertions exits.
///
/// If the dynamic API server is restarted, it will re-register any routes that were previously asserted.
///
/// ## Static handlers and services
///
/// In addition to dynamic routes, callers can register static HTTP handlers and gRPC services up-front via
/// [`with_handler`][Self::with_handler], [`with_optional_handler`][Self::with_optional_handler], and
/// [`with_grpc_service`][Self::with_grpc_service]. These form a base router that's cloned on every rebuild and merged
/// with the currently-asserted dynamic routes. Static routes take precedence on conflicts: a dynamic route whose path
/// and method overlap with a static route is skipped (with a warning) until the conflict clears.
///
/// ## Assertions
///
/// - `BoundApiAddress`: the actual listen address bound by the API server. Identifier is `"dynamic-<type>-api"`, where
///   `type` is the stringified value of `EndpointType::as_str` (for example, `"dynamic-privileged-api"`)
pub struct DynamicAPIBuilder {
    endpoint_type: EndpointType,
    listen_address: ListenAddress,
    tls_config: Option<ServerConfig>,
    http_router: Router,
    grpc_router: RoutesBuilder,
}

impl DynamicAPIBuilder {
    /// Creates a new `DynamicAPIBuilder` for the given endpoint type and listen address.
    pub fn new(endpoint_type: EndpointType, listen_address: ListenAddress) -> Self {
        Self {
            endpoint_type,
            listen_address,
            tls_config: None,
            http_router: Router::new(),
            grpc_router: RoutesBuilder::default(),
        }
    }

    /// Adds the given handler as a static HTTP handler.
    ///
    /// The handler's initial state and routes are merged into the base router. These routes are always served by the
    /// API regardless of which dynamic routes are currently asserted.
    pub fn with_handler<H>(mut self, handler: H) -> Self
    where
        H: APIHandler,
    {
        let handler_router = handler.generate_routes();
        let handler_state = handler.generate_initial_state();
        self.http_router = self.http_router.merge(handler_router.with_state(handler_state));
        self
    }

    /// Adds the given optional handler as a static HTTP handler.
    ///
    /// If `handler` is `Some`, its initial state and routes are merged into the base router. Otherwise the builder is
    /// returned unchanged.
    pub fn with_optional_handler<H>(self, handler: Option<H>) -> Self
    where
        H: APIHandler,
    {
        if let Some(handler) = handler {
            self.with_handler(handler)
        } else {
            self
        }
    }

    /// Adds the given gRPC service as a static service on the base router.
    pub fn with_grpc_service<S>(mut self, svc: S) -> Self
    where
        S: Service<Request<GrpcBody>, Response = Response<GrpcBody>, Error = Infallible>
            + NamedService
            + Clone
            + Send
            + Sync
            + 'static,
        S::Future: Send + 'static,
        S::Error: Into<Box<dyn Error + Send + Sync>> + Send,
    {
        self.grpc_router.add_service(svc);
        self
    }

    /// Sets the TLS configuration for the server.
    pub fn with_tls_config(mut self, config: ServerConfig) -> Self {
        self.tls_config = Some(config);
        self
    }

    /// Sets the TLS configuration for the server based on a dynamically generated, self-signed certificate.
    pub fn with_self_signed_tls(self) -> Self {
        let CertifiedKey { cert, signing_key } = generate_simple_self_signed(["localhost".to_owned()]).unwrap();
        let cert_chain = vec![cert.der().clone()];
        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(signing_key.serialize_der()));

        let config = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(cert_chain, key)
            .unwrap();

        self.with_tls_config(config)
    }
}

#[async_trait]
impl Supervisable for DynamicAPIBuilder {
    fn name(&self) -> &str {
        match self.endpoint_type {
            EndpointType::Unprivileged => "dynamic-unprivileged-api",
            EndpointType::Privileged => "dynamic-privileged-api",
        }
    }

    async fn initialize(&self, process_shutdown: ProcessShutdown) -> Result<SupervisorFuture, InitializationError> {
        // Build the static base routers.
        //
        // We reset the fallback route of the base gRPC router as Tonic's `unimplemented` fallback handle will collide
        // when merging additional gRPC service routers together. We do this for every gRPC service router that we merge
        // and then we re-apply a fallback handler that returns a standard gRPC `UNIMPLEMENTED` response when we have
        // our final, merged gRPC router.
        let base_http = self.http_router.clone();
        let base_grpc = self.grpc_router.clone().routes().into_axum_router().reset_fallback();

        // Create dynamic inner routers for both HTTP and gRPC sides, seeded with the static base so that the static
        // routes are served even before any dynamic routes are asserted. The gRPC seed gets the unimplemented fallback
        // applied so unmatched gRPC requests return the correct status from the start.
        let (inner_http, outer_http) = create_dynamic_router(base_http.clone());
        let (inner_grpc, outer_grpc) = create_dynamic_router(grpc_post_process(base_grpc.clone()));

        let dataspace = DataspaceRegistry::try_current().ok_or_else(|| generic_error!("Dataspace not available."))?;

        // Subscribe to all dynamic route assertions.
        let route_assertions = dataspace.subscribe::<DynamicRoute>(IdentifierFilter::All);

        // Bind the HTTP listener immediately so we fail fast on bind errors.
        let listener = ConnectionOrientedListener::from_listen_address(self.listen_address.clone())
            .await
            .map_err(|e| InitializationError::Failed { source: e.into() })?;

        // Assert the actual bound address so other processes can discover it (e.g. when using port 0).
        let bound_addr = listener
            .local_addr()
            .map_err(|e| InitializationError::Failed { source: e.into() })?;
        dataspace.assert(BoundApiAddress(bound_addr), Identifier::named(self.name()));

        let multiplexed_service = TowerToHyperService::new(MultiplexService::new(outer_http, outer_grpc));

        let mut http_server = HttpServer::from_listener(listener, multiplexed_service);
        if let Some(tls_config) = self.tls_config.clone() {
            http_server = http_server.with_tls_config(tls_config);
        }
        let (shutdown_handle, error_handle) = http_server.listen();

        let endpoint_type = self.endpoint_type;
        let listen_address = self.listen_address.clone();

        Ok(Box::pin(async move {
            info!("Serving {} API on {}.", endpoint_type.name(), listen_address);

            run_event_loop(
                inner_http,
                inner_grpc,
                base_http,
                base_grpc,
                route_assertions,
                endpoint_type,
                process_shutdown,
                shutdown_handle,
                error_handle,
            )
            .await
        }))
    }
}

/// A [`tower::Service`] that routes a request based on a dynamically-updated [`Router`].
///
/// When installed as the fallback service for a top-level [`Router`], `DynamicRouterService` dynamically routing
/// requests based on the current defined "inner" router, which itself can be hot-swapped at runtime. This allows for
/// seamless updates to the API endpoint routing without requiring a restart of the HTTP listener or complicated
/// configuration changes.
#[derive(Clone)]
struct DynamicRouterService {
    inner_router: Arc<ArcSwap<Router>>,
}

impl DynamicRouterService {
    fn from_inner(inner_router: &Arc<ArcSwap<Router>>) -> Self {
        Self {
            inner_router: Arc::clone(inner_router),
        }
    }
}

impl Service<http::Request<AxumBody>> for DynamicRouterService {
    type Response = Response<AxumBody>;
    type Error = Infallible;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: http::Request<AxumBody>) -> Self::Future {
        let mut router = Arc::unwrap_or_clone(self.inner_router.load_full());
        Box::pin(async move { router.call(request).await })
    }
}

/// Runs the event loop that listens for route assertions/retractions and hot-swaps the inner routers.
#[allow(clippy::too_many_arguments)]
async fn run_event_loop(
    inner_http: Arc<ArcSwap<Router>>, inner_grpc: Arc<ArcSwap<Router>>, base_http: Router, base_grpc: Router,
    mut route_assertions: Subscription<DynamicRoute>, endpoint_type: EndpointType,
    mut process_shutdown: ProcessShutdown, shutdown_handle: ShutdownHandle, error_handle: ErrorHandle,
) -> Result<(), GenericError> {
    let mut http_handlers = FastIndexMap::default();
    let mut grpc_handlers = FastIndexMap::default();

    let shutdown = process_shutdown.wait_for_shutdown();
    pin!(shutdown);
    pin!(error_handle);

    loop {
        select! {
            _ = &mut shutdown => {
                debug!("Dynamic API shutting down.");
                shutdown_handle.shutdown();
                break;
            }

            maybe_err = &mut error_handle => {
                if let Some(e) = maybe_err {
                    return Err(GenericError::from(e));
                }
                break;
            }

            maybe_update = route_assertions.recv() => {
                let Some(update) = maybe_update else {
                    warn!("Route subscription channel closed.");
                    break;
                };

                let mut rebuild_http = false;
                let mut rebuild_grpc = false;

                match update {
                    AssertionUpdate::Asserted(id, route) => {
                        if route.endpoint_type() != endpoint_type {
                            continue;
                        }

                        match route.endpoint_protocol() {
                            EndpointProtocol::Http => {
                                debug!(?id, "Registering dynamic HTTP handler.");
                                http_handlers.insert(id, route.into_router());

                                rebuild_http = true;
                            },
                            EndpointProtocol::Grpc => {
                                debug!(?id, "Registering dynamic gRPC handler.");
                                grpc_handlers.insert(id, route.into_router());

                                rebuild_grpc = true;
                            },
                        }
                    }
                    AssertionUpdate::Retracted(id) => {
                        if http_handlers.swap_remove(&id).is_some() {
                            debug!(?id, "Withdrawing dynamic HTTP handler.");
                            rebuild_http = true;
                        }

                        if grpc_handlers.swap_remove(&id).is_some() {
                            debug!(?id, "Withdrawing dynamic gRPC handler.");
                            rebuild_grpc = true;
                        }
                    }
                }

                if rebuild_http {
                    rebuild_router(&inner_http, &base_http, &http_handlers, http_post_process);
                }

                if rebuild_grpc {
                    rebuild_router(
                        &inner_grpc,
                        &base_grpc,
                        &grpc_handlers,
                        grpc_post_process,
                    );
                }
            }
        }
    }

    Ok(())
}

/// Creates a dynamic router pair: a swappable inner router (seeded with `initial`) and an outer router that delegates
/// to it.
fn create_dynamic_router(initial: Router) -> (Arc<ArcSwap<Router>>, Router) {
    let inner = Arc::new(ArcSwap::from_pointee(initial));
    let outer = Router::new().fallback_service(DynamicRouterService::from_inner(&inner));
    (inner, outer)
}

/// Attempts to merge `other` into `base`, returning the merged router on success.
///
/// `Router::merge` panics when two routers define overlapping routes (same path and HTTP method) and axum exposes no
/// fallible alternative. Since `Router` is opaque -- there is no public API to inspect which paths/methods a router
/// carries -- we can't detect conflicts ahead of time.
///
/// To recover from the panic without losing the accumulated router state, we clone `base` before the merge attempt.
/// The clone is passed into `catch_unwind`: if the merge panics, only the clone is in a partially-mutated state and it
/// is simply dropped. The original `base` remains intact and is returned as-is. `AssertUnwindSafe` is sound here
/// because:
///
/// - The closure captures only the clone (`candidate`) and a clone of `other`. Neither aliases mutable state that
///   outlives the closure.
/// - The panic originates from a deterministic format string in axum's `panic_on_err!` macro -- no locks are held and
///   no resources are leaked in the panic path.
/// - On panic, `candidate` is dropped without further use, so any internal inconsistency is irrelevant.
fn try_merge_router(base: &Router, id: &Identifier, other: &Router) -> Result<Router, String> {
    let candidate = base.clone();
    match catch_unwind(AssertUnwindSafe(|| candidate.merge(other.clone()))) {
        Ok(merged) => Ok(merged),
        Err(payload) => {
            let reason = payload
                .downcast_ref::<String>()
                .map(|s| s.as_str())
                .or_else(|| payload.downcast_ref::<&str>().copied())
                .unwrap_or("unknown");
            Err(format!("failed to merge dynamic handler {id:?}: {reason}"))
        }
    }
}

/// Rebuilds the merged inner router from the static `base` and all currently-registered dynamic handlers, applies
/// `post_process` to the merged router, then stores the result in the [`ArcSwap`].
fn rebuild_router(
    inner_router: &Arc<ArcSwap<Router>>, base: &Router, handlers: &FastIndexMap<Identifier, Router>,
    post_process: fn(Router) -> Router,
) {
    let mut merged = base.clone();
    let mut skipped = 0usize;

    for (id, router) in handlers.iter() {
        let resetable = router.clone().reset_fallback();
        match try_merge_router(&merged, id, &resetable) {
            Ok(new_merged) => merged = new_merged,
            Err(reason) => {
                warn!(%reason, "Skipping dynamic handler due to overlapping route.");
                skipped += 1;
            }
        }
    }

    let merged = post_process(merged);
    inner_router.store(Arc::new(merged));
    debug!(handler_count = handlers.len(), skipped, "Rebuilt inner router.");
}

fn http_post_process(router: Router) -> Router {
    router
}

/// Adds a fallback handler that returns a standard gRPC `UNIMPLEMENTED` response when no other handler matches.
fn grpc_post_process(router: Router) -> Router {
    router.fallback(grpc_unimplemented)
}

async fn grpc_unimplemented() -> Response<AxumBody> {
    tonic::Status::unimplemented("").into_http()
}

#[cfg(test)]
mod tests {
    use std::{net::SocketAddr, time::Duration};

    use async_trait::async_trait;
    use axum::Router;
    use http_body_util::{BodyExt as _, Empty};
    use hyper::{body::Bytes, StatusCode};
    use hyper_util::{client::legacy::Client, rt::TokioExecutor};
    use saluki_api::{APIHandler, DynamicRoute, EndpointType};
    use saluki_core::runtime::{
        state::{AssertionUpdate, DataspaceRegistry, Identifier, IdentifierFilter},
        InitializationError, ProcessShutdown, Supervisable, Supervisor, SupervisorFuture,
    };
    use tokio::{
        sync::{mpsc, oneshot},
        task::JoinHandle,
        time::{sleep, timeout, Instant},
    };

    use super::*;

    struct SimpleHandler {
        path: &'static str,
        body: &'static str,
    }

    impl APIHandler for SimpleHandler {
        type State = ();

        fn generate_initial_state(&self) -> Self::State {}

        fn generate_routes(&self) -> Router<Self::State> {
            let body = self.body;
            Router::new().route(self.path, axum::routing::get(move || async move { body }))
        }
    }

    enum RouteCommand {
        Assert { id: Identifier, route: DynamicRoute },
        Retract { id: Identifier },
    }

    struct RouteAsserter {
        commands_rx: std::sync::Mutex<Option<mpsc::Receiver<RouteCommand>>>,
        addr_tx: std::sync::Mutex<Option<oneshot::Sender<SocketAddr>>>,
        endpoint_type: EndpointType,
    }

    #[async_trait]
    impl Supervisable for RouteAsserter {
        fn name(&self) -> &str {
            "route-asserter"
        }

        async fn initialize(
            &self, mut process_shutdown: ProcessShutdown,
        ) -> Result<SupervisorFuture, InitializationError> {
            let mut commands_rx =
                self.commands_rx
                    .lock()
                    .unwrap()
                    .take()
                    .ok_or_else(|| InitializationError::Failed {
                        source: generic_error!("RouteAsserter can only be initialized once"),
                    })?;
            let addr_tx = self.addr_tx.lock().unwrap().take();
            let endpoint_type = self.endpoint_type;

            Ok(Box::pin(async move {
                let dataspace =
                    DataspaceRegistry::try_current().ok_or_else(|| generic_error!("Dataspace not available."))?;

                // Wait for the DynamicAPIBuilder to assert its bound address.
                let bound_addr_name = match endpoint_type {
                    EndpointType::Unprivileged => "dynamic-unprivileged-api",
                    EndpointType::Privileged => "dynamic-privileged-api",
                };
                let mut addr_sub =
                    dataspace.subscribe::<BoundApiAddress>(IdentifierFilter::exact(Identifier::named(bound_addr_name)));

                let addr = match addr_sub.recv().await {
                    Some(AssertionUpdate::Asserted(_, BoundApiAddress(mut addr))) => {
                        // Convert 0.0.0.0 to 127.0.0.1 so the test client can connect.
                        if addr.ip().is_unspecified() {
                            addr.set_ip(std::net::Ipv4Addr::LOCALHOST.into());
                        }
                        addr
                    }
                    other => return Err(generic_error!("unexpected bound address update: {:?}", other)),
                };

                if let Some(tx) = addr_tx {
                    let _ = tx.send(addr);
                }

                // Process route commands until shutdown.
                let shutdown = process_shutdown.wait_for_shutdown();
                tokio::pin!(shutdown);

                loop {
                    tokio::select! {
                        _ = &mut shutdown => break,
                        cmd = commands_rx.recv() => {
                            let Some(cmd) = cmd else { break };
                            match cmd {
                                RouteCommand::Assert { id, route } => {
                                    dataspace.assert(route, id);
                                }
                                RouteCommand::Retract { id } => {
                                    dataspace.retract::<DynamicRoute>(id);
                                }
                            }
                        }
                    }
                }

                Ok(())
            }))
        }
    }

    struct TestHarness {
        addr: SocketAddr,
        commands: mpsc::Sender<RouteCommand>,
        _shutdown: oneshot::Sender<()>,
        _handle: JoinHandle<()>,
    }

    impl TestHarness {
        async fn assert_route(&self, id: impl Into<Identifier>, route: DynamicRoute) {
            self.commands
                .send(RouteCommand::Assert { id: id.into(), route })
                .await
                .unwrap();
        }

        async fn retract_route(&self, id: impl Into<Identifier>) {
            self.commands
                .send(RouteCommand::Retract { id: id.into() })
                .await
                .unwrap();
        }
    }

    async fn setup_test_harness(endpoint_type: EndpointType) -> TestHarness {
        setup_test_harness_with(endpoint_type, |b| b).await
    }

    async fn setup_test_harness_with<F>(endpoint_type: EndpointType, configure: F) -> TestHarness
    where
        F: FnOnce(DynamicAPIBuilder) -> DynamicAPIBuilder,
    {
        let (commands_tx, commands_rx) = mpsc::channel(16);
        let (addr_tx, addr_rx) = oneshot::channel();

        let api_builder = configure(DynamicAPIBuilder::new(endpoint_type, ListenAddress::any_tcp(0)));
        let route_asserter = RouteAsserter {
            commands_rx: std::sync::Mutex::new(Some(commands_rx)),
            addr_tx: std::sync::Mutex::new(Some(addr_tx)),
            endpoint_type,
        };

        let mut sup = Supervisor::new("test-dynamic-api").unwrap();
        sup.add_worker(api_builder);
        sup.add_worker(route_asserter);

        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let handle = tokio::spawn(async move {
            let _ = sup.run_with_shutdown(shutdown_rx).await;
        });

        let addr = timeout(Duration::from_secs(5), addr_rx)
            .await
            .expect("timed out waiting for bound address")
            .expect("addr channel closed");

        TestHarness {
            addr,
            commands: commands_tx,
            _shutdown: shutdown_tx,
            _handle: handle,
        }
    }

    async fn http_get(addr: SocketAddr, path: &str) -> (StatusCode, String) {
        let client: Client<_, Empty<Bytes>> = Client::builder(TokioExecutor::new()).build_http();
        let uri = format!("http://{}{}", addr, path);
        let resp = client.get(uri.parse().unwrap()).await.unwrap();
        let status = resp.status();
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let body_str = String::from_utf8_lossy(&body).into_owned();
        (status, body_str)
    }

    async fn grpc_post(addr: SocketAddr, path: &str) -> (StatusCode, http::HeaderMap) {
        let client: Client<_, Empty<Bytes>> = Client::builder(TokioExecutor::new()).build_http();
        let uri: hyper::Uri = format!("http://{}{}", addr, path).parse().unwrap();
        let req = hyper::Request::builder()
            .uri(uri)
            .method(hyper::Method::POST)
            .header(hyper::header::CONTENT_TYPE, "application/grpc")
            .body(Empty::<Bytes>::new())
            .unwrap();
        let resp = client.request(req).await.unwrap();
        (resp.status(), resp.headers().clone())
    }

    async fn assert_status_eventually(addr: SocketAddr, path: &str, expected: StatusCode) -> String {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let (status, body) = http_get(addr, path).await;
            if status == expected {
                return body;
            }
            if Instant::now() > deadline {
                panic!("expected {} for {} but got {}", expected, path, status);
            }
            sleep(Duration::from_millis(50)).await;
        }
    }

    // -- Tests ---------------------------------------------------------------------------

    #[tokio::test]
    async fn serves_asserted_http_route() {
        let harness = setup_test_harness(EndpointType::Unprivileged).await;

        let route = DynamicRoute::http(
            EndpointType::Unprivileged,
            SimpleHandler {
                path: "/health",
                body: "ok",
            },
        );
        harness.assert_route("health", route).await;

        let body = assert_status_eventually(harness.addr, "/health", StatusCode::OK).await;
        assert_eq!(body, "ok");
    }

    #[tokio::test]
    async fn returns_404_for_unknown_route() {
        let harness = setup_test_harness(EndpointType::Unprivileged).await;
        let (status, _) = http_get(harness.addr, "/nonexistent").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn route_retraction_removes_route() {
        let harness = setup_test_harness(EndpointType::Unprivileged).await;

        let route = DynamicRoute::http(
            EndpointType::Unprivileged,
            SimpleHandler {
                path: "/temp",
                body: "temporary",
            },
        );
        harness.assert_route("temp", route).await;
        assert_status_eventually(harness.addr, "/temp", StatusCode::OK).await;

        harness.retract_route("temp").await;
        assert_status_eventually(harness.addr, "/temp", StatusCode::NOT_FOUND).await;
    }

    #[tokio::test]
    async fn multiple_routes_independent_lifecycle() {
        let harness = setup_test_harness(EndpointType::Unprivileged).await;

        let route_a = DynamicRoute::http(
            EndpointType::Unprivileged,
            SimpleHandler {
                path: "/a",
                body: "alpha",
            },
        );
        let route_b = DynamicRoute::http(
            EndpointType::Unprivileged,
            SimpleHandler {
                path: "/b",
                body: "bravo",
            },
        );
        harness.assert_route("a", route_a).await;
        harness.assert_route("b", route_b).await;

        assert_status_eventually(harness.addr, "/a", StatusCode::OK).await;
        assert_status_eventually(harness.addr, "/b", StatusCode::OK).await;

        // Retract only /a -- /b should remain.
        harness.retract_route("a").await;
        assert_status_eventually(harness.addr, "/a", StatusCode::NOT_FOUND).await;

        let body = assert_status_eventually(harness.addr, "/b", StatusCode::OK).await;
        assert_eq!(body, "bravo");
    }

    #[tokio::test]
    async fn ignores_routes_for_different_endpoint_type() {
        let harness = setup_test_harness(EndpointType::Unprivileged).await;

        // Assert a Privileged route on an Unprivileged server -- should be ignored.
        let wrong_route = DynamicRoute::http(
            EndpointType::Privileged,
            SimpleHandler {
                path: "/secret",
                body: "secret",
            },
        );
        harness.assert_route("secret", wrong_route).await;

        let (status, _) = http_get(harness.addr, "/secret").await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        // Now assert the same path with the correct endpoint type.
        let right_route = DynamicRoute::http(
            EndpointType::Unprivileged,
            SimpleHandler {
                path: "/secret",
                body: "not secret",
            },
        );
        harness.assert_route("secret-unpriv", right_route).await;

        let body = assert_status_eventually(harness.addr, "/secret", StatusCode::OK).await;
        assert_eq!(body, "not secret");
    }

    #[tokio::test]
    async fn overlapping_routes_do_not_crash_server() {
        let harness = setup_test_harness(EndpointType::Unprivileged).await;

        // Assert a route at /health with identifier "health-1".
        let route_1 = DynamicRoute::http(
            EndpointType::Unprivileged,
            SimpleHandler {
                path: "/health",
                body: "health-1",
            },
        );
        harness.assert_route("health-1", route_1).await;
        let body = assert_status_eventually(harness.addr, "/health", StatusCode::OK).await;
        assert_eq!(body, "health-1");

        // Assert a DIFFERENT identifier with the SAME path/method. Previously this caused a panic
        // in rebuild_router. The server should remain alive with first-writer-wins semantics.
        let route_2 = DynamicRoute::http(
            EndpointType::Unprivileged,
            SimpleHandler {
                path: "/health",
                body: "health-2",
            },
        );
        harness.assert_route("health-2", route_2).await;

        // Give the event loop time to process and rebuild.
        sleep(Duration::from_millis(200)).await;

        // Server is still alive; first handler wins.
        let (status, body) = http_get(harness.addr, "/health").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, "health-1");

        // Non-overlapping routes are unaffected.
        let route_info = DynamicRoute::http(
            EndpointType::Unprivileged,
            SimpleHandler {
                path: "/info",
                body: "info",
            },
        );
        harness.assert_route("info", route_info).await;
        let body = assert_status_eventually(harness.addr, "/info", StatusCode::OK).await;
        assert_eq!(body, "info");

        // Retract the first /health handler -- the previously-skipped second handler should now
        // become active since the conflict no longer exists.
        harness.retract_route("health-1").await;
        let body = assert_status_eventually(harness.addr, "/health", StatusCode::OK).await;
        assert_eq!(body, "health-2");
    }

    #[tokio::test]
    async fn overlapping_route_retraction_then_reassertion() {
        let harness = setup_test_harness(EndpointType::Unprivileged).await;

        // Assert two overlapping handlers.
        let route_a = DynamicRoute::http(
            EndpointType::Unprivileged,
            SimpleHandler {
                path: "/overlap",
                body: "a",
            },
        );
        let route_b = DynamicRoute::http(
            EndpointType::Unprivileged,
            SimpleHandler {
                path: "/overlap",
                body: "b",
            },
        );
        harness.assert_route("ov-a", route_a).await;
        harness.assert_route("ov-b", route_b).await;

        // Server alive; first writer wins.
        let body = assert_status_eventually(harness.addr, "/overlap", StatusCode::OK).await;
        assert_eq!(body, "a");

        // Retract both.
        harness.retract_route("ov-a").await;
        harness.retract_route("ov-b").await;
        assert_status_eventually(harness.addr, "/overlap", StatusCode::NOT_FOUND).await;

        // Re-assert a single handler -- should work cleanly.
        let route_c = DynamicRoute::http(
            EndpointType::Unprivileged,
            SimpleHandler {
                path: "/overlap",
                body: "c",
            },
        );
        harness.assert_route("ov-c", route_c).await;
        let body = assert_status_eventually(harness.addr, "/overlap", StatusCode::OK).await;
        assert_eq!(body, "c");
    }

    #[tokio::test]
    async fn static_handler_served_without_dynamic_routes() {
        let harness = setup_test_harness_with(EndpointType::Unprivileged, |b| {
            b.with_handler(SimpleHandler {
                path: "/static",
                body: "static",
            })
        })
        .await;

        let body = assert_status_eventually(harness.addr, "/static", StatusCode::OK).await;
        assert_eq!(body, "static");
    }

    #[tokio::test]
    async fn static_and_dynamic_routes_coexist() {
        let harness = setup_test_harness_with(EndpointType::Unprivileged, |b| {
            b.with_handler(SimpleHandler {
                path: "/static",
                body: "static",
            })
        })
        .await;

        // Static route is served immediately.
        let body = assert_status_eventually(harness.addr, "/static", StatusCode::OK).await;
        assert_eq!(body, "static");

        // Add a dynamic route on a different path -- both should serve.
        let dynamic_route = DynamicRoute::http(
            EndpointType::Unprivileged,
            SimpleHandler {
                path: "/dynamic",
                body: "dynamic",
            },
        );
        harness.assert_route("dyn", dynamic_route).await;

        let body = assert_status_eventually(harness.addr, "/dynamic", StatusCode::OK).await;
        assert_eq!(body, "dynamic");

        let (status, body) = http_get(harness.addr, "/static").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, "static");

        // Retracting the dynamic route leaves the static route untouched.
        harness.retract_route("dyn").await;
        assert_status_eventually(harness.addr, "/dynamic", StatusCode::NOT_FOUND).await;

        let (status, body) = http_get(harness.addr, "/static").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, "static");
    }

    #[tokio::test]
    async fn unknown_grpc_method_returns_unimplemented() {
        let harness = setup_test_harness(EndpointType::Unprivileged).await;
        let (status, headers) = grpc_post(harness.addr, "/some.Service/Method").await;

        // gRPC errors are reported with HTTP 200 plus a `grpc-status` header. UNIMPLEMENTED is code 12.
        assert_eq!(status, StatusCode::OK);
        let grpc_status = headers.get("grpc-status").and_then(|v| v.to_str().ok());
        assert_eq!(grpc_status, Some("12"));
    }

    #[tokio::test]
    async fn static_route_wins_overlap_with_dynamic() {
        let harness = setup_test_harness_with(EndpointType::Unprivileged, |b| {
            b.with_handler(SimpleHandler {
                path: "/overlap",
                body: "static",
            })
        })
        .await;

        // Static route is served.
        let body = assert_status_eventually(harness.addr, "/overlap", StatusCode::OK).await;
        assert_eq!(body, "static");

        // Asserting a dynamic route at the same path is skipped due to overlap -- static still wins.
        let dynamic_route = DynamicRoute::http(
            EndpointType::Unprivileged,
            SimpleHandler {
                path: "/overlap",
                body: "dynamic",
            },
        );
        harness.assert_route("dyn-overlap", dynamic_route).await;

        sleep(Duration::from_millis(200)).await;

        let (status, body) = http_get(harness.addr, "/overlap").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, "static");
    }
}
