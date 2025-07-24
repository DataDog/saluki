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
        let text = response.text().await.unwrap();
        let json: serde_json::Value = serde_json::from_str(&text).unwrap();
        info!(
            "Config retrieved successfully:\n{}",
            serde_json::to_string_pretty(&json).unwrap()
        );
    } else {
        error!("Failed to retrieve config: {}.", response.status());
    }
}
