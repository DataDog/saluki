use std::time::{Duration, Instant};

use memory_accounting::{ComponentBounds, ComponentRegistry};
use saluki_app::prelude::*;
use saluki_components::{
    encoders::{
        BufferedIncrementalConfiguration, DatadogEventsConfiguration, DatadogMetricsConfiguration,
        DatadogServiceChecksConfiguration,
    },
    forwarders::DatadogConfiguration,
    sources::DogStatsDConfiguration,
    transforms::{
        AggregateConfiguration, ChainedConfiguration, DogstatsDMapperConfiguration, DogstatsDPrefixFilterConfiguration,
        HostEnrichmentConfiguration, HostTagsConfiguration, PreaggregationFilterConfiguration,
    },
};
use saluki_config::{ConfigurationLoader, GenericConfiguration};
use saluki_core::topology::TopologyBlueprint;
use saluki_env::EnvironmentProvider as _;
use saluki_error::{ErrorContext as _, GenericError};
use saluki_health::HealthRegistry;
use tokio::{select, time::interval};
use tracing::{error, info, warn};

use crate::config::RunConfig;
use crate::env_provider::ADPEnvironmentProvider;
use crate::internal::{spawn_control_plane, spawn_internal_observability_topology};

pub async fn run(started: Instant, run_config: RunConfig) -> Result<(), GenericError> {
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
        .try_from_yaml(&run_config.config)
        .from_environment("DD")?
        .with_dynamic_configuration()?
        .with_default_secrets_resolution()
        .await?
        .into_generic()
        .await?;

    // Set up all of the building blocks for building our topologies and launching internal processes.
    let component_registry = ComponentRegistry::default();
    let health_registry = HealthRegistry::new();
    let env_provider =
        ADPEnvironmentProvider::from_configuration(&configuration, &component_registry, &health_registry).await?;

    // Create our primary data topology and spawn any internal processes, which will ensure all relevant components are
    // registered and accounted for in terms of memory usage.
    let blueprint = create_topology(&configuration, &env_provider, &component_registry).await?;

    spawn_internal_observability_topology(&configuration, &component_registry, health_registry.clone())
        .error_context("Failed to spawn internal observability topology.")?;
    spawn_control_plane(
        configuration.clone(),
        &component_registry,
        health_registry.clone(),
        env_provider,
    )
    .error_context("Failed to spawn control plane.")?;

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
        .with_workload_provider(env_provider.workload().clone());
    let dsd_agg_config = AggregateConfiguration::from_configuration(configuration)
        .error_context("Failed to configure aggregate transform.")?;
    let dsd_prefix_filter_configuration = DogstatsDPrefixFilterConfiguration::from_configuration(configuration)?;
    let host_enrichment_config = HostEnrichmentConfiguration::from_environment_provider(env_provider.clone());
    let dsd_mapper_config = DogstatsDMapperConfiguration::from_configuration(configuration)?;
    let mut enrich_config = ChainedConfiguration::default()
        .with_transform_builder("host_enrichment", host_enrichment_config)
        .with_transform_builder("dogstatsd_mapper", dsd_mapper_config);

    let in_standalone_mode = configuration.get_typed_or_default::<bool>("adp.standalone_mode");
    if !in_standalone_mode {
        let host_tags_config = HostTagsConfiguration::from_configuration(configuration).await?;
        enrich_config = enrich_config.with_transform_builder("host_tags", host_tags_config);
    }

    let dd_metrics_config = DatadogMetricsConfiguration::from_configuration(configuration)
        .error_context("Failed to configure Datadog Metrics encoder.")?;
    let dd_events_config = DatadogEventsConfiguration::from_configuration(configuration)
        .map(BufferedIncrementalConfiguration::from_encoder_builder)
        .error_context("Failed to configure Datadog Events encoder.")?;
    let dd_service_checks_config = DatadogServiceChecksConfiguration::from_configuration(configuration)
        .map(BufferedIncrementalConfiguration::from_encoder_builder)
        .error_context("Failed to configure Datadog Service Checks encoder.")?;
    let dd_forwarder_config = DatadogConfiguration::from_configuration(configuration)
        .error_context("Failed to configure Datadog forwarder.")?;

    let mut blueprint = TopologyBlueprint::new("primary", component_registry);
    blueprint
        // Components.
        .add_source("dsd_in", dsd_config)?
        .add_transform("dsd_agg", dsd_agg_config)?
        .add_transform("dsd_enrich", enrich_config)?
        .add_transform("dsd_prefix_filter", dsd_prefix_filter_configuration)?
        .add_encoder("dd_metrics_encode", dd_metrics_config)?
        .add_encoder("dd_events_encode", dd_events_config)?
        .add_encoder("dd_service_checks_encode", dd_service_checks_config)?
        .add_forwarder("dd_out", dd_forwarder_config)?
        // Metrics.
        .connect_component("dsd_agg", ["dsd_in.metrics"])?
        .connect_component("dsd_prefix_filter", ["dsd_agg"])?
        .connect_component("dsd_enrich", ["dsd_prefix_filter"])?
        .connect_component("dd_metrics_encode", ["dsd_enrich"])?
        // Events.
        .connect_component("dd_events_encode", ["dsd_in.events"])?
        // Service checks.
        .connect_component("dd_service_checks_encode", ["dsd_in.service_checks"])?
        // Forwarding.
        .connect_component(
            "dd_out",
            ["dd_metrics_encode", "dd_events_encode", "dd_service_checks_encode"],
        )?;

    if configuration.get_typed_or_default::<bool>("enable_preaggr_pipeline") {
        let preaggr_dd_url = configuration
            .try_get_typed::<String>("preaggr_dd_url")
            .error_context("Failed to query pre-aggregation pipeline URL.")?
            .unwrap_or_else(|| "https://api.datad0g.com".to_string());
        let preaggr_api_key = configuration
            .get_typed::<String>("preaggr_api_key")
            .error_context("Failed to query pre-aggregation pipeline API key.")?;
        let preaggr_request_path = configuration
            .try_get_typed::<String>("preaggr_request_path")
            .error_context("Failed to query pre-aggregation pipeline request path.")?
            .unwrap_or_else(|| "/api/intake/pipelines/ddseries".to_string());

        let preaggr_processing = ChainedConfiguration::default()
            .with_transform_builder("preaggr_filter", PreaggregationFilterConfiguration::default());

        let preaggr_dd_metrics_config = DatadogMetricsConfiguration::from_configuration(configuration)
            .and_then(|config| config.with_endpoint_path_override(preaggr_request_path))
            .error_context("Failed to configure pre-aggregation Datadog Metrics encoder.")?;

        let preaggr_dd_forwarder_config = DatadogConfiguration::from_configuration(configuration)
            .map(|config| config.with_endpoint_override(preaggr_dd_url, preaggr_api_key))
            .error_context("Failed to configure pre-aggregation Datadog forwarder.")?;

        blueprint
            .add_transform("preaggr_processing", preaggr_processing)?
            .add_encoder("preaggr_dd_metrics_encode", preaggr_dd_metrics_config)?
            .add_forwarder("preaggr_dd_out", preaggr_dd_forwarder_config)?
            .connect_component("preaggr_processing", ["enrich"])?
            .connect_component("preaggr_dd_metrics_encode", ["preaggr_processing"])?
            .connect_component("preaggr_dd_out", ["preaggr_dd_metrics_encode"])?;
    }

    Ok(blueprint)
}

fn write_sizing_guide(bounds: ComponentBounds) -> Result<(), GenericError> {
    use std::{
        fs::File,
        io::{BufWriter, Write},
    };

    let template = include_str!("../sizing_guide_template.html");
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
