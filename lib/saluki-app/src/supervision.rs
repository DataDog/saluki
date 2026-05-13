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

use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};

use async_trait::async_trait;
use memory_accounting::allocator::{AllocationGroupRegistry, AllocationStatsSnapshot};
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
use tokio::{pin, select, sync::Mutex, time::sleep_until};
use tracing::{debug, warn};

/// How often to refresh the materialized snapshot when no dataspace updates have arrived. Keeps `live_bytes` fresh
/// in the absence of structural changes to the supervision tree.
const MEMORY_REFRESH_INTERVAL: Duration = Duration::from_secs(1);

/// Visits the [`AllocationGroupRegistry`] and returns a map of `process_name -> live_bytes` for every group.
///
/// The key matches the `name` field on [`ProcessSnapshot`]: supervisors and workers each register their group under
/// their fully-scoped process name. Groups without a corresponding process snapshot (such as the implicit `root`
/// group) are still included; they just won't be looked up.
fn build_live_bytes_map() -> HashMap<String, u64> {
    let mut out = HashMap::new();
    AllocationGroupRegistry::global().visit_allocation_groups(|name, stats| {
        let snapshot = stats.snapshot_delta(&AllocationStatsSnapshot::empty());
        out.insert(name.to_string(), snapshot.live_bytes() as u64);
    });
    out
}

/// Materializes the current set of snapshots into the shared state, enriching each with live memory usage from the
/// allocation group registry.
async fn materialize(current: &HashMap<Identifier, ProcessSnapshot>, shared: &SharedSnapshots) {
    let started = Instant::now();
    let live_bytes_map = build_live_bytes_map();

    let materialized: Vec<ProcessSnapshot> = current
        .values()
        .map(|snapshot| {
            let mut snapshot = snapshot.clone();
            snapshot.live_bytes = live_bytes_map.get(snapshot.name.as_str()).copied();
            snapshot
        })
        .collect();

    let mut guard = shared.lock().await;
    *guard = materialized;
    drop(guard);

    let elapsed = started.elapsed();
    if elapsed > Duration::from_millis(50) {
        warn!(
            elapsed_ms = elapsed.as_millis(),
            "Supervision snapshot materialization took longer than expected."
        );
    }
}

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

            // Tick on a fixed interval to keep `live_bytes` fresh even when the tree structure itself isn't
            // changing. `sleep_until` lets us reset the deadline whenever we materialize for another reason.
            let mut next_refresh = tokio::time::Instant::now() + MEMORY_REFRESH_INTERVAL;
            let refresh_timer = sleep_until(next_refresh);
            pin!(refresh_timer);

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

                        materialize(&current, &shared_snapshots).await;

                        // Push out the periodic refresh -- we just refreshed.
                        next_refresh = tokio::time::Instant::now() + MEMORY_REFRESH_INTERVAL;
                        refresh_timer.as_mut().reset(next_refresh);
                    }
                    _ = &mut refresh_timer => {
                        materialize(&current, &shared_snapshots).await;

                        next_refresh = tokio::time::Instant::now() + MEMORY_REFRESH_INTERVAL;
                        refresh_timer.as_mut().reset(next_refresh);
                    }
                }
            }

            Ok(())
        }))
    }
}

#[cfg(test)]
mod tests {
    use memory_accounting::allocator::AllocationGroupRegistry;
    use saluki_core::runtime::introspection::{ProcessKind, ProcessStatus, ShutdownStrategyKind};

    use super::*;

    fn sample_snapshot(name: &str) -> ProcessSnapshot {
        ProcessSnapshot {
            id: 1,
            parent_id: None,
            name: name.to_string(),
            kind: ProcessKind::Worker,
            status: ProcessStatus::Running,
            started_at: std::time::SystemTime::UNIX_EPOCH,
            restart_count: 0,
            last_restarted_at: None,
            last_failure: None,
            shutdown_strategy: ShutdownStrategyKind::Graceful { timeout_ms: 5000 },
            restart_strategy: None,
            runtime_mode: None,
            live_bytes: None,
        }
    }

    #[tokio::test]
    async fn materialize_populates_live_bytes_from_registered_group() {
        // Register a group under a unique name so the materialization can find it. The tracking allocator may not be
        // installed in the test process; in that case `live_bytes` is `Some(0)` rather than missing, since the group
        // exists in the registry. Either way the lookup must succeed.
        let group_name = "supervision_test_materialize_populates_live_bytes_from_registered_group";
        let _token = AllocationGroupRegistry::global().register_allocation_group(group_name);

        let mut current = HashMap::new();
        current.insert(Identifier::named("dummy"), sample_snapshot(group_name));
        let shared: SharedSnapshots = Arc::new(Mutex::new(Vec::new()));

        materialize(&current, &shared).await;

        let guard = shared.lock().await;
        assert_eq!(guard.len(), 1);
        assert!(
            guard[0].live_bytes.is_some(),
            "live_bytes should be populated when a matching allocation group exists"
        );
    }

    #[tokio::test]
    async fn materialize_leaves_live_bytes_none_when_no_matching_group() {
        let mut current = HashMap::new();
        current.insert(
            Identifier::named("dummy"),
            sample_snapshot("supervision_test_no_such_group_xyz_42"),
        );
        let shared: SharedSnapshots = Arc::new(Mutex::new(Vec::new()));

        materialize(&current, &shared).await;

        let guard = shared.lock().await;
        assert_eq!(guard.len(), 1);
        assert!(
            guard[0].live_bytes.is_none(),
            "live_bytes should be None when no matching allocation group is found"
        );
    }
}
