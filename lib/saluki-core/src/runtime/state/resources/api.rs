use saluki_api::{
    extract::State,
    response::IntoResponse,
    routing::{get, Router},
    APIHandler, Json,
};

use super::ResourceRegistry;

/// State used for the resource registry API handler.
#[derive(Clone)]
pub struct ResourceRegistryState {
    registry: ResourceRegistry,
}

/// An API handler for reporting the state of all registered resources.
///
/// This handler exposes a single route -- `/resources/status` -- returning a JSON array describing every registered
/// resource group: its key and kind, how many instances it holds, how many are currently lent out, and which subsystem
/// and process hold them.
pub struct ResourceRegistryAPIHandler {
    state: ResourceRegistryState,
}

impl ResourceRegistryAPIHandler {
    pub(super) fn from_registry(registry: ResourceRegistry) -> Self {
        Self {
            state: ResourceRegistryState { registry },
        }
    }

    async fn status_handler(State(state): State<ResourceRegistryState>) -> impl IntoResponse {
        Json(state.registry.snapshot())
    }
}

impl APIHandler for ResourceRegistryAPIHandler {
    type State = ResourceRegistryState;

    fn generate_initial_state(&self) -> Self::State {
        self.state.clone()
    }

    fn generate_routes(&self) -> Router<Self::State> {
        Router::new().route("/resources/status", get(Self::status_handler))
    }
}
