use agent_data_plane_config::ControlConfiguration;
use agent_data_plane_config_system::{ConfigViewProducer, EnvConfig};
use resource_accounting::ComponentRegistry;
use saluki_app::logging::LoggingOverrideController;
use saluki_component_config::ScopedConfig;
use saluki_core::health::HealthRegistry;
use saluki_core::runtime::Supervisor;
use saluki_error::GenericError;

mod control_plane;
pub use self::control_plane::create_control_plane_supervisor;

mod control_surfaces;
pub use self::control_surfaces::{DogStatsDControlSurface, TopologyControlSurfaces};

pub mod env;

pub mod logging;

pub mod remote_agent;
use self::remote_agent::RemoteAgentServices;

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
#[allow(clippy::too_many_arguments)]
pub async fn create_internal_supervisor(
    control: &ControlConfiguration, env_config: &EnvConfig, component_registry: &ComponentRegistry,
    health_registry: HealthRegistry, control_surfaces: TopologyControlSurfaces,
    remote_agent_services: Option<RemoteAgentServices>, logging_controller: LoggingOverrideController,
    log_level: ScopedConfig<String>, view_producer: ConfigViewProducer,
) -> Result<Supervisor, GenericError> {
    // The root supervisor runs in ambient mode (caller's runtime) since its children each have their own
    // dedicated runtimes. The default restart strategy (one-for-one, 1 restart per 5s) applies to the child
    // supervisors as units.
    let mut root = Supervisor::new("internal-sup")?;

    // Add control plane supervisor (dedicated single-threaded runtime)
    root.add_worker(
        create_control_plane_supervisor(
            control,
            env_config,
            component_registry,
            health_registry.clone(),
            control_surfaces,
            remote_agent_services,
            logging_controller,
            log_level,
            view_producer,
        )
        .await?,
    );

    Ok(root)
}
