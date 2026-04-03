use async_trait::async_trait;
use saluki_api::{DynamicRoute, EndpointType};

use super::HealthRegistry;
use crate::runtime::{
    state::{DataspaceRegistry, Handle},
    InitializationError, ProcessShutdown, Supervisable, SupervisorFuture,
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

    async fn initialize(&self, process_shutdown: ProcessShutdown) -> Result<SupervisorFuture, InitializationError> {
        let runner = self.health_registry.clone().into_runner()?;

        let health_routes = DynamicRoute::http(EndpointType::Unprivileged, self.health_registry.api_handler());

        Ok(Box::pin(async move {
            // Publish our health endpoint route and retract it after the registry task completes,
            // regardless of whether it failed or not.
            let dataspace_registry = DataspaceRegistry::global();
            dataspace_registry.assert(health_routes, Handle::current_process());

            runner.run(process_shutdown).await;

            // TODO: OK, yeah, this is clunky. We should just make it a drop guard.
            dataspace_registry.retract::<DynamicRoute>(Handle::current_process());

            Ok(())
        }))
    }
}
