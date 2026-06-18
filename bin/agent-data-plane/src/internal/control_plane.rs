use agent_data_plane_config::ControlConfiguration;
use agent_data_plane_config_system::{ConfigViewProducer, EnvConfig};
use datadog_agent_commons::ipc::{config::IpcAuthConfiguration, tls::build_ipc_server_tls_config};
use resource_accounting::ComponentRegistry;
use saluki_api::EndpointType;
use saluki_app::{
    accounting::ResourceTelemetryWorker,
    config::{ConfigViewStrings, ConfigWorker},
    dynamic_api::DynamicAPIBuilder,
    logging::LoggingOverrideController,
};
use saluki_component_config::ScopedConfig;
use saluki_core::{
    health::HealthRegistry,
    runtime::{RestartStrategy, RuntimeConfiguration, Supervisor},
};
use saluki_error::GenericError;

use crate::internal::{
    logging::DynamicLogLevelWorker, remote_agent::RemoteAgentServices, telemetry::InternalTelemetryAPIWorker,
    TopologyControlSurfaces,
};

/// Creates the control plane supervisor.
///
/// This supervisor manages the health registry, unprivileged and privileged APIs, the live `/config`
/// views, the dynamic log-level worker, and optionally the remote-agent gRPC services.
///
/// It runs on a dedicated single-threaded runtime.
///
/// # Errors
///
/// If the supervisor can't be created, an error is returned.
#[allow(clippy::too_many_arguments)]
pub async fn create_control_plane_supervisor(
    control: &ControlConfiguration, env_config: &EnvConfig, component_registry: &ComponentRegistry,
    health_registry: HealthRegistry, control_surfaces: TopologyControlSurfaces,
    remote_agent_services: Option<RemoteAgentServices>, logging_controller: LoggingOverrideController,
    log_level: ScopedConfig<String>, view_producer: ConfigViewProducer,
) -> Result<Supervisor, GenericError> {
    let mut supervisor = Supervisor::new("ctrl-pln")?
        .with_dedicated_runtime(RuntimeConfiguration::single_threaded())
        .with_restart_strategy(RestartStrategy::one_to_one());

    supervisor.add_worker(health_registry.worker());
    supervisor.add_worker(ResourceTelemetryWorker::new(component_registry));
    supervisor.add_worker(InternalTelemetryAPIWorker::new());
    supervisor.add_worker(DynamicLogLevelWorker::new(log_level, logging_controller));

    // The `/config` worker serves the typed config views live-on-request: it holds a clone-able view
    // producer and never a raw configuration map.
    let config_producer = std::sync::Arc::new(move || {
        let views = view_producer();
        ConfigViewStrings {
            raw: views.raw.as_str().to_string(),
            internal: views.internal.as_str().to_string(),
        }
    });
    supervisor.add_worker(ConfigWorker::new(config_producer));

    supervisor.add_worker(DynamicAPIBuilder::new(
        EndpointType::Unprivileged,
        control.api_listen_address.clone(),
    ));
    // `IpcAuthConfiguration` is a commons type that reads the IPC certificate path from the raw source
    // map; it is supplied via the confined `EnvConfig` pass-through.
    let ipc_config = IpcAuthConfiguration::from_configuration(env_config.raw())?;
    let tls_config = build_ipc_server_tls_config(ipc_config.ipc_cert_file_path()).await?;

    let mut privileged_api =
        DynamicAPIBuilder::new(EndpointType::Privileged, control.secure_api_listen_address.clone())
            .with_tls_config(tls_config);

    privileged_api = control_surfaces.register_control_surfaces(privileged_api);

    // If we registered ourselves as a remote agent, add the necessary gRPC services to the API.
    if let Some(remote_agent_services) = &remote_agent_services {
        privileged_api = privileged_api
            .with_grpc_service(remote_agent_services.create_status_service())
            .with_grpc_service(remote_agent_services.create_flare_service())
            .with_grpc_service(remote_agent_services.create_telemetry_service());
    }

    supervisor.add_worker(privileged_api);

    Ok(supervisor)
}
