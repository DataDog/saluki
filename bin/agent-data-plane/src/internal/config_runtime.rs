//! Runtime configuration API handler.

use std::sync::Arc;

use agent_data_plane_config::SalukiConfiguration;
use arc_swap::ArcSwap;
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

/// State used for the runtime configuration API handler.
#[derive(Clone)]
pub struct ConfigRuntimeState {
    current: Arc<ArcSwap<SalukiConfiguration>>,
}

/// An API handler for returning the translated runtime configuration.
///
/// This handler exposes a single route -- `/config/runtime` -- that returns the current
/// translated [`SalukiConfiguration`] in its serialized JSON form. It reflects any dynamic updates
/// applied to the configuration since startup.
pub struct ConfigRuntimeAPIHandler {
    state: ConfigRuntimeState,
}

impl ConfigRuntimeAPIHandler {
    fn new(current: Arc<ArcSwap<SalukiConfiguration>>) -> Self {
        Self {
            state: ConfigRuntimeState { current },
        }
    }

    async fn config_handler(State(state): State<ConfigRuntimeState>) -> impl IntoResponse {
        let config = state.current.load();
        match serde_json::to_string(&**config) {
            Ok(body) => (StatusCode::OK, body).into_response(),
            Err(e) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to serialize configuration: {}", e),
            )
                .into_response(),
        }
    }
}

impl APIHandler for ConfigRuntimeAPIHandler {
    type State = ConfigRuntimeState;

    fn generate_initial_state(&self) -> Self::State {
        self.state.clone()
    }

    fn generate_routes(&self) -> Router<Self::State> {
        Router::new().route("/config/runtime", get(Self::config_handler))
    }
}

/// A worker for exposing an endpoint that returns the translated runtime configuration.
///
/// When running, the worker asserts a route (based on [`ConfigRuntimeAPIHandler`]) that returns
/// the current translated configuration. As the configuration may contain sensitive data, this
/// route is only present on the privileged API endpoint.
pub struct ConfigRuntimeWorker {
    handler: ConfigRuntimeAPIHandler,
}

impl ConfigRuntimeWorker {
    /// Creates a new [`ConfigRuntimeWorker`] serving the given configuration.
    pub fn new(current: Arc<ArcSwap<SalukiConfiguration>>) -> Self {
        Self {
            handler: ConfigRuntimeAPIHandler::new(current),
        }
    }
}

#[async_trait]
impl Supervisable for ConfigRuntimeWorker {
    fn name(&self) -> &str {
        "config-runtime-api"
    }

    async fn initialize(&self, process_shutdown: ShutdownHandle) -> Result<SupervisorFuture, InitializationError> {
        let config_route = DynamicRoute::http(EndpointType::Privileged, &self.handler);

        Ok(Box::pin(async move {
            let dataspace =
                DataspaceRegistry::try_current().ok_or_else(|| generic_error!("Dataspace not available."))?;

            dataspace.assert(config_route, "config-runtime-api");

            process_shutdown.await;
            Ok(())
        }))
    }
}
