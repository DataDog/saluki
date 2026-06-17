use argh::FromArgs;
use saluki_common::scrubber;
use saluki_config_tools::GenericConfiguration;
use tracing::{error, info};

use crate::cli::utils::DataPlaneAPIClient;

/// Prints the current configuration.
#[derive(FromArgs, Debug)]
#[argh(subcommand, name = "config")]
pub struct ConfigCommand {}

/// Entrypoint for the `config` command.
pub async fn handle_config_command(bootstrap_config: &GenericConfiguration) {
    let mut api_client = match DataPlaneAPIClient::from_config(bootstrap_config) {
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

    let scrubber = scrubber::default_scrubber();
    let scrubbed_bytes = scrubber.scrub_bytes(response_body.as_bytes());

    // The privileged `/config` endpoint returns JSON (`ConfigAPIHandler`); parse as JSON after scrubbing.
    let config_value: serde_json::Value = match serde_json::from_slice(&scrubbed_bytes) {
        Ok(v) => v,
        Err(e) => {
            error!(
                "Failed to parse configuration response as JSON after scrubbing (malformed payload or scrubber bug): {:#}",
                e
            );
            std::process::exit(1);
        }
    };
    let formatted = match serde_json::to_string_pretty(&config_value) {
        Ok(s) => s,
        Err(e) => {
            error!("Failed to format configuration response as JSON: {:#}", e);
            std::process::exit(1);
        }
    };

    info!("Full configuration:\n{}", formatted);
}
