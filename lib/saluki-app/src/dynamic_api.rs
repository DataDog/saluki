//! Dynamic API server.
//!
//! Unlike [`APIBuilder`][crate::api::APIBuilder], which constructs its route set once at build time,
//! `DynamicAPIBuilder` subscribes to runtime notifications via the dataspace registry and acquires route resources from
//! the resource registry, hot-swapping the inner router behind an [`ArcSwap`] as handlers are asserted or retracted.

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
use saluki_api::{APIEndpointInterest, EndpointType};
use saluki_common::collections::FastIndexMap;
use saluki_core::runtime::{
    state::{AssertionUpdate, DataspaceRegistry, Handle, ResourceRegistry, Subscription},
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
use tonic::service::RoutesBuilder;
use tower::Service;
use tracing::{debug, info, warn};

/// A dynamic API server that can add and remove route sets at runtime.
///
/// `DynamicAPIBuilder` serves HTTP on a given address using an outer [`Router`] whose fallback delegates every request
/// to an inner [`Router`] stored behind an [`ArcSwap`]. A background event loop subscribes to the [`DataspaceRegistry`]
/// for [`APIEndpointInterest`] assertions and retractions, acquires `Router<()>` resources from the
/// [`ResourceRegistry`], and atomically swaps the inner router as handlers are added or removed.
///
/// ## Publisher protocol
///
/// Any worker that wants to dynamically register API routes must:
///
/// 1. Build a `Router<()>` with state applied (i.e. call `handler.generate_routes().with_state(handler.generate_initial_state())`)
/// 2. Publish the `Router<()>` to the [`ResourceRegistry`] under a [`Handle`]
/// 3. Assert an [`APIEndpointInterest`] in the [`DataspaceRegistry`] under the same [`Handle`]
///
/// The resource **must** be published before the assertion. If the assertion arrives before the resource, the
/// registration will be silently skipped with a warning.
///
/// To withdraw routes, retract the [`APIEndpointInterest`] from the [`DataspaceRegistry`].
pub struct DynamicAPIBuilder {
    endpoint_type: EndpointType,
    listen_address: ListenAddress,
    tls_config: Option<ServerConfig>,
    dataspace_registry: DataspaceRegistry,
    resource_registry: ResourceRegistry,
}

impl DynamicAPIBuilder {
    /// Creates a new `DynamicAPIBuilder` for the given endpoint type.
    pub fn new(
        endpoint_type: EndpointType, listen_address: ListenAddress, dataspace_registry: DataspaceRegistry,
        resource_registry: ResourceRegistry,
    ) -> Self {
        Self {
            endpoint_type,
            listen_address,
            tls_config: None,
            dataspace_registry,
            resource_registry,
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
        // Create our default, empty router.
        let inner_router = Arc::new(ArcSwap::from_pointee(Router::new()));
        let outer_router = Router::new().fallback_service(DynamicRouterService::from_inner(&inner_router));

        // Register our interest in all asserted API endpoints.
        let endpoint_subscriptions = self.dataspace_registry.subscribe_all::<APIEndpointInterest>();

        // Bind the HTTP listener immediately so we fail fast on bind errors.
        let listener = ConnectionOrientedListener::from_listen_address(self.listen_address.clone())
            .await
            .map_err(|e| InitializationError::Failed { source: e.into() })?;

        // Wrap the outer router in the same MultiplexService + TowerToHyperService pattern used by APIBuilder::serve.
        let multiplexed_service = TowerToHyperService::new(MultiplexService::new(
            outer_router,
            RoutesBuilder::default().routes().into_axum_router(),
        ));

        let mut http_server = HttpServer::from_listener(listener, multiplexed_service);
        if let Some(tls_config) = self.tls_config.clone() {
            http_server = http_server.with_tls_config(tls_config);
        }
        let (shutdown_handle, error_handle) = http_server.listen();

        let resource_registry = self.resource_registry.clone();
        let endpoint_type = self.endpoint_type.clone();
        let listen_address = self.listen_address.clone();

        Ok(Box::pin(async move {
            info!(
                "Serving dynamic {} API on {}.",
                endpoint_name(&endpoint_type),
                listen_address
            );

            run_event_loop(
                inner_router,
                endpoint_subscriptions,
                resource_registry,
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

/// Runs the event loop that listens for API endpoint interest assertions/retractions and hot-swaps the inner router.
async fn run_event_loop(
    inner_router: Arc<ArcSwap<Router>>, mut subscription: Subscription<APIEndpointInterest>,
    resource_registry: ResourceRegistry, endpoint_type: EndpointType, mut process_shutdown: ProcessShutdown,
    shutdown_handle: ShutdownHandle, error_handle: ErrorHandle,
) -> Result<(), GenericError> {
    let mut registered_handlers: FastIndexMap<Handle, Router> = FastIndexMap::default();

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

            maybe_update = subscription.recv() => {
                let Some(update) = maybe_update else {
                    warn!("API endpoint interest channel closed.");
                    break;
                };

                match update {
                    AssertionUpdate::Asserted(handle, interest) => {
                        // Only process interests targeting our endpoint type.
                        if interest.endpoint != endpoint_type {
                            continue;
                        }

                        // Acquire the Router<()> from the resource registry (non-blocking).
                        // The publisher must have published the resource before asserting the interest.
                        match resource_registry.try_acquire::<Router>(handle) {
                            Some(guard) => {
                                let router: Router = (*guard).clone();
                                drop(guard);

                                info!(
                                    handle = ?handle,
                                    "Registering dynamic API handler.",
                                );
                                registered_handlers.insert(handle, router);
                                rebuild_router(&inner_router, &registered_handlers);
                            }
                            None => {
                                warn!(
                                    handle = ?handle,
                                    "Received assertion but router resource not found in registry. Ignoring.",
                                );
                            }
                        }
                    }
                    AssertionUpdate::Retracted(handle) => {
                        if registered_handlers.swap_remove(&handle).is_some() {
                            info!(
                                handle = ?handle,
                                "Withdrawing dynamic API handler.",
                            );
                            rebuild_router(&inner_router, &registered_handlers);
                        }
                    }
                }
            }
        }
    }

    Ok(())
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

fn endpoint_name(endpoint_type: &EndpointType) -> &'static str {
    match endpoint_type {
        EndpointType::Unprivileged => "unprivileged",
        EndpointType::Privileged => "privileged",
    }
}
