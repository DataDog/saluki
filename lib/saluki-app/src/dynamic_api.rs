//! Dynamic API server.
//!
//! Unlike [`APIBuilder`][crate::api::APIBuilder], which constructs its route set once at build time,
//! `DynamicAPIBuilder` subscribes to runtime notifications via the dataspace registry and hot-swaps the inner HTTP and
//! gRPC routers behind [`ArcSwap`]s as routes are asserted or retracted.

use std::{
    convert::Infallible,
    future::Future,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use arc_swap::ArcSwap;
use async_trait::async_trait;
use axum::{body::Body as AxumBody, Router};
use http::Response;
use rcgen::{generate_simple_self_signed, CertifiedKey};
use rustls::{pki_types::PrivateKeyDer, ServerConfig};
use rustls_pki_types::PrivatePkcs8KeyDer;
use saluki_api::{DynamicRoute, EndpointProtocol, EndpointType};
use saluki_common::collections::FastIndexMap;
use saluki_core::runtime::{
    state::{AssertionUpdate, DataspaceRegistry, Handle, Subscription},
    InitializationError, ProcessShutdown, Supervisable, SupervisorFuture,
};
use saluki_error::GenericError;
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
use tower::Service;
use tracing::{debug, info, warn};

/// A dynamic API server that can add and remove routes at runtime.
///
/// `DynamicAPIBuilder` serves HTTP and gRPC on a given address, multiplexing both protocols on a single port. A
/// background event loop subscribes to the [`DataspaceRegistry`] for [`DynamicHttpRoute`] and [`DynamicGrpcRoute`]
/// assertions and retractions, and atomically swaps the inner routers as handlers are added or removed.
///
/// ## Publisher protocol
///
/// Any process that wants to dynamically register API routes must:
///
/// 1. Build a `Router<()>` (for HTTP, or via `tonic::Routes::into_axum_router()` for gRPC).
/// 2. Assert a [`DynamicHttpRoute`] or [`DynamicGrpcRoute`] in the [`DataspaceRegistry`] under a [`Handle`].
///
/// To withdraw routes, retract the assertion from the [`DataspaceRegistry`].
pub struct DynamicAPIBuilder {
    endpoint_type: EndpointType,
    listen_address: ListenAddress,
    tls_config: Option<ServerConfig>,
    dataspace_registry: DataspaceRegistry,
}

impl DynamicAPIBuilder {
    /// Creates a new `DynamicAPIBuilder` for the given endpoint type.
    pub fn new(
        endpoint_type: EndpointType, listen_address: ListenAddress, dataspace_registry: DataspaceRegistry,
    ) -> Self {
        Self {
            endpoint_type,
            listen_address,
            tls_config: None,
            dataspace_registry,
        }
    }

    /// Sets the TLS configuration for the server.
    pub fn with_tls_config(mut self, config: ServerConfig) -> Self {
        self.tls_config = Some(config);
        self
    }

    /// Sets the TLS configuration for the server based on a dynamically generated self-signed certificate.
    pub fn with_self_signed_tls(self) -> Self {
        let CertifiedKey { cert, key_pair } = generate_simple_self_signed(["localhost".to_owned()]).unwrap();
        let cert_chain = vec![cert.der().clone()];
        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key_pair.serialize_der()));

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
        // Create dynamic inner routers for both HTTP and gRPC sides.
        let (inner_http, outer_http) = create_dynamic_router();
        let (inner_grpc, outer_grpc) = create_dynamic_router();

        // Subscribe to all dynamic route assertions.
        let route_assertions = self.dataspace_registry.subscribe_all::<DynamicRoute>();

        // Bind the HTTP listener immediately so we fail fast on bind errors.
        let listener = ConnectionOrientedListener::from_listen_address(self.listen_address.clone())
            .await
            .map_err(|e| InitializationError::Failed { source: e.into() })?;

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
async fn run_event_loop(
    inner_http: Arc<ArcSwap<Router>>, inner_grpc: Arc<ArcSwap<Router>>,
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
                    AssertionUpdate::Asserted(handle, route) => {
                        if route.endpoint_type() != endpoint_type {
                            continue;
                        }

                        match route.endpoint_protocol() {
                            EndpointProtocol::Http => {
                                debug!(?handle, "Registering dynamic HTTP handler.");
                                http_handlers.insert(handle, route.into_router());

                                rebuild_http = true;
                            },
                            EndpointProtocol::Grpc => {
                                debug!(?handle, "Registering dynamic gRPC handler.");
                                grpc_handlers.insert(handle, route.into_router());

                                rebuild_grpc = true;
                            },
                        }
                    }
                    AssertionUpdate::Retracted(handle) => {
                        if http_handlers.swap_remove(&handle).is_some() {
                            debug!(?handle, "Withdrawing dynamic HTTP handler.");
                            rebuild_http = true;
                        }

                        if grpc_handlers.swap_remove(&handle).is_some() {
                            debug!(?handle, "Withdrawing dynamic gRPC handler.");
                            rebuild_grpc = true;
                        }
                    }
                }

                if rebuild_http {
                    rebuild_router(&inner_http, &http_handlers);
                }

                if rebuild_grpc {
                    rebuild_router(&inner_grpc, &grpc_handlers);
                }
            }
        }
    }

    Ok(())
}

/// Creates a dynamic router pair: a swappable inner router and an outer router that delegates to it.
fn create_dynamic_router() -> (Arc<ArcSwap<Router>>, Router) {
    let inner = Arc::new(ArcSwap::from_pointee(Router::new()));
    let outer = Router::new().fallback_service(DynamicRouterService::from_inner(&inner));
    (inner, outer)
}

/// Rebuilds the merged inner router from all currently-registered handlers and stores it in the [`ArcSwap`].
fn rebuild_router(inner_router: &Arc<ArcSwap<Router>>, handlers: &FastIndexMap<Handle, Router>) {
    let mut merged = Router::new();
    for router in handlers.values() {
        merged = merged.merge(router.clone());
    }
    inner_router.store(Arc::new(merged));
    debug!(handler_count = handlers.len(), "Rebuilt inner router.");
}
