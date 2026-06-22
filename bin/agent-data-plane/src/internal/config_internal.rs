//! Internal configuration API handler.
//!
//! Serves the ADP-native [`SalukiConfiguration`] as JSON on the privileged `/config/internal`
//! route. This is the observable surface used to verify translation end-to-end, and a secondary
//! operator-debugging aid alongside the source-shaped `/config` route.
//!
//! The handler holds the shared [`ConfigurationSystem`] and reads the translated master per request,
//! so it automatically reflects any later re-translation without changing this worker. The body is
//! served raw, without scrubbing, exactly like the existing privileged `/config` route; scrubbing
//! is a client-side display concern.
//!
//! [`SalukiConfiguration`]: agent-data-plane-config

use std::sync::Arc;

use agent_data_plane_config_system::ConfigurationSystem;
use async_trait::async_trait;
use http::StatusCode;
use saluki_api::{
    extract::State,
    response::IntoResponse,
    routing::{get, Router},
    APIHandler, DynamicRoute, EndpointType,
};
use saluki_common::sync::shutdown::ShutdownHandle;
use saluki_core::runtime::{state::DataspaceRegistry, InitializationError, Supervisable, SupervisorFuture};
use saluki_error::generic_error;

/// State for the internal configuration API handler.
#[derive(Clone)]
pub struct InternalConfigState {
    system: Arc<ConfigurationSystem>,
}

/// An API handler for returning the ADP-native configuration.
///
/// Exposes a single route -- `/config/internal` -- that serializes the translated
/// `SalukiConfiguration` to JSON.
pub struct InternalConfigAPIHandler {
    state: InternalConfigState,
}

impl InternalConfigAPIHandler {
    fn new(system: Arc<ConfigurationSystem>) -> Self {
        Self {
            state: InternalConfigState { system },
        }
    }

    async fn config_handler(State(state): State<InternalConfigState>) -> impl IntoResponse {
        match serde_json::to_string(&state.system.saluki()) {
            Ok(body) => (StatusCode::OK, body).into_response(),
            Err(e) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to serialize configuration: {}", e),
            )
                .into_response(),
        }
    }
}

impl APIHandler for InternalConfigAPIHandler {
    type State = InternalConfigState;

    fn generate_initial_state(&self) -> Self::State {
        self.state.clone()
    }

    fn generate_routes(&self) -> Router<Self::State> {
        Router::new().route("/config/internal", get(Self::config_handler))
    }
}

/// A worker for exposing the ADP-native configuration.
///
/// Asserts the `/config/internal` route on the privileged API endpoint. As the configuration may
/// contain sensitive data, the route is only present on the privileged endpoint.
pub struct InternalConfigWorker {
    handler: InternalConfigAPIHandler,
}

impl InternalConfigWorker {
    /// Creates a new [`InternalConfigWorker`] backed by the shared configuration system.
    pub fn new(system: Arc<ConfigurationSystem>) -> Self {
        Self {
            handler: InternalConfigAPIHandler::new(system),
        }
    }
}

#[async_trait]
impl Supervisable for InternalConfigWorker {
    fn name(&self) -> &str {
        "config-internal-api"
    }

    async fn initialize(&self, process_shutdown: ShutdownHandle) -> Result<SupervisorFuture, InitializationError> {
        let config_route = DynamicRoute::http(EndpointType::Privileged, &self.handler);

        Ok(Box::pin(async move {
            DataspaceRegistry::try_current()
                .ok_or_else(|| generic_error!("Dataspace not available."))?
                .assert(config_route, "config-internal-api");

            process_shutdown.await;
            Ok(())
        }))
    }
}
