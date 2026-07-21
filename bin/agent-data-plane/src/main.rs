//! Main benchmarking binary.
//!
//! This binary emulates the standalone DogStatsD binary, listening for DogStatsD over UDS, aggregating metrics over a
//! 10 second window, and shipping those metrics to the Datadog Platform.

#![deny(warnings)]
#![deny(missing_docs)]
use std::path::{Path, PathBuf};
use std::time::Instant;

// Pull in the Antithesis coverage-instrumentation runtime shim only when
// building for antithesis. Load-baring: equired to avoid the shim being dropped
// as unused.
#[cfg(feature = "antithesis")]
use antithesis_instrumentation as _;
use datadog_agent_commons::platform::PlatformSettings;
// TODO: remove after migration to typed config; bootstrap still feeds legacy flat-key consumers.
use datadog_agent_config::{DatadogRemapper, KEY_ALIASES};
use metrics::Level;
use saluki_app::bootstrap::{AppBootstrapper, Bootstrap, BootstrapGuard};
use saluki_config::{ConfigurationLoader, GenericConfiguration};
use saluki_core::runtime::Supervisor;
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use tracing::{error, info, warn};

mod cli;
use self::cli::*;
use crate::internal::logging::LoggingConfigurationTranslator;

mod components;
mod config;
#[allow(
    dead_code,
    reason = "wired into the DogStatsD CLI and API by subsequent feature tasks"
)]
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

#[tokio::main]
async fn main() -> Result<(), GenericError> {
    let started = Instant::now();

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
    let bootstrap_config = load_bootstrap_config(&bootstrap_config_path)?.bootstrap_generic();

    // Translate the bootstrap configuration into ADP's logging configuration, applying ADP-specific rules
    // (per-subagent log file key, never sharing a file with the Core Agent).
    let bootstrap_logging_config = LoggingConfigurationTranslator::translate(&bootstrap_config)
        .error_context("Failed to translate logging configuration during bootstrap phase.")?;

    let metrics_default_level = parse_metrics_level(&bootstrap_config)?;

    // Proceed with bootstrapping.
    //
    // This initializes logging, metrics, allocator telemetry, TLS, and more. We get handled a guard that we need to
    // hold until the application is about to exit, which ensures things like flushing any buffered logs, and so on.
    let bootstrapper = AppBootstrapper::from_configuration(&bootstrap_config)
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
        bootstrap_config_path,
        bootstrap_config,
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
fn load_bootstrap_config(bootstrap_config_path: &Path) -> Result<ConfigurationLoader, GenericError> {
    let loaded = ConfigurationLoader::default()
        .with_key_aliases(KEY_ALIASES)
        .from_yaml(bootstrap_config_path)
        .error_context("Failed to load Datadog Agent configuration file during bootstrap.")
        .and_then(|loader| {
            loader
                .add_providers([DatadogRemapper::new()])
                .from_environment(PlatformSettings::get_env_var_prefix())
                .error_context("Environment variable prefix should not be empty.")
        });
    // A graceful config rejection exits 1 rather than crashing; classify that against a clean boot.
    saluki_antithesis::always_or_unreachable!(
        loaded.is_ok(),
        "agent-data-plane boots under sampled config",
        { "phase": "config_load", "error": loaded.as_ref().err().map(|e| format!("{e:?}")) }
    );
    loaded
}

fn parse_metrics_level(config: &GenericConfiguration) -> Result<Level, GenericError> {
    let raw = config
        .try_get_typed::<String>("metrics_level")
        .error_context("Failed to read `metrics_level`.")?;
    match raw {
        Some(value) => {
            Level::try_from(value.as_str()).map_err(|e| generic_error!("Failed to parse `metrics_level`: {}", e))
        }
        None => Ok(Level::INFO),
    }
}

async fn run_inner(
    action: Action, started: Instant, config_path: PathBuf, bootstrap_config: GenericConfiguration,
    bootstrap_guard: &mut BootstrapGuard, bootstrap_supervisor: Supervisor,
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

            // `Run` owns the full configuration lifecycle, so it takes the config path and builds the
            // loader itself; the static `bootstrap_config` serves only the other subcommands below.
            let exit_code = match handle_run_command(started, config_path, bootstrap_guard, bootstrap_supervisor).await
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
        Action::Debug(cmd) => handle_debug_command(&bootstrap_config, cmd).await,
        Action::Config(_) => handle_config_command(&bootstrap_config).await,
        Action::Dogstatsd(cmd) => handle_dogstatsd_command(&bootstrap_config, cmd).await,
        Action::Version(v) => handle_version_command(v.json).await,
    }

    Ok(None)
}
