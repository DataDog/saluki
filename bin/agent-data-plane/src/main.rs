//! Main benchmarking binary.
//!
//! This binary emulates the standalone DogStatsD binary, listening for DogStatsD over UDS, aggregating metrics over a
//! 10 second window, and shipping those metrics to the Datadog Platform.
#![deny(warnings)]
#![deny(missing_docs)]
mod env_provider;

use std::time::{Duration, Instant};

use saluki_components::{
    destinations::DatadogMetricsConfiguration,
    sources::{DogStatsDConfiguration, InternalMetricsConfiguration},
    transforms::{
        AggregateConfiguration, ChainedConfiguration, HostEnrichmentConfiguration, OriginEnrichmentConfiguration,
    },
};
use saluki_config::ConfigurationLoader;
use tracing::info;

use saluki_app::{logging::initialize_logging, metrics::initialize_metrics};
use saluki_core::topology::blueprint::TopologyBlueprint;
use saluki_io::net::addr::ListenAddress;

use crate::env_provider::ADPEnvironmentProvider;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let start = Instant::now();

    initialize_logging()?;
    initialize_metrics("datadog.saluki").await?;

    info!("agent-data-plane starting...");

    let configuration = ConfigurationLoader::default()
        .from_environment("DD")
        .into_generic()
        .expect("should not fail to load configuration");

    let api_key = configuration
        .get_typed::<String>("api_key")
        .expect("API key must be a string")
        .expect("API key must be specified (`api_key` or `DD_API_KEY` environment variable)");
    let api_endpoint = configuration
        .get_typed::<String>("dd_url")
        .expect("Datadog API URL key must be a string")
        .expect("Datadog API URL key must be specified (`dd_url` or `DD_DD_URL` environment variable)");

    let env_provider =
        ADPEnvironmentProvider::with_configuration(&configuration).expect("failed to create environment provider");

    // Create a simple pipeline that runs a DogStatsD source, an aggregation transform to bucket into 10 second windows,
    // and a Datadog Metrics destination that forwards aggregated buckets to the Datadog Platform.
    //
    // TODO: Pull these configuration values from the actual configuration, most likely by making each component take a
    // reference to the configuration itself.
    let dsd_listen_addr = ListenAddress::try_from("unixgram:///tmp/adp-dsd.sock")?;
    let dsd_config = DogStatsDConfiguration::from_listen_address(dsd_listen_addr)?;
    let dsd_agg_config = AggregateConfiguration::from_window(Duration::from_secs(10)).with_context_limit(15500);
    let int_metrics_config = InternalMetricsConfiguration;
    let int_metrics_agg_config = AggregateConfiguration::from_window(Duration::from_secs(10));
    let enrich_config = ChainedConfiguration::default()
        .with_transform_builder(HostEnrichmentConfiguration::from_environment_provider(
            env_provider.clone(),
        ))
        .with_transform_builder(OriginEnrichmentConfiguration::from_environment_provider(env_provider));
    let dd_metrics_config = DatadogMetricsConfiguration { api_key, api_endpoint };

    let mut blueprint = TopologyBlueprint::default();
    blueprint
        .add_source("dsd_in", dsd_config)?
        .add_source("internal_metrics_in", int_metrics_config)?
        .add_transform("dsd_agg", dsd_agg_config)?
        .add_transform("internal_metrics_agg", int_metrics_agg_config)?
        .add_transform("enrich", enrich_config)?
        .add_destination("dd_metrics_out", dd_metrics_config)?
        .connect_component("dsd_agg", ["dsd_in"])?
        .connect_component("internal_metrics_agg", ["internal_metrics_in"])?
        .connect_component("enrich", ["dsd_agg", "internal_metrics_agg"])?
        .connect_component("dd_metrics_out", ["enrich"])?;
    let built_topology = blueprint.build().await?;
    let running_topology = built_topology.spawn().await?;

    let startup_time = start.elapsed();

    info!(
        init_time_ms = startup_time.as_millis(),
        "topology running, waiting for interrupt..."
    );

    tokio::signal::ctrl_c().await?;

    info!("received ctrl-c, shutting down...");

    running_topology.shutdown().await?;

    info!("topology shut down, exiting.");

    Ok(())
}
