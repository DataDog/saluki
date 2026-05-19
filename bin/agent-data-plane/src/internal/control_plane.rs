use datadog_agent_commons::ipc::{config::IpcAuthConfiguration, tls::build_ipc_server_tls_config};
use memory_accounting::ComponentRegistry;
use saluki_api::EndpointType;
use saluki_app::{
    config::ConfigWorker, dynamic_api::DynamicAPIBuilder, logging::LoggingOverrideController,
    memory::AllocationTelemetryWorker,
};
use saluki_components::{
    destinations::DogStatsDStatisticsConfiguration,
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
    internal::{logging::DynamicLogLevelWorker, remote_agent::RemoteAgentBootstrap},
};

/// DogStatsD-specific control plane integrations.
#[derive(Clone)]
pub struct DogStatsDControlPlaneConfiguration {
    stats_config: DogStatsDStatisticsConfiguration,
    capture_api_handler: Option<DogStatsDCaptureAPIHandler>,
    replay_api_handler: Option<DogStatsDReplayAPIHandler>,
}

impl DogStatsDControlPlaneConfiguration {
    /// Creates a new `DogStatsDControlPlaneConfiguration`.
    pub fn new(
        stats_config: DogStatsDStatisticsConfiguration, capture_api_handler: Option<DogStatsDCaptureAPIHandler>,
        replay_api_handler: Option<DogStatsDReplayAPIHandler>,
    ) -> Self {
        Self {
            stats_config,
            capture_api_handler,
            replay_api_handler,
        }
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
    health_registry: HealthRegistry, dsd_config: DogStatsDControlPlaneConfiguration,
    ra_bootstrap: Option<RemoteAgentBootstrap>, logging_controller: LoggingOverrideController,
) -> Result<Supervisor, GenericError> {
    let mut supervisor = Supervisor::new("ctrl-pln")?
        .with_dedicated_runtime(RuntimeConfiguration::single_threaded())
        .with_restart_strategy(RestartStrategy::one_to_one());

    supervisor.add_worker(health_registry.worker());
    supervisor.add_worker(AllocationTelemetryWorker::new(component_registry));
    supervisor.add_worker(DynamicLogLevelWorker::new(config, logging_controller));
    supervisor.add_worker(ConfigWorker::new(config.clone()));

    supervisor.add_worker(DynamicAPIBuilder::new(
        EndpointType::Unprivileged,
        dp_config.api_listen_address().clone(),
    ));
    let ipc_config = IpcAuthConfiguration::from_configuration(config)?;
    let tls_config = build_ipc_server_tls_config(ipc_config.ipc_cert_file_path()).await?;
    let DogStatsDControlPlaneConfiguration {
        stats_config,
        capture_api_handler,
        replay_api_handler,
    } = dsd_config;

    let mut privileged_api =
        DynamicAPIBuilder::new(EndpointType::Privileged, dp_config.secure_api_listen_address().clone())
            .with_tls_config(tls_config)
            .with_handler(stats_config.api_handler())
            .with_optional_handler(capture_api_handler)
            .with_optional_handler(replay_api_handler);

    // If we bootstrapped ourselves as a remote agent, add the necessary gRPC services to the API.
    if let Some(ra_bootstrap) = &ra_bootstrap {
        privileged_api = privileged_api
            .with_grpc_service(ra_bootstrap.create_status_service())
            .with_grpc_service(ra_bootstrap.create_flare_service());

        // Only register the telemetry service if telemetry is actually enabled.
        if let Some(telemetry_service) = ra_bootstrap.create_telemetry_service() {
            privileged_api = privileged_api.with_grpc_service(telemetry_service);
        }
    }

    supervisor.add_worker(privileged_api);

    Ok(supervisor)
}
