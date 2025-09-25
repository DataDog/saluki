use tracing::{error, info};

use crate::{
    cli::utils::APIClient,
    config::{DebugConfig, SetLogLevelConfig, SetMetricLevelConfig},
};

mod workload;
use self::workload::handle_workload_command;

/// Entrypoint for all `debug` subcommands.
pub async fn handle_debug_command(config: DebugConfig) {
    match config {
        DebugConfig::ResetLogLevel => reset_log_level().await,
        DebugConfig::SetLogLevel(config) => set_log_level(config).await,
        DebugConfig::ResetMetricLevel => reset_metric_level().await,
        DebugConfig::SetMetricLevel(config) => set_metric_level(config).await,
        DebugConfig::Workload(config) => handle_workload_command(config).await,
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

/// Resets the metric level to the default configuration.
async fn reset_metric_level() {
    let api_client = APIClient::new();
    match api_client.reset_metric_level().await {
        Ok(()) => info!("Metric level reset successful."),
        Err(e) => {
            error!("Failed to reset metric level: {}", e);
            std::process::exit(1);
        }
    }
}

/// Sets the metric level filter directive for a specified duration in seconds.
async fn set_metric_level(config: SetMetricLevelConfig) {
    let api_client = APIClient::new();
    match api_client.set_metric_level(config.level, config.duration_secs).await {
        Ok(()) => info!("Metric level override successful."),
        Err(e) => {
            error!("Failed to override metric level: {}", e);
            std::process::exit(1);
        }
    }
}
