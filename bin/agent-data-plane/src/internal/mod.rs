use memory_accounting::ComponentRegistry;
use saluki_components::destinations::DogStatsDStatisticsConfiguration;
use saluki_config::GenericConfiguration;
use saluki_core::runtime::Supervisor;
use saluki_error::GenericError;
use saluki_health::HealthRegistry;

mod control_plane;
pub use self::control_plane::create_control_plane_supervisor;

mod observability;
pub use self::observability::create_observability_supervisor;

pub mod platform;

pub mod remote_agent;
use self::remote_agent::RemoteAgentBootstrap;
use crate::{config::DataPlaneConfiguration, env_provider::ADPEnvironmentProvider};

/// Creates the root internal supervisor containing control plane and observability subsystems.
///
/// The internal supervisor manages:
/// - **Control plane**: Health registry, unprivileged and privileged APIs, remote agent registration
/// - **Observability**: Internal telemetry topology (Prometheus metrics endpoint)
///
/// Each subsystem runs on its own dedicated single-threaded runtime for isolation.
///
/// # Errors
///
/// If the supervisor cannot be created, an error is returned.
pub fn create_internal_supervisor(
    config: &GenericConfiguration, dp_config: &DataPlaneConfiguration, component_registry: &ComponentRegistry,
    health_registry: HealthRegistry, env_provider: ADPEnvironmentProvider,
    dsd_stats_config: DogStatsDStatisticsConfiguration, ra_bootstrap: Option<RemoteAgentBootstrap>,
) -> Result<Supervisor, GenericError> {
    let mut root = Supervisor::new("internal-sup")?;

    // Add control plane supervisor (dedicated single-threaded runtime)
    root.add_worker(create_control_plane_supervisor(
        config,
        dp_config,
        component_registry,
        health_registry.clone(),
        env_provider,
        dsd_stats_config,
        ra_bootstrap,
    )?);

    // Add observability supervisor if telemetry is enabled (dedicated single-threaded runtime)
    if let Some(observability_sup) = create_observability_supervisor(dp_config, component_registry, health_registry)? {
        root.add_worker(observability_sup);
    }

    Ok(root)
}
