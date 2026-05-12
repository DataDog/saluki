use std::{
    path::PathBuf,
    sync::Mutex,
    time::{Duration, Instant},
};

use saluki_app::bootstrap::BootstrapGuard;

// Bootstrap sets global process state (logging, TLS, metrics). Hold the guard in a static
// so ASAN can trace the allocation as reachable, and so the logger threads stay alive.
static BOOTSTRAP_GUARD: Mutex<Option<BootstrapGuard>> = Mutex::new(None);

use memory_accounting::ComponentRegistry;
use saluki_app::{
    bootstrap::AppBootstrapper,
    memory::{initialize_memory_bounds, MemoryBoundsConfiguration},
    metrics::emit_startup_metrics,
};
use saluki_components::{
    config::{DatadogRemapper, KEY_ALIASES},
    destinations::DogStatsDStatisticsConfiguration,
    forwarders::DatadogConfiguration,
};
use saluki_config::{ConfigurationLoader, GenericConfiguration};
use saluki_core::health::HealthRegistry;
use saluki_core::topology::TopologyBlueprint;
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use tokio::{select, time::interval};
use tracing::{error, info, warn};

use crate::cli::run::{
    add_baseline_metrics_pipeline_to_blueprint, add_dsd_pipeline_to_blueprint, add_otlp_pipeline_to_blueprint,
};
use crate::internal::platform::PlatformSettings;
use crate::{config::DataPlaneConfiguration, env_provider::ADPEnvironmentProvider};

// TODO: fuzz entry modeled after the content of that command
// extract stuff to get a function that gives us enough to build the topology
// get rid of the supervisor - we don't need it
// the topology should shut itself down after a fixed delay
// its shutdown sequence will include a flush to the intake ("flush open bucket")
// ( or a sleep of 17secs after sending the payloads )
// tokio runtime
// spawn topology
// call the network calls to send packets to the topology
/// Entrypoint for the `run` commands.
pub async fn handle_run_command(
    bootstrap_config_path: PathBuf, shutdown: tokio::sync::oneshot::Receiver<()>, started: Instant,
) -> Result<(), GenericError> {
    info!("Agent Data Plane starting...");

    // Load our static configuration

    let config = ConfigurationLoader::default()
        .with_key_aliases(KEY_ALIASES)
        .from_yaml(bootstrap_config_path)
        .error_context("Failed to load Datadog Agent configuration file during bootstrap.")?
        .add_providers([DatadogRemapper::new()])
        .from_environment(PlatformSettings::get_env_var_prefix())
        .error_context("Environment variable prefix should not be empty.")?
        .with_default_secrets_resolution()
        .await
        .error_context("Failed to load secrets resolution configuration during bootstrap.")?
        .bootstrap_generic();
    let dp_config = DataPlaneConfiguration::from_configuration(&config)
        .error_context("Failed to load data plane configuration.")?;

    let in_standalone_mode = dp_config.standalone_mode();
    // Bootstrap (logging, TLS, metrics) sets global process state — only run once.
    // Drop the MutexGuard before `.await` to keep the future Send.
    let needs_bootstrap = BOOTSTRAP_GUARD.lock().expect("BOOTSTRAP_GUARD mutex poisoned").is_none();
    if needs_bootstrap {
        let bootstrapper = AppBootstrapper::from_configuration(&config)?;
        let guard = bootstrapper.bootstrap_without_logging().await?;
        *BOOTSTRAP_GUARD.lock().expect("BOOTSTRAP_GUARD mutex poisoned") = Some(guard);
    }
    // simpl: no remote loading, only the local config
    assert!(!dp_config.remote_agent_enabled());
    assert!(!dp_config.use_new_config_stream_endpoint());


    if !in_standalone_mode && !dp_config.enabled() {
        info!("Agent Data Plane is not enabled. Exiting.");
        return Ok(());
    }

    // Set up all of the building blocks for building our topologies and launching internal processes.
    let component_registry = ComponentRegistry::default();
    let health_registry = HealthRegistry::new();
    let env_provider =
        ADPEnvironmentProvider::from_configuration(&config, &dp_config, &component_registry, &health_registry).await?;

    let dsd_stats_config = DogStatsDStatisticsConfiguration::new();

    // Create our primary data topology and spawn any internal processes, which will ensure all relevant components are
    // registered and accounted for in terms of memory usage.
    let blueprint = create_topology(
        &config,
        &dp_config,
        &env_provider,
        &component_registry,
        dsd_stats_config.clone(),
    )
    .await?;

    // Run memory bounds validation to ensure that we can launch the topology with our configured memory limit, if any.
    let bounds_config = MemoryBoundsConfiguration::try_from_config(&config)?;
    let memory_limiter = initialize_memory_bounds(bounds_config, component_registry.root())?;

    // Bounds validation succeeded, so now we'll build and spawn the topology.
    let built_topology = blueprint.build().await?;
    let mut running_topology = built_topology.spawn(&health_registry, memory_limiter).await?;

    let startup_time = started.elapsed();

    // Emit the startup metrics for the application.
    emit_startup_metrics();

    info!(
        init_time_ms = startup_time.as_millis(),
        "Topology running. Waiting for interrupt..."
    );

    // Wait for all components to become ready.
    tokio::spawn(async move {
        let mut check_interval = interval(Duration::from_millis(100));

        let mut report_interval = interval(Duration::from_millis(1000));
        report_interval.tick().await;

        loop {
            select! {
                _ = check_interval.tick() => {
                    if health_registry.all_ready() {
                        break;
                    }
                },
                _ = report_interval.tick() => {
                    info!("Topology still not healthy...");
                }
            }
        }

        info!(ready_time_ms = started.elapsed().as_millis(), "Topology healthy.");
    });

    let mut topology_failed = false;
    select! {
        _ = shutdown => {info!("shutdown request");},
        _ = running_topology.wait_for_unexpected_finish() => {
            error!("Topology component unexpectedly finished. Shutting down...");
            topology_failed = true;
        },
        _ = tokio::signal::ctrl_c() => {
            info!("Received SIGINT, shutting down...");
        }
    }

    // Shutdown the primary topology
    let topology_result = running_topology.shutdown_with_timeout(Duration::from_secs(30)).await;

    // Figure out the final "result" of this run: did something fail? did we stop cleanly?
    match topology_result {
        Ok(()) => {
            if topology_failed {
                warn!("Topology shutdown complete despite error(s).");
            } else {
                info!("Topology shutdown successfully.");
            }
            Ok(())
        }
        Err(e) => Err(e),
    }
}

async fn create_topology(
    config: &GenericConfiguration, dp_config: &DataPlaneConfiguration, env_provider: &ADPEnvironmentProvider,
    component_registry: &ComponentRegistry, dsd_stats_config: DogStatsDStatisticsConfiguration,
) -> Result<TopologyBlueprint, GenericError> {
    let mut blueprint = TopologyBlueprint::new("primary", component_registry);

    // If no data pipelines are enabled, then there's nothing for us to do.
    if !dp_config.data_pipelines_enabled() {
        return Err(generic_error!("No data pipelines are enabled. Exiting."));
    }

    // Create our baseline pipelines if necessary.
    //
    // We check if the "metrics" or "logs" pipeline is required, which represent the basic components necessary to
    // forward metrics and logs to Datadog. This means that if either are enabled, we always create the forwarder, but
    // we additionally create metrics- and logs-specific components connected to that forwarder depending on which of
    // the baseline pipelines are required.
    //
    // Notably, we _don't_ need either of these if all we're doing is running the OTLP pipeline in proxy mode, which
    // is the only reason we're differentiating here.
    if dp_config.metrics_pipeline_required()
        || dp_config.logs_pipeline_required()
        || dp_config.traces_pipeline_required()
    {
        let dd_forwarder_config =
            DatadogConfiguration::from_configuration(config).error_context("Failed to configure Datadog forwarder.")?;
        blueprint.add_forwarder("dd_out", dd_forwarder_config)?;
    }

    if dp_config.metrics_pipeline_required() {
        add_baseline_metrics_pipeline_to_blueprint(&mut blueprint, config, dp_config, env_provider).await?;
    }

    if dp_config.logs_pipeline_required() {
        error!("[Fuzz] logs not supported");
    }

    if dp_config.traces_pipeline_required() {
        error!("[Fuzz] traces not supported");
    }

    // Now we move on to our actual data pipelines.
    if dp_config.dogstatsd().enabled() {
        add_dsd_pipeline_to_blueprint(&mut blueprint, config, env_provider, dsd_stats_config).await?;
    }

    if dp_config.otlp().enabled() {
        add_otlp_pipeline_to_blueprint(&mut blueprint, config, dp_config, env_provider)?;
    }

    Ok(blueprint)
}
