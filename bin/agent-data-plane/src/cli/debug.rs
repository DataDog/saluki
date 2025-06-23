use tracing::error;

use crate::config::{DebugConfig, SetLogLevelConfig};

pub async fn handle_debug_command(config: DebugConfig) {
    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .unwrap();

    match config {
        DebugConfig::ResetLogLevel => {
            reset_log_level(client).await;
        }
        DebugConfig::SetLogLevel(config) => {
            set_log_level(client, config).await;
        }
    }
}

/// Resets the log level to the default configuration.
async fn reset_log_level(client: reqwest::Client) {
    let response = match client.post("https://localhost:5101/logging/reset").send().await {
        Ok(resp) => resp,
        Err(e) => {
            error!("Failed to send request: {}", e);
            std::process::exit(1);
        }
    };

    if response.status().is_success() {
        println!("Log level reset successful");
    } else {
        eprintln!("Failed to reset log level: {}", response.status());
    }
}

/// Sets the log level filter directives for a specified duration in seconds.
async fn set_log_level(client: reqwest::Client, config: SetLogLevelConfig) {
    let filter_directives = config.filter_directives;
    let duration_secs = config.duration_secs;

    let response = match client
        .post("https://localhost:5101/logging/override")
        .query(&[("time_secs", duration_secs)])
        .body(filter_directives)
        .send()
        .await
    {
        Ok(resp) => resp,
        Err(e) => {
            error!("Failed to send request: {}", e);
            std::process::exit(1);
        }
    };

    if response.status().is_success() {
        println!("Log level override successful");
    } else {
        eprintln!("Failed to override log level: {}", response.status());
    }
}
