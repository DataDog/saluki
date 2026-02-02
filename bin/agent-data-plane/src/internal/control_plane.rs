use std::path::PathBuf;

use async_trait::async_trait;
use memory_accounting::ComponentRegistry;
use saluki_app::{
    api::APIBuilder, config::ConfigAPIHandler, logging::acquire_logging_api_handler,
    metrics::acquire_metrics_api_handler,
};
use saluki_components::destinations::DogStatsDStatisticsConfiguration;
use saluki_config::GenericConfiguration;
use saluki_core::runtime::{
    InitializationError, ProcessShutdown, RestartStrategy, RuntimeConfiguration, Supervisable, Supervisor,
    SupervisorFuture,
};
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use saluki_health::HealthRegistry;
use saluki_io::net::{build_datadog_agent_server_tls_config, get_ipc_cert_file_path};
use tracing::info;

use crate::{
    config::DataPlaneConfiguration,
    env_provider::ADPEnvironmentProvider,
    internal::{platform::PlatformSettings, remote_agent::RemoteAgentBootstrap},
};

/// Gets the IPC certificate file path from the configuration.
fn get_cert_path_from_config(config: &GenericConfiguration) -> Result<PathBuf, GenericError> {
    // Try to get the auth token file path first
    let auth_token_file_path = config
        .try_get_typed::<PathBuf>("auth_token_file_path")
        .error_context("Failed to get Agent auth token file path.")?
        .unwrap_or_else(PlatformSettings::get_auth_token_path);

    // Try to get the explicit IPC cert file path
    let ipc_cert_file_path = config
        .try_get_typed::<Option<PathBuf>>("ipc_cert_file_path")
        .error_context("Failed to get Agent IPC cert file path.")?
        .flatten();

    Ok(get_ipc_cert_file_path(
        ipc_cert_file_path.as_ref(),
        &auth_token_file_path,
    ))
}

/// A worker that runs the health registry.
pub struct HealthRegistryWorker {
    health_registry: HealthRegistry,
}

impl HealthRegistryWorker {
    /// Creates a new `HealthRegistryWorker`.
    pub fn new(health_registry: HealthRegistry) -> Self {
        Self { health_registry }
    }
}

#[async_trait]
impl Supervisable for HealthRegistryWorker {
    fn name(&self) -> &str {
        "health-registry"
    }

    async fn initialize(&self, mut process_shutdown: ProcessShutdown) -> Result<SupervisorFuture, InitializationError> {
        // Spawn the health registry runner - it runs forever until the process exits
        //
        // TODO: This pattern doesn't allow us to gracefully restart the health registry worker if the supervisor itself
        // is restarted. We could do something simpler like `HealthRegistry` having a mutex to ensure only one instance is
        // running at a time, and that way the worker can be restarted gracefully, picking up where it left off.
        let handle = self
            .health_registry
            .clone()
            .spawn()
            .await
            .map_err(|e| InitializationError::Failed { source: e })?;

        Ok(Box::pin(async move {
            tokio::select! {
                result = handle => {
                    // The health registry task completed (shouldn't happen normally)
                    if let Err(e) = result {
                        return Err(generic_error!("Health registry task panicked: {}", e));
                    }
                    Ok(())
                }
                _ = process_shutdown.wait_for_shutdown() => {
                    // On shutdown, we just let the spawned task continue until the runtime shuts down.
                    // The health registry doesn't have a graceful shutdown mechanism.
                    Ok(())
                }
            }
        }))
    }
}

/// A worker that serves the unprivileged HTTP API.
pub struct UnprivilegedApiWorker {
    dp_config: DataPlaneConfiguration,
    health_registry: HealthRegistry,
    component_registry: ComponentRegistry,
}

impl UnprivilegedApiWorker {
    /// Creates a new `UnprivilegedApiWorker`.
    pub fn new(
        dp_config: DataPlaneConfiguration, health_registry: HealthRegistry, component_registry: ComponentRegistry,
    ) -> Self {
        Self {
            dp_config,
            health_registry,
            component_registry,
        }
    }
}

#[async_trait]
impl Supervisable for UnprivilegedApiWorker {
    fn name(&self) -> &str {
        "unprivileged-api"
    }

    async fn initialize(&self, process_shutdown: ProcessShutdown) -> Result<SupervisorFuture, InitializationError> {
        let api_builder = APIBuilder::new()
            .with_handler(self.health_registry.api_handler())
            .with_handler(self.component_registry.api_handler());

        let listen_address = self.dp_config.api_listen_address().clone();

        Ok(Box::pin(async move {
            info!("Serving unprivileged API on {}.", listen_address);
            api_builder.serve(listen_address, process_shutdown).await
        }))
    }
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
}

impl PrivilegedApiWorker {
    /// Creates a new `PrivilegedApiWorker`.
    pub fn new(
        config: GenericConfiguration, dp_config: DataPlaneConfiguration, env_provider: ADPEnvironmentProvider,
        dsd_stats_config: DogStatsDStatisticsConfiguration, ra_bootstrap: Option<RemoteAgentBootstrap>,
    ) -> Self {
        Self {
            config,
            dp_config,
            env_provider,
            dsd_stats_config,
            ra_bootstrap,
        }
    }
}

#[async_trait]
impl Supervisable for PrivilegedApiWorker {
    fn name(&self) -> &str {
        "privileged-api"
    }

    async fn initialize(&self, process_shutdown: ProcessShutdown) -> Result<SupervisorFuture, InitializationError> {
        // Load our TLS configuration.
        //
        // TODO: should this need to happen during process init or could we do it once when creating `PrivilegedApiWorker`
        // and simplify things?
        let cert_path =
            get_cert_path_from_config(&self.config).map_err(|e| InitializationError::Failed { source: e })?;
        let tls_config = build_datadog_agent_server_tls_config(cert_path)
            .await
            .map_err(|e| InitializationError::Failed { source: e })?;

        let mut api_builder = APIBuilder::new()
            .with_tls_config(tls_config)
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
/// registration task. It runs on a dedicated single-threaded runtime.
///
/// # Errors
///
/// If the supervisor cannot be created, an error is returned.
pub fn create_control_plane_supervisor(
    config: &GenericConfiguration, dp_config: &DataPlaneConfiguration, component_registry: &ComponentRegistry,
    health_registry: HealthRegistry, env_provider: ADPEnvironmentProvider,
    dsd_stats_config: DogStatsDStatisticsConfiguration, ra_bootstrap: Option<RemoteAgentBootstrap>,
) -> Result<Supervisor, GenericError> {
    let mut supervisor = Supervisor::new("ctrl-pln")?
        .with_dedicated_runtime(RuntimeConfiguration::single_threaded())
        .with_restart_strategy(RestartStrategy::one_to_one());

    // TODO: just make the API handler for `ComponentRegistry` cloneable so we can create/hold on to it in `UnprivilegedApiWorker`
    // without having to create a scoped one here just to maintain the ownership necessary
    let scoped_registry = component_registry.get_or_create("control-plane");

    supervisor.add_worker(HealthRegistryWorker::new(health_registry.clone()));
    supervisor.add_worker(UnprivilegedApiWorker::new(
        dp_config.clone(),
        health_registry,
        scoped_registry,
    ));
    supervisor.add_worker(PrivilegedApiWorker::new(
        config.clone(),
        dp_config.clone(),
        env_provider,
        dsd_stats_config,
        ra_bootstrap,
    ));

    Ok(supervisor)
}
