use std::{future::pending, path::PathBuf};

use memory_accounting::ComponentRegistry;
use saluki_app::{
    api::APIBuilder, config::ConfigAPIHandler, logging::acquire_logging_api_handler,
    metrics::acquire_metrics_api_handler,
};
use saluki_common::task::spawn_traced_named;
use saluki_components::destinations::DogStatsDStatisticsConfiguration;
use saluki_config::GenericConfiguration;
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use saluki_health::HealthRegistry;
use saluki_io::net::{build_datadog_agent_server_tls_config, get_ipc_cert_file_path, GrpcTargetAddress, ListenAddress};
use serde::Deserialize;
use tracing::{error, info};

use crate::internal::{platform, remote_agent::RemoteAgentHelperConfiguration};
use crate::{env_provider::ADPEnvironmentProvider, internal::initialize_and_launch_runtime};

const fn default_api_listen_address() -> ListenAddress {
    ListenAddress::any_tcp(5100)
}

const fn default_secure_api_listen_address() -> ListenAddress {
    ListenAddress::any_tcp(5101)
}

#[derive(Deserialize)]
pub struct ControlPlaneConfiguration {
    #[serde(rename = "data_plane_api_listen_addr", default = "default_api_listen_address")]
    pub api_listen_address: ListenAddress,

    #[serde(
        rename = "data_plane_secure_api_listen_addr",
        default = "default_secure_api_listen_address"
    )]
    pub secure_api_listen_address: ListenAddress,
}

impl ControlPlaneConfiguration {
    /// Creates a new `ControlPlaneConfiguration` from the given generic configuration.
    ///
    /// # Errors
    ///
    /// If the configuration is invalid, an error will be returned.
    pub fn from_config(config: &GenericConfiguration) -> Result<Self, GenericError> {
        let config = config.as_typed()?;
        Ok(config)
    }
}

/// Gets the IPC certificate file path from the configuration.
fn get_cert_path_from_config(config: &GenericConfiguration) -> Result<PathBuf, GenericError> {
    // Try to get the auth token file path first
    let auth_token_file_path = config
        .try_get_typed::<PathBuf>("auth_token_file_path")
        .error_context("Failed to get Agent auth token file path.")?
        .unwrap_or_else(|| PathBuf::from(platform::DATADOG_AGENT_AUTH_TOKEN));

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
    config: GenericConfiguration, component_registry: &ComponentRegistry, health_registry: HealthRegistry,
    env_provider: ADPEnvironmentProvider, dsd_stats_config: DogStatsDStatisticsConfiguration,
) -> Result<(), GenericError> {
    // Build our unprivileged and privileged API server.
    //
    // The unprivileged API is purely for things like health checks or read-only information. The privileged API is
    // meant for sensitive information or actions that require elevated permissions.
    let unprivileged_api = APIBuilder::new()
        .with_handler(health_registry.api_handler())
        .with_handler(component_registry.api_handler());

    // Build the privileged API with certificate-based TLS configuration
    let cert_path = get_cert_path_from_config(&config)?;
    let tls_config = build_datadog_agent_server_tls_config(cert_path).await?;

    let privileged_api = APIBuilder::new()
        .with_tls_config(tls_config)
        .with_optional_handler(acquire_logging_api_handler())
        .with_optional_handler(acquire_metrics_api_handler())
        .with_handler(ConfigAPIHandler::new(config.clone()))
        .with_optional_handler(env_provider.workload_api_handler())
        .with_handler(dsd_stats_config.api_handler());

    let init = async move {
        // Handle any final configuration of our API endpoints and spawn them.
        configure_and_spawn_api_endpoints(&config, unprivileged_api, privileged_api).await?;

        health_registry.spawn().await?;

        Ok(())
    };

    initialize_and_launch_runtime("rt-control-plane", init, |_| pending())
}

async fn configure_and_spawn_api_endpoints(
    config: &GenericConfiguration, unprivileged_api: APIBuilder, mut privileged_api: APIBuilder,
) -> Result<(), GenericError> {
    let control_plane_config = ControlPlaneConfiguration::from_config(config)?;
    let api_listen_address = control_plane_config.api_listen_address;
    let secure_api_listen_address = control_plane_config.secure_api_listen_address;

    // When not in standalone mode, install the necessary components for registering ourselves with the Datadog Agent as
    // a "remote agent", which wires up ADP to allow the Datadog Agent to query it for status and flare information.
    let in_standalone_mode = config.get_typed_or_default::<bool>("adp.standalone_mode");
    if !in_standalone_mode {
        let secure_api_grpc_target_addr = GrpcTargetAddress::try_from_listen_addr(&secure_api_listen_address)
            .ok_or_else(|| generic_error!("Failed to get valid gRPC target address from secure API listen address."))?;

        let telemetry_enabled = config.get_typed_or_default::<bool>("telemetry_enabled");
        let mut prometheus_listen_addr = None;
        if telemetry_enabled {
            let addr = config
                .try_get_typed("prometheus_listen_addr")
                .error_context("Failed to get Prometheus listen address.")?
                .unwrap_or_else(|| ListenAddress::any_tcp(5102));

            prometheus_listen_addr = Some(
                addr.as_local_connect_addr()
                    .ok_or_else(|| generic_error!("Failed to get local Prometheus listen address to advertise."))?,
            );
        }

        // Build our helper configuration for registering ourselves with the Datadog Agent as a remote agent.
        let mut remote_agent_config = RemoteAgentHelperConfiguration::from_configuration(
            config,
            secure_api_grpc_target_addr,
            prometheus_listen_addr,
        )
        .await?;

        // Create and register the Remote Agent gRPC services with the privileged API.
        // Each service is tracked automatically for registration with the Remote Agent Registry.
        privileged_api = privileged_api.with_grpc_service(remote_agent_config.create_status_service());
        privileged_api = privileged_api.with_grpc_service(remote_agent_config.create_flare_service());
        // Only register the telemetry service if telemetry is enabled
        if telemetry_enabled {
            privileged_api = privileged_api.with_grpc_service(remote_agent_config.create_telemetry_service());
        }

        // Spawn the remote agent helper task.
        remote_agent_config.spawn();
    }

    spawn_unprivileged_api(unprivileged_api, api_listen_address).await?;
    spawn_privileged_api(privileged_api, secure_api_listen_address).await?;

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
