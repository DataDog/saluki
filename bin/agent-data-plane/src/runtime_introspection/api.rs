//! API handler serving a snapshot of the supervision tree.

use async_trait::async_trait;
use http::StatusCode;
use saluki_api::{
    extract::State,
    response::IntoResponse,
    routing::{get, Router},
    APIHandler, DynamicRoute, EndpointType,
};
use saluki_common::sync::shutdown::ShutdownHandle;
use saluki_core::{
    diagnostic::DiagnosticsEmitter,
    runtime::{state::DataspaceRegistry, InitializationError, Supervisable, SupervisionTreeHandle, SupervisorFuture},
    support::SubsystemIdentifier,
};
use saluki_error::generic_error;

use super::RUNTIME_PROCESSES_ROUTE;

/// Name this worker is known by, in the supervision tree and in the artifacts it publishes.
const WORKER_NAME: &str = "runtime-processes-api";

/// State used for the supervision tree API handler.
#[derive(Clone)]
pub struct RuntimeProcessesState {
    tree: SupervisionTreeHandle,
}

impl RuntimeProcessesState {
    /// Renders the current supervision tree as JSON.
    ///
    /// Returns an error message in place of the tree if it can't be serialized, so that a caller always gets
    /// something it can act on.
    fn tree_json(&self) -> Result<String, String> {
        serde_json::to_string(&self.tree.snapshot()).map_err(|e| format!("Failed to serialize supervision tree: {}", e))
    }
}

/// An API handler for returning a snapshot of the supervision tree.
///
/// Exposes a single route -- [`RUNTIME_PROCESSES_ROUTE`] -- returning every process in the tree: its name, process
/// identifier, restart policy, restart count, lifetime, and resource usage, with each process's children nested
/// beneath it.
pub struct RuntimeProcessesAPIHandler {
    state: RuntimeProcessesState,
}

impl RuntimeProcessesAPIHandler {
    fn new(tree: SupervisionTreeHandle) -> Self {
        Self {
            state: RuntimeProcessesState { tree },
        }
    }

    async fn processes_handler(State(state): State<RuntimeProcessesState>) -> impl IntoResponse {
        match state.tree_json() {
            Ok(body) => (StatusCode::OK, body).into_response(),
            Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
        }
    }
}

impl APIHandler for RuntimeProcessesAPIHandler {
    type State = RuntimeProcessesState;

    fn generate_initial_state(&self) -> Self::State {
        self.state.clone()
    }

    fn generate_routes(&self) -> Router<Self::State> {
        Router::new().route(RUNTIME_PROCESSES_ROUTE, get(Self::processes_handler))
    }
}

/// A worker exposing an endpoint that returns a snapshot of the supervision tree.
///
/// The route is only present on the privileged API endpoint. A snapshot names every process in the tree and reports
/// its memory and CPU accounting, which is more than should be readable without authentication, and it matches how
/// every other state dump (`/config`, `/config/runtime`, the workload dumps) is exposed.
///
/// The same snapshot is also registered as a flare artifact, so a support flare carries the state of the tree at the
/// moment it was taken.
pub struct RuntimeProcessesWorker {
    handler: RuntimeProcessesAPIHandler,
}

impl RuntimeProcessesWorker {
    /// Creates a new [`RuntimeProcessesWorker`] reporting on the tree behind the given handle.
    ///
    /// The handle should be taken from the root supervisor, since a snapshot only covers the subtree beneath whatever
    /// supervisor it was taken from.
    pub fn new(tree: SupervisionTreeHandle) -> Self {
        Self {
            handler: RuntimeProcessesAPIHandler::new(tree),
        }
    }
}

#[async_trait]
impl Supervisable for RuntimeProcessesWorker {
    fn name(&self) -> &str {
        WORKER_NAME
    }

    async fn initialize(&self, process_shutdown: ShutdownHandle) -> Result<SupervisorFuture, InitializationError> {
        let processes_route = DynamicRoute::http(EndpointType::Privileged, &self.handler);
        let diagnostics_state = self.handler.state.clone();

        Ok(Box::pin(async move {
            let dataspace =
                DataspaceRegistry::try_current().ok_or_else(|| generic_error!("Dataspace not available."))?;

            dataspace.assert(processes_route, WORKER_NAME);

            let diagnostics =
                DiagnosticsEmitter::from_dataspace(SubsystemIdentifier::from_segments([WORKER_NAME]), dataspace);
            diagnostics.register_collector("supervision_tree.json", move || {
                diagnostics_state.tree_json().unwrap_or_else(|e| e).into_bytes()
            });

            process_shutdown.await;
            Ok(())
        }))
    }
}
