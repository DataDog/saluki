use std::future::pending;

use saluki_app::api::APIBuilder;
use saluki_config::GenericConfiguration;
use saluki_error::{ErrorContext as _, GenericError};
use saluki_io::net::ListenAddress;
use tracing::{error, info};

const PRIMARY_UNPRIVILEGED_API_PORT: u16 = 5100;
const PRIMARY_PRIVILEGED_API_PORT: u16 = 5101;

pub async fn configure_and_spawn_api_endpoints(config: &GenericConfiguration, unprivileged_api: APIBuilder, privileged_api: APIBuilder) -> Result<(), GenericError> {
	spawn_unprivileged_api(config, unprivileged_api).await?;
	spawn_privileged_api(config, privileged_api).await?;

	Ok(())
}

async fn spawn_unprivileged_api(
    configuration: &GenericConfiguration, api_builder: APIBuilder,
) -> Result<(), GenericError> {
    let api_listen_address = configuration
        .try_get_typed("api_listen_address")
        .error_context("Failed to get API listen address.")?
        .unwrap_or_else(|| ListenAddress::any_tcp(PRIMARY_UNPRIVILEGED_API_PORT));

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

async fn spawn_privileged_api(
    configuration: &GenericConfiguration, api_builder: APIBuilder,
) -> Result<(), GenericError> {
    let api_listen_address = configuration
        .try_get_typed("secure_api_listen_address")
        .error_context("Failed to get secure API listen address.")?
        .unwrap_or_else(|| ListenAddress::any_tcp(PRIMARY_PRIVILEGED_API_PORT));

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
