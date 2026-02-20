//! Main benchmarking binary.
//!
//! This binary emulates the standalone DogStatsD binary, listening for DogStatsD over UDS, aggregating metrics over a
//! 10 second window, and shipping those metrics to the Datadog Platform.

#![deny(warnings)]
#![deny(missing_docs)]
use std::{path::PathBuf, time::Instant};

use saluki_app::bootstrap::AppBootstrapper;
use saluki_config::{ConfigurationLoader, GenericConfiguration};
use saluki_error::{ErrorContext as _, GenericError};
use tracing::{error, info, warn};

mod cli;
use self::cli::*;
use crate::internal::platform::PlatformSettings;

mod components;
mod config;
mod env_provider;
mod internal;

pub(crate) mod state;

#[cfg(all(target_os = "linux", not(system_allocator)))]
#[global_allocator]
static ALLOC: memory_accounting::allocator::TrackingAllocator<tikv_jemallocator::Jemalloc> =
    memory_accounting::allocator::TrackingAllocator::new(tikv_jemallocator::Jemalloc);

#[cfg(any(not(target_os = "linux"), system_allocator))]
#[global_allocator]
static ALLOC: memory_accounting::allocator::TrackingAllocator<std::alloc::System> =
    memory_accounting::allocator::TrackingAllocator::new(std::alloc::System);

#[tokio::main]
async fn main() -> Result<(), GenericError> {
    let started = Instant::now();
    let cli: Cli = argh::from_env();

    // Load our "bootstrap" configuration -- static configuration on disk or from environment variables -- so we can
    // initialize basic subsystems before executing the given subcommand.
    let bootstrap_config_path = cli.config_file.unwrap_or_else(PlatformSettings::get_config_file_path);
    let bootstrap_config = ConfigurationLoader::default()
        .from_yaml(&bootstrap_config_path)
        .error_context("Failed to load Datadog Agent configuration file during bootstrap.")?
        .from_environment(PlatformSettings::get_env_var_prefix())
        .error_context("Environment variable prefix should not be empty.")?
        .with_default_secrets_resolution()
        .await
        .error_context("Failed to load secrets resolution configuration during bootstrap.")?
        .bootstrap_generic();

    // Proceed with bootstrapping.
    //
    // This initializes logging, metrics, allocator telemetry, TLS, and more. We get handled a guard that we need to
    // hold until the application is about to exit, which ensures things like flushing any buffered logs, and so on.
    let bootstrapper = AppBootstrapper::from_configuration(&bootstrap_config)
        .error_context("Failed to parse bootstrap configuration during bootstrap phase.")?
        .with_metrics_prefix("adp");
    let _bootstrap_guard = bootstrapper
        .bootstrap()
        .await
        .error_context("Failed to complete bootstrap phase.")?;

    // Run the given subcommand.
    let maybe_exit_code = run_inner(cli.action, started, bootstrap_config_path, bootstrap_config).await?;

    // Drop the bootstrap guard to ensure logs are flushed, etc.
    drop(_bootstrap_guard);

    // Exit with the specific exit code, if one was provided.
    if let Some(exit_code) = maybe_exit_code {
        std::process::exit(exit_code);
    }

    Ok(())
}

async fn run_inner(
    action: Action, started: Instant, bootstrap_config_path: PathBuf, bootstrap_config: GenericConfiguration,
) -> Result<Option<i32>, GenericError> {
    match action {
        Action::Run(cmd) => {
            // Populate our PID file, if configured.
            if let Some(pid_file) = &cmd.pid_file {
                let pid = std::process::id();
                if let Err(e) = std::fs::write(pid_file, pid.to_string()) {
                    error!(error = %e, path = %pid_file.display(), "Failed to update PID file. Exiting.");
                    return Ok(Some(1));
                }
            }

            let exit_code = match handle_run_command(started, bootstrap_config_path, bootstrap_config).await {
                Ok(()) => {
                    info!("Agent Data Plane stopped.");
                    None
                }
                Err(e) => {
                    error!("{:?}", e);
                    Some(1)
                }
            };

            // Remove the PID file, if configured.
            if let Some(pid_file) = &cmd.pid_file {
                if let Err(e) = std::fs::remove_file(pid_file) {
                    warn!(error = %e, path = %pid_file.display(), "Failed to delete PID file while exiting.");
                }
            }

            if let Some(exit_code) = exit_code {
                return Ok(Some(exit_code));
            }
        }
        Action::Debug(cmd) => handle_debug_command(&bootstrap_config, cmd).await,
        Action::Config(_) => handle_config_command(&bootstrap_config).await,
        Action::Dogstatsd(cmd) => handle_dogstatsd_command(&bootstrap_config, cmd).await,
    }

    Ok(None)
}
