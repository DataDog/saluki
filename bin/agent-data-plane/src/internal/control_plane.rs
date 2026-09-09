use std::sync::Arc;

use agent_data_plane_config::SalukiConfiguration;
use agent_data_plane_config_system::ConfigurationSystem;
use arc_swap::ArcSwap;
use datadog_agent_commons::ipc::tls::build_ipc_server_tls_config;
use saluki_api::EndpointType;
use saluki_app::{
    accounting::ResourceTelemetryWorker, api::APIBuilder, config::ConfigWorker, logging::LoggingOverrideController,
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
        config_runtime::ConfigRuntimeWorker, logging::DynamicLogLevelWorker, remote_agent::RemoteAgentBootstrap,
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
    supervisor.add_worker(DynamicLogLevelWorker::new(
        config_system.live(|config| &config.control.logging.level),
        logging_controller,
    ));
    supervisor.add_worker(ConfigWorker::new(raw_map));
    supervisor.add_worker(ConfigRuntimeWorker::new(current_config));

    let api_listen_address = dp.api_listen_address()?;
    let secure_api_listen_address = dp.secure_api_listen_address()?;

    supervisor.add_worker(APIBuilder::new(EndpointType::Unprivileged, api_listen_address).into_supervisor());
    let ipc_config = dp.ipc_auth_configuration();
    let tls_config = build_ipc_server_tls_config(ipc_config.ipc_cert_file_path()).await?;

    let mut privileged_api =
        APIBuilder::new(EndpointType::Privileged, secure_api_listen_address).with_tls_config(tls_config);

    privileged_api = control_surfaces.register_control_surfaces(privileged_api);

    if let Some(ra_bootstrap) = &ra_bootstrap {
        supervisor.add_worker(ra_bootstrap.create_dataspace_anchor());
        supervisor.add_worker(ra_bootstrap.create_event_reporter());
        privileged_api = privileged_api
            .with_grpc_service(ra_bootstrap.create_status_service())
            .with_grpc_service(ra_bootstrap.create_flare_service())
            .with_grpc_service(ra_bootstrap.create_telemetry_service());
    }

    supervisor.add_worker(privileged_api.into_supervisor());

    Ok(supervisor)
}
