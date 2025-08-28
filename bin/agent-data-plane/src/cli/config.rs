use saluki_common::scrubber;
use tracing::{error, info};

/// Handles the config subcommand.
pub async fn handle_config_command() {
    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .unwrap();

    get_config(client).await;
}

/// Gets the current configuration.
async fn get_config(client: reqwest::Client) {
    let response = match client.get("https://localhost:5101/config").send().await {
        Ok(resp) => resp,
        Err(e) => {
            error!("Failed to send request: {}.", e);
            std::process::exit(1);
        }
    };

    if response.status().is_success() {
        let bytes = response.bytes().await.unwrap_or_default();

        let scrubber = scrubber::default_scrubber();
        let scrubbed_bytes = scrubber.scrub_bytes(&bytes).await;

        let yaml_value: serde_yaml::Value = serde_yaml::from_slice(&scrubbed_bytes).unwrap();
        let yaml = serde_yaml::to_string(&yaml_value).unwrap_or_default();

        info!("\n{}", yaml);
    } else {
        error!("Failed to retrieve config: {}.", response.status());
    }
}
