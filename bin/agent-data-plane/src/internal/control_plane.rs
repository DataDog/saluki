use std::{future::pending, path::PathBuf};

use memory_accounting::ComponentRegistry;
use saluki_app::{
    api::APIBuilder, config::ConfigAPIHandler, logging::acquire_logging_api_handler,
    metrics::acquire_metrics_api_handler,
};
use saluki_common::task::spawn_traced_named;
use saluki_components::destinations::DogStatsDStatisticsConfiguration;
use saluki_config::GenericConfiguration;
use saluki_error::{ErrorContext as _, GenericError};
use saluki_health::HealthRegistry;
use saluki_io::net::{build_datadog_agent_server_tls_config, get_ipc_cert_file_path, ListenAddress};
use tracing::{error, info};

use crate::{
    config::DataPlaneConfiguration,
    env_provider::ADPEnvironmentProvider,
    internal::{initialize_and_launch_runtime, platform::PlatformSettings, remote_agent::RemoteAgentBootstrap},
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

/// Spawns the control plane for the ADP process.
///
/// This includes the unprivileged and privileged API servers, health registry, Remote Agent Registry integration, and
/// more. Control plane components are isolated from other asynchronous runtimes and components within the process, and
/// are meant to be available even when the primary topology is experiencing issues or is under duress.
///
/// # Errors
///
/// If the APIs cannot be spawned, or if the health registry cannot be spawned, an error will be returned.
pub async fn spawn_control_plane(
    config: &GenericConfiguration, dp_config: &DataPlaneConfiguration, component_registry: &ComponentRegistry,
    health_registry: HealthRegistry, env_provider: ADPEnvironmentProvider,
    dsd_stats_config: DogStatsDStatisticsConfiguration, ra_bootstrap: Option<RemoteAgentBootstrap>,
) -> Result<(), GenericError> {
    // Build our unprivileged and privileged API server.
    //
    // The unprivileged API is purely for things like health checks or read-only information. The privileged API is
    // meant for sensitive information or actions that require elevated permissions.
    let unprivileged_api = APIBuilder::new()
        .with_handler(health_registry.api_handler())
        .with_handler(component_registry.api_handler());

    // Build the privileged API with certificate-based TLS configuration
    let cert_path = get_cert_path_from_config(config)?;
    let tls_config = build_datadog_agent_server_tls_config(cert_path).await?;

    let privileged_api = APIBuilder::new()
        .with_tls_config(tls_config)
        .with_optional_handler(acquire_logging_api_handler())
        .with_optional_handler(acquire_metrics_api_handler())
        .with_handler(ConfigAPIHandler::new(config.clone()))
        .with_optional_handler(env_provider.workload_api_handler())
        .with_handler(dsd_stats_config.api_handler());

    let dp_config = dp_config.clone();
    let init = async move {
        // Handle any final configuration of our API endpoints and spawn them.
        configure_and_spawn_api_endpoints(dp_config, unprivileged_api, privileged_api, ra_bootstrap).await?;

        health_registry.spawn().await?;

        Ok(())
    };

    initialize_and_launch_runtime("rt-control-plane", init, |_| pending())
}

async fn configure_and_spawn_api_endpoints(
    dp_config: DataPlaneConfiguration, unprivileged_api: APIBuilder, mut privileged_api: APIBuilder,
    ra_bootstrap: Option<RemoteAgentBootstrap>,
) -> Result<(), GenericError> {
    // If we're not in standalone mode and we've gotten some bootstrapped remote agent state,
    // wire it up to the privileged API so the Core Agent can communicate with us.
    if let Some(ra_bootstrap) = ra_bootstrap {
        privileged_api = privileged_api.with_grpc_service(ra_bootstrap.create_status_service());
        privileged_api = privileged_api.with_grpc_service(ra_bootstrap.create_flare_service());
        if let Some(telemetry_service) = ra_bootstrap.create_telemetry_service() {
            privileged_api = privileged_api.with_grpc_service(telemetry_service);
        }
    }

    spawn_unprivileged_api(unprivileged_api, dp_config.api_listen_address().clone()).await?;
    spawn_privileged_api(privileged_api, dp_config.secure_api_listen_address().clone()).await?;

    Ok(())
}

async fn spawn_unprivileged_api(
    api_builder: APIBuilder, api_listen_address: ListenAddress,
) -> Result<(), GenericError> {
    // TODO: Use something better than `pending()`... perhaps something like a more generalized
    // `ComponentShutdownCoordinator` that allows for triggering and waiting for all attached tasks to signal that
    // they've shutdown.
    spawn_traced_named("adp-unprivileged-http-api", async move {
        info!("Serving unprivileged API on {}.", api_listen_address);

        if let Err(e) = api_builder.serve(api_listen_address, pending()).await {
            error!("Failed to serve unprivileged API: {}", e);
        }
    });

    Ok(())
}

async fn spawn_privileged_api(api_builder: APIBuilder, api_listen_address: ListenAddress) -> Result<(), GenericError> {
    // TODO: Use something better than `pending()`... perhaps something like a more generalized
    // `ComponentShutdownCoordinator` that allows for triggering and waiting for all attached tasks to signal that
    // they've shutdown.
    spawn_traced_named("adp-privileged-http-api", async move {
        info!("Serving privileged API on {}.", api_listen_address);

        if let Err(e) = api_builder.serve(api_listen_address, pending()).await {
            error!("Failed to serve privileged API: {}", e);
        }
    });

    Ok(())
}
