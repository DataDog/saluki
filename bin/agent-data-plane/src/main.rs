//! Main benchmarking binary.
//!
//! This binary emulates the standalone DogStatsD binary, listening for DogStatsD over UDS, aggregating metrics over a
//! 10 second window, and shipping those metrics to the Datadog Platform.

#![deny(warnings)]
#![deny(missing_docs)]
use std::{path::Path, time::Instant};

use agent_data_plane_config_system::{EnvPrecedence, LoadedConfiguration};
// Pull in the Antithesis coverage-instrumentation runtime shim only when
// building for antithesis. Load-baring: equired to avoid the shim being dropped
// as unused.
#[cfg(feature = "antithesis")]
use antithesis_instrumentation as _;
use datadog_agent_commons::platform::PlatformSettings;
use metrics::Level;
use saluki_app::bootstrap::{AppBootstrapper, Bootstrap, BootstrapGuard};
use saluki_core::runtime::Supervisor;
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use saluki_metadata::AppDetails;
use tracing::{error, info, warn};

mod cli;
use self::cli::*;
use crate::internal::logging::LoggingConfigurationTranslator;

mod components;
mod config;
mod dogstatsd_contexts;
mod internal;

pub(crate) mod state;

#[cfg(all(target_os = "linux", not(system_allocator)))]
#[global_allocator]
static ALLOC: saluki_common::resource_tracking::TrackingAllocator<tikv_jemallocator::Jemalloc> =
    saluki_common::resource_tracking::TrackingAllocator::new(tikv_jemallocator::Jemalloc);

#[cfg(any(not(target_os = "linux"), system_allocator))]
#[global_allocator]
static ALLOC: saluki_common::resource_tracking::TrackingAllocator<std::alloc::System> =
    saluki_common::resource_tracking::TrackingAllocator::new(std::alloc::System);

/// Identity of this application.
///
/// Registered at the very start of `main`, which is what makes it visible to the libraries ADP is built from.
const APP_DETAILS: AppDetails = saluki_metadata::declare_app_details!(
    full_name = "Agent Data Plane",
    short_name = "data-plane",
    identifier = "adp",
);

#[tokio::main]
async fn main() -> Result<(), GenericError> {
    let started = Instant::now();

    // Register who we are before anything else, so that everything from here on -- including code that runs before
    // bootstrap -- reports this application rather than an unknown one.
    saluki_metadata::set_app_details(APP_DETAILS);

    #[cfg(feature = "antithesis")]
    initialize_antithesis();

    let cli: Cli = argh::from_env();

    // Print version and exit early without requiring config.
    if let Action::Version(v) = &cli.action {
        handle_version_command(v.json).await;
        return Ok(());
    }

    // Load our "bootstrap" configuration -- static configuration on disk or from environment variables -- so we can
    // initialize basic subsystems before executing the given subcommand.
    let bootstrap_config_path = cli.config_file.unwrap_or_else(PlatformSettings::get_config_file_path);
    let local_config = load_bootstrap_config(&bootstrap_config_path).await?;

    // Translate the bootstrap configuration into ADP's logging configuration, applying ADP-specific rules
    // (per-subagent log file key, never sharing a file with the Core Agent).
    let mut bootstrap_logging_config = LoggingConfigurationTranslator::translate(&local_config.local().control.logging)
        .error_context("Failed to translate logging configuration during bootstrap phase.")?;
    if matches!(&cli.action, Action::Config(command) if command.json) {
        bootstrap_logging_config.log_to_console = false;
    }

    let metrics_default_level = parse_metrics_level(&local_config.local().shared.metrics_level)?;

    // Proceed with bootstrapping.
    //
    // This initializes logging, metrics, allocator telemetry, TLS, and more. We get handled a guard that we need to
    // hold until the application is about to exit, which ensures things like flushing any buffered logs, and so on.
    let bootstrapper = AppBootstrapper::from_configuration(&local_config.raw_config())
        .error_context("Failed to parse bootstrap configuration during bootstrap phase.")?
        .with_metrics_prefix("adp")
        .with_metrics_default_level(metrics_default_level)
        .with_logging_configuration(bootstrap_logging_config);
    let Bootstrap {
        supervisor: bootstrap_supervisor,
        guard: mut bootstrap_guard,
    } = bootstrapper
        .bootstrap()
        .await
        .error_context("Failed to complete bootstrap phase.")?;

    // Bootstrap-integration probe: proves the Antithesis SDK is linked, cataloging works, and the
    // instrumentation path is wired.
    saluki_antithesis::reachable!("agent-data-plane completed bootstrap");

    // Run the given subcommand. The bootstrap supervisor is forwarded by value; only the long-lived `run`
    // subcommand actually drives it (it is added as a child of the internal supervisor inside
    // `handle_run_command`). All other subcommands drop it on entry.
    let maybe_exit_code = run_inner(
        cli.action,
        started,
        local_config,
        &mut bootstrap_guard,
        bootstrap_supervisor,
    )
    .await?;

    // Drop the bootstrap guard to ensure logs are flushed, etc.
    drop(bootstrap_guard);

    // Exit with the specific exit code, if one was provided.
    if let Some(exit_code) = maybe_exit_code {
        std::process::exit(exit_code);
    }

    Ok(())
}

/// Initializes the Antithesis SDK and installs a panic-reporting hook. Set
/// ideally before any panics are possible.
#[cfg(feature = "antithesis")]
fn initialize_antithesis() {
    saluki_antithesis::init();

    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let location = info.location().map_or_else(String::new, |l| l.to_string());
        let payload = info.payload();
        let message = payload
            .downcast_ref::<&str>()
            .map(|s| (*s).to_string())
            .or_else(|| payload.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "<non-string panic payload>".to_string());
        saluki_antithesis::unreachable!(
            "agent-data-plane panicked",
            { "message": message, "location": location }
        );
        default_hook(info);
    }));
}

/// Loads bootstrap configuration from the on-disk file and environment
/// variables.
async fn load_bootstrap_config(bootstrap_config_path: &Path) -> Result<LoadedConfiguration, GenericError> {
    let loaded = LoadedConfiguration::load(bootstrap_config_path, EnvPrecedence::AfterFile)
        .await
        .with_error_context(|| {
            format!(
                "Failed to load local configuration from {} and the environment",
                bootstrap_config_path.display()
            )
        });
    // A graceful config rejection exits 1 rather than crashing; classify that against a clean boot.
    saluki_antithesis::always_or_unreachable!(
        loaded.is_ok(),
        "agent-data-plane boots under sampled config",
        { "phase": "config_load", "error": loaded.as_ref().err().map(|e| format!("{e:?}")) }
    );
    loaded
}

fn parse_metrics_level(level: &str) -> Result<Level, GenericError> {
    Level::try_from(level).map_err(|e| generic_error!("Failed to parse `metrics_level`: {}", e))
}

async fn run_inner(
    action: Action, started: Instant, local_config: LoadedConfiguration, bootstrap_guard: &mut BootstrapGuard,
    bootstrap_supervisor: Supervisor,
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

            // `Run` consumes the local configuration and selects its runtime authority.
            let exit_code = match handle_run_command(started, local_config, bootstrap_guard, bootstrap_supervisor).await
            {
                Ok(()) => {
                    info!("Agent Data Plane stopped.");
                    None
                }
                Err(e) => {
                    error!("{:?}", e);
                    // Same boot property as the config-load gate, distinguished by `phase` in the details.
                    saluki_antithesis::always_or_unreachable!(
                        false,
                        "agent-data-plane boots under sampled config",
                        { "phase": "run_setup", "error": format!("{e:?}") }
                    );
                    Some(1)
                }
            };

            // Remove the PID file, if configured.
            if let Some(pid_file) = &cmd.pid_file {
                if let Err(e) = std::fs::remove_file(pid_file) {
                    warn!(error = %e, path = %pid_file.display(), "Failed to delete PID file while exiting.");
                }
            }

            return Ok(exit_code);
        }
        Action::Debug(cmd) => handle_debug_command(local_config, cmd).await,
        Action::Config(cmd) => handle_config_command(local_config, cmd).await,
        Action::Dogstatsd(cmd) => handle_dogstatsd_command(local_config, cmd).await,
        // Handled before bootstrap, so that reporting the version never depends on there being usable configuration.
        Action::Version(_) => unreachable!("version is handled before bootstrap"),
    }

    Ok(None)
}

#[cfg(test)]
mod tests {
    use metrics::Level;

    use super::parse_metrics_level;

    #[test]
    fn parse_metrics_level_accepts_only_a_known_level() {
        assert_eq!(parse_metrics_level("debug").expect("known level parses"), Level::DEBUG);

        for level in ["", "verbose"] {
            let error = parse_metrics_level(level).expect_err("unrecognized level is rejected");
            assert!(
                error.to_string().contains("metrics_level"),
                "error should name the setting: {error}"
            );
        }
    }
}
