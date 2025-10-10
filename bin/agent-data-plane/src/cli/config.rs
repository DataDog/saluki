use saluki_common::scrubber;
use saluki_config::GenericConfiguration;
use tracing::{error, info};

use crate::cli::utils::ControlPlaneAPIClient;

/// Entrypoint for the `config` subcommand.
pub async fn handle_config_command(bootstrap_config: &GenericConfiguration) {
    let api_client = match ControlPlaneAPIClient::from_config(bootstrap_config) {
        Ok(client) => client,
        Err(e) => {
            error!("Failed to create control plane API client: {:#}", e);
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

    let yaml_value: serde_yaml::Value = serde_yaml::from_slice(&scrubbed_bytes).unwrap();
    let yaml = serde_yaml::to_string(&yaml_value).unwrap_or_default();

    info!("Full configuration:\n{}", yaml);
}
