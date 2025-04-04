use std::future::{pending, Future};

use memory_accounting::{ComponentRegistry, MemoryLimiter};
use saluki_app::metrics::collect_runtime_metrics;
use saluki_components::{destinations::PrometheusConfiguration, sources::InternalMetricsConfiguration};
use saluki_config::GenericConfiguration;
use saluki_core::topology::{RunningTopology, TopologyBlueprint};
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use saluki_health::HealthRegistry;
use tracing::info;

use crate::components::remapper::AgentTelemetryRemapperConfiguration;

/// Spawns all relevant internal ADP processes, such as internal telemetry, API endpoints, and so on.
///
/// # Errors
///
/// If an error occurs while spawning any of the internal processes, an error is returned.
pub fn spawn_internal_processes(config: &GenericConfiguration, component_registry: &ComponentRegistry, health_registry: HealthRegistry) -> Result<(), GenericError>
{
    // We spawn two separate current-thread runtimes: one for APIs/health checks, and one for the internal observability topology.
    //
    // This is because we want to ensure these are isolated from any badness in the primary data topology, either in
    // terms of how its affecting the runtime behavior or in terms of the topology, and things like high data volume
    // consuming event buffers and starving the internal observability components, and so on.

    spawn_internal_observability_topology(config, component_registry, health_registry.clone())?;
    spawn_control_plane(config, internal_metrics, unprivileged_api, privileged_api, health_registry)?;

    Ok(())
}

fn spawn_control_plane(config: &GenericConfiguration, internal_metrics: _, unprivileged_api: _, privileged_api: _, health_registry: HealthRegistry) -> _ {
    // Build our unprivileged and privileged API server.
    //
    // The unprivileged API is purely for things like health checks or read-only information. The privileged API is
    // meant for sensitive information or actions that require elevated permissions.
    let unprivileged_api = APIBuilder::new()
        .with_handler(health_registry.api_handler())
        .with_handler(component_registry.api_handler());

    let privileged_api = APIBuilder::new()
        .with_self_signed_tls()
        .with_handler(logging_api_handler)
        .with_optional_handler(env_provider.workload_api_handler());

        let internal_metrics = initialize_shared_metrics_state().await;

// Handle any final configuration of our API endpoints and spawn them.
configure_and_spawn_api_endpoints(&configuration, internal_metrics, unprivileged_api, privileged_api).await?;
    
    health_registry.spawn().await?;
}

fn spawn_internal_observability_topology(config: &GenericConfiguration, component_registry: &ComponentRegistry, health_registry: HealthRegistry) -> Result<(), GenericError> {
    // When telemetry is enabled, we need to collect internal metrics, so add those components and route them here.
    let telemetry_enabled = config.get_typed_or_default::<bool>("telemetry_enabled");
    if !telemetry_enabled {
        info!("Internal telemetry disabled. Skipping internal observability topology.");
        return Ok(());
    }

    // Build the internal telemetry topology.
    let int_metrics_config = InternalMetricsConfiguration;
    let int_metrics_remap_config = AgentTelemetryRemapperConfiguration::new();
    let prometheus_config = PrometheusConfiguration::from_configuration(config)?;

    info!("Internal telemetry enabled. Spawning Prometheus scrape endpoint on {}.", prometheus_config.listen_address());

    let mut blueprint = TopologyBlueprint::new("internal", component_registry);
    blueprint
        .add_source("internal_metrics_in", int_metrics_config)?
        .add_transform("internal_metrics_remap", int_metrics_remap_config)?
        .add_destination("internal_metrics_out", prometheus_config)?
        .connect_component("internal_metrics_remap", ["internal_metrics_in"])?
        .connect_component("internal_metrics_out", ["internal_metrics_remap"])?;

    let init = async move {
        let built_topology = blueprint.build().await?;
        built_topology.spawn(&health_registry, MemoryLimiter::noop()).await
    };

    let main = |_: RunningTopology| pending::<()>();

    initialize_and_launch_runtime("internal-telemetry", init, main)
}

/// Creates a single-threaded Tokio runtime, initializing it and driving it to completion.
///
/// A dedicated background thread is spawned on which the runtime executes. The `init` future is run within the context
/// of the runtime and is expected to return a `Result<T, GenericError>` that indicates that initialization has either
/// succeeded or failed. `main_task` is used to create the future which the runtime will ultimately drive to completion.
///
/// If initialization succeeds, `main_task` is called the result from `init` to create the main task future,, and this
/// function returns `Ok(())`. If initialization fails, this function returns `Err(e)`.
///
/// # Errors
///
/// If the current thread runtime cannot be created, or the background thread for the runtime cannot be created, or an
/// error is returned from the execution of `init`, an error will be returned.
fn initialize_and_launch_runtime<F, T, F2, T2>(name: &str, init: F, main_task: F2) -> Result<(), GenericError>
where
    F: Future<Output = Result<T, GenericError>> + Send + 'static,
    F2: FnOnce(T) -> T2 + Send + 'static,
    T2: Future,
{
    let mut builder = tokio::runtime::Builder::new_current_thread();
    let runtime = builder.enable_all()
        .max_blocking_threads(2)
        .build()
        .error_context("Failed to build current thread runtime.")?;

    let runtime_id = name.to_string();
    let (init_tx, init_rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name(name.to_string())
        .spawn(move || {
            // Run the initialization routine within the context of the runtime.
            match runtime.block_on(init) {
                Ok(init_value) => {
                    // Initialization succeeded, so inform the main thread that the runtime has been initialized and
                    // will continue running, and pass whatever we got back from initialization and drive the main
                    // task to completion.
                    init_tx.send(Ok(())).unwrap();

                    // Start collecting runtime metrics.
                    tokio::spawn(async move {
                        collect_runtime_metrics(&runtime_id).await;
                    });

                    let _ = runtime.block_on(main_task(init_value));
                }
                Err(e) => {
                    // Initialization failed, so send the error back to the main thread.
                    init_tx.send(Err(e)).unwrap();
                }
            }
        })
        .with_error_context(|| format!("Failed to spawn thread for runtime '{}'.", name))?;

    // Wait for the initialization to complete and forward back the result if we get one.
    match init_rx.recv() {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(e),
        Err(_) => Err(generic_error!("Initialization result channel closed unexpectedly. Runtime likely in an unexpected/corrupted state.")),
    }
}
