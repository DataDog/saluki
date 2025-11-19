use argh::FromArgs;
use saluki_config::GenericConfiguration;
use tracing::{error, info};

use crate::cli::utils::ControlPlaneAPIClient;

mod workload;
use self::workload::{handle_workload_command, WorkloadCommand};

/// Debug command.
#[derive(FromArgs, Debug)]
#[argh(subcommand, name = "debug")]
pub struct DebugCommand {
    #[argh(subcommand)]
    subcommand: DebugSubcommand,
}

/// Debug subcommand.
#[derive(FromArgs, Debug)]
#[argh(subcommand)]
pub enum DebugSubcommand {
    /// Resets log level.
    ResetLogLevel(ResetLogLevelCommand),

    /// Overrides the current log level.
    SetLogLevel(SetLogLevelCommand),

    /// Resets metric level.
    ResetMetricLevel(ResetMetricLevelCommand),

    /// Overrides the current metric level.
    SetMetricLevel(SetMetricLevelCommand),

    /// Query and interact with the workload provider.
    Workload(WorkloadCommand),
}

/// Reset log level command.
#[derive(FromArgs, Debug)]
#[argh(subcommand, name = "reset-log-level")]
pub struct ResetLogLevelCommand {}

/// Set log level command.
#[derive(FromArgs, Debug)]
#[argh(subcommand, name = "set-log-level")]
pub struct SetLogLevelCommand {
    /// filter directives to apply (e.g. `INFO`, `DEBUG`, `TRACE`, `WARN`, `ERROR`).
    #[argh(option)]
    pub filter_directives: String,

    /// amount of time to apply the log level override, in seconds.
    #[argh(option)]
    pub duration_secs: u64,
}

/// Reset metric level command.
#[derive(FromArgs, Debug)]
#[argh(subcommand, name = "reset-metric-level")]
pub struct ResetMetricLevelCommand {}

/// Set metric level command.
#[derive(FromArgs, Debug)]
#[argh(subcommand, name = "set-metric-level")]
pub struct SetMetricLevelCommand {
    /// metric level filter to apply (e.g. `INFO`, `DEBUG`, `TRACE`, `WARN`, `ERROR`).
    #[argh(option)]
    pub level: String,

    /// amount of time to apply the metric level override, in seconds.
    #[argh(option)]
    pub duration_secs: u64,
}

/// Entrypoint for all `debug` subcommands.
pub async fn handle_debug_command(bootstrap_config: &GenericConfiguration, cmd: DebugCommand) {
    let api_client = match ControlPlaneAPIClient::from_config(bootstrap_config) {
        Ok(client) => client,
        Err(e) => {
            error!("Failed to create control plane API client: {:#}", e);
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
async fn set_log_level(api_client: ControlPlaneAPIClient, cmd: SetLogLevelCommand) {
    match api_client.set_log_level(cmd.filter_directives, cmd.duration_secs).await {
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
async fn set_metric_level(api_client: ControlPlaneAPIClient, cmd: SetMetricLevelCommand) {
    match api_client.set_metric_level(cmd.level, cmd.duration_secs).await {
        Ok(()) => info!("Metric level override successful."),
        Err(e) => {
            error!("Failed to override metric level: {:#}", e);
            std::process::exit(1);
        }
    }
}
