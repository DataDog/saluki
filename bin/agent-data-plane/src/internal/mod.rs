use agent_data_plane_config::ConfigViews;
use resource_accounting::ComponentRegistry;
use saluki_app::logging::LoggingOverrideController;
use saluki_component_config::ScopedConfig;
use saluki_core::health::HealthRegistry;
use saluki_core::runtime::Supervisor;
use saluki_error::GenericError;

use crate::config::DataPlaneConfiguration;

mod config_views;
mod control_plane;
pub use self::control_plane::create_control_plane_supervisor;

mod control_surfaces;
pub use self::control_surfaces::{DogStatsDControlSurface, TopologyControlSurfaces};

pub mod env;

pub mod logging;

pub mod remote_agent;
use self::remote_agent::RemoteAgentBootstrap;

mod telemetry;

/// Creates the root internal supervisor containing control plane and environment subsystems.
///
/// # Errors
///
/// If the supervisor can't be created, an error is returned.
pub async fn create_internal_supervisor(
    dp_config: &DataPlaneConfiguration, component_registry: &ComponentRegistry, health_registry: HealthRegistry,
    control_surfaces: TopologyControlSurfaces, ra_bootstrap: Option<RemoteAgentBootstrap>, config_views: ConfigViews,
    log_level: ScopedConfig<Option<String>>, logging_controller: LoggingOverrideController,
) -> Result<Supervisor, GenericError> {
    let mut root = Supervisor::new("internal-sup")?;

    root.add_worker(
        create_control_plane_supervisor(
            dp_config,
            component_registry,
            health_registry.clone(),
            control_surfaces,
            ra_bootstrap,
            config_views,
            log_level,
            logging_controller,
        )
        .await?,
    );

    Ok(root)
}
