use tracing::{error, info};

use crate::{
    cli::utils::APIClient,
    config::{DebugConfig, SetLogLevelConfig},
};

/// Entrypoint for all `debug` subcommands.
pub async fn handle_debug_command(config: DebugConfig) {
    match config {
        DebugConfig::ResetLogLevel => reset_log_level().await,
        DebugConfig::SetLogLevel(config) => set_log_level(config).await,
    }
}

/// Resets the log level to the default configuration.
async fn reset_log_level() {
    let api_client = APIClient::new();
    match api_client.reset_log_level().await {
        Ok(()) => info!("Log level reset successful."),
        Err(e) => {
            error!("Failed to reset log level: {}", e);
            std::process::exit(1);
        }
    }
}

/// Sets the log level filter directives for a specified duration in seconds.
async fn set_log_level(config: SetLogLevelConfig) {
    let api_client = APIClient::new();
    match api_client
        .set_log_level(config.filter_directives, config.duration_secs)
        .await
    {
        Ok(()) => info!("Log level override successful."),
        Err(e) => {
            error!("Failed to override log level: {}", e);
            std::process::exit(1);
        }
    }
}
