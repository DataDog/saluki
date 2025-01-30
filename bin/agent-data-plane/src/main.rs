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

use memory_accounting::{ComponentBounds, ComponentRegistry};
use saluki_app::{api::APIBuilder, logging::LoggingAPIHandler, prelude::*};
use saluki_components::{
    destinations::{
        new_remote_agent_service, DatadogEventsServiceChecksConfiguration, DatadogMetricsConfiguration,
        DatadogStatusFlareConfiguration, PrometheusConfiguration,
    },
    sources::{DogStatsDConfiguration, InternalMetricsConfiguration},
    transforms::{
        AggregateConfiguration, ChainedConfiguration, DogstatsDPrefixFilterConfiguration, HostEnrichmentConfiguration,
    },
};
use saluki_config::{ConfigurationLoader, GenericConfiguration, RefreshableConfiguration, RefresherConfiguration};
use saluki_core::topology::TopologyBlueprint;
use saluki_env::EnvironmentProvider as _;
use saluki_error::{ErrorContext as _, GenericError};
use saluki_health::HealthRegistry;
use saluki_io::net::ListenAddress;
use tokio::select;
use tracing::{error, info, warn};

mod components;
use self::components::remapper::AgentTelemetryRemapperConfiguration;

mod env_provider;
use self::env_provider::ADPEnvironmentProvider;

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

    let logging_api_handler = match initialize_dynamic_logging(None).await {
        Ok(handler) => handler,
        Err(e) => {
            fatal_and_exit(format!("failed to initialize logging: {}", e));
            return;
        }
    };

    if let Err(e) = initialize_metrics("adp").await {
        fatal_and_exit(format!("failed to initialize metrics: {}", e));
    }

    if let Err(e) = initialize_allocator_telemetry().await {
        fatal_and_exit(format!("failed to initialize allocator telemetry: {}", e));
    }

    if let Err(e) = initialize_tls() {
        fatal_and_exit(format!("failed to initialize TLS: {}", e));
    }

    match run(started, logging_api_handler).await {
        Ok(()) => info!("Agent Data Plane stopped."),
        Err(e) => {
            // TODO: It'd be better to take the error cause chain and write it out as a list of errors, instead of
            // having it be multi-line, since the multi-line bit messes with the log formatting.
            error!("{:?}", e);
            std::process::exit(1);
        }
    }
}

async fn run(started: Instant, logging_api_handler: LoggingAPIHandler) -> Result<(), GenericError> {
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
    let blueprint = create_topology(&configuration, &env_provider, &component_registry).await?;

    // Build our unprivileged and privileged API server.
    //
    // The unprivileged API is purely for things like health checks or read-only information. The privileged API is
    // meant for sensitive information or actions that require elevated permissions.
    let unprivileged_api = APIBuilder::new()
        .with_handler(health_registry.api_handler())
        .with_handler(component_registry.api_handler());

    let privileged_api = APIBuilder::new()
        .with_self_signed_tls()
        .with_grpc_service(new_remote_agent_service())
        .with_handler(logging_api_handler)
        .with_optional_handler(env_provider.workload_api_handler());

    // Run memory bounds validation to ensure that we can launch the topology with our configured memory limit, if any.
    let bounds_configuration = MemoryBoundsConfiguration::try_from_config(&configuration)?;
    let memory_limiter = initialize_memory_bounds(bounds_configuration, &component_registry)?;

    if let Ok(val) = std::env::var("DD_ADP_WRITE_SIZING_GUIDE") {
        if val != "false" {
            if let Err(error) = write_sizing_guide(component_registry.as_bounds()) {
                warn!("Failed to write sizing guide: {}", error);
            } else {
                return Ok(());
            }
        }
    }

    // Bounds validation succeeded, so now we'll build and spawn the topology.
    let built_topology = blueprint.build().await?;
    let mut running_topology = built_topology.spawn(&health_registry, memory_limiter).await?;

    // Spawn the health checker.
    health_registry.spawn().await?;

    // Spawn both of our API servers.
    spawn_unprivileged_api(&configuration, unprivileged_api).await?;
    spawn_privileged_api(&configuration, privileged_api).await?;

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
    configuration: &GenericConfiguration, env_provider: &ADPEnvironmentProvider, component_registry: &ComponentRegistry,
) -> Result<TopologyBlueprint, GenericError> {
    // Create a simple pipeline that runs a DogStatsD source, an aggregation transform to bucket into 10 second windows,
    // and a Datadog Metrics destination that forwards aggregated buckets to the Datadog Platform.
    let dsd_config = DogStatsDConfiguration::from_configuration(configuration)
        .error_context("Failed to configure DogStatsD source.")?
        .with_origin_tags_resolver(env_provider.workload().clone());
    let dsd_agg_config = AggregateConfiguration::from_configuration(configuration)
        .error_context("Failed to configure aggregate transform.")?;
    let dsd_prefix_filter_configuration = DogstatsDPrefixFilterConfiguration::from_configuration(configuration)?;
    let host_enrichment_config = HostEnrichmentConfiguration::from_environment_provider(env_provider.clone());
    let enrich_config = ChainedConfiguration::default().with_transform_builder(host_enrichment_config);
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

    let events_service_checks_config = DatadogEventsServiceChecksConfiguration::from_configuration(configuration)
        .error_context("Failed to configure Datadog Events/Service Checks destination.")?;

    let topology_registry = component_registry.get_or_create("topology");
    let mut blueprint = TopologyBlueprint::from_component_registry(topology_registry);

    blueprint
        .add_source("dsd_in", dsd_config)?
        .add_transform("dsd_agg", dsd_agg_config)?
        .add_transform("enrich", enrich_config)?
        .add_transform("dsd_prefix_filter", dsd_prefix_filter_configuration)?
        .add_destination("dd_metrics_out", dd_metrics_config)?
        .add_destination("dd_events_sc_out", events_service_checks_config)?
        .connect_component("dsd_agg", ["dsd_in.metrics"])?
        .connect_component("dsd_prefix_filter", ["dsd_agg"])?
        .connect_component("enrich", ["dsd_prefix_filter"])?
        .connect_component("dd_metrics_out", ["enrich"])?
        .connect_component("dd_events_sc_out", ["dsd_in.events", "dsd_in.service_checks"])?;

    let telemetry_enabled = configuration.get_typed_or_default::<bool>("telemetry_enabled");
    let in_standalone_mode = configuration.get_typed_or_default::<bool>("adp.standalone_mode");

    // When telemetry is enabled, or we're not in standalone mode, we need to collect internal metrics, so add those
    // components and route them here.
    if telemetry_enabled || !in_standalone_mode {
        let int_metrics_config = InternalMetricsConfiguration;
        let int_metrics_remap_config = AgentTelemetryRemapperConfiguration::new();

        blueprint
            .add_source("internal_metrics_in", int_metrics_config)?
            .add_transform("internal_metrics_remap", int_metrics_remap_config)?
            .connect_component("internal_metrics_remap", ["internal_metrics_in"])?;
    }

    // When not in standalone mode, install the necessary components for registering ourselves with the Datadog Agent as
    // a "remote agent", which wires up ADP to allow the Datadog Agent to query it for status and flare information.
    if !in_standalone_mode {
        let status_configuration = DatadogStatusFlareConfiguration::from_configuration(configuration).await?;
        blueprint
            .add_destination("dd_status_flare_out", status_configuration)?
            .connect_component("dd_status_flare_out", ["internal_metrics_remap"])?;
    }

    // When internal telemetry is enabled, expose a Prometheus scrape endpoint that the Datadog Agent will pull from.
    if telemetry_enabled {
        let prometheus_config = PrometheusConfiguration::from_configuration(configuration)?;
        info!(
            "Serving telemetry scrape endpoint on {}.",
            prometheus_config.listen_address()
        );

        blueprint
            .add_destination("internal_metrics_out", prometheus_config)?
            .connect_component("internal_metrics_out", ["internal_metrics_remap"])?;
    }

    Ok(blueprint)
}

fn write_sizing_guide(bounds: ComponentBounds) -> Result<(), GenericError> {
    use std::{
        fs::File,
        io::{BufWriter, Write},
    };

    let template = include_str!("sizing_guide_template.html");
    let mut output = BufWriter::new(File::create("sizing_guide.html")?);
    for line in template.lines() {
        if line.trim() == "<!-- INSERT GENERATED CONTENT -->" {
            serde_json::to_writer_pretty(&mut output, &bounds.to_exprs())?;
        } else {
            output.write_all(line.as_bytes())?;
        }
        output.write_all(b"\n")?;
    }
    info!("Wrote sizing guide to sizing_guide.html");
    output.flush()?;

    Ok(())
}

async fn spawn_unprivileged_api(
    configuration: &GenericConfiguration, api_builder: APIBuilder,
) -> Result<(), GenericError> {
    let api_listen_address = configuration
        .try_get_typed("api_listen_address")
        .error_context("Failed to get API listen address.")?
        .unwrap_or_else(|| ListenAddress::Tcp(([0, 0, 0, 0], 5100).into()));

    // TODO: Use something better than `pending()`... perhaps something like a more generalized
    // `ComponentShutdownCoordinator` that allows for triggering and waiting for all attached tasks to signal that
    // they've shutdown.
    tokio::spawn(async move {
        info!("Serving unprivileged API on {}.", api_listen_address);

        if let Err(e) = api_builder.serve(api_listen_address, pending()).await {
            error!("Failed to serve unprivileged API: {}", e);
        }
    });

    Ok(())
}

async fn spawn_privileged_api(
    configuration: &GenericConfiguration, api_builder: APIBuilder,
) -> Result<(), GenericError> {
    let api_listen_address = configuration
        .try_get_typed("secure_api_listen_address")
        .error_context("Failed to get secure API listen address.")?
        .unwrap_or_else(|| ListenAddress::Tcp(([0, 0, 0, 0], 5101).into()));

    // TODO: Use something better than `pending()`... perhaps something like a more generalized
    // `ComponentShutdownCoordinator` that allows for triggering and waiting for all attached tasks to signal that
    // they've shutdown.
    tokio::spawn(async move {
        info!("Serving privileged API on {}.", api_listen_address);

        if let Err(e) = api_builder.serve(api_listen_address, pending()).await {
            error!("Failed to serve privileged API: {}", e);
        }
    });

    Ok(())
}
