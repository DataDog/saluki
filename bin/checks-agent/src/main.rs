use memory_accounting::ComponentRegistry;
use saluki_app::{api::APIBuilder, metrics::emit_startup_metrics, prelude::*};
use saluki_components::{
    destinations::BlackholeConfiguration,
    sources::ChecksConfiguration,
    transforms::{AggregateConfiguration, ChainedConfiguration, HostEnrichmentConfiguration},
};
use saluki_config::{ConfigurationLoader, GenericConfiguration};
use saluki_core::topology::TopologyBlueprint;
use saluki_error::{ErrorContext as _, GenericError};
use saluki_health::HealthRegistry;
use saluki_io::net::ListenAddress;

mod env_provider;
use self::env_provider::ADPEnvironmentProvider;

use std::future::pending;
use tokio::select;
use tracing::{error, info, Level};
use tracing_subscriber::FmtSubscriber;

const PRIMARY_UNPRIVILEGED_API_PORT: u16 = 5100;

#[tokio::main]
async fn main() {
    // a builder for `FmtSubscriber`.
    let subscriber = FmtSubscriber::builder()
        // all spans/events with a level higher than TRACE (e.g, debug, info, warn, etc.)
        // will be written to stdout.
        .with_max_level(Level::TRACE)
        // completes the builder.
        .finish();

    tracing::subscriber::set_global_default(subscriber).expect("setting default subscriber failed");

    match run().await {
        Ok(_) => (),
        Err(e) => eprintln!("Error: {:?}", e),
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let configuration = ConfigurationLoader::default()
        .try_from_yaml("./datadog.yaml")
        .from_environment("DD")?
        .with_default_secrets_resolution()
        .await?
        .into_generic()?;

    println!("{:?}", configuration);

    let component_registry = ComponentRegistry::default();
    let health_registry = HealthRegistry::new();

    let env_provider =
        ADPEnvironmentProvider::from_configuration(&configuration, &component_registry, &health_registry).await?;

    // Create a simple pipeline
    let blueprint = create_topology(&configuration, &env_provider, &component_registry).await?;

    // Run memory bounds validation to ensure that we can launch the topology with our configured memory limit, if any.
    let bounds_configuration = MemoryBoundsConfiguration::try_from_config(&configuration)?;
    let memory_limiter = initialize_memory_bounds(bounds_configuration, &component_registry)?;

    // Build and spawn the topology
    let built_topology = blueprint.build().await?;
    let mut running_topology = built_topology.spawn(&health_registry, memory_limiter).await?;

    // Build our unprivileged API server.
    let unprivileged_api = APIBuilder::new()
        .with_handler(health_registry.api_handler())
        .with_handler(component_registry.api_handler());

    let api_listen_address = configuration
        .try_get_typed("api_listen_address")
        .error_context("Failed to get API listen address.")?
        .unwrap_or_else(|| ListenAddress::any_tcp(PRIMARY_UNPRIVILEGED_API_PORT));

    tokio::spawn(async move {
        info!("Serving unprivileged API on {}.", api_listen_address);

        if let Err(e) = unprivileged_api.serve(api_listen_address, pending()).await {
            error!("Failed to serve unprivileged API: {}", e);
        }
    });

    // Spawn the health checker.
    health_registry.spawn().await?;

    emit_startup_metrics();
    info!("Topology running, waiting for interrupt...");

    // Wait for the shutdown signal or unexpected component finish before exiting.
    select! {
        _ = running_topology.wait_for_unexpected_finish() => {
            error!("Component unexpectedly finished. Shutting down...");
        },
        _ = tokio::signal::ctrl_c() => {
            info!("Shutdown signal received. Exiting...");
        }
    }

    // Gracefully shut down the topology
    match running_topology
        .shutdown_with_timeout(std::time::Duration::from_secs(30))
        .await
    {
        Ok(()) => info!("Topology shutdown successfully."),
        Err(e) => error!("Error shutting down topology: {:?}", e),
    }

    Ok(())
}

async fn create_topology(
    configuration: &GenericConfiguration, env_provider: &ADPEnvironmentProvider, component_registry: &ComponentRegistry,
) -> Result<TopologyBlueprint, GenericError> {
    // Create a simplified topology with minimal components for now
    let topology_registry = component_registry.get_or_create("topology");
    let mut blueprint = TopologyBlueprint::from_component_registry(topology_registry);

    // Aggregation currently only supports time windows
    // we'll need to add support for a "check sampler" like aggregator
    // basically this _only_ does metric aggregation rules, no time rules.
    // flushing is similar to current behavior, just need to think about 'counter' resets
    // for now, just use time windows, if we make each window shorter than the check interval, it should be fine
    let checks_agg_config = AggregateConfiguration::with_defaults();
    let check_config = ChecksConfiguration::from_configuration(configuration)?;
    let host_enrichment_config = HostEnrichmentConfiguration::from_environment_provider(env_provider.clone());
    let enrich_config = ChainedConfiguration::default().with_transform_builder(host_enrichment_config);

    // Add a destination component to receive data from the 'enrich' transform
    let blackhole_config = BlackholeConfiguration::default();

    blueprint
        .add_source("checks_in", check_config)?
        .add_transform("checks_agg", checks_agg_config)?
        .add_transform("enrich", enrich_config)?
        .add_destination("metrics_out", blackhole_config)?
        .connect_component("checks_agg", ["checks_in"])?
        .connect_component("enrich", ["checks_agg"])?
        .connect_component("metrics_out", ["enrich"])?;

    Ok(blueprint)
}
