use std::{
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
use crate::{config::DataPlaneConfiguration, env_provider::ADPEnvironmentProvider};

/// Per-connection handler used to wire the Datadog forwarder to an in-process intake.
///
/// See [`saluki_io::net::client::http::HttpsCapableConnectorBuilder::with_in_process_handler`].
/// In production this is always `None`; in the integration test it routes every outbound HTTP
/// connection through an in-memory [`tokio::io::duplex`] pair so simulated time stays in
/// lockstep with request progress.
pub type InProcessIntakeHandler = saluki_io::net::client::http::InProcessHandler;



// extract stuff to get a function that gives us enough to build the topology
// get rid of the supervisor - we don't need it
// the topology should shut itself down after a fixed delay
// its shutdown sequence will include a flush to the intake ("flush open bucket")
// ( or a sleep of 17secs after sending the payloads )
// tokio runtime
// spawn topology
// call the network calls to send packets to the topology
/// Entrypoint for the fuzz `run` command.
///
/// When `intake` is `Some`, the Datadog forwarder is wired to an in-process intake (see
/// [`InProcessIntakeHandler`]) so the integration test can observe payloads without standing up
/// a real TCP listener. In production callers always pass `None`.
pub async fn handle_run_command(
    bootstrap_config: serde_json::Value,
    intake: Option<InProcessIntakeHandler>,
    shutdown: tokio::sync::oneshot::Receiver<()>,
    started: Instant,
) -> Result<(), GenericError> {
    info!("Agent Data Plane starting...");

    // Load our static configuration
    let (config, maybe_sender) = ConfigurationLoader::for_tests(Some(bootstrap_config), None, false).await;
    assert!(maybe_sender.is_none());
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
        intake,
    )
    .await?;

    // Run memory bounds validation to ensure that we can launch the topology with our configured memory limit, if any.
    let bounds_config = MemoryBoundsConfiguration::try_from_config(&config)?;
    let memory_limiter = initialize_memory_bounds(bounds_config, component_registry.root())?;

    // Bounds validation succeeded, so now we'll build and spawn the topology.
    //
    // Run components on the ambient (calling) runtime instead of the default dedicated
    // multi-thread pool. The integration test builds the runtime with
    // `tokio::runtime::Builder::start_paused(true)` to drive simulated time; a separate
    // multi-thread pool would not inherit that and would run components against real wall
    // clock, defeating auto-advance and stretching the test out for many real seconds.
    let built_topology = blueprint.build().await?.with_ambient_worker_pool();
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
    intake: Option<InProcessIntakeHandler>,
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
        let mut dd_forwarder_config =
            DatadogConfiguration::from_configuration(config).error_context("Failed to configure Datadog forwarder.")?;

        if let Some(handler) = intake {
            dd_forwarder_config = dd_forwarder_config.with_in_process_handler(handler);
        }

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
