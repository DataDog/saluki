use std::{num::NonZeroUsize, time::Duration};

use async_trait::async_trait;
use memory_accounting::{ComponentRegistry, MemoryLimiter};
use saluki_components::{destinations::PrometheusConfiguration, sources::InternalMetricsConfiguration};
use saluki_core::{
    runtime::{
        InitializationError, ProcessShutdown, RestartStrategy, RuntimeConfiguration, Supervisable, Supervisor,
        SupervisorFuture,
    },
    topology::TopologyBlueprint,
};
use saluki_error::{generic_error, GenericError};
use saluki_health::HealthRegistry;
use tracing::info;

use crate::{components::remapper::AgentTelemetryRemapperConfiguration, config::DataPlaneConfiguration};

// SAFETY: This value is clearly non-zero.
const DEFAULT_INTERCONNECT_CAPACITY: NonZeroUsize = NonZeroUsize::new(4).unwrap();

/// A worker that runs the internal telemetry topology.
///
/// This topology collects internal metrics and exposes them via a Prometheus scrape endpoint.
pub struct InternalTelemetryWorker {
    dp_config: DataPlaneConfiguration,
    component_registry: ComponentRegistry,
    health_registry: HealthRegistry,
}

impl InternalTelemetryWorker {
    /// Creates a new `InternalTelemetryWorker`.
    pub fn new(
        dp_config: DataPlaneConfiguration, component_registry: ComponentRegistry, health_registry: HealthRegistry,
    ) -> Self {
        Self {
            dp_config,
            component_registry,
            health_registry,
        }
    }
}

#[async_trait]
impl Supervisable for InternalTelemetryWorker {
    fn name(&self) -> &str {
        "internal-telemetry"
    }

    async fn initialize(&self, mut process_shutdown: ProcessShutdown) -> Result<SupervisorFuture, InitializationError> {
        // Build the internal telemetry topology blueprint
        let int_metrics_config = InternalMetricsConfiguration;
        let int_metrics_remap_config = AgentTelemetryRemapperConfiguration::new();
        let prometheus_config =
            PrometheusConfiguration::from_listen_address(self.dp_config.telemetry_listen_addr().clone());

        info!(
            "Internal telemetry enabled. Spawning Prometheus scrape endpoint on {}.",
            self.dp_config.telemetry_listen_addr()
        );

        let mut blueprint = TopologyBlueprint::new("internal", &self.component_registry);
        blueprint.with_interconnect_capacity(DEFAULT_INTERCONNECT_CAPACITY);
        blueprint
            .add_source("internal_metrics_in", int_metrics_config)
            .map_err(|e| InitializationError::Failed { source: e })?
            .add_transform("internal_metrics_remap", int_metrics_remap_config)
            .map_err(|e| InitializationError::Failed { source: e })?
            .add_destination("internal_metrics_out", prometheus_config)
            .map_err(|e| InitializationError::Failed { source: e })?
            .connect_component("internal_metrics_remap", ["internal_metrics_in"])
            .map_err(|e| InitializationError::Failed { source: e })?
            .connect_component("internal_metrics_out", ["internal_metrics_remap"])
            .map_err(|e| InitializationError::Failed { source: e })?;

        // Build and spawn the topology
        let built_topology = blueprint
            .build()
            .await
            .map_err(|e| InitializationError::Failed { source: e })?;
        let mut running_topology = built_topology
            .spawn(&self.health_registry, MemoryLimiter::noop())
            .await
            .map_err(|e| InitializationError::Failed { source: e })?;

        Ok(Box::pin(async move {
            tokio::select! {
                _ = running_topology.wait_for_unexpected_finish() => {
                    Err(generic_error!("Internal telemetry topology finished unexpectedly."))
                }
                _ = process_shutdown.wait_for_shutdown() => {
                    running_topology
                        .shutdown_with_timeout(Duration::from_secs(5))
                        .await
                }
            }
        }))
    }
}

/// Creates the observability supervisor.
///
/// This supervisor manages the internal telemetry topology (Prometheus metrics endpoint).
/// It runs on a dedicated single-threaded runtime.
///
/// Returns `None` if telemetry is not enabled.
///
/// # Errors
///
/// If the supervisor cannot be created, an error is returned.
pub fn create_observability_supervisor(
    dp_config: &DataPlaneConfiguration, component_registry: &ComponentRegistry, health_registry: HealthRegistry,
) -> Result<Option<Supervisor>, GenericError> {
    if !dp_config.telemetry_enabled() {
        info!("Internal telemetry disabled. Skipping observability supervisor.");
        return Ok(None);
    }

    let mut supervisor = Supervisor::new("int-o11y")?
        .with_dedicated_runtime(RuntimeConfiguration::single_threaded())
        .with_restart_strategy(RestartStrategy::one_to_one());

    // Get a scoped component registry for internal observability
    let scoped_registry = component_registry.get_or_create("internal-observability");

    supervisor.add_worker(InternalTelemetryWorker::new(
        dp_config.clone(),
        scoped_registry,
        health_registry,
    ));

    Ok(Some(supervisor))
}
