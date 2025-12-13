use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

use argh::FromArgs;
use memory_accounting::{ComponentBounds, ComponentRegistry};
use saluki_app::{
    memory::{initialize_memory_bounds, MemoryBoundsConfiguration},
    metrics::emit_startup_metrics,
};
#[cfg(feature = "python-checks")]
use saluki_components::sources::ChecksConfiguration;
use saluki_components::{
    destinations::DogStatsDStatisticsConfiguration,
    encoders::{
        BufferedIncrementalConfiguration, DatadogEventsConfiguration, DatadogLogsConfiguration,
        DatadogMetricsConfiguration, DatadogServiceChecksConfiguration,
    },
    forwarders::{DatadogConfiguration, TraceAgentForwarderConfiguration},
    relays::otlp::OtlpRelayConfiguration,
    sources::{DogStatsDConfiguration, OtlpConfiguration},
    transforms::{
        AggregateConfiguration, ChainedConfiguration, DogstatsDMapperConfiguration, DogstatsDPrefixFilterConfiguration,
        HostEnrichmentConfiguration, HostTagsConfiguration,
    },
};
use saluki_config::{ConfigurationLoader, GenericConfiguration};
use saluki_core::topology::TopologyBlueprint;
use saluki_env::{configstream::create_config_stream, EnvironmentProvider as _};
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use saluki_health::HealthRegistry;
use tokio::{select, time::interval};
use tracing::{error, info, warn};

use crate::internal::{spawn_control_plane, spawn_internal_observability_topology};
use crate::{config::DataPlaneConfiguration, env_provider::ADPEnvironmentProvider};

/// Runs the data plane.
#[derive(FromArgs, Debug)]
#[argh(subcommand, name = "run")]
pub struct RunCommand {
    /// path to the PID file
    #[argh(option, short = 'p', long = "pidfile")]
    pub pid_file: Option<PathBuf>,
}

/// Entrypoint for the `run` commands.
pub async fn handle_run_command(
    started: Instant, bootstrap_config_path: PathBuf, bootstrap_config: GenericConfiguration,
) -> Result<(), GenericError> {
    let app_details = saluki_metadata::get_app_details();
    info!(
        version = app_details.version().raw(),
        git_hash = app_details.git_hash(),
        target_arch = app_details.target_arch(),
        build_time = app_details.build_time(),
        process_id = std::process::id(),
        "Agent Data Plane starting..."
    );

    // Load our "bootstrap" configuration, and then determine if we need to actually source our configuration from the control plane.
    //
    // When we're in standalone mode, we'll use the bootstrap configuration as our final configuration.
    let bootstrap_dp_config = DataPlaneConfiguration::from_configuration(&bootstrap_config)
        .error_context("Failed to load data plane configuration.")?;

    info!("Bootstrap DP config: {:?}", bootstrap_dp_config);

    let in_standalone_mode = bootstrap_dp_config.standalone_mode();
    let use_new_config_stream_endpoint = bootstrap_dp_config.use_new_config_stream_endpoint();
    let (config, dp_config) = if !in_standalone_mode && use_new_config_stream_endpoint {
        let config_updates_receiver = create_config_stream(&bootstrap_config)
            .await
            .error_context("Failed to create configuration updates stream from control plane.")?;

        // Build a new configuration that uses the configuration sent by the control plane as the authoritative
        // configuration source, but with environment variables on top of that to allow for ADP-specific overriding: log
        // level, etc.
        let dynamic_config = ConfigurationLoader::default()
            .from_yaml(&bootstrap_config_path)
            .error_context("Failed to load Datadog Agent configuration file.")?
            .with_dynamic_configuration(config_updates_receiver)
            .from_environment(crate::internal::platform::DATADOG_AGENT_ENV_VAR_PREFIX)?
            .with_default_secrets_resolution()
            .await?
            .into_generic()
            .await?;

        info!("Waiting for initial configuration from Datadog Agent...");
        dynamic_config.ready().await;
        info!("Initial configuration received.");

        // Reload our data plane configuration based on the dynamic configuration.
        let dynamic_dp_config = DataPlaneConfiguration::from_configuration(&dynamic_config)
            .error_context("Failed to load data plane configuration.")?;

        (dynamic_config, dynamic_dp_config)
    } else {
        // If dynamic configuration is disabled, the bootstrap configuration is already the complete and final configuration.
        (bootstrap_config, bootstrap_dp_config)
    };

    if !in_standalone_mode && !dp_config.enabled() {
        info!("Agent Data Plane is not enabled. Exiting.");
        return Ok(());
    }

    // Set up all of the building blocks for building our topologies and launching internal processes.
    let component_registry = ComponentRegistry::default();
    let health_registry = HealthRegistry::new();
    let env_provider =
        ADPEnvironmentProvider::from_configuration(&config, &dp_config, &component_registry, &health_registry).await?;

    let dsd_stats_config = DogStatsDStatisticsConfiguration::new();

    // Create our primary data topology and spawn any internal processes, which will ensure all relevant components are
    // registered and accounted for in terms of memory usage.
    let blueprint = create_topology(
        &config,
        &dp_config,
        &env_provider,
        &component_registry,
        dsd_stats_config.clone(),
    )
    .await?;

    spawn_internal_observability_topology(&dp_config, &component_registry, health_registry.clone())
        .error_context("Failed to spawn internal observability topology.")?;
    spawn_control_plane(
        &config,
        &dp_config,
        &component_registry,
        health_registry.clone(),
        env_provider,
        dsd_stats_config,
    )
    .await
    .error_context("Failed to spawn control plane.")?;

    // Run memory bounds validation to ensure that we can launch the topology with our configured memory limit, if any.
    let bounds_config = MemoryBoundsConfiguration::try_from_config(&config)?;
    let memory_limiter = initialize_memory_bounds(bounds_config, &component_registry)?;

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
    config: &GenericConfiguration, dp_config: &DataPlaneConfiguration, env_provider: &ADPEnvironmentProvider,
    component_registry: &ComponentRegistry, dsd_stats_config: DogStatsDStatisticsConfiguration,
) -> Result<TopologyBlueprint, GenericError> {
    let mut blueprint = TopologyBlueprint::new("primary", component_registry);

    // If no data pipelines are enabled, then there's nothing for us to do.
    if !dp_config.data_pipelines_enabled() {
        return Err(generic_error!("No data pipelines are enabled. Exiting."));
    }

    // Create our baseline pipelines if necessary.
    //
    // We check if the "metrics" or "logs" pipeline is required, which represent the basic components necessary to
    // forward metrics and logs to Datadog. This means that if either are enabled, we always create the forwarder, but
    // we additionally create metrics- and logs-specific components connected to that forwarder depending on which of
    // the baseline pipelines are required.
    //
    // Notably, we _don't_ need either of these if all we're doing is running the OTLP pipeline in proxy mode, which
    // is the only reason we're differentiating here.
    if dp_config.metrics_pipeline_required() || dp_config.logs_pipeline_required() {
        let dd_forwarder_config =
            DatadogConfiguration::from_configuration(config).error_context("Failed to configure Datadog forwarder.")?;
        blueprint.add_forwarder("dd_out", dd_forwarder_config)?;
    }

    if dp_config.metrics_pipeline_required() {
        add_baseline_metrics_pipeline_to_blueprint(&mut blueprint, config, dp_config, env_provider).await?;
    }

    if dp_config.logs_pipeline_required() {
        add_baseline_logs_pipeline_to_blueprint(&mut blueprint, config).await?;
    }

    // Now we move on to our actual data pipelines.
    if dp_config.dogstatsd().enabled() {
        add_dsd_pipeline_to_blueprint(&mut blueprint, config, env_provider, dsd_stats_config).await?;
    }

    if dp_config.otlp().enabled() {
        add_otlp_pipeline_to_blueprint(&mut blueprint, config, dp_config, env_provider)?;
    }

    #[cfg(feature = "python-checks")]
    add_checks_to_blueprint(&mut blueprint, config, env_provider)?;

    Ok(blueprint)
}

async fn add_baseline_metrics_pipeline_to_blueprint(
    blueprint: &mut TopologyBlueprint, config: &GenericConfiguration, dp_config: &DataPlaneConfiguration,
    env_provider: &ADPEnvironmentProvider,
) -> Result<(), GenericError> {
    // Create the back half of the metrics processing pipeline.
    let metrics_agg_config =
        AggregateConfiguration::from_configuration(config).error_context("Failed to configure aggregate transform.")?;
    let host_enrichment_config = HostEnrichmentConfiguration::from_environment_provider(env_provider.clone());
    let mut metrics_enrich_config =
        ChainedConfiguration::default().with_transform_builder("host_enrichment", host_enrichment_config);

    if !dp_config.standalone_mode() {
        let host_tags_config = HostTagsConfiguration::from_configuration(config).await?;
        metrics_enrich_config = metrics_enrich_config.with_transform_builder("host_tags", host_tags_config);
    }

    let dd_metrics_config = DatadogMetricsConfiguration::from_configuration(config)
        .error_context("Failed to configure Datadog Metrics encoder.")?;

    blueprint
        // Components.
        .add_transform("metrics_agg", metrics_agg_config)?
        .add_transform("metrics_enrich", metrics_enrich_config)?
        .add_encoder("dd_metrics_encode", dd_metrics_config)?
        // Metrics.
        .connect_component("metrics_enrich", ["metrics_agg"])?
        .connect_component("dd_metrics_encode", ["metrics_enrich"])?
        // Forwarding.
        .connect_component("dd_out", ["dd_metrics_encode"])?;

    Ok(())
}

async fn add_baseline_logs_pipeline_to_blueprint(
    blueprint: &mut TopologyBlueprint, config: &GenericConfiguration,
) -> Result<(), GenericError> {
    // Create the back half of the logs processing pipeline.
    let dd_logs_config = DatadogLogsConfiguration::from_configuration(config)
        .map(BufferedIncrementalConfiguration::from_encoder_builder)
        .error_context("Failed to configure Datadog Logs encoder.")?;

    blueprint
        // Components.
        .add_encoder("dd_logs_encode", dd_logs_config)?
        // Logs.
        .connect_component("dd_out", ["dd_logs_encoder"])?;

    Ok(())
}

async fn add_dsd_pipeline_to_blueprint(
    blueprint: &mut TopologyBlueprint, config: &GenericConfiguration, env_provider: &ADPEnvironmentProvider,
    dsd_stats_config: DogStatsDStatisticsConfiguration,
) -> Result<(), GenericError> {
    // We're creating the "front half" of the DogStatsD pipeline, which deals solely with accepting DogStatsD payloads,
    // and enriching/processing them in DSD-specific ways, relevant to how the Datadog Agent is expected to behave.
    //
    //                                         ┌─────────────────────┐
    //                                         │      DogStatsD      │
    //                          ┌──────────────│       (source)      │──────────────┐
    //                          │              └─────────────────────┘              │
    //                          │                         │                         │
    //                          │ metrics                 │ service checks          │ events
    //                          ▼                         ▼                         ▼
    //               ┌─────────────────────┐   ┌─────────────────────┐   ┌─────────────────────┐
    //               │  DSD Prefix/Filter  │   │ DSD Service Checks  │   │     DSD Events      │
    //               │     (transform)     │   │      (encoder)      │   │      (encoder)      │
    //               └──────────┬──────────┘   └─────────┬───────────┘   └─────────────────────┘
    //                          │                        │                          │
    //                          ▼                        │                          └──────┐
    //               ┌─────────────────────┐             └──────────────────────────────┐  │
    //               │     DSD Enrich      │                                            │  │
    //               │ (chained transform) │       ┌ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ┐    │  │
    //               │┌───────────────────┐│       │        Metrics Pipeline       │    │  │
    //               ││    DSD Mapper     ││──────▶│  (aggregate, enrich, encode)  │    │  │
    //               │└───────────────────┘│       └ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ┘    │  │
    //               └─────────────────────┘                        │                   │  │
    //                                                              │                   │  │
    //                                                              ▼                   ▼  ▼
    //               ┌ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ┐
    //               │                               Forwarder                                 │
    //               │                           (Datadog Platform)                            │
    //               └ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ┘

    let dsd_config = DogStatsDConfiguration::from_configuration(config)
        .error_context("Failed to configure DogStatsD source.")?
        .with_workload_provider(env_provider.workload().clone());
    let dsd_prefix_filter_configuration = DogstatsDPrefixFilterConfiguration::from_configuration(config)?;
    let dsd_mapper_config = DogstatsDMapperConfiguration::from_configuration(config)?;
    let dsd_enrich_config =
        ChainedConfiguration::default().with_transform_builder("dogstatsd_mapper", dsd_mapper_config);
    let dd_events_config = DatadogEventsConfiguration::from_configuration(config)
        .map(BufferedIncrementalConfiguration::from_encoder_builder)
        .error_context("Failed to configure Datadog Events encoder.")?;
    let dd_service_checks_config = DatadogServiceChecksConfiguration::from_configuration(config)
        .map(BufferedIncrementalConfiguration::from_encoder_builder)
        .error_context("Failed to configure Datadog Service Checks encoder.")?;

    blueprint
        // Components.
        .add_source("dsd_in", dsd_config)?
        .add_transform("dsd_prefix_filter", dsd_prefix_filter_configuration)?
        .add_transform("dsd_enrich", dsd_enrich_config)?
        .add_encoder("dd_events_encode", dd_events_config)?
        .add_encoder("dd_service_checks_encode", dd_service_checks_config)?
        .add_destination("dsd_stats_out", dsd_stats_config)?
        // Metrics.
        .connect_component("dsd_prefix_filter", ["dsd_in.metrics"])?
        .connect_component("dsd_enrich", ["dsd_prefix_filter"])?
        .connect_component("metrics_agg", ["dsd_enrich"])?
        .connect_component("dd_service_checks_encode", ["dsd_in.service_checks"])?
        .connect_component("dd_events_encode", ["dsd_in.events"])?
        .connect_component("dd_out", ["dd_service_checks_encode", "dd_events_encode"])?
        // DogStatsD Stats.
        .connect_component("dsd_stats_out", ["dsd_in.metrics"])?;

    Ok(())
}

fn add_otlp_pipeline_to_blueprint(
    blueprint: &mut TopologyBlueprint, config: &GenericConfiguration, dp_config: &DataPlaneConfiguration,
    env_provider: &ADPEnvironmentProvider,
) -> Result<(), GenericError> {
    if dp_config.otlp().proxy().enabled() {
        // In proxy mode, we forward OTLP payloads from either the Core Agent or Trace agent, depending on the signal type.
        //
        // This means that ADP is taking over the typical OTLP Ingest configuration (otlp_config.receiver) while we instruct
        // the Core Agent to listen on a _different_ port for receiving OTLP payloads -- the Trace Agent continues to listen
        // as it normally would -- and we forward the payloads.
        //
        // This is why we use a specific override destination for the OTLP payloads that we forward to the Core Agent.
        let core_agent_otlp_endpoint = dp_config.otlp().proxy().core_agent_otlp_endpoint().to_string();

        let api_key = config.get_typed::<String>("api_key").expect("API key is required");

        let otlp_relay_config = OtlpRelayConfiguration::from_configuration(config)?;
        let local_agent_otlp_forwarder_config =
            DatadogConfiguration::from_configuration(config)?.with_endpoint_override(core_agent_otlp_endpoint, api_key);
        let local_trace_agent_otlp_forwarder_config = TraceAgentForwarderConfiguration::from_configuration(config)?;

        blueprint
            // Components.
            .add_relay("otlp_relay_in", otlp_relay_config)?
            .add_forwarder("local_agent_otlp_out", local_agent_otlp_forwarder_config)?
            .add_forwarder("local_trace_agent_otlp_out", local_trace_agent_otlp_forwarder_config)?
            // Metrics and logs.
            .connect_component("local_agent_otlp_out", ["otlp_relay_in"])?
            .connect_component("local_trace_agent_otlp_out", ["otlp_relay_in"])?;
    } else {
        let otlp_config =
            OtlpConfiguration::from_configuration(config)?.with_workload_provider(env_provider.workload().clone());
        let dd_logs_config = DatadogLogsConfiguration::from_configuration(config)
            .map(BufferedIncrementalConfiguration::from_encoder_builder)
            .error_context("Failed to configure Datadog Logs encoder.")?;

        blueprint
            // Components.
            .add_source("otlp_in", otlp_config)?
            .add_encoder("dd_logs_encode", dd_logs_config)?
            .connect_component("dsd_prefix_filter", ["otlp_in.metrics"])?
            // Metrics and logs.
            //
            // We send OTLP metrics directly to the enrichment stag of the metrics pipeline, skipping aggregation,
            // to avoid transforming counters into rates.
            .connect_component("metrics_enrich", ["otlp_in.metrics"])?
            .connect_component("dd_out", ["dd_logs_encode"])?;
    }
    Ok(())
}

#[cfg(feature = "python-checks")]
fn add_checks_to_blueprint(
    blueprint: &mut TopologyBlueprint, config: &GenericConfiguration, env_provider: &ADPEnvironmentProvider,
) -> Result<(), GenericError> {
    let checks_config = ChecksConfiguration::from_configuration(config)
        .error_context("Failed to configure checks source.")?
        .with_environment_provider(env_provider.clone());

    blueprint
        .add_source("checks_in", checks_config)?
        .connect_component("dsd_agg", ["checks_in.metrics"])?
        .connect_component("dd_service_checks_encode", ["checks_in.service_checks"])?
        .connect_component("dd_events_encode", ["checks_in.events"])?;

    Ok(())
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
