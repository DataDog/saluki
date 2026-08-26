use async_trait::async_trait;
use saluki_api::{DynamicRoute, EndpointType};
use saluki_common::sync::shutdown::ShutdownHandle;
use saluki_error::generic_error;
use tracing::warn;

use super::ResourceRegistry;
use crate::{
    diagnostic::DiagnosticsEmitter,
    runtime::{state::DataspaceRegistry, InitializationError, Supervisable, SupervisorFuture},
    support::SubsystemIdentifier,
};

/// A worker that exposes the resource registry over the control plane.
///
/// The registry itself needs no event loop -- it only does work when a resource is acquired or returned -- so this
/// worker exists purely to publish it: it asserts the resource API routes as a [`DynamicRoute`] on the unprivileged
/// endpoint and registers a diagnostic artifact, then idles until shutdown. Both registrations are retracted when the
/// worker's process exits.
pub struct ResourceRegistryWorker {
    resource_registry: ResourceRegistry,
}

impl ResourceRegistryWorker {
    pub(super) fn new(resource_registry: ResourceRegistry) -> Self {
        Self { resource_registry }
    }
}

#[async_trait]
impl Supervisable for ResourceRegistryWorker {
    fn name(&self) -> &str {
        "resource-registry"
    }

    async fn initialize(&self, process_shutdown: ShutdownHandle) -> Result<SupervisorFuture, InitializationError> {
        let resource_routes = DynamicRoute::http(EndpointType::Unprivileged, self.resource_registry.api_handler());

        let resource_registry = self.resource_registry.clone();

        Ok(Box::pin(async move {
            let dataspace =
                DataspaceRegistry::try_current().ok_or_else(|| generic_error!("Dataspace not available."))?;

            // Register our API routes before we actually start running.
            dataspace.assert(resource_routes, "resource-registry-api");

            // Expose our diagnostic artifact via the diagnostics control surface.
            let diagnostics = DiagnosticsEmitter::from_dataspace(
                SubsystemIdentifier::from_segments(["resource-registry"]),
                dataspace,
            );
            diagnostics.register_collector("resources.json", move || {
                let snapshot = resource_registry.snapshot();
                let snapshot_pretty = match serde_json::to_string_pretty(&snapshot) {
                    Ok(json) => json,
                    Err(e) => {
                        warn!(error = %e, "Failed to serialize resource registry snapshot during diagnostics collection.");
                        String::from(r#"{"error": "failed to serialize"}"#)
                    },
                };

                snapshot_pretty
            });

            process_shutdown.await;

            Ok(())
        }))
    }
}
