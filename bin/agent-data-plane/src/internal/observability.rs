use std::num::NonZeroUsize;

use memory_accounting::{ComponentRegistry, MemoryLimiter};
use saluki_components::{destinations::PrometheusConfiguration, sources::InternalMetricsConfiguration};
use saluki_core::topology::{RunningTopology, TopologyBlueprint};
use saluki_error::GenericError;
use saluki_health::HealthRegistry;
use tracing::{info, warn};

use crate::{
    components::remapper::AgentTelemetryRemapperConfiguration, config::DataPlaneConfiguration,
    internal::initialize_and_launch_runtime,
};

// SAFETY: This value is clearly non-zero.
const DEFAULT_INTERCONNECT_CAPACITY: NonZeroUsize = NonZeroUsize::new(4).unwrap();

pub fn spawn_internal_observability_topology(
    dp_config: &DataPlaneConfiguration, component_registry: &ComponentRegistry, health_registry: HealthRegistry,
) -> Result<(), GenericError> {
    // When telemetry is enabled, we need to collect internal metrics, so add those components and route them here.
    if !dp_config.telemetry_enabled() {
        info!("Internal telemetry disabled. Skipping internal observability topology.");
        return Ok(());
    }

    // Build the internal telemetry topology.
    let int_metrics_config = InternalMetricsConfiguration;
    let int_metrics_remap_config = AgentTelemetryRemapperConfiguration::new();
    let prometheus_config = PrometheusConfiguration::from_listen_address(dp_config.telemetry_listen_addr().clone());

    info!(
        "Internal telemetry enabled. Spawning Prometheus scrape endpoint on {}.",
        dp_config.telemetry_listen_addr()
    );

    let mut blueprint = TopologyBlueprint::new("internal", component_registry);
    blueprint.with_interconnect_capacity(DEFAULT_INTERCONNECT_CAPACITY);
    blueprint
        .add_source("internal_metrics_in", int_metrics_config)?
        .add_transform("internal_metrics_remap", int_metrics_remap_config)?
        .add_destination("internal_metrics_out", prometheus_config)?
        .connect_component("internal_metrics_remap", ["internal_metrics_in"])?
        .connect_component("internal_metrics_out", ["internal_metrics_remap"])?;

    let init = async move {
        let built_topology = blueprint.build().await?;
        built_topology.spawn(&health_registry, MemoryLimiter::noop()).await
    };

    let main = |mut topology: RunningTopology| async move {
        topology.wait_for_unexpected_finish().await;
        warn!("Internal telemetry topology finished unexpectedly.");
    };

    initialize_and_launch_runtime("rt-internal-telemetry", init, main)
}
