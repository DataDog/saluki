use std::sync::Arc;

use agent_data_plane_config::SalukiConfiguration;
use agent_data_plane_config_system::ConfigurationSystem;
use arc_swap::ArcSwap;
use datadog_agent_commons::ipc::{config::IpcAuthConfiguration, tls::build_ipc_server_tls_config};
use saluki_api::EndpointType;
use saluki_app::{
    accounting::ResourceTelemetryWorker, config::ConfigWorker, dynamic_api::DynamicAPIBuilder,
    logging::LoggingOverrideController,
};
use saluki_core::accounting::ComponentRegistry;
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
    config_system: &ConfigurationSystem, component_registry: &ComponentRegistry, health_registry: HealthRegistry,
    control_surfaces: TopologyControlSurfaces, ra_bootstrap: Option<RemoteAgentBootstrap>,
    logging_controller: LoggingOverrideController, current_config: Arc<ArcSwap<SalukiConfiguration>>,
) -> Result<Supervisor, GenericError> {
    let config = config_system.config();
    let dp = DataPlaneConfiguration::from_configuration(&config);
    let raw_map = config_system.raw_map();
    let mut supervisor = Supervisor::new("ctrl-pln")?
        .with_dedicated_runtime(RuntimeConfiguration::single_threaded())
        .with_restart_strategy(RestartStrategy::one_to_one());

    supervisor.add_worker(health_registry.worker());
    supervisor.add_worker(ResourceTelemetryWorker::new(component_registry));
    supervisor.add_worker(InternalTelemetryAPIWorker::new());
    supervisor.add_worker(DynamicLogLevelWorker::new(&raw_map, logging_controller));
    supervisor.add_worker(ConfigWorker::new(raw_map.clone()));
    supervisor.add_worker(ConfigInternalWorker::new(current_config));

    supervisor.add_worker(DynamicAPIBuilder::new(
        EndpointType::Unprivileged,
        dp.api_listen_address().clone(),
    ));
    let ipc_config = IpcAuthConfiguration::from_configuration(&raw_map)?;
    let tls_config = build_ipc_server_tls_config(ipc_config.ipc_cert_file_path()).await?;

    let mut privileged_api = DynamicAPIBuilder::new(EndpointType::Privileged, dp.secure_api_listen_address().clone())
        .with_tls_config(tls_config);

    privileged_api = control_surfaces.register_control_surfaces(privileged_api);

    if let Some(ra_bootstrap) = &ra_bootstrap {
        supervisor.add_worker(ra_bootstrap.create_dataspace_anchor());
        supervisor.add_worker(ra_bootstrap.create_event_reporter());
        privileged_api = privileged_api
            .with_grpc_service(ra_bootstrap.create_status_service())
            .with_grpc_service(ra_bootstrap.create_flare_service())
            .with_grpc_service(ra_bootstrap.create_telemetry_service());
    }

    supervisor.add_worker(privileged_api);

    Ok(supervisor)
}
