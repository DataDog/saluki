use std::num::NonZeroUsize;

use memory_accounting::{ComponentRegistry, MemoryLimiter};
use saluki_components::{destinations::PrometheusConfiguration, sources::InternalMetricsConfiguration};
use saluki_config::GenericConfiguration;
use saluki_core::topology::{RunningTopology, TopologyBlueprint};
use saluki_error::GenericError;
use saluki_health::HealthRegistry;
use tracing::{info, warn};

use crate::{components::remapper::AgentTelemetryRemapperConfiguration, internal::initialize_and_launch_runtime};

// SAFETY: These are obviously all non-zero.
const INTERNAL_TELEMETRY_EVENT_BUFFER_SIZE: NonZeroUsize = unsafe { NonZeroUsize::new_unchecked(768) };
const INTERNAL_TELEMETRY_EVENT_BUFFER_POOL_SIZE_MIN: NonZeroUsize = unsafe { NonZeroUsize::new_unchecked(1) };
const INTERNAL_TELEMETRY_EVENT_BUFFER_POOL_SIZE_MAX: NonZeroUsize = unsafe { NonZeroUsize::new_unchecked(4) };

pub fn spawn_internal_observability_topology(
    config: &GenericConfiguration, component_registry: &ComponentRegistry, health_registry: HealthRegistry,
) -> Result<(), GenericError> {
    // When telemetry is enabled, we need to collect internal metrics, so add those components and route them here.
    let telemetry_enabled = config.get_typed_or_default::<bool>("telemetry_enabled");
    if !telemetry_enabled {
        info!("Internal telemetry disabled. Skipping internal observability topology.");
        return Ok(());
    }

    // Build the internal telemetry topology.
    let int_metrics_config = InternalMetricsConfiguration;
    let int_metrics_remap_config = AgentTelemetryRemapperConfiguration::new();
    let prometheus_config = PrometheusConfiguration::from_configuration(config)?;

    info!(
        "Internal telemetry enabled. Spawning Prometheus scrape endpoint on {}.",
        prometheus_config.listen_address()
    );

    let mut blueprint = TopologyBlueprint::new("internal", component_registry);
    blueprint
        // We use a custom sized global event buffer pool since we send a fixed number of events on a fixed schedule,
        // and without lowering the size of the pool, we'd both needlessly pre-allocate buffers _and_ have a very large
        // firm bound for this topology.
        .with_global_event_buffer_pool_size(
            INTERNAL_TELEMETRY_EVENT_BUFFER_SIZE,
            INTERNAL_TELEMETRY_EVENT_BUFFER_POOL_SIZE_MIN,
            INTERNAL_TELEMETRY_EVENT_BUFFER_POOL_SIZE_MAX,
        )
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
