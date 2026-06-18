use agent_data_plane_config::BootstrapConfiguration;
use argh::FromArgs;
use tracing::{error, info};

use crate::cli::utils::DataPlaneAPIClient;

/// Prints the current configuration.
#[derive(FromArgs, Debug)]
#[argh(subcommand, name = "config")]
pub struct ConfigCommand {}

/// Entrypoint for the `config` command.
///
/// Queries the running ADP's `/config` endpoint over HTTP. The endpoint already returns a scrubbed,
/// source-shaped view (an alias for `/config/raw`); the CLI just prints it.
pub async fn handle_config_command(bootstrap: &BootstrapConfiguration) {
    let mut api_client = match DataPlaneAPIClient::from_config(bootstrap) {
        Ok(client) => client,
        Err(e) => {
            error!("Failed to create data plane API client: {:#}", e);
            std::process::exit(1);
        }
    };

    let response_body = match api_client.config().await {
        Ok(body) => body,
        Err(e) => {
            error!("Failed to get configuration: {:#}.", e);
            std::process::exit(1);
        }
    };

    info!("Full configuration:\n{}", response_body);
}
