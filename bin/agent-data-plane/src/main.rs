//! Main benchmarking binary.
//!
//! This binary emulates the standalone DogStatsD binary, listening for DogStatsD over UDS, aggregating metrics over a
//! 10 second window, and shipping those metrics to the Datadog Platform.

#![deny(warnings)]
#![deny(missing_docs)]
mod env_provider;

use std::time::Instant;

use memory_accounting::ComponentRegistry;
use saluki_app::prelude::*;
use saluki_components::{
    destinations::{BlackholeConfiguration, DatadogMetricsConfiguration, PrometheusConfiguration},
    sources::{DogStatsDConfiguration, InternalMetricsConfiguration},
    transforms::{
        AggregateConfiguration, ChainedConfiguration, HostEnrichmentConfiguration, OriginEnrichmentConfiguration,
    },
};
use saluki_config::ConfigurationLoader;
use saluki_core::topology::TopologyBlueprint;
use saluki_error::GenericError;
use tracing::{error, info};

use crate::env_provider::ADPEnvironmentProvider;

#[global_allocator]
static ALLOC: memory_accounting::allocator::TrackingAllocator<std::alloc::System> =
    memory_accounting::allocator::TrackingAllocator::new(std::alloc::System);

const ADP_VERSION: &str = env!("ADP_VERSION");
const ADP_BUILD_DESC: &str = env!("ADP_BUILD_DESC");

#[tokio::main]
async fn main() {
    let started = Instant::now();

    if let Err(e) = initialize_logging() {
        fatal_and_exit(format!("failed to initialize logging: {}", e));
    }

    if let Err(e) = initialize_metrics("datadog.saluki").await {
        fatal_and_exit(format!("failed to initialize metrics: {}", e));
    }

    if let Err(e) = initialize_allocator_telemetry().await {
        fatal_and_exit(format!("failed to initialize allocator telemetry: {}", e));
    }

    if let Err(e) = initialize_tls() {
        fatal_and_exit(format!("failed to initialize TLS: {}", e));
    }

    match run(started).await {
        Ok(()) => info!("Agent Data Plane stopped."),
        Err(e) => {
            error!("{:?}", e);
            std::process::exit(1);
        }
    }
}

async fn run(started: Instant) -> Result<(), GenericError> {
    info!(
        version = ADP_VERSION,
        build_desc = ADP_BUILD_DESC,
        "Agent Data Plane starting..."
    );

    let mut component_registry = ComponentRegistry::default();

    let configuration = ConfigurationLoader::default()
        .try_from_yaml("/etc/datadog-agent/datadog.yaml")
        .from_environment("DD")?
        .into_generic()?;

    let env_provider =
        ADPEnvironmentProvider::from_configuration(&configuration, component_registry.get_or_create("env_provider"))
            .await?;

    // Create a simple pipeline that runs a DogStatsD source, an aggregation transform to bucket into 10 second windows,
    // and a Datadog Metrics destination that forwards aggregated buckets to the Datadog Platform.
    let dsd_config = DogStatsDConfiguration::from_configuration(&configuration)?;
    let dsd_agg_config = AggregateConfiguration::from_configuration(&configuration)?;
    let int_metrics_config = InternalMetricsConfiguration;
    let int_metrics_agg_config = AggregateConfiguration::with_defaults();

    let host_enrichment_config = HostEnrichmentConfiguration::from_environment_provider(env_provider.clone());
    let origin_enrichment_config = OriginEnrichmentConfiguration::from_configuration(&configuration)?
        .with_environment_provider(env_provider.clone());
    let enrich_config = ChainedConfiguration::default()
        .with_transform_builder(host_enrichment_config)
        .with_transform_builder(origin_enrichment_config);
    let dd_metrics_config = DatadogMetricsConfiguration::from_configuration(&configuration)?;
    let blackhole_config = BlackholeConfiguration;

    let topology_registry = component_registry.get_or_create("topology");
    let mut blueprint = TopologyBlueprint::from_component_registry(topology_registry);
    blueprint
        .add_source("dsd_in", dsd_config)?
        .add_source("internal_metrics_in", int_metrics_config)?
        .add_transform("dsd_agg", dsd_agg_config)?
        .add_transform("internal_metrics_agg", int_metrics_agg_config)?
        .add_transform("enrich", enrich_config)?
        .add_destination("dd_metrics_out", dd_metrics_config)?
        .add_destination("blackhole", blackhole_config)?
        .connect_component("dsd_agg", ["dsd_in.metrics"])?
        .connect_component("blackhole", ["dsd_in.events", "dsd_in.service_checks"])?
        .connect_component("internal_metrics_agg", ["internal_metrics_in"])?
        .connect_component("enrich", ["dsd_agg", "internal_metrics_agg"])?
        .connect_component("dd_metrics_out", ["enrich"])?;

    // Insert a Prometheus scrape destination if we've been instructed to enable internal telemetry.
    if configuration.get_typed_or_default::<bool>("telemetry_enabled") {
        let prometheus_config = PrometheusConfiguration::from_configuration(&configuration)?;
        blueprint
            .add_destination("internal_metrics_out", prometheus_config)?
            .connect_component("internal_metrics_out", ["internal_metrics_in"])?;
    }

    // With our environment provider and topology blueprint established, go through bounds validation.
    let bounds_configuration = MemoryBoundsConfiguration::try_from_config(&configuration)?;
    let memory_limiter = initialize_memory_bounds(bounds_configuration, component_registry)?;

    // Time to run the topology!
    let built_topology = blueprint.build().await?;
    let running_topology = built_topology.spawn(memory_limiter).await?;

    let startup_time = started.elapsed();

    info!(
        init_time_ms = startup_time.as_millis(),
        "Topology running, waiting for interrupt..."
    );

    tokio::signal::ctrl_c().await?;

    info!("Received SIGINT, shutting down...");

    running_topology.shutdown().await
}
