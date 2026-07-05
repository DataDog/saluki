use agent_data_plane_config_system::ConfigurationSystem;
use datadog_agent_commons::ipc::{config::IpcAuthConfiguration, tls::build_ipc_server_tls_config};
use resource_accounting::ComponentRegistry;
use saluki_api::EndpointType;
use saluki_app::{
    accounting::ResourceTelemetryWorker, config::ConfigWorker, dynamic_api::DynamicAPIBuilder,
    logging::LoggingOverrideController,
};
use saluki_config::GenericConfiguration;
use saluki_core::{
    health::HealthRegistry,
    runtime::{RestartStrategy, RuntimeConfiguration, Supervisor},
};
use saluki_error::GenericError;

use crate::{
    config::DataPlaneConfiguration,
    internal::{
        config_internal::ConfigInternalWorker, logging::DynamicLogLevelWorker, remote_agent::RemoteAgentBootstrap,
        telemetry::InternalTelemetryAPIWorker, TopologyControlSurfaces,
    },
};

/// Creates the control plane supervisor.
///
/// This supervisor manages the health registry, unprivileged and privileged APIs, and optionally the remote agent
/// registration task.
///
/// It runs on a dedicated single-threaded runtime.
///
/// # Errors
///
/// If the supervisor can't be created, an error is returned.
pub async fn create_control_plane_supervisor(
    config: &GenericConfiguration, dp_config: &DataPlaneConfiguration, component_registry: &ComponentRegistry,
    health_registry: HealthRegistry, control_surfaces: TopologyControlSurfaces,
    ra_bootstrap: Option<RemoteAgentBootstrap>, logging_controller: LoggingOverrideController,
    config_system: &ConfigurationSystem,
) -> Result<Supervisor, GenericError> {
    let mut supervisor = Supervisor::new("ctrl-pln")?
        .with_dedicated_runtime(RuntimeConfiguration::single_threaded())
        .with_restart_strategy(RestartStrategy::one_to_one());

    supervisor.add_worker(health_registry.worker());
    supervisor.add_worker(ResourceTelemetryWorker::new(component_registry));
    supervisor.add_worker(InternalTelemetryAPIWorker::new());
    supervisor.add_worker(DynamicLogLevelWorker::new(
        config_system.live(|c| &c.control.logging.level),
        logging_controller,
    ));
    supervisor.add_worker(ConfigWorker::new(config.clone()));
    supervisor.add_worker(ConfigInternalWorker::new(config_system.current_handle()));

    supervisor.add_worker(DynamicAPIBuilder::new(
        EndpointType::Unprivileged,
        dp_config.api_listen_address().clone(),
    ));
    let ipc_config = IpcAuthConfiguration::from_configuration(config)?;
    let tls_config = build_ipc_server_tls_config(ipc_config.ipc_cert_file_path()).await?;

    let mut privileged_api =
        DynamicAPIBuilder::new(EndpointType::Privileged, dp_config.secure_api_listen_address().clone())
            .with_tls_config(tls_config);

    privileged_api = control_surfaces.register_control_surfaces(privileged_api);

    // If we bootstrapped ourselves as a remote agent, add the necessary gRPC services to the API
    // and a worker that captures the dataspace for diagnostic artifact collection.
    if let Some(ra_bootstrap) = &ra_bootstrap {
        supervisor.add_worker(ra_bootstrap.create_dataspace_anchor());
        privileged_api = privileged_api
            .with_grpc_service(ra_bootstrap.create_status_service())
            .with_grpc_service(ra_bootstrap.create_flare_service())
            .with_grpc_service(ra_bootstrap.create_telemetry_service());
    }

    supervisor.add_worker(privileged_api);

    Ok(supervisor)
}
