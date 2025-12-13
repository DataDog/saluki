use argh::FromArgs;
use saluki_config::GenericConfiguration;
use tracing::{error, info};

use crate::cli::utils::DataPlaneAPIClient;

mod workload;
use self::workload::{handle_workload_command, WorkloadCommand};

/// General debugging commands.
#[derive(FromArgs, Debug)]
#[argh(subcommand, name = "debug")]
pub struct DebugCommand {
    #[argh(subcommand)]
    subcommand: DebugSubcommand,
}

#[derive(FromArgs, Debug)]
#[argh(subcommand)]
enum DebugSubcommand {
    ResetLogLevel(ResetLogLevelCommand),
    SetLogLevel(SetLogLevelCommand),
    ResetMetricLevel(ResetMetricLevelCommand),
    SetMetricLevel(SetMetricLevelCommand),
    Workload(WorkloadCommand),
}

/// Resets the log level.
#[derive(FromArgs, Debug)]
#[argh(subcommand, name = "reset-log-level")]
pub struct ResetLogLevelCommand {}

/// Overrides the current log level.
#[derive(FromArgs, Debug)]
#[argh(subcommand, name = "set-log-level")]
pub struct SetLogLevelCommand {
    /// filter directives to apply (e.g. `INFO`, `DEBUG`, `TRACE`, `WARN`, `ERROR`)
    #[argh(option)]
    pub filter_directives: String,

    /// amount of time to apply the log level override, in seconds
    #[argh(option)]
    pub duration_secs: u64,
}

/// Resets the metric level.
#[derive(FromArgs, Debug)]
#[argh(subcommand, name = "reset-metric-level")]
pub struct ResetMetricLevelCommand {}

/// Overrides the current metric level.
#[derive(FromArgs, Debug)]
#[argh(subcommand, name = "set-metric-level")]
pub struct SetMetricLevelCommand {
    /// metric level filter to apply (e.g. `INFO`, `DEBUG`, `TRACE`, `WARN`, `ERROR`)
    #[argh(option)]
    pub level: String,

    /// amount of time to apply the metric level override, in seconds
    #[argh(option)]
    pub duration_secs: u64,
}

/// Entrypoint for the `debug` commands.
pub async fn handle_debug_command(bootstrap_config: &GenericConfiguration, cmd: DebugCommand) {
    let api_client = match DataPlaneAPIClient::from_config(bootstrap_config) {
        Ok(client) => client,
        Err(e) => {
            error!("Failed to create data plane API client: {:#}", e);
            std::process::exit(1);
        }
    };

    match cmd.subcommand {
        DebugSubcommand::ResetLogLevel(_) => reset_log_level(api_client).await,
        DebugSubcommand::SetLogLevel(cmd) => set_log_level(api_client, cmd).await,
        DebugSubcommand::ResetMetricLevel(_) => reset_metric_level(api_client).await,
        DebugSubcommand::SetMetricLevel(cmd) => set_metric_level(api_client, cmd).await,
        DebugSubcommand::Workload(cmd) => handle_workload_command(api_client, cmd).await,
    }
}

/// Resets the log level to the default configuration.
async fn reset_log_level(api_client: DataPlaneAPIClient) {
    match api_client.reset_log_level().await {
        Ok(()) => info!("Log level reset successful."),
        Err(e) => {
            error!("Failed to reset log level: {:#}", e);
            std::process::exit(1);
        }
    }
}

/// Sets the log level filter directives for a specified duration in seconds.
async fn set_log_level(api_client: DataPlaneAPIClient, cmd: SetLogLevelCommand) {
    match api_client.set_log_level(cmd.filter_directives, cmd.duration_secs).await {
        Ok(()) => info!("Log level override successful."),
        Err(e) => {
            error!("Failed to override log level: {:#}", e);
            std::process::exit(1);
        }
    }
}

/// Resets the metric level to the default configuration.
async fn reset_metric_level(api_client: DataPlaneAPIClient) {
    match api_client.reset_metric_level().await {
        Ok(()) => info!("Metric level reset successful."),
        Err(e) => {
            error!("Failed to reset metric level: {:#}", e);
            std::process::exit(1);
        }
    }
}

/// Sets the metric level filter directive for a specified duration in seconds.
async fn set_metric_level(api_client: DataPlaneAPIClient, cmd: SetMetricLevelCommand) {
    match api_client.set_metric_level(cmd.level, cmd.duration_secs).await {
        Ok(()) => info!("Metric level override successful."),
        Err(e) => {
            error!("Failed to override metric level: {:#}", e);
            std::process::exit(1);
        }
    }
}
