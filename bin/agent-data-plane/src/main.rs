//! Main benchmarking binary.
//!
//! This binary emulates the standalone DogStatsD binary, listening for DogStatsD over UDS, aggregating metrics over a
//! 10 second window, and shipping those metrics to the Datadog Platform.

#![deny(warnings)]
#![deny(missing_docs)]
use std::time::Instant;

use clap::Parser as _;
use saluki_app::bootstrap::AppBootstrapper;
use saluki_config::ConfigurationLoader;
use saluki_error::{ErrorContext as _, GenericError};
use tracing::{error, info, warn};

mod components;
mod config;
use self::config::{Action, Cli};

mod env_provider;
mod internal;

mod cli;
use self::cli::{
    config::handle_config_command, debug::handle_debug_command, dogstatsd::handle_dogstatsd_subcommand, run::run,
};

pub(crate) mod state;

// TODO: Consider using `malloc-best-effort` to choose between TCMalloc on Linux and Mimalloc on other platforms
// (notably Windows) so that we can use the best supported allocator regardless of platform.
#[cfg(target_os = "linux")]
#[global_allocator]
static ALLOC: memory_accounting::allocator::TrackingAllocator<tcmalloc_better::TCMalloc> =
    memory_accounting::allocator::TrackingAllocator::new(tcmalloc_better::TCMalloc);

#[cfg(not(target_os = "linux"))]
#[global_allocator]
static ALLOC: memory_accounting::allocator::TrackingAllocator<std::alloc::System> =
    memory_accounting::allocator::TrackingAllocator::new(std::alloc::System);

#[tokio::main]
async fn main() -> Result<(), GenericError> {
    #[cfg(target_os = "linux")]
    tcmalloc_better::TCMalloc::process_background_actions_thread();

    let started = Instant::now();
    let cli = Cli::parse();

    // Load our "bootstrap" configuration -- static configuration on disk or from environment variables -- so we can
    // initialize basic subsystems before executing the given subcommand.
    let bootstrap_config = ConfigurationLoader::default()
        .try_from_yaml(
            cli.config_file
                .unwrap_or_else(|| self::internal::platform::DATADOG_AGENT_CONF_YAML.into()),
        )
        .from_environment(self::internal::platform::DATADOG_AGENT_ENV_VAR_PREFIX)
        .error_context("Environment variable prefix should not be empty.")?
        .with_default_secrets_resolution()
        .await
        .error_context("Failed to load secrets resolution configuration.")?
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
    match cli.action {
        Action::Run(config) => {
            // Populate our PID file, if configured.
            if let Some(pid_file) = &config.pid_file {
                let pid = std::process::id();
                if let Err(e) = std::fs::write(pid_file, pid.to_string()) {
                    error!(error = %e, path = %pid_file.display(), "Failed to update PID file. Exiting.");
                    std::process::exit(1);
                }
            }

            let exit_code = match run(started, bootstrap_config).await {
                Ok(()) => {
                    info!("Agent Data Plane stopped.");
                    0
                }
                Err(e) => {
                    error!("{:?}", e);
                    1
                }
            };

            // Remove the PID file, if configured.
            if let Some(pid_file) = &config.pid_file {
                if let Err(e) = std::fs::remove_file(pid_file) {
                    warn!(error = %e, path = %pid_file.display(), "Failed to delete PID file while exiting.");
                }
            }

            std::process::exit(exit_code);
        }
        Action::Debug(config) => handle_debug_command(&bootstrap_config, config).await,
        Action::Config => handle_config_command(&bootstrap_config).await,
        Action::Dogstatsd(config) => handle_dogstatsd_subcommand(&bootstrap_config, config).await,
    }

    Ok(())
}
