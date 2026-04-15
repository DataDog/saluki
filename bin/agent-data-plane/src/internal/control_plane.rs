use std::path::PathBuf;

use async_trait::async_trait;
use memory_accounting::ComponentRegistry;
use saluki_api::EndpointType;
use saluki_app::{
    api::APIBuilder, config::ConfigAPIHandler, dynamic_api::DynamicAPIBuilder, logging::acquire_logging_api_handler,
    memory::AllocationTelemetryWorker, metrics::acquire_metrics_api_handler,
};
use saluki_components::destinations::DogStatsDStatisticsConfiguration;
use saluki_config::GenericConfiguration;
use saluki_core::{
    health::HealthRegistry,
    runtime::{
        InitializationError, ProcessShutdown, RestartStrategy, RuntimeConfiguration, Supervisable, Supervisor,
        SupervisorFuture,
    },
};
use saluki_error::{ErrorContext as _, GenericError};
use saluki_io::net::{build_datadog_agent_server_tls_config, get_ipc_cert_file_path, ServerConfig};
use tracing::info;

use crate::{
    config::DataPlaneConfiguration,
    env_provider::ADPEnvironmentProvider,
    internal::{platform::PlatformSettings, remote_agent::RemoteAgentBootstrap},
};

/// Gets the IPC certificate file path from the configuration.
fn get_cert_path_from_config(config: &GenericConfiguration) -> Result<PathBuf, GenericError> {
    let auth_token_file_path = config
        .try_get_typed::<PathBuf>("auth_token_file_path")
        .error_context("Failed to get Agent auth token file path.")?
        .unwrap_or_else(PlatformSettings::get_auth_token_path);

    let ipc_cert_file_path = config
        .try_get_typed::<Option<PathBuf>>("ipc_cert_file_path")
        .error_context("Failed to get Agent IPC cert file path.")?
        .flatten();

    Ok(get_ipc_cert_file_path(
        ipc_cert_file_path.as_ref(),
        &auth_token_file_path,
    ))
}

/// A worker that serves the privileged HTTP API with TLS.
///
/// This worker also handles remote agent registration when not in standalone mode. The remote agent gRPC services are
/// registered on the privileged API, and a background task periodically refreshes the registration with the Datadog
/// Agent.
pub struct PrivilegedApiWorker {
    config: GenericConfiguration,
    dp_config: DataPlaneConfiguration,
    env_provider: ADPEnvironmentProvider,
    dsd_stats_config: DogStatsDStatisticsConfiguration,
    ra_bootstrap: Option<RemoteAgentBootstrap>,
    tls_config: ServerConfig,
}

impl PrivilegedApiWorker {
    /// Creates a new `PrivilegedApiWorker`.
    ///
    /// # Errors
    ///
    /// If the TLS configuration cannot be loaded, an error is returned.
    pub async fn new(
        config: GenericConfiguration, dp_config: DataPlaneConfiguration, env_provider: ADPEnvironmentProvider,
        dsd_stats_config: DogStatsDStatisticsConfiguration, ra_bootstrap: Option<RemoteAgentBootstrap>,
    ) -> Result<Self, GenericError> {
        let cert_path = get_cert_path_from_config(&config)?;
        let tls_config = build_datadog_agent_server_tls_config(cert_path).await?;

        Ok(Self {
            config,
            dp_config,
            env_provider,
            dsd_stats_config,
            ra_bootstrap,
            tls_config,
        })
    }
}

#[async_trait]
impl Supervisable for PrivilegedApiWorker {
    fn name(&self) -> &str {
        "privileged-api"
    }

    async fn initialize(&self, process_shutdown: ProcessShutdown) -> Result<SupervisorFuture, InitializationError> {
        let mut api_builder = APIBuilder::new()
            .with_tls_config(self.tls_config.clone())
            // TODO: make these handlers cloneable and move them up to the config for the worker so they can
            // be cloned for each initialization
            .with_optional_handler(acquire_logging_api_handler())
            .with_optional_handler(acquire_metrics_api_handler())
            .with_handler(ConfigAPIHandler::new(self.config.clone()))
            .with_optional_handler(self.env_provider.workload_api_handler())
            .with_handler(self.dsd_stats_config.api_handler());

        // If we bootstrapped ourselves as a remote agent, add the necessary gRPC services to the API.
        if let Some(ra_bootstrap) = &self.ra_bootstrap {
            api_builder = api_builder.with_grpc_service(ra_bootstrap.create_status_service());
            api_builder = api_builder.with_grpc_service(ra_bootstrap.create_flare_service());

            // Only register the telemetry service if telemetry is actually enabled.
            if let Some(telemetry_service) = ra_bootstrap.create_telemetry_service() {
                api_builder = api_builder.with_grpc_service(telemetry_service);
            }
        }

        let listen_address = self.dp_config.secure_api_listen_address().clone();

        Ok(Box::pin(async move {
            info!("Serving privileged API on {}.", listen_address);
            api_builder.serve(listen_address, process_shutdown).await
        }))
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
/// If the supervisor cannot be created, an error is returned.
pub async fn create_control_plane_supervisor(
    config: &GenericConfiguration, dp_config: &DataPlaneConfiguration, component_registry: &ComponentRegistry,
    health_registry: HealthRegistry, env_provider: ADPEnvironmentProvider,
    dsd_stats_config: DogStatsDStatisticsConfiguration, ra_bootstrap: Option<RemoteAgentBootstrap>,
) -> Result<Supervisor, GenericError> {
    let mut supervisor = Supervisor::new("ctrl-pln")?
        .with_dedicated_runtime(RuntimeConfiguration::single_threaded())
        .with_restart_strategy(RestartStrategy::one_to_one());

    supervisor.add_worker(health_registry.worker());
    supervisor.add_worker(AllocationTelemetryWorker::new(component_registry));

    supervisor.add_worker(DynamicAPIBuilder::new(
        EndpointType::Unprivileged,
        dp_config.api_listen_address().clone(),
    ));
    supervisor.add_worker(
        PrivilegedApiWorker::new(
            config.clone(),
            dp_config.clone(),
            env_provider,
            dsd_stats_config,
            ra_bootstrap,
        )
        .await?,
    );

    Ok(supervisor)
}
