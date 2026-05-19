use async_trait::async_trait;
use saluki_api::{DynamicRoute, EndpointType};
use saluki_error::generic_error;

use super::HealthRegistry;
use crate::runtime::{state::DataspaceRegistry, InitializationError, ProcessShutdown, Supervisable, SupervisorFuture};

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
            // Register our API routes before we actually start running.
            DataspaceRegistry::try_current()
                .ok_or_else(|| generic_error!("Dataspace not available."))?
                .assert(health_routes, "health-registry-api");

            runner.run(process_shutdown).await;

            Ok(())
        }))
    }
}
