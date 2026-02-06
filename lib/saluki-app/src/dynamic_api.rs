//! Dynamic API server.
//!
//! Unlike [`APIBuilder`][crate::api::APIBuilder], which constructs its route set once at build time,
//! `DynamicAPIBuilder` subscribes to runtime notifications via the pub/sub registry and acquires route resources from
//! the resource registry, hot-swapping the inner router behind an [`ArcSwap`] as handlers are registered or withdrawn.

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
use saluki_api::{APIEndpointInterest, EndpointType, InterestType};
use saluki_common::collections::FastIndexMap;
use saluki_core::runtime::{
    state::{PubSubRegistry, ResourceRegistry, Subscription},
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

/// The well-known pub/sub topic for API endpoint interest notifications.
const API_ENDPOINT_INTEREST_TOPIC: &str = "api-endpoint-interest";

/// A dynamic API server that can add and remove route sets at runtime.
///
/// `DynamicAPIBuilder` serves HTTP on a given address using an outer [`Router`] whose fallback delegates every request
/// to an inner [`Router`] stored behind an [`ArcSwap`]. A background event loop subscribes to the [`PubSubRegistry`]
/// for [`APIEndpointInterest`] notifications, acquires `Router<()>` resources from the [`ResourceRegistry`], and
/// atomically swaps the inner router as handlers register or withdraw.
///
/// ## Publisher protocol
///
/// Any worker that wants to dynamically register API routes must:
///
/// 1. Build a `Router<()>` with state applied (i.e. call `handler.generate_routes().with_state(handler.generate_initial_state())`)
/// 2. Publish the `Router<()>` to the [`ResourceRegistry`] under a string identifier
/// 3. Publish an [`APIEndpointInterest`] to the [`PubSubRegistry`] on the `"api-endpoint-interest"` topic
///
/// The resource **must** be published before the interest. If the interest arrives before the resource, the
/// registration will be silently skipped with a warning.
pub struct DynamicAPIBuilder {
    endpoint_type: EndpointType,
    listen_address: ListenAddress,
    tls_config: Option<ServerConfig>,
    pubsub_registry: PubSubRegistry,
    resource_registry: ResourceRegistry,
}

impl DynamicAPIBuilder {
    /// Creates a new `DynamicAPIBuilder` for the given endpoint type.
    pub fn new(
        endpoint_type: EndpointType, listen_address: ListenAddress, pubsub_registry: PubSubRegistry,
        resource_registry: ResourceRegistry,
    ) -> Self {
        Self {
            endpoint_type,
            listen_address,
            tls_config: None,
            pubsub_registry,
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
        // Create the ArcSwap holding the initial (empty) inner router.
        let inner_router: Arc<ArcSwap<Router>> = Arc::new(ArcSwap::from_pointee(Router::new()));

        // Build the outer "shell" router whose fallback delegates to whatever is currently in the ArcSwap.
        let outer_router = build_outer_router(Arc::clone(&inner_router));

        // Subscribe to the pub/sub topic for APIEndpointInterest notifications.
        let subscription: Subscription<APIEndpointInterest> =
            self.pubsub_registry.subscribe(API_ENDPOINT_INTEREST_TOPIC);

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
                subscription,
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

/// Builds the outer shell router with a fallback service that delegates to the current inner router.
fn build_outer_router(inner: Arc<ArcSwap<Router>>) -> Router {
    Router::new().fallback_service(ArcSwapRouterService { inner })
}

/// A [`tower::Service`] that reads the current [`Router`] from an [`ArcSwap`] on every request.
///
/// This is used as the fallback service for the outer shell router, allowing the inner route set to be hot-swapped at
/// runtime without restarting the HTTP listener.
#[derive(Clone)]
struct ArcSwapRouterService {
    inner: Arc<ArcSwap<Router>>,
}

impl Service<http::Request<AxumBody>> for ArcSwapRouterService {
    type Response = Response<AxumBody>;
    type Error = Infallible;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: http::Request<AxumBody>) -> Self::Future {
        // ArcSwap::load_full() is wait-free; Router::clone() is cheap (Arc internals).
        let router = self.inner.load_full();
        Box::pin(async move {
            let mut svc = (*router).clone();
            // Router<()> implements Service<Request<AxumBody>, Response = Response<AxumBody>, Error = Infallible>.
            svc.call(request).await
        })
    }
}

/// Runs the event loop that listens for API endpoint interest notifications and hot-swaps the inner router.
async fn run_event_loop(
    inner_router: Arc<ArcSwap<Router>>, mut subscription: Subscription<APIEndpointInterest>,
    resource_registry: ResourceRegistry, endpoint_type: EndpointType, mut process_shutdown: ProcessShutdown,
    shutdown_handle: ShutdownHandle, error_handle: ErrorHandle,
) -> Result<(), GenericError> {
    let mut registered_handlers: FastIndexMap<String, Router> = FastIndexMap::default();

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

            maybe_interest = subscription.recv() => {
                let Some(interest) = maybe_interest else {
                    warn!("API endpoint interest channel closed.");
                    break;
                };

                // Only process interests targeting our endpoint type.
                if interest.endpoint != endpoint_type {
                    continue;
                }

                match interest.interest {
                    InterestType::Register => {
                        // Acquire the Router<()> from the resource registry (non-blocking).
                        // The publisher must have published the resource before sending the interest.
                        match resource_registry.try_acquire::<Router>(interest.identifier.as_str()) {
                            Some(guard) => {
                                let router: Router = (*guard).clone();
                                drop(guard);

                                info!(
                                    identifier = %interest.identifier,
                                    "Registering dynamic API handler.",
                                );
                                registered_handlers.insert(interest.identifier, router);
                                rebuild_router(&inner_router, &registered_handlers);
                            }
                            None => {
                                warn!(
                                    identifier = %interest.identifier,
                                    "Received Register interest but router resource not found in registry. Ignoring.",
                                );
                            }
                        }
                    }
                    InterestType::Withdraw => {
                        if registered_handlers.swap_remove(&interest.identifier).is_some() {
                            info!(
                                identifier = %interest.identifier,
                                "Withdrawing dynamic API handler.",
                            );
                            rebuild_router(&inner_router, &registered_handlers);
                        } else {
                            warn!(
                                identifier = %interest.identifier,
                                "Received Withdraw interest but no handler was registered with this identifier.",
                            );
                        }
                    }
                }
            }
        }
    }

    Ok(())
}

/// Rebuilds the merged inner router from all currently-registered handlers and stores it in the [`ArcSwap`].
fn rebuild_router(inner_router: &Arc<ArcSwap<Router>>, handlers: &FastIndexMap<String, Router>) {
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
