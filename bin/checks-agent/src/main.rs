use std::time::{Duration, Instant};

use memory_accounting::ComponentRegistry;
use saluki_app::{api::APIBuilder, metrics::emit_startup_metrics, prelude::*};
use saluki_components::{
    destinations::PrometheusConfiguration,
    encoders::DatadogMetricsConfiguration,
    forwarders::DatadogConfiguration,
    sources::{ChecksConfiguration, InternalMetricsConfiguration},
    transforms::{AggregateConfiguration, ChainedConfiguration, HostEnrichmentConfiguration},
};
use saluki_config::{ConfigurationLoader, GenericConfiguration};
use saluki_core::topology::TopologyBlueprint;
use saluki_error::{ErrorContext as _, GenericError};
use saluki_health::HealthRegistry;
use saluki_io::net::ListenAddress;

mod env_provider;

use std::future::pending;

use tokio::select;
use tracing::{error, info};

use self::env_provider::ChecksAgentEnvProvider;

const PRIMARY_UNPRIVILEGED_API_PORT: u16 = 5105;

#[cfg(target_os = "linux")]
#[global_allocator]
static ALLOC: memory_accounting::allocator::TrackingAllocator<tikv_jemallocator::Jemalloc> =
    memory_accounting::allocator::TrackingAllocator::new(tikv_jemallocator::Jemalloc);

#[cfg(not(target_os = "linux"))]
#[global_allocator]
static ALLOC: memory_accounting::allocator::TrackingAllocator<std::alloc::System> =
    memory_accounting::allocator::TrackingAllocator::new(std::alloc::System);

#[tokio::main]
async fn main() {
    let started = Instant::now();

    match initialize_dynamic_logging(None).await {
        Ok(handler) => handler,
        Err(e) => {
            fatal_and_exit(format!("failed to initialize logging: {}", e));
        }
    };

    if let Err(e) = initialize_metrics("checks-agent").await {
        fatal_and_exit(format!("failed to initialize metrics: {}", e));
    }

    if let Err(e) = initialize_allocator_telemetry().await {
        fatal_and_exit(format!("failed to initialize allocator telemetry: {}", e));
    }

    if let Err(e) = initialize_tls() {
        fatal_and_exit(format!("failed to initialize TLS: {}", e));
    }

    match run(started).await {
        Ok(()) => info!("Checks Agent stopped."),
        Err(e) => {
            // TODO: It'd be better to take the error cause chain and write it out as a list of errors, instead of
            // having it be multi-line, since the multi-line bit messes with the log formatting.
            error!("{:?}", e);
            std::process::exit(1);
        }
    }
}

async fn run(started: Instant) -> Result<(), Box<dyn std::error::Error>> {
    let configuration = ConfigurationLoader::default()
        .try_from_yaml("/etc/datadog-agent/datadog.yaml")
        .from_environment("DD")?
        .with_default_secrets_resolution()
        .await?
        .into_generic()?;

    let component_registry = ComponentRegistry::default();
    let health_registry = HealthRegistry::new();

    let env_provider = ChecksAgentEnvProvider::from_configuration(&configuration, &component_registry).await?;

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

    let startup_time = started.elapsed();

    emit_startup_metrics();

    info!(
        init_time_ms = startup_time.as_millis(),
        "Topology running, waiting for interrupt..."
    );

    tokio::spawn(async move {
        info!("Serving unprivileged API on {}.", api_listen_address);

        if let Err(e) = unprivileged_api.serve(api_listen_address, pending()).await {
            error!("Failed to serve unprivileged API: {}", e);
        }
    });

    // Spawn the health checker.
    health_registry.spawn().await?;

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
    match running_topology.shutdown_with_timeout(Duration::from_secs(30)).await {
        Ok(()) => info!("Topology shutdown successfully."),
        Err(e) => error!("Error shutting down topology: {:?}", e),
    }

    Ok(())
}

async fn create_topology(
    configuration: &GenericConfiguration, env_provider: &ChecksAgentEnvProvider, component_registry: &ComponentRegistry,
) -> Result<TopologyBlueprint, GenericError> {
    let checks_config = ChecksConfiguration::from_configuration(configuration)
        .error_context("Failed to configure checks source.")?
        .with_autodiscovery_provider(env_provider.autodiscovery_provider().clone());

    let dsd_agg_config = AggregateConfiguration::from_configuration(configuration)
        .error_context("Failed to configure aggregate transform.")?;
    let host_enrichment_config = HostEnrichmentConfiguration::from_environment_provider(env_provider.clone());
    let enrich_config =
        ChainedConfiguration::default().with_transform_builder("host_enrichment", host_enrichment_config);

    let dd_metrics_config = DatadogMetricsConfiguration::from_configuration(configuration)
        .error_context("Failed to configure Datadog Metrics encoder.")?;

    let dd_forwarder_config = DatadogConfiguration::from_configuration(configuration)
        .error_context("Failed to configure Datadog forwarder.")?;

    // Create a simplified topology with minimal components for now.
    let mut blueprint = TopologyBlueprint::new("primary", component_registry);

    blueprint
        .add_source("checks_in", checks_config)?
        .add_transform("dsd_agg", dsd_agg_config)?
        .add_transform("enrich", enrich_config)?
        .add_encoder("dd_metrics_encode", dd_metrics_config)?
        .add_forwarder("dd_out", dd_forwarder_config)?
        .connect_component("dsd_agg", ["checks_in"])?
        .connect_component("enrich", ["dsd_agg"])?
        .connect_component("dd_metrics_encode", ["enrich"])?
        .connect_component("dd_out", ["dd_metrics_encode"])?;

    // When telemetry is enabled, we need to collect internal metrics, so add those components and route them here.
    let telemetry_enabled = configuration.get_typed_or_default::<bool>("telemetry_enabled");
    if telemetry_enabled {
        let int_metrics_config = InternalMetricsConfiguration;
        let prometheus_config = PrometheusConfiguration::from_configuration(configuration)?;

        info!(
            "Serving telemetry scrape endpoint on {}.",
            prometheus_config.listen_address()
        );

        blueprint
            .add_source("internal_metrics_in", int_metrics_config)?
            .add_destination("internal_metrics_out", prometheus_config)?
            .connect_component("internal_metrics_out", ["internal_metrics_in"])?;
    }

    Ok(blueprint)
}
