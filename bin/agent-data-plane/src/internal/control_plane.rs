use std::future::pending;

use memory_accounting::ComponentRegistry;
use saluki_app::{api::APIBuilder, config::ConfigAPIHandler, prelude::acquire_logging_api_handler};
use saluki_common::task::spawn_traced_named;
use saluki_config::GenericConfiguration;
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use saluki_health::HealthRegistry;
use saluki_io::net::ListenAddress;
use tracing::{error, info};

use crate::internal::remote_agent::RemoteAgentHelperConfiguration;
use crate::{env_provider::ADPEnvironmentProvider, internal::initialize_and_launch_runtime};

const PRIMARY_UNPRIVILEGED_API_PORT: u16 = 5100;
const PRIMARY_PRIVILEGED_API_PORT: u16 = 5101;

/// Spawns the control plane for the ADP process.
///
/// This includes the unprivileged and privileged API servers, health registry, Remote Agent Registry integration, and
/// more. Control plane components are isolated from other asynchronous runtimes and components within the process, and
/// are meant to be available even when the primary topology is experiencing issues or is under duress.
///
/// # Errors
///
/// If the APIs cannot be spawned, or if the health registry cannot be spawned, an error will be returned.
pub fn spawn_control_plane(
    config: GenericConfiguration, component_registry: &ComponentRegistry, health_registry: HealthRegistry,
    env_provider: ADPEnvironmentProvider,
) -> Result<(), GenericError> {
    // Build our unprivileged and privileged API server.
    //
    // The unprivileged API is purely for things like health checks or read-only information. The privileged API is
    // meant for sensitive information or actions that require elevated permissions.
    let unprivileged_api = APIBuilder::new()
        .with_handler(health_registry.api_handler())
        .with_handler(component_registry.api_handler());

    let privileged_api = APIBuilder::new()
        .with_self_signed_tls()
        .with_optional_handler(acquire_logging_api_handler())
        .with_optional_handler(config.get_refreshable_handle().map(ConfigAPIHandler::from_state))
        .with_optional_handler(env_provider.workload_api_handler());

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
    let api_listen_address = config
        .try_get_typed("api_listen_address")
        .error_context("Failed to get API listen address.")?
        .unwrap_or_else(|| ListenAddress::any_tcp(PRIMARY_UNPRIVILEGED_API_PORT));

    let secure_api_listen_address = config
        .try_get_typed("secure_api_listen_address")
        .error_context("Failed to get secure API listen address.")?
        .unwrap_or_else(|| ListenAddress::any_tcp(PRIMARY_PRIVILEGED_API_PORT));

    // When not in standalone mode, install the necessary components for registering ourselves with the Datadog Agent as
    // a "remote agent", which wires up ADP to allow the Datadog Agent to query it for status and flare information.
    let in_standalone_mode = config.get_typed_or_default::<bool>("adp.standalone_mode");
    if !in_standalone_mode {
        let local_secure_api_listen_addr = secure_api_listen_address
            .as_local_connect_addr()
            .ok_or_else(|| generic_error!("Failed to get local secure API listen address to advertise."))?;

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

        // Build and spawn our helper task for registering ourselves with the Datadog Agent as a remote agent.
        let remote_agent_config = RemoteAgentHelperConfiguration::from_configuration(
            config,
            local_secure_api_listen_addr,
            prometheus_listen_addr,
        )
        .await?;
        let remote_agent_service = remote_agent_config.spawn().await;

        // Register our Remote Agent gRPC service with the privileged API.
        privileged_api = privileged_api.with_grpc_service(remote_agent_service);
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
