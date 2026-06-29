use async_trait::async_trait;
use saluki_api::{DynamicRoute, EndpointType};
use saluki_common::sync::shutdown::ShutdownHandle;
use saluki_error::generic_error;

use super::HealthRegistry;
use crate::{
    diagnostic::DiagnosticHandle,
    runtime::{state::DataspaceRegistry, InitializationError, Supervisable, SupervisorFuture},
};

/// A worker that runs the health registry.
///
/// This is the only way to run the health registry's liveness probing event loop. The worker
/// implements [`Supervisable`], so it should be added to a [`Supervisor`][crate::runtime::Supervisor]
/// to be managed as part of a supervision tree.
pub struct HealthRegistryWorker {
    health_registry: HealthRegistry,
}

impl HealthRegistryWorker {
    pub(super) fn new(health_registry: HealthRegistry) -> Self {
        Self { health_registry }
    }
}

#[async_trait]
impl Supervisable for HealthRegistryWorker {
    fn name(&self) -> &str {
        "health-registry"
    }

    async fn initialize(&self, process_shutdown: ShutdownHandle) -> Result<SupervisorFuture, InitializationError> {
        let runner = self.health_registry.clone().into_runner()?;

        let health_routes = DynamicRoute::http(EndpointType::Unprivileged, self.health_registry.api_handler());

        let health_registry = self.health_registry.clone();
        let flare_handle =
            DiagnosticHandle::new("health.json", move || health_registry.snapshot_json().into_bytes());

        Ok(Box::pin(async move {
            let dataspace =
                DataspaceRegistry::try_current().ok_or_else(|| generic_error!("Dataspace not available."))?;

            // Register our API routes and flare diagnostic handle before we actually start running.
            dataspace.assert(health_routes, "health-registry-api");
            dataspace.assert(flare_handle, "diag-health");

            // We pass the shutdown handle into the runner here, instead of our usual `select! { shutdown => ...,
            // main_loop_future => ... }` pattern because we try to ensure that we give back the liveness receiver
            // before the runner completes.
            //
            // TODO: We should actually use something like a proper mutex guard so that returning the receiver happens
            // automatically when the runner future goes out of scope and is dropped, since right now we wouldn't be
            // able to ensure the current behavior (returning the receiver before the runner completes) happens in the
            // face of an exceptional error.
            runner.run(process_shutdown).await;

            Ok(())
        }))
    }
}
