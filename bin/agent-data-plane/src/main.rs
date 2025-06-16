//! Main benchmarking binary.
//!
//! This binary emulates the standalone DogStatsD binary, listening for DogStatsD over UDS, aggregating metrics over a
//! 10 second window, and shipping those metrics to the Datadog Platform.

#![deny(warnings)]
#![deny(missing_docs)]
use std::time::{Duration, Instant};

use clap::Parser as _;
use memory_accounting::{ComponentBounds, ComponentRegistry};
use saluki_app::prelude::*;
use saluki_components::{
    destinations::{DatadogEventsConfiguration, DatadogMetricsConfiguration, DatadogServiceChecksConfiguration},
    sources::DogStatsDConfiguration,
    transforms::{
        AggregateConfiguration, ChainedConfiguration, DogstatsDMapperConfiguration, DogstatsDPrefixFilterConfiguration,
        HostEnrichmentConfiguration, HostTagsConfiguration, PreaggregationFilterConfiguration,
    },
};
use saluki_config::{ConfigurationLoader, GenericConfiguration, RefresherConfiguration};
use saluki_core::topology::TopologyBlueprint;
use saluki_env::EnvironmentProvider as _;
use saluki_error::{ErrorContext as _, GenericError};
use saluki_health::HealthRegistry;
use tokio::select;
use tracing::{error, info, warn};

mod components;

mod config;
use self::config::{Action, Cli, DebugConfig, RunConfig};

mod env_provider;
use self::env_provider::ADPEnvironmentProvider;

mod internal;
use self::internal::{spawn_control_plane, spawn_internal_observability_topology};

pub(crate) mod state;

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
    let cli = Cli::parse();

    if let Err(e) = initialize_dynamic_logging(None).await {
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

    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .unwrap();

    match cli.action {
        Some(Action::Run(config)) => match run(started, config).await {
            Ok(()) => info!("Agent Data Plane stopped."),
            Err(e) => {
                error!("{:?}", e);
                std::process::exit(1);
            }
        },
        Some(Action::Debug(DebugConfig::ResetLogLevel)) => {
            // TODO: call to /logging/reset
            println!("brianna Resetting log level");
            let response = match client.post("https://localhost:5101/logging/reset").send().await {
                Ok(resp) => resp,
                Err(e) => {
                    error!("Failed to send request: {}", e);
                    std::process::exit(1);
                }
            };
            println!("brianna Response: {:?}", response);
            if response.status().is_success() {
                println!("Log level reset successful");
            } else {
                eprintln!("Failed to reset log level: {}", response.status());
            }

            println!("brianna Response: {:?}", response);
        }
        Some(Action::Debug(DebugConfig::SetLogLevel(config))) => {
            // TODO: call to /logging/override
            println!("brianna Setting log level");
            let filter_directives = config.filter_directives;
            let duration_secs = config.duration_secs;
            println!("brianna Filter directives: {}", filter_directives);
            println!("brianna Duration: {}", duration_secs);

            let response = match client
                .post("https://localhost:5101/logging/override")
                .query(&[("time_secs", duration_secs)])
                .body(filter_directives)
                .send()
                .await
            {
                Ok(resp) => resp,
                Err(e) => {
                    error!("Failed to send request: {}", e);
                    std::process::exit(1);
                }
            };
            println!("brianna Response: {:?}", response);
            if response.status().is_success() {
                println!("Log level override successful");
            } else {
                eprintln!("Failed to override log level: {}", response.status());
            }
        }
        None => {
            let default_config = RunConfig {
                config: std::path::PathBuf::from("/etc/datadog-agent/datadog.yaml"),
            };
            match run(started, default_config).await {
                Ok(()) => info!("Agent Data Plane stopped."),
                Err(e) => {
                    error!("{:?}", e);
                    std::process::exit(1);
                }
            }
        }
    }
}

async fn run(started: Instant, run_config: RunConfig) -> Result<(), GenericError> {
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
        .with_default_secrets_resolution()
        .await?
        .into_generic()?;

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

    let mut dd_metrics_config = DatadogMetricsConfiguration::from_configuration(configuration)
        .error_context("Failed to configure Datadog Metrics destination.")?;
    let mut dd_events_config = DatadogEventsConfiguration::from_configuration(configuration)
        .error_context("Failed to configure Datadog Events destination.")?;
    let mut dd_service_checks_config = DatadogServiceChecksConfiguration::from_configuration(configuration)
        .error_context("Failed to configure Datadog Service Checks destination.")?;

    match RefresherConfiguration::from_configuration(configuration) {
        Ok(refresher_configuration) => {
            let refreshable_configuration = refresher_configuration.build()?;

            dd_metrics_config.add_refreshable_configuration(refreshable_configuration.clone());
            dd_events_config.add_refreshable_configuration(refreshable_configuration.clone());
            dd_service_checks_config.add_refreshable_configuration(refreshable_configuration);
        }
        Err(_) => {
            info!(
               "Dynamic configuration refreshing will be unavailable due to failure to configure refresher configuration."
           )
        }
    }

    let mut blueprint = TopologyBlueprint::new("primary", component_registry);
    blueprint
        .add_source("dsd_in", dsd_config)?
        .add_transform("dsd_agg", dsd_agg_config)?
        .add_transform("enrich", enrich_config)?
        .add_transform("dsd_prefix_filter", dsd_prefix_filter_configuration)?
        .add_destination("dd_metrics_out", dd_metrics_config)?
        .add_destination("dd_events_out", dd_events_config)?
        .add_destination("dd_service_checks_out", dd_service_checks_config)?
        .connect_component("dsd_agg", ["dsd_in.metrics"])?
        .connect_component("dsd_prefix_filter", ["dsd_agg"])?
        .connect_component("enrich", ["dsd_prefix_filter"])?
        .connect_component("dd_metrics_out", ["enrich"])?
        .connect_component("dd_events_out", ["dsd_in.events"])?
        .connect_component("dd_service_checks_out", ["dsd_in.service_checks"])?;

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

        let mut preaggr_processing = ChainedConfiguration::default()
            .with_transform_builder("preaggr_filter", PreaggregationFilterConfiguration::default());

        if !in_standalone_mode {
            let mut host_tags_config = HostTagsConfiguration::from_configuration(configuration).await?;
            host_tags_config.ignore_duration();
            preaggr_processing = preaggr_processing.with_transform_builder("preaggr_host_tags", host_tags_config);
        }

        let dd_metrics_config = DatadogMetricsConfiguration::from_configuration(configuration)
            .and_then(|config| config.with_endpoint_override(preaggr_dd_url, preaggr_api_key, preaggr_request_path))
            .error_context("Failed to configure pre-aggregation Datadog Metrics destination.")?;

        blueprint
            .add_transform("preaggr_processing", preaggr_processing)?
            .add_destination("preaggr_dd_metrics_out", dd_metrics_config)?
            .connect_component("preaggr_processing", ["enrich"])?
            .connect_component("preaggr_dd_metrics_out", ["preaggr_processing"])?;
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
