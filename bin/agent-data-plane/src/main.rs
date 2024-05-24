//! Main benchmarking binary.
//!
//! This binary emulates the standalone DogStatsD binary, listening for DogStatsD over UDS, aggregating metrics over a
//! 10 second window, and shipping those metrics to the Datadog Platform.
#![deny(warnings)]
#![deny(missing_docs)]
mod env_provider;

use std::time::{Duration, Instant};

use memory_accounting::{BoundsVerifier, MemoryBounds, MemoryBoundsBuilder, MemoryGrant};
use saluki_components::{
    destinations::DatadogMetricsConfiguration,
    sources::{ChecksConfiguration, DogStatsDConfiguration, InternalMetricsConfiguration},
    transforms::{
        AggregateConfiguration, ChainedConfiguration, HostEnrichmentConfiguration, OriginEnrichmentConfiguration,
    },
};
use saluki_config::{ConfigurationLoader, GenericConfiguration};
use saluki_error::{ErrorContext as _, GenericError};
use tracing::{error, info, warn};

use saluki_app::{
    logging::{fatal_and_exit, initialize_logging},
    metrics::initialize_metrics,
};
use saluki_core::topology::blueprint::TopologyBlueprint;
use ubyte::{ByteUnit, ToByteUnit as _};

use crate::env_provider::ADPEnvironmentProvider;

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

    let configuration = ConfigurationLoader::default()
        .try_from_yaml("/etc/datadog-agent/datadog.yaml")
        .from_environment("DD")
        .into_generic()?;

    let env_provider = ADPEnvironmentProvider::from_configuration(&configuration).await?;

    // Create a simple pipeline that runs a DogStatsD source, an aggregation transform to bucket into 10 second windows,
    // and a Datadog Metrics destination that forwards aggregated buckets to the Datadog Platform.
    let dsd_config = DogStatsDConfiguration::from_configuration(&configuration)?;
    let dsd_agg_config = AggregateConfiguration::from_window(Duration::from_secs(10)).with_context_limit(15500);
    // Aggregation currently only supports time windows
    // we'll need to add support for a "check sampler" like aggregator
    // basically this _only_ does metric aggregation rules, no time rules.
    // flushing is similar to current behavior, just need to think about 'counter' resets
    // for now, just use time windows, if we make each window shorter than the check interval, it should be fine
    let checks_agg_config = AggregateConfiguration::from_window(Duration::from_secs(15));
    let int_metrics_config = InternalMetricsConfiguration;
    let int_metrics_agg_config = AggregateConfiguration::from_window(Duration::from_secs(10)).flush_open_windows(true);

    let host_enrichment_config = HostEnrichmentConfiguration::from_environment_provider(env_provider.clone());
    let origin_enrichment_config = OriginEnrichmentConfiguration::from_configuration(&configuration)?
        .with_environment_provider(env_provider.clone());
    let enrich_config = ChainedConfiguration::default()
        .with_transform_builder(host_enrichment_config)
        .with_transform_builder(origin_enrichment_config);
    let dd_metrics_config = DatadogMetricsConfiguration::from_configuration(&configuration)?;

    let check_config = ChecksConfiguration::from_configuration(&configuration)?;

    let mut blueprint = TopologyBlueprint::default();
    blueprint
        .add_source("dsd_in", dsd_config)?
        .add_source("internal_metrics_in", int_metrics_config)?
        .add_source("checks_in", check_config)?
        .add_transform("dsd_agg", dsd_agg_config)?
        .add_transform("checks_agg", checks_agg_config)?
        .add_transform("internal_metrics_agg", int_metrics_agg_config)?
        .add_transform("enrich", enrich_config)?
        .add_destination("dd_metrics_out", dd_metrics_config)?
        .connect_component("dsd_agg", ["dsd_in"])?
        .connect_component("checks_agg", ["checks_in"])?
        .connect_component("internal_metrics_agg", ["internal_metrics_in"])?
        .connect_component("enrich", ["dsd_agg", "internal_metrics_agg", "checks_agg"])?
        .connect_component("dd_metrics_out", ["enrich"])?;

    verify_memory_bounds(&configuration, &blueprint)?;

    let built_topology = blueprint.build().await?;
    let running_topology = built_topology.spawn().await?;

    let startup_time = started.elapsed();

    info!(
        init_time_ms = startup_time.as_millis(),
        "Topology running, waiting for interrupt..."
    );

    tokio::signal::ctrl_c().await?;

    info!("Received SIGINT, shutting down...");

    running_topology.shutdown().await
}

fn verify_memory_bounds(
    configuration: &GenericConfiguration, blueprint: &TopologyBlueprint,
) -> Result<(), GenericError> {
    let memory_limit = match configuration
        .try_get_typed::<ByteUnit>("memory_limit")
        .error_context("Failed to parse memory limit setting.")?
    {
        Some(limit) => limit,
        None => {
            info!("No memory limit set for the process. Skipping memory bounds verification.");
            return Ok(());
        }
    };

    let slop_factor = configuration
        .try_get_typed::<f64>("memory_slop_factor")
        .error_context("Failed to get memory slop factor setting.")?
        .unwrap_or(0.25);

    let initial_grant = MemoryGrant::with_slop_factor(memory_limit.as_u64() as usize, slop_factor)?;
    let mut bounds_builder = MemoryBoundsBuilder::new();
    let mut blueprint_builder = bounds_builder.component("topology");
    blueprint.specify_bounds(&mut blueprint_builder);
    let component_bounds = bounds_builder.finalize();

    let bounds_verifier = BoundsVerifier::new(initial_grant, component_bounds);
    match bounds_verifier.verify() {
        Ok(verified_bounds) => {
            info!(
                "Verified memory bounds. Minimum memory requirement of {}, with a calculated firm memory bound of {} out of {} available, out of an initial {} grant.",
                verified_bounds.total_minimum_required_bytes().bytes(),
                verified_bounds.total_firm_limit_bytes().bytes(),
                verified_bounds.total_available_bytes().bytes(),
                initial_grant.initial_limit_bytes().bytes(),
            );
        }
        Err(e) => {
            warn!("Failed to verify memory bounds: {}", e);
        }
    }

    Ok(())
}
