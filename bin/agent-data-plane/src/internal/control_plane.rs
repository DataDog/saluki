use std::sync::Arc;

use datadog_agent_commons::ipc::tls::build_ipc_server_tls_config;
use datadog_agent_runtime::{
    control_plane::{ConfigRuntimeWorker, RuntimeConfigSnapshot},
    facade::{Initialized, RemoteAgent},
    remote_agent::StatusSectionProvider,
};
use saluki_api::EndpointType;
use saluki_app::{
    accounting::ResourceTelemetryWorker,
    api::APIBuilder,
    config::ConfigWorker,
    logging::{DynamicLogLevelWorker, LoggingOverrideController},
};
use saluki_core::{
    observability::metrics::InternalTelemetryAPIWorker,
    runtime::{RestartStrategy, RuntimeConfiguration, Supervisor},
};
use saluki_error::GenericError;

use crate::{
    config::DataPlaneConfiguration,
    internal::{
        logging::get_translated_log_level_stream, AgentDataPlane, DogStatsDStatusSection, TopologyControlSurfaces,
    },
    state::metrics::{get_compat_remappings, get_datadog_agent_remappings},
};

/// Creates the control plane supervisor.
///
/// This supervisor manages the health registry, unprivileged and privileged APIs, and the remote agent supervisor
/// from the facade.
///
/// It runs on a dedicated single-threaded runtime.
///
/// # Errors
///
/// If the supervisor can't be created, an error is returned.
pub async fn create_control_plane_supervisor(
    agent: &RemoteAgent<AgentDataPlane, Initialized>, control_surfaces: TopologyControlSurfaces,
    logging_controller: LoggingOverrideController,
) -> Result<Supervisor, GenericError> {
    let config_system = agent.config().system();
    let config = config_system.config();
    let dp = DataPlaneConfiguration::from_configuration(&config);
    let mut supervisor = Supervisor::new("ctrl-pln")?
        .with_dedicated_runtime(RuntimeConfiguration::single_threaded())
        .with_restart_strategy(RestartStrategy::one_to_one());

    supervisor.add_worker(agent.health_registry().worker());
    supervisor.add_worker(ResourceTelemetryWorker::new(agent.component_registry()));
    supervisor.add_worker(InternalTelemetryAPIWorker::new(Some(get_compat_remappings())));
    supervisor.add_worker(DynamicLogLevelWorker::new(
        get_translated_log_level_stream(config_system),
        logging_controller,
    ));
    supervisor.add_worker(ConfigWorker::new(config_system.raw_snapshot()));
    supervisor.add_worker(ConfigRuntimeWorker::new(RuntimeConfigSnapshot::from_shared(
        config_system.current_handle(),
    )));

    let api_listen_address = dp.api_listen_address()?;
    let secure_api_listen_address = dp.secure_api_listen_address()?;

    supervisor.add_worker(APIBuilder::new(EndpointType::Unprivileged, api_listen_address).into_supervisor());
    let ipc_config = dp.ipc_auth_configuration();
    let tls_config = build_ipc_server_tls_config(ipc_config.ipc_cert_file_path()).await?;

    let mut privileged_api =
        APIBuilder::new(EndpointType::Privileged, secure_api_listen_address).with_tls_config(tls_config);

    privileged_api = control_surfaces.register_control_surfaces(privileged_api);

    let status_sections: Vec<Arc<dyn StatusSectionProvider>> = vec![Arc::new(DogStatsDStatusSection::new())];
    supervisor.add_worker(agent.create_remote_agent_supervisor(status_sections, get_datadog_agent_remappings())?);

    supervisor.add_worker(privileged_api.into_supervisor());

    Ok(supervisor)
}
