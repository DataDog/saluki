use async_trait::async_trait;
use saluki_api::{DynamicRoute, EndpointType};
use saluki_common::sync::shutdown::ShutdownHandle;
use saluki_error::generic_error;

use super::SupervisionTreeHandle;
use crate::{
    diagnostic::DiagnosticsEmitter,
    runtime::{state::DataspaceRegistry, InitializationError, Supervisable, SupervisorFuture},
    support::SubsystemIdentifier,
};

/// A worker that exposes a supervision tree over the control plane.
///
/// A tree needs no event loop of its own -- a snapshot is only assembled when one is asked for -- so this worker
/// exists purely to publish it: it asserts the tree API route as a [`DynamicRoute`] and registers a diagnostic
/// artifact, then idles until shutdown. Both registrations are retracted when the worker's process exits.
///
/// The route is asserted on the **privileged** endpoint. A snapshot names every process in the tree and reports its
/// memory and CPU accounting, which is more than should be readable without authentication.
///
/// The worker is itself supervised, and so appears in the tree it reports on.
pub struct SupervisionTreeWorker {
    tree: SupervisionTreeHandle,
}

impl SupervisionTreeWorker {
    pub(super) fn new(tree: SupervisionTreeHandle) -> Self {
        Self { tree }
    }
}

#[async_trait]
impl Supervisable for SupervisionTreeWorker {
    fn name(&self) -> &str {
        "supervision-tree"
    }

    async fn initialize(&self, process_shutdown: ShutdownHandle) -> Result<SupervisorFuture, InitializationError> {
        let tree_routes = DynamicRoute::http(EndpointType::Privileged, self.tree.api_handler());

        let tree = self.tree.clone();

        Ok(Box::pin(async move {
            let dataspace =
                DataspaceRegistry::try_current().ok_or_else(|| generic_error!("Dataspace not available."))?;

            // Register our API route before we actually start running.
            dataspace.assert(tree_routes, "supervision-tree-api");

            // Expose our diagnostic artifact via the diagnostics control surface.
            let diagnostics =
                DiagnosticsEmitter::from_dataspace(SubsystemIdentifier::from_segments(["supervision-tree"]), dataspace);
            diagnostics.register_collector("supervision_tree.json", move || tree.snapshot_json());

            process_shutdown.await;

            Ok(())
        }))
    }
}
