//! Check runner binary.
//!
//! This binary runs a single Datadog check configured via stdin and forwards results
//! to the Datadog platform.

#![deny(warnings)]
#![deny(missing_docs)]

use std::time::{Duration, Instant};

use memory_accounting::{ComponentRegistry, MemoryLimiter};
use saluki_app::bootstrap::AppBootstrapper;
use saluki_config::ConfigurationLoader;
use saluki_error::{ErrorContext as _, GenericError};
use saluki_health::HealthRegistry;
use tokio::select;
use tracing::{error, info};

mod cli;
mod config;
mod env_provider;
mod single_check_autodiscovery;
mod topology;

use self::cli::Cli;
use self::config::{CheckRunnerInput, ExecutionMode};
use self::env_provider::CheckRunnerEnvironmentProvider;
use self::single_check_autodiscovery::SingleCheckAutodiscoveryProvider;
use self::topology::create_topology;

/// Default path to the Datadog Agent configuration file.
#[cfg(target_os = "linux")]
const DATADOG_AGENT_CONF_YAML: &str = "/etc/datadog-agent/datadog.yaml";

#[cfg(target_os = "macos")]
const DATADOG_AGENT_CONF_YAML: &str = "/opt/datadog-agent/etc/datadog.yaml";

/// Environment variable prefix for Datadog Agent configuration.
const DATADOG_AGENT_ENV_VAR_PREFIX: &str = "DD";

#[cfg(target_os = "linux")]
#[global_allocator]
static ALLOC: memory_accounting::allocator::TrackingAllocator<tikv_jemallocator::Jemalloc> =
    memory_accounting::allocator::TrackingAllocator::new(tikv_jemallocator::Jemalloc);

#[cfg(not(target_os = "linux"))]
#[global_allocator]
static ALLOC: memory_accounting::allocator::TrackingAllocator<std::alloc::System> =
    memory_accounting::allocator::TrackingAllocator::new(std::alloc::System);

#[tokio::main]
async fn main() -> Result<(), GenericError> {
    let started = Instant::now();
    let cli: Cli = argh::from_env();

    // Load bootstrap configuration from the Agent YAML file and environment variables
    let bootstrap_config_path = cli.config_file.unwrap_or_else(|| DATADOG_AGENT_CONF_YAML.into());
    let bootstrap_config = ConfigurationLoader::default()
        .from_yaml(&bootstrap_config_path)
        .error_context("Failed to load Datadog Agent configuration file.")?
        .from_environment(DATADOG_AGENT_ENV_VAR_PREFIX)
        .error_context("Environment variable prefix should not be empty.")?
        .with_default_secrets_resolution()
        .await
        .error_context("Failed to load secrets resolution configuration.")?
        .bootstrap_generic();

    // Bootstrap the application (logging, metrics, TLS, etc.)
    let bootstrapper = AppBootstrapper::from_configuration(&bootstrap_config)
        .error_context("Failed to parse bootstrap configuration during bootstrap phase.")?
        .with_metrics_prefix("check_runner");
    let _bootstrap_guard = bootstrapper
        .bootstrap()
        .await
        .error_context("Failed to complete bootstrap phase.")?;

    let app_details = saluki_metadata::get_app_details();
    info!(
        version = app_details.version().raw(),
        git_hash = app_details.git_hash(),
        target_arch = app_details.target_arch(),
        build_time = app_details.build_time(),
        process_id = std::process::id(),
        "Check Runner starting..."
    );

    // Read check configuration from stdin
    info!("Reading check configuration from stdin...");
    let check_input = CheckRunnerInput::from_stdin().error_context("Failed to read check configuration from stdin.")?;

    let execution_mode = check_input.execution_mode;
    info!(
        check_name = %check_input.name,
        execution_mode = ?execution_mode,
        instance_count = check_input.instances.len(),
        "Received check configuration."
    );

    // Convert input to CheckConfig
    let check_config = check_input
        .into_check_config()
        .error_context("Failed to convert check input to CheckConfig.")?;

    // Set up building blocks for the topology
    let component_registry = ComponentRegistry::default();
    let health_registry = HealthRegistry::new();

    // Create the autodiscovery provider with our single check
    let autodiscovery_provider = SingleCheckAutodiscoveryProvider::new(check_config);

    // Create the environment provider
    let env_provider = CheckRunnerEnvironmentProvider::from_configuration(
        &bootstrap_config,
        &component_registry,
        &health_registry,
        autodiscovery_provider,
    )
    .await
    .error_context("Failed to create environment provider.")?;

    // Create the topology
    let blueprint = create_topology(&bootstrap_config, &env_provider, &component_registry, execution_mode)
        .await
        .error_context("Failed to create topology.")?;

    // Build and spawn the topology
    let built_topology = blueprint.build().await?;
    let mut running_topology = built_topology.spawn(&health_registry, MemoryLimiter::noop()).await?;

    let startup_time = started.elapsed();
    info!(init_time_ms = startup_time.as_millis(), "Topology running.");

    // Handle execution based on mode
    match execution_mode {
        ExecutionMode::Once => {
            info!("Running in 'once' mode. Will exit after check completion and flush.");

            // Wait for the aggregation flush interval plus buffer time
            // Default flush interval is 15 seconds, we wait 20 to ensure data is forwarded
            tokio::time::sleep(Duration::from_secs(20)).await;

            info!("Flush interval complete. Shutting down..");
        }
        ExecutionMode::Continuous => {
            info!("Running in 'continuous' mode. Press Ctrl+C to stop.");

            select! {
                _ = running_topology.wait_for_unexpected_finish() => {
                    error!("Component unexpectedly finished. Shutting down...");
                },
                _ = tokio::signal::ctrl_c() => {
                    info!("Received SIGINT, shutting down...");
                }
            }
        }
    }

    // Graceful shutdown
    match running_topology.shutdown_with_timeout(Duration::from_secs(30)).await {
        Ok(()) => {
            info!("Topology shutdown successfully.");
            Ok(())
        }
        Err(e) => Err(e),
    }
}
