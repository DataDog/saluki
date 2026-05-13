//! Supervision tree introspection API.
//!
//! Materializes a live view of the running supervision tree from the [`DataspaceRegistry`] and serves it via the
//! unprivileged HTTP API.
//!
//! # Architecture
//!
//! Snapshot collection and HTTP serving are split across two collaborating types:
//!
//! - [`SupervisionIntrospectionWorker`] is a [`Supervisable`] that subscribes to all [`ProcessSnapshot`] assertions
//!   and maintains a resolved `Vec<ProcessSnapshot>` reflecting the current set. The vector lives behind a
//!   [`tokio::sync::Mutex`] shared with the API handler.
//! - [`SupervisionAPIHandler`] reads from the shared snapshot when serving `GET /supervision/tree`. It has no direct
//!   access to the dataspace.
//!
//! This split keeps the dataspace as an implementation detail of the worker, while the handler stays a thin reader
//! over a well-defined shared state.

use std::{collections::HashMap, sync::Arc};

use async_trait::async_trait;
use saluki_api::{
    extract::State,
    response::IntoResponse,
    routing::{get, Router},
    APIHandler, DynamicRoute, EndpointType, StatusCode,
};
use saluki_core::runtime::{
    introspection::ProcessSnapshot,
    state::{AssertionUpdate, DataspaceRegistry, Identifier, IdentifierFilter},
    InitializationError, ProcessShutdown, Supervisable, SupervisorFuture,
};
use saluki_error::generic_error;
use serde::Serialize;
use tokio::{pin, select, sync::Mutex};
use tracing::{debug, warn};

/// Shared snapshot of the supervision tree.
///
/// Wrapped in an [`Arc`] so the worker and the handler can both hold references; wrapped in a
/// [`tokio::sync::Mutex`] so updates by the worker and reads by the handler stay coherent across `.await` points.
type SharedSnapshots = Arc<Mutex<Vec<ProcessSnapshot>>>;

/// State for the supervision API handler.
#[derive(Clone)]
pub struct SupervisionHandlerState {
    snapshots: SharedSnapshots,
}

/// JSON response payload for the supervision tree endpoint.
#[derive(Serialize)]
struct TreeResponse<'a> {
    processes: &'a [ProcessSnapshot],
}

/// HTTP API handler that returns the resolved supervision tree.
///
/// Exposes a single route, `GET /supervision/tree`, which returns a JSON object containing the flat list of
/// [`ProcessSnapshot`]s most recently materialized by [`SupervisionIntrospectionWorker`]. The handler does not access
/// the [`DataspaceRegistry`] directly; it reads from the shared state owned by the worker.
pub struct SupervisionAPIHandler {
    state: SupervisionHandlerState,
}

impl SupervisionAPIHandler {
    fn new(snapshots: SharedSnapshots) -> Self {
        Self {
            state: SupervisionHandlerState { snapshots },
        }
    }

    async fn tree_handler(State(state): State<SupervisionHandlerState>) -> impl IntoResponse {
        let guard = state.snapshots.lock().await;
        let response = TreeResponse { processes: &guard };
        match serde_json::to_string(&response) {
            Ok(body) => (StatusCode::OK, [("content-type", "application/json")], body),
            Err(e) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                [("content-type", "text/plain")],
                format!("failed to serialize supervision tree: {}", e),
            ),
        }
    }
}

impl APIHandler for SupervisionAPIHandler {
    type State = SupervisionHandlerState;

    fn generate_initial_state(&self) -> Self::State {
        self.state.clone()
    }

    fn generate_routes(&self) -> Router<Self::State> {
        Router::new().route("/supervision/tree", get(Self::tree_handler))
    }
}

/// A worker that materializes the running supervision tree from the dataspace.
///
/// At initialization, the worker:
///
/// 1. Subscribes to every [`ProcessSnapshot`] assertion (and retraction) in the [`DataspaceRegistry`].
/// 2. Asserts a [`SupervisionAPIHandler`] route on the unprivileged API endpoint.
///
/// While running, it processes assertion/retraction updates and refreshes the shared snapshot used by the handler.
/// When the worker shuts down, the route assertion is automatically retracted along with the process.
pub struct SupervisionIntrospectionWorker {
    snapshots: SharedSnapshots,
}

impl SupervisionIntrospectionWorker {
    /// Creates a new `SupervisionIntrospectionWorker` with an empty initial snapshot.
    pub fn new() -> Self {
        Self {
            snapshots: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn api_handler(&self) -> SupervisionAPIHandler {
        SupervisionAPIHandler::new(Arc::clone(&self.snapshots))
    }
}

impl Default for SupervisionIntrospectionWorker {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Supervisable for SupervisionIntrospectionWorker {
    fn name(&self) -> &str {
        "supervision-introspection"
    }

    async fn initialize(&self, mut process_shutdown: ProcessShutdown) -> Result<SupervisorFuture, InitializationError> {
        let dataspace = DataspaceRegistry::try_current().ok_or_else(|| InitializationError::Failed {
            source: generic_error!("Dataspace not available."),
        })?;
        let routes = DynamicRoute::http(EndpointType::Unprivileged, self.api_handler());
        let shared_snapshots = Arc::clone(&self.snapshots);

        Ok(Box::pin(async move {
            debug!("Asserting supervision tree API route.");
            dataspace.assert(routes, "supervision-api");

            let mut subscription = dataspace.subscribe::<ProcessSnapshot>(IdentifierFilter::All);
            let mut current: HashMap<Identifier, ProcessSnapshot> = HashMap::new();

            let shutdown = process_shutdown.wait_for_shutdown();
            pin!(shutdown);

            loop {
                select! {
                    _ = &mut shutdown => {
                        debug!("Supervision introspection worker shutting down.");
                        break;
                    }
                    maybe_update = subscription.recv() => {
                        let Some(update) = maybe_update else {
                            warn!("Supervision snapshot subscription channel closed.");
                            break;
                        };

                        match update {
                            AssertionUpdate::Asserted(id, snapshot) => {
                                current.insert(id, snapshot);
                            }
                            AssertionUpdate::Retracted(id) => {
                                current.remove(&id);
                            }
                        }

                        let materialized: Vec<ProcessSnapshot> = current.values().cloned().collect();
                        let mut guard = shared_snapshots.lock().await;
                        *guard = materialized;
                    }
                }
            }

            Ok(())
        }))
    }
}
