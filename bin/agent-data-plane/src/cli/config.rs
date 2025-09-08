use saluki_common::scrubber;
use tracing::{error, info};

use crate::cli::utils::APIClient;

/// Entrypoint for the `config` subcommand.
pub async fn handle_config_command() {
    let api_client = APIClient::new();
    let response_body = match api_client.config().await {
        Ok(body) => body,
        Err(e) => {
            error!("Failed to get configuration: {}.", e);
            std::process::exit(1);
        }
    };

    let scrubber = scrubber::default_scrubber();
    let scrubbed_bytes = scrubber.scrub_bytes(response_body.as_bytes());

    let yaml_value: serde_yaml::Value = serde_yaml::from_slice(&scrubbed_bytes).unwrap();
    let yaml = serde_yaml::to_string(&yaml_value).unwrap_or_default();

    info!("\n{}", yaml);
}
