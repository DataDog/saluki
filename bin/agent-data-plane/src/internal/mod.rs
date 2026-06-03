use resource_accounting::ComponentRegistry;
use saluki_app::logging::LoggingOverrideController;
use saluki_config::GenericConfiguration;
use saluki_core::health::HealthRegistry;
use saluki_core::runtime::Supervisor;
use saluki_error::GenericError;

use crate::config::DataPlaneConfiguration;

mod control_plane;
pub use self::control_plane::{create_control_plane_supervisor, DogStatsDControlSurface};

pub mod env;

pub mod logging;

pub mod remote_agent;
use self::remote_agent::RemoteAgentBootstrap;

mod telemetry;

/// Creates the root internal supervisor containing control plane and environment subsystems.
///
/// The internal supervisor manages:
/// - **Control plane**: Health registry, unprivileged and privileged APIs (including the
///   `/metrics` and `/compat/metrics` telemetry routes), remote agent registration, and the rest
///   of ADP's internal HTTP/gRPC surface.
///
/// Each subsystem runs on its own dedicated single-threaded runtime for isolation.
///
/// # Errors
///
/// If the supervisor can't be created, an error is returned.
pub async fn create_internal_supervisor(
    config: &GenericConfiguration, dp_config: &DataPlaneConfiguration, component_registry: &ComponentRegistry,
    health_registry: HealthRegistry, dsd_control_surface: Option<DogStatsDControlSurface>,
    ra_bootstrap: Option<RemoteAgentBootstrap>, logging_controller: LoggingOverrideController,
) -> Result<Supervisor, GenericError> {
    // The root supervisor runs in ambient mode (caller's runtime) since its children each have their own
    // dedicated runtimes. The default restart strategy (one-for-one, 1 restart per 5s) applies to the child
    // supervisors as units.
    let mut root = Supervisor::new("internal-sup")?;

    // Add control plane supervisor (dedicated single-threaded runtime)
    root.add_worker(
        create_control_plane_supervisor(
            config,
            dp_config,
            component_registry,
            health_registry.clone(),
            dsd_control_surface,
            ra_bootstrap,
            logging_controller,
        )
        .await?,
    );

    Ok(root)
}
