//! Simple http endpoints ported from the go trace agent for backward compatibility
use axum::{response::IntoResponse, routing::get, Router};
use saluki_api::{APIHandler, DynamicRoute, EndpointType};
use saluki_common::sync::shutdown::ShutdownHandle;
use saluki_core::runtime::{state::DataspaceRegistry, InitializationError, Supervisable, SupervisorFuture};
use saluki_error::generic_error;
use tonic::async_trait;

struct ServicesEndpointsAPIHandler {}

impl ServicesEndpointsAPIHandler {
    fn new() -> Self {
        Self {}
    }

    async fn services_handler() -> impl IntoResponse {
        "OK"
    }
}

impl APIHandler for ServicesEndpointsAPIHandler {
    type State = ();

    fn generate_initial_state(&self) -> Self::State {}

    fn generate_routes(&self) -> axum::Router<Self::State> {
        Router::new()
            .route("/services", get(Self::services_handler))
            .route("/v0.1/services", get(Self::services_handler))
            .route("/v0.2/services", get(Self::services_handler))
            .route("/v0.3/services", get(Self::services_handler))
            .route("/v0.4/services", get(Self::services_handler))
    }
}

/// Answers "OK" to http calls to /services and /v0.{1,2,3,4}/services
pub struct ServicesEndpointsWorker {}

impl ServicesEndpointsWorker {
    /// Creates a new [`ServicesEndpointsWorker`].
    pub fn new() -> Self {
        Self {}
    }
}

#[async_trait]
impl Supervisable for ServicesEndpointsWorker {
    fn name(&self) -> &str {
        "services-endpoints"
    }

    async fn initialize(&self, process_shutdown: ShutdownHandle) -> Result<SupervisorFuture, InitializationError> {
        let handler = ServicesEndpointsAPIHandler::new();
        let route = DynamicRoute::http(EndpointType::Unprivileged, &handler);

        Ok(Box::pin(async move {
            DataspaceRegistry::try_current()
                .ok_or_else(|| generic_error!("Dataspace not available."))?
                .assert(route, "services-endpoints-api");

            process_shutdown.await;
            Ok(())
        }))
    }
}
