use datadog_agent_commons::ipc::{config::IpcAuthConfiguration, tls::build_ipc_server_tls_config};
use resource_accounting::ComponentRegistry;
use saluki_api::EndpointType;
use saluki_app::{
    accounting::ResourceTelemetryWorker, config::ConfigWorker, dynamic_api::DynamicAPIBuilder,
    logging::LoggingOverrideController,
};
use saluki_components::{
    destinations::DogStatsDAPIHandler,
    sources::{DogStatsDCaptureAPIHandler, DogStatsDReplayAPIHandler},
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
        logging::DynamicLogLevelWorker, remote_agent::RemoteAgentBootstrap, telemetry::InternalTelemetryAPIWorker,
    },
};

/// Unified DogStatsD control-plane surface.
///
/// Contains all API handlers for the DogStatsD pipeline. This is present only when DogStatsD is
/// enabled; when it is `None`, none of the DogStatsD control-plane endpoints are registered.
pub struct DogStatsDControlSurface {
    /// API handler for the `/dogstatsd/stats` endpoint.
    pub(crate) stats_api_handler: DogStatsDAPIHandler,
    /// API handler for the `/dogstatsd/capture/trigger` endpoint.
    pub(crate) capture_api_handler: DogStatsDCaptureAPIHandler,
    /// API handler for the `/dogstatsd/replay/session` endpoints.
    pub(crate) replay_api_handler: DogStatsDReplayAPIHandler,
}

impl DogStatsDControlSurface {
    pub(crate) fn register_handlers(self, builder: DynamicAPIBuilder) -> DynamicAPIBuilder {
        builder
            .with_handler(self.stats_api_handler)
            .with_handler(self.capture_api_handler)
            .with_handler(self.replay_api_handler)
    }
}

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
    health_registry: HealthRegistry, dsd_control_surface: Option<DogStatsDControlSurface>,
    ra_bootstrap: Option<RemoteAgentBootstrap>, logging_controller: LoggingOverrideController,
) -> Result<Supervisor, GenericError> {
    let mut supervisor = Supervisor::new("ctrl-pln")?
        .with_dedicated_runtime(RuntimeConfiguration::single_threaded())
        .with_restart_strategy(RestartStrategy::one_to_one());

    supervisor.add_worker(health_registry.worker());
    supervisor.add_worker(ResourceTelemetryWorker::new(component_registry));
    supervisor.add_worker(InternalTelemetryAPIWorker::new());
    supervisor.add_worker(DynamicLogLevelWorker::new(config, logging_controller));
    supervisor.add_worker(ConfigWorker::new(config.clone()));

    supervisor.add_worker(DynamicAPIBuilder::new(
        EndpointType::Unprivileged,
        dp_config.api_listen_address().clone(),
    ));
    let ipc_config = IpcAuthConfiguration::from_configuration(config)?;
    let tls_config = build_ipc_server_tls_config(ipc_config.ipc_cert_file_path()).await?;

    let mut privileged_api =
        DynamicAPIBuilder::new(EndpointType::Privileged, dp_config.secure_api_listen_address().clone())
            .with_tls_config(tls_config);

    if let Some(surface) = dsd_control_surface {
        privileged_api = surface.register_handlers(privileged_api);
    }

    // If we bootstrapped ourselves as a remote agent, add the necessary gRPC services to the API.
    if let Some(ra_bootstrap) = &ra_bootstrap {
        privileged_api = privileged_api
            .with_grpc_service(ra_bootstrap.create_status_service())
            .with_grpc_service(ra_bootstrap.create_flare_service())
            .with_grpc_service(ra_bootstrap.create_telemetry_service());
    }

    supervisor.add_worker(privileged_api);

    Ok(supervisor)
}
