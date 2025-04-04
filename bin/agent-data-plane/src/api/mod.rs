use std::future::pending;

use saluki_app::api::APIBuilder;
use saluki_config::GenericConfiguration;
use saluki_core::state::reflector::Reflector;
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use saluki_io::net::ListenAddress;
use tracing::{error, info};

mod remote_agent;
use self::remote_agent::RemoteAgentHelperConfiguration;
use crate::state::metrics::AggregatedMetricsProcessor;

const PRIMARY_UNPRIVILEGED_API_PORT: u16 = 5100;
const PRIMARY_PRIVILEGED_API_PORT: u16 = 5101;

pub async fn configure_and_spawn_api_endpoints(
    config: &GenericConfiguration, internal_metrics: Reflector<AggregatedMetricsProcessor>,
    mut unprivileged_api: APIBuilder, mut privileged_api: APIBuilder,
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

        // Build and spawn our helper task for registering ourselves with the Datadog Agent as a remote agent.
        let remote_agent_config =
            RemoteAgentHelperConfiguration::from_configuration(config, local_secure_api_listen_addr, internal_metrics)
                .await?;
        let remote_agent_service = remote_agent_config.spawn().await;

        // Register our Remote Agent gRPC service with the privileged API.
        privileged_api = privileged_api.with_grpc_service(remote_agent_service);
    }

    // Configure runtime tracing with `console_subscriber`.
    let console_server_parts = saluki_app::logging::get_console_server_parts()
        .ok_or(generic_error!("Console subscriber server parts already consumed."))?;

    tokio::spawn(console_server_parts.aggregator.run());

    unprivileged_api = unprivileged_api.with_grpc_service(console_server_parts.instrument_server);

    // Finally, spawn the APIs.
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
    tokio::spawn(async move {
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
    tokio::spawn(async move {
        info!("Serving privileged API on {}.", api_listen_address);

        if let Err(e) = api_builder.serve(api_listen_address, pending()).await {
            error!("Failed to serve privileged API: {}", e);
        }
    });

    Ok(())
}
