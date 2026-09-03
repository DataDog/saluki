use saluki_api::{
    extract::State,
    response::IntoResponse,
    routing::{get, Router},
    APIHandler, Json,
};

use super::{SupervisionTreeHandle, SUPERVISION_TREE_ROUTE};

/// State used for the supervision tree API handler.
#[derive(Clone)]
pub struct SupervisionTreeState {
    tree: SupervisionTreeHandle,
}

/// An API handler for reporting the state of a supervision tree.
///
/// This handler exposes a single route -- `/runtime/processes` -- returning every process in the tree: its name,
/// process identifier, restart policy, restart count, lifetime, and resource usage, with each process's children
/// nested beneath it.
pub struct SupervisionTreeAPIHandler {
    state: SupervisionTreeState,
}

impl SupervisionTreeAPIHandler {
    pub(super) fn from_handle(tree: SupervisionTreeHandle) -> Self {
        Self {
            state: SupervisionTreeState { tree },
        }
    }

    async fn processes_handler(State(state): State<SupervisionTreeState>) -> impl IntoResponse {
        Json(state.tree.snapshot())
    }
}

impl APIHandler for SupervisionTreeAPIHandler {
    type State = SupervisionTreeState;

    fn generate_initial_state(&self) -> Self::State {
        self.state.clone()
    }

    fn generate_routes(&self) -> Router<Self::State> {
        Router::new().route(SUPERVISION_TREE_ROUTE, get(Self::processes_handler))
    }
}
