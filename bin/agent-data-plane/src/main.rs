//! Main benchmarking binary.
//!
//! This binary emulates the standalone DogStatsD binary, listening for DogStatsD over UDS, aggregating metrics over a
//! 10 second window, and shipping those metrics to the Datadog Platform.

#![deny(warnings)]
#![deny(missing_docs)]
use std::{
    future::pending,
    time::{Duration, Instant},
};

use memory_accounting::ComponentRegistry;

use saluki_app::{api::APIBuilder, prelude::*};
use saluki_components::{
    destinations::{
        DatadogEventsServiceChecksConfiguration, DatadogMetricsConfiguration, DatadogStatusFlareConfiguration,
        PrometheusConfiguration,
    },
    sources::{DogStatsDConfiguration, InternalMetricsConfiguration},
    transforms::{
        AggregateConfiguration, ChainedConfiguration, HostEnrichmentConfiguration, OriginEnrichmentConfiguration,
    },
};
use saluki_config::{ConfigurationLoader, GenericConfiguration, RefreshableConfiguration, RefresherConfiguration};
use saluki_core::topology::TopologyBlueprint;
use saluki_error::{ErrorContext as _, GenericError};
use saluki_health::HealthRegistry;
use saluki_io::net::ListenAddress;
use tokio::select;
use tracing::{error, info, warn};

mod components;
use self::components::remapper::AgentTelemetryRemapperConfiguration;

mod env_provider;
use self::env_provider::ADPEnvironmentProvider;

#[global_allocator]
static ALLOC: memory_accounting::allocator::TrackingAllocator<std::alloc::System> =
    memory_accounting::allocator::TrackingAllocator::new(std::alloc::System);

#[tokio::main]
async fn main() {
    let started = Instant::now();

    if let Err(e) = initialize_logging(None) {
        fatal_and_exit(format!("failed to initialize logging: {}", e));
    }

    if let Err(e) = initialize_metrics("adp").await {
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
            // TODO: It'd be better to take the error cause chain and write it out as a list of errors, instead of
            // having it be multi-line, since the multi-line bit messes with the log formatting.
            error!("{:?}", e);
            std::process::exit(1);
        }
    }
}

async fn run(started: Instant) -> Result<(), GenericError> {
    let app_details = saluki_metadata::get_app_details();
    info!(
        version = app_details.version().raw(),
        git_hash = app_details.git_hash(),
        target_arch = app_details.target_arch(),
        build_time = app_details.build_time(),
        "Agent Data Plane starting..."
    );

    // Load our configuration and create all high-level primitives (health registry, component registry, environment
    // provider, etc) that are needed to build the topology.
    let configuration = ConfigurationLoader::default()
        .try_from_yaml("/etc/datadog-agent/datadog.yaml")
        .from_environment("DD")?
        .with_default_secrets_resolution()
        .await?
        .into_generic()?;

    let component_registry = ComponentRegistry::default();
    let health_registry = HealthRegistry::new();

    let env_provider =
        ADPEnvironmentProvider::from_configuration(&configuration, &component_registry, &health_registry).await?;

    // Create a simple pipeline that runs a DogStatsD source, an aggregation transform to bucket into 10 second windows,
    // and a Datadog Metrics destination that forwards aggregated buckets to the Datadog Platform.
    let blueprint = create_topology(&configuration, env_provider, &component_registry).await?;

    // Build our administrative API server.
    let primary_api_listen_address = configuration
        .try_get_typed("api_listen_address")
        .error_context("Failed to get API listen address.")?
        .unwrap_or_else(|| ListenAddress::Tcp(([0, 0, 0, 0], 5100).into()));

    let primary_api = APIBuilder::new()
        .with_handler(health_registry.api_handler())
        .with_handler(component_registry.api_handler())
        .with_self_signed_tls();

    // Run memory bounds validation to ensure that we can launch the topology with our configured memory limit, if any.
    let bounds_configuration = MemoryBoundsConfiguration::try_from_config(&configuration)?;
    let memory_limiter = initialize_memory_bounds(bounds_configuration, component_registry)?;

    // Bounds validation succeeded, so now we'll build and spawn the topology.
    let built_topology = blueprint.build().await?;
    let mut running_topology = built_topology.spawn(&health_registry, memory_limiter).await?;

    // Spawn the health checker.
    health_registry.spawn().await?;

    // Run the API server now that we've been able to launch the topology.
    //
    // TODO: Use something better than `pending()`... perhaps something like a more generalized
    // `ComponentShutdownCoordinator` that allows for triggering and waiting for all attached tasks to signal that
    // they've shutdown.
    primary_api.serve(primary_api_listen_address, pending()).await?;

    let startup_time = started.elapsed();

    info!(
        init_time_ms = startup_time.as_millis(),
        "Topology running, waiting for interrupt..."
    );

    let mut finished_with_error = false;
    select! {
        _ = running_topology.wait_for_unexpected_finish() => {
            error!("Component unexpectedly finished. Shutting down...");
            finished_with_error = true;
        },
        _ = tokio::signal::ctrl_c() => {
            info!("Received SIGINT, shutting down...");
        }
    }

    match running_topology.shutdown_with_timeout(Duration::from_secs(30)).await {
        Ok(()) => {
            if finished_with_error {
                warn!("Topology shutdown complete despite error(s).")
            } else {
                info!("Topology shutdown successfully.")
            }
            Ok(())
        }
        Err(e) => Err(e),
    }
}

async fn create_topology(
    configuration: &GenericConfiguration, env_provider: ADPEnvironmentProvider, component_registry: &ComponentRegistry,
) -> Result<TopologyBlueprint, GenericError> {
    // Create a simple pipeline that runs a DogStatsD source, an aggregation transform to bucket into 10 second windows,
    // and a Datadog Metrics destination that forwards aggregated buckets to the Datadog Platform.
    let dsd_config = DogStatsDConfiguration::from_configuration(configuration)
        .error_context("Failed to configure DogStatsD source.")?;
    let dsd_agg_config = AggregateConfiguration::from_configuration(configuration)
        .error_context("Failed to configure aggregate transform.")?;

    let host_enrichment_config = HostEnrichmentConfiguration::from_environment_provider(env_provider.clone());
    let origin_enrichment_config = OriginEnrichmentConfiguration::from_configuration(configuration)
        .error_context("Failed to configure origin enrichment transform.")?
        .with_environment_provider(env_provider);
    let enrich_config = ChainedConfiguration::default()
        .with_transform_builder(host_enrichment_config)
        .with_transform_builder(origin_enrichment_config);

    let mut dd_metrics_config = DatadogMetricsConfiguration::from_configuration(configuration)
        .error_context("Failed to configure Datadog Metrics destination.")?;

    match RefresherConfiguration::from_configuration(configuration) {
        Ok(refresher_configuration) => {
            let refreshable_configuration: RefreshableConfiguration = refresher_configuration.build()?;
            dd_metrics_config.add_refreshable_configuration(refreshable_configuration);
        }
        Err(_) => {
            info!(
                "Dynamic configuration refreshing will be unavailable due to failure to configure refresher configuration."
            )
        }
    }

    let status_configuration = DatadogStatusFlareConfiguration::from_configuration(configuration).await?;

    let events_service_checks_config = DatadogEventsServiceChecksConfiguration::from_configuration(configuration)
        .error_context("Failed to configure Datadog Events/Service Checks destination.")?;

    let int_metrics_config = InternalMetricsConfiguration;
    let int_metrics_remap_config = AgentTelemetryRemapperConfiguration::new();

    let topology_registry = component_registry.get_or_create("topology");
    let mut blueprint = TopologyBlueprint::from_component_registry(topology_registry);

    blueprint
        .add_source("dsd_in", dsd_config)?
        .add_transform("dsd_agg", dsd_agg_config)?
        .add_transform("enrich", enrich_config)?
        .add_destination("dd_metrics_out", dd_metrics_config)?
        .add_destination("dd_events_sc_out", events_service_checks_config)?
        .connect_component("dsd_agg", ["dsd_in.metrics"])?
        .connect_component("enrich", ["dsd_agg"])?
        .connect_component("dd_metrics_out", ["enrich"])?
        .connect_component("dd_events_sc_out", ["dsd_in.events", "dsd_in.service_checks"])?;

    let use_prometheus = configuration.get_typed_or_default::<bool>("telemetry_enabled");
    let use_status_flare_component = !configuration.get_typed_or_default::<bool>("adp_standalone_mode");

    // Insert internal metrics source only if internal telemetry or status and flare component is enabled.
    if use_prometheus || use_status_flare_component {
        blueprint
            .add_source("internal_metrics_in", int_metrics_config)?
            .add_transform("internal_metrics_remap", int_metrics_remap_config)?
            .connect_component("internal_metrics_remap", ["internal_metrics_in"])?;
    }

    // Insert a Datadog Status Flare destination if we've been instructed to not do a no-op.
    if use_status_flare_component {
        blueprint
            .add_destination("dd_status_flare_out", status_configuration)?
            .connect_component("dd_status_flare_out", ["internal_metrics_remap"])?;
    }

    // Insert a Prometheus scrape destination if we've been instructed to enable internal telemetry.
    if use_prometheus {
        let prometheus_config = PrometheusConfiguration::from_configuration(configuration)?;
        blueprint
            .add_destination("internal_metrics_out", prometheus_config)?
            .connect_component("internal_metrics_out", ["internal_metrics_remap"])?;
    }

    Ok(blueprint)
}
