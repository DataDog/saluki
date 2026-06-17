use agent_data_plane_config::ConfigViews;
use datadog_agent_commons::ipc::tls::build_ipc_server_tls_config;
use resource_accounting::ComponentRegistry;
use saluki_api::EndpointType;
use saluki_app::{
    accounting::ResourceTelemetryWorker, dynamic_api::DynamicAPIBuilder, logging::LoggingOverrideController,
};
use saluki_component_config::ScopedConfig;
use saluki_core::{
    health::HealthRegistry,
    runtime::{RestartStrategy, RuntimeConfiguration, Supervisor},
};
use saluki_error::GenericError;

use crate::{
    config::DataPlaneConfiguration,
    internal::{
        config_views::ConfigViewsWorker, logging::DynamicLogLevelWorker, remote_agent::RemoteAgentBootstrap,
        telemetry::InternalTelemetryAPIWorker, TopologyControlSurfaces,
    },
};

/// Creates the control plane supervisor.
///
/// # Errors
///
/// If the supervisor can't be created, an error is returned.
pub async fn create_control_plane_supervisor(
    dp_config: &DataPlaneConfiguration, component_registry: &ComponentRegistry, health_registry: HealthRegistry,
    control_surfaces: TopologyControlSurfaces, ra_bootstrap: Option<RemoteAgentBootstrap>, config_views: ConfigViews,
    log_level: ScopedConfig<Option<String>>, logging_controller: LoggingOverrideController,
) -> Result<Supervisor, GenericError> {
    let mut supervisor = Supervisor::new("ctrl-pln")?
        .with_dedicated_runtime(RuntimeConfiguration::single_threaded())
        .with_restart_strategy(RestartStrategy::one_to_one());

    supervisor.add_worker(health_registry.worker());
    supervisor.add_worker(ResourceTelemetryWorker::new(component_registry));
    supervisor.add_worker(InternalTelemetryAPIWorker::new());
    supervisor.add_worker(DynamicLogLevelWorker::new(log_level, logging_controller));
    supervisor.add_worker(ConfigViewsWorker::new(config_views));

    supervisor.add_worker(DynamicAPIBuilder::new(
        EndpointType::Unprivileged,
        dp_config.api_listen_address().clone(),
    ));
    let tls_config = build_ipc_server_tls_config(dp_config.ipc_auth().ipc_cert_file_path()).await?;

    let mut privileged_api =
        DynamicAPIBuilder::new(EndpointType::Privileged, dp_config.secure_api_listen_address().clone())
            .with_tls_config(tls_config);

    privileged_api = control_surfaces.register_control_surfaces(privileged_api);

    if let Some(ra_bootstrap) = &ra_bootstrap {
        privileged_api = privileged_api
            .with_grpc_service(ra_bootstrap.create_status_service())
            .with_grpc_service(ra_bootstrap.create_flare_service())
            .with_grpc_service(ra_bootstrap.create_telemetry_service());
    }

    supervisor.add_worker(privileged_api);

    Ok(supervisor)
}
