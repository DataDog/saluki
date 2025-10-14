use saluki_config::GenericConfiguration;
use tracing::{error, info};

use crate::{
    cli::utils::ControlPlaneAPIClient,
    config::{DebugConfig, SetLogLevelConfig, SetMetricLevelConfig},
};

mod workload;
use self::workload::handle_workload_command;

/// Entrypoint for all `debug` subcommands.
pub async fn handle_debug_command(bootstrap_config: &GenericConfiguration, config: DebugConfig) {
    let api_client = match ControlPlaneAPIClient::from_config(bootstrap_config) {
        Ok(client) => client,
        Err(e) => {
            error!("Failed to create control plane API client: {:#}", e);
            std::process::exit(1);
        }
    };

    match config {
        DebugConfig::ResetLogLevel => reset_log_level(api_client).await,
        DebugConfig::SetLogLevel(config) => set_log_level(api_client, config).await,
        DebugConfig::ResetMetricLevel => reset_metric_level(api_client).await,
        DebugConfig::SetMetricLevel(config) => set_metric_level(api_client, config).await,
        DebugConfig::Workload(config) => handle_workload_command(api_client, config).await,
    }
}

/// Resets the log level to the default configuration.
async fn reset_log_level(api_client: ControlPlaneAPIClient) {
    match api_client.reset_log_level().await {
        Ok(()) => info!("Log level reset successful."),
        Err(e) => {
            error!("Failed to reset log level: {:#}", e);
            std::process::exit(1);
        }
    }
}

/// Sets the log level filter directives for a specified duration in seconds.
async fn set_log_level(api_client: ControlPlaneAPIClient, config: SetLogLevelConfig) {
    match api_client
        .set_log_level(config.filter_directives, config.duration_secs)
        .await
    {
        Ok(()) => info!("Log level override successful."),
        Err(e) => {
            error!("Failed to override log level: {:#}", e);
            std::process::exit(1);
        }
    }
}

/// Resets the metric level to the default configuration.
async fn reset_metric_level(api_client: ControlPlaneAPIClient) {
    match api_client.reset_metric_level().await {
        Ok(()) => info!("Metric level reset successful."),
        Err(e) => {
            error!("Failed to reset metric level: {:#}", e);
            std::process::exit(1);
        }
    }
}

/// Sets the metric level filter directive for a specified duration in seconds.
async fn set_metric_level(api_client: ControlPlaneAPIClient, config: SetMetricLevelConfig) {
    match api_client.set_metric_level(config.level, config.duration_secs).await {
        Ok(()) => info!("Metric level override successful."),
        Err(e) => {
            error!("Failed to override metric level: {:#}", e);
            std::process::exit(1);
        }
    }
}
