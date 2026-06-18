use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

use agent_data_plane_config::{ComponentConfiguration, DatadogRuntimeAuthority, SalukiConfiguration};
use agent_data_plane_config_system::{DynamicConfigHandles, LoadedConfigurationSystem};
use argh::FromArgs;
use datadog_agent_commons::platform::PlatformSettings;
use resource_accounting::{ComponentBounds, ComponentRegistry};
use saluki_app::{
    accounting::{initialize_memory_bounds, MemoryBoundsConfiguration},
    bootstrap::BootstrapGuard,
    metrics::emit_startup_metrics,
};
use saluki_component_config::{ChecksIpcConfig, ListenAddress as NativeListenAddress};
use saluki_components::{
    config::{AutoscalingFailoverConfiguration, ClusterAgentConfiguration, MrfConfiguration},
    decoders::otlp::OtlpDecoderConfiguration,
    destinations::{DogStatsDDebugLogConfiguration, DogStatsDStatisticsConfiguration},
    encoders::{
        BufferedIncrementalConfiguration, DatadogApmStatsEncoderConfiguration, DatadogEventsConfiguration,
        DatadogLogsConfiguration, DatadogMetricsConfiguration, DatadogServiceChecksConfiguration,
        DatadogTraceConfiguration,
    },
    forwarders::{ClusterAgentForwarderConfiguration, DatadogForwarderConfiguration, OtlpForwarderConfiguration},
    relays::otlp::OtlpRelayConfiguration,
    sources::{ChecksIPCConfiguration, DogStatsDConfiguration, OtlpConfiguration},
    transforms::{
        AggregateConfiguration, ApmStatsTransformConfiguration, AutoscalingFailoverGatewayConfiguration,
        ChainedConfiguration, DogStatsDMapperConfiguration, HostEnrichmentConfiguration,
        MrfMetricsGatewayConfiguration, TraceObfuscationConfiguration, TraceSamplerConfiguration,
    },
};
use saluki_config_tools::GenericConfiguration;
use saluki_core::health::HealthRegistry;
use saluki_core::runtime::{RestartMode, RestartStrategy, Supervisor};
use saluki_core::topology::TopologyBlueprint;
use saluki_env::EnvironmentProvider as _;
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use saluki_io::net::ListenAddress;
use tracing::{info, warn};

use crate::{
    components::{
        apm_onboarding::ApmOnboardingConfiguration,
        dogstatsd_post_aggregate_filter::DogStatsDPostAggregateFilterConfiguration,
        dogstatsd_prefix_filter::DogStatsDPrefixFilterConfiguration, ottl_filter_processor::OttlFilterConfiguration,
        ottl_transform_processor::OttlTransformConfiguration, tag_filterlist::TagFilterlistConfiguration,
    },
    config::DataPlaneConfiguration,
    internal::env::ADPEnvironmentProvider,
    internal::{create_internal_supervisor, DogStatsDControlSurface, TopologyControlSurfaces},
};

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
    started: Instant, loaded_config: LoadedConfigurationSystem, bootstrap_guard: &mut BootstrapGuard,
    bootstrap_supervisor: Supervisor,
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

    let started_config = loaded_config.start_runtime(DatadogRuntimeAuthority::Stream).await?;
    let saluki = started_config.saluki();
    let handles = started_config.dynamic_handles();
    let attachments = started_config.attachments();
    let dp_config = DataPlaneConfiguration::from_control(&saluki.control)
        .error_context("Failed to load data plane control configuration.")?;
    let _ = (
        dp_config.remote_agent_enabled(),
        dp_config.use_new_config_stream_endpoint(),
    );

    if !dp_config.standalone_mode() && !dp_config.enabled() {
        info!("Agent Data Plane is not enabled. Exiting.");
        return Ok(());
    }

    let component_registry = ComponentRegistry::default();
    let health_registry = HealthRegistry::new();
    let (env_provider, maybe_env_supervisor) = ADPEnvironmentProvider::from_data_plane_config(
        &dp_config,
        &saluki.components.workload.source,
        &attachments,
        &component_registry,
        &health_registry,
    )
    .await?;

    let (mut blueprint, control_surfaces) =
        create_topology(&saluki, &handles, started_config.datadog_config(), &dp_config, &env_provider, &component_registry).await?;

    let ra_bootstrap = match attachments.datadog_agent.clone() {
        Some(connection) => {
            Some(crate::internal::remote_agent::RemoteAgentBootstrap::from_connection(connection).await)
        }
        None => None,
    };

    let mut internal_supervisor = create_internal_supervisor(
        &dp_config,
        &component_registry,
        health_registry.clone(),
        control_surfaces,
        ra_bootstrap,
        started_config.config_views(),
        handles.log_level.clone(),
        bootstrap_guard.logging().controller(),
    )
    .await
    .error_context("Failed to create internal supervisor.")?;

    let bounds_config: MemoryBoundsConfiguration = serde_json::from_value(serde_json::json!({}))?;
    let memory_limiter = initialize_memory_bounds(bounds_config, component_registry.root())?;

    if let Ok(val) = std::env::var("DD_ADP_WRITE_SIZING_GUIDE") {
        if val != "false" {
            if let Err(error) = write_sizing_guide(component_registry.as_bounds()) {
                warn!("Failed to write sizing guide: {}", error);
            } else {
                return Ok(());
            }
        }
    }

    blueprint
        .with_health_registry(health_registry.clone())
        .with_memory_limiter(memory_limiter)
        .with_environment_readiness(env_provider.wait_for_ready());

    let topology_ready = blueprint.topology_ready();

    let root_restart_strategy = RestartStrategy::new(RestartMode::OneForOne, 0, Duration::from_secs(5));
    let mut root_supervisor = Supervisor::new("adp-root")?.with_restart_strategy(root_restart_strategy);

    root_supervisor.add_worker(bootstrap_supervisor);
    if let Some(env_supervisor) = maybe_env_supervisor {
        internal_supervisor.add_worker(env_supervisor);
    }
    root_supervisor.add_worker(internal_supervisor);
    root_supervisor.add_worker(blueprint);

    tokio::spawn(async move {
        if topology_ready.wait().await {
            info!(
                topology_ready_ms = started.elapsed().as_millis(),
                "Topology healthy. Waiting for interrupt..."
            );
            emit_startup_metrics();
        }
    });

    info!("Agent Data Plane running.");
    match root_supervisor.run_with_shutdown(wait_for_sigint()).await {
        Ok(()) => {
            info!("Agent Data Plane shut down successfully.");
            Ok(())
        }
        Err(e) => Err(e.into()),
    }
}

async fn wait_for_sigint() {
    let _ = tokio::signal::ctrl_c().await;
    info!("Received SIGINT, shutting down...");
}

async fn create_topology(
    saluki: &SalukiConfiguration, handles: &DynamicConfigHandles, config: &GenericConfiguration,
    dp_config: &DataPlaneConfiguration, env_provider: &ADPEnvironmentProvider,
    component_registry: &ComponentRegistry,
) -> Result<(TopologyBlueprint, TopologyControlSurfaces), GenericError> {
    let mut blueprint = TopologyBlueprint::new("primary", component_registry);
    blueprint.with_shutdown_timeout(dp_config.stop_timeout());

    let mut control_surfaces = TopologyControlSurfaces::default();

    if !dp_config.data_pipelines_enabled() {
        return Err(generic_error!("No data pipelines are enabled. Exiting."));
    }

    if dp_config.metrics_pipeline_required()
        || dp_config.logs_pipeline_required()
        || dp_config.events_pipeline_required()
        || dp_config.service_checks_pipeline_required()
        || dp_config.traces_pipeline_required()
    {
        blueprint.add_forwarder(
            "dd_out",
            DatadogForwarderConfiguration::from_native(handles.forwarder.clone()),
        )?;
    }

    if dp_config.metrics_pipeline_required() {
        add_baseline_metrics_pipeline_to_blueprint(
            &mut blueprint,
            &saluki.components,
            handles,
            config,
            dp_config,
            env_provider,
        )
        .await?;
    }
    if dp_config.logs_pipeline_required() {
        add_baseline_logs_pipeline_to_blueprint(&mut blueprint, &saluki.components).await?;
    }
    if dp_config.events_pipeline_required() {
        add_baseline_events_pipeline_to_blueprint(&mut blueprint, &saluki.components).await?;
    }
    if dp_config.service_checks_pipeline_required() {
        add_baseline_service_checks_pipeline_to_blueprint(&mut blueprint, &saluki.components).await?;
    }
    if dp_config.traces_pipeline_required() {
        add_baseline_traces_pipeline_to_blueprint(&mut blueprint, &saluki.components, env_provider).await?;
    }
    if dp_config.checks().enabled() {
        add_checks_pipeline_to_blueprint(&mut blueprint, &saluki.components.checks.ipc).await?;
    }
    if dp_config.dogstatsd().enabled() {
        let dsd_control_surface =
            add_dsd_pipeline_to_blueprint(&mut blueprint, &saluki.components, handles, env_provider).await?;
        control_surfaces.attach_dogstatsd(dsd_control_surface);
    }
    if dp_config.otlp().enabled() {
        add_otlp_pipeline_to_blueprint(&mut blueprint, &saluki.components, dp_config, env_provider)?;
    }

    Ok((blueprint, control_surfaces))
}

async fn add_checks_pipeline_to_blueprint(
    blueprint: &mut TopologyBlueprint, config: &ChecksIpcConfig,
) -> Result<(), GenericError> {
    let endpoint = if config.endpoint.is_empty() {
        ListenAddress::any_tcp(5105)
    } else {
        parse_io_listen_address(&config.endpoint, "tcp")?
    };
    let checks_config = ChecksIPCConfiguration::from_native(endpoint);

    blueprint
        .add_source("checks_ipc_in", checks_config)?
        .connect_components("checks_ipc_in.metrics", "metrics_enrich")?
        .connect_components("checks_ipc_in.logs", "dd_logs_encode")?
        .connect_components("checks_ipc_in.events", "dd_events_encode")?
        .connect_components("checks_ipc_in.service_checks", "dd_service_checks_encode")?;

    Ok(())
}

async fn add_baseline_metrics_pipeline_to_blueprint(
    blueprint: &mut TopologyBlueprint, components: &ComponentConfiguration, handles: &DynamicConfigHandles,
    config: &GenericConfiguration, dp_config: &DataPlaneConfiguration, env_provider: &ADPEnvironmentProvider,
) -> Result<(), GenericError> {
    let host_enrichment_config = HostEnrichmentConfiguration::from_environment_provider(env_provider.clone());
    let metrics_enrich_config =
        ChainedConfiguration::default().with_transform_builder("host_enrichment", host_enrichment_config);

    let _ = dp_config;

    let metrics_encoder_config = DatadogMetricsConfiguration::from_native(components.metrics.datadog_encoder.clone());

    blueprint
        .add_transform("metrics_enrich", metrics_enrich_config)?
        .add_encoder("dd_metrics_encode", metrics_encoder_config)?
        .connect_components_in_order(["metrics_enrich", "dd_metrics_encode", "dd_out"])?;

    add_mrf_metrics_pipeline_to_blueprint(blueprint, components, handles)?;
    add_autoscaling_failover_metrics_pipeline_to_blueprint(blueprint, config)?;

    Ok(())
}

fn add_mrf_metrics_pipeline_to_blueprint(
    blueprint: &mut TopologyBlueprint, components: &ComponentConfiguration, handles: &DynamicConfigHandles,
) -> Result<(), GenericError> {
    let mrf_native_config = components.metrics.multi_region_failover.clone();
    let mrf_config = MrfConfiguration::from_native(mrf_native_config.clone());

    let Some((mrf_dd_url, mrf_api_key)) = mrf_config.metrics_endpoint_override() else {
        if mrf_config.is_enabled() {
            warn!("Multi-Region Failover is enabled, but static endpoint settings are incomplete.");
        }
        return Ok(());
    };

    let mrf_gateway_config =
        MrfMetricsGatewayConfiguration::new(mrf_config.clone(), handles.multi_region_failover.clone());
    let mrf_forwarder_config = DatadogForwarderConfiguration::from_native(handles.forwarder.clone())
        .with_endpoint_override_and_api_key_refresh_config_path(
            mrf_dd_url,
            mrf_api_key,
            "multi_region_failover.api_key",
        );

    let metrics_encoder_config = DatadogMetricsConfiguration::from_native(components.metrics.datadog_encoder.clone());

    blueprint
        .add_transform("mrf_metrics_gateway", mrf_gateway_config)?
        .add_encoder("mrf_metrics_encode", metrics_encoder_config)?
        .add_forwarder("mrf_dd_out", mrf_forwarder_config)?
        .connect_components_in_order([
            "metrics_enrich",
            "mrf_metrics_gateway",
            "mrf_metrics_encode",
            "mrf_dd_out",
        ])?;

    Ok(())
}

fn add_autoscaling_failover_metrics_pipeline_to_blueprint(
    blueprint: &mut TopologyBlueprint, config: &GenericConfiguration,
) -> Result<(), GenericError> {
    let af_config = AutoscalingFailoverConfiguration::from_configuration(config)
        .error_context("Failed to configure autoscaling failover metrics pipeline.")?;
    let ca_config = ClusterAgentConfiguration::from_configuration(config)
        .error_context("Failed to configure Cluster Agent metrics forwarding.")?;

    let Some((ca_url, ca_token)) = ca_config.endpoint_and_token() else {
        if af_config.is_branch_requested() {
            warn!(
                "autoscaling.failover is enabled, but cluster_agent.enabled, cluster_agent.auth_token, and a resolvable \
                 Cluster Agent endpoint are required. Set cluster_agent.url or provide Kubernetes service env vars for \
                 cluster_agent.kubernetes_service_name. The autoscaling failover metrics branch will not be wired, and \
                 primary forwarding will continue."
            );
        }

        return Ok(());
    };

    if !af_config.is_branch_requested() {
        return Ok(());
    }

    let af_gateway_config = AutoscalingFailoverGatewayConfiguration::new(af_config);
    let af_metrics_config = DatadogMetricsConfiguration::from_configuration(config)
        .error_context("Failed to configure autoscaling failover metrics encoder.")?;
    let cluster_agent_forwarder_config =
        ClusterAgentForwarderConfiguration::from_configuration(config, ca_url, ca_token)
            .error_context("Failed to configure Cluster Agent forwarder.")?;

    blueprint
        .add_transform("af_metrics_gateway", af_gateway_config)?
        .add_encoder("af_metrics_encode", af_metrics_config)?
        .add_forwarder("cluster_agent_out", cluster_agent_forwarder_config)?
        .connect_components_in_order([
            "metrics_enrich",
            "af_metrics_gateway",
            "af_metrics_encode",
            "cluster_agent_out",
        ])?;

    Ok(())
}

async fn add_baseline_logs_pipeline_to_blueprint(
    blueprint: &mut TopologyBlueprint, components: &ComponentConfiguration,
) -> Result<(), GenericError> {
    let dd_logs_config = BufferedIncrementalConfiguration::from_encoder_builder(DatadogLogsConfiguration::from_native(
        components.logs.datadog_encoder.clone(),
    ));
    blueprint
        .add_encoder("dd_logs_encode", dd_logs_config)?
        .connect_components("dd_logs_encode", "dd_out")?;
    Ok(())
}

async fn add_baseline_events_pipeline_to_blueprint(
    blueprint: &mut TopologyBlueprint, components: &ComponentConfiguration,
) -> Result<(), GenericError> {
    let dd_events_config = BufferedIncrementalConfiguration::from_encoder_builder(
        DatadogEventsConfiguration::from_native(components.events.datadog_encoder.clone()),
    );
    blueprint
        .add_encoder("dd_events_encode", dd_events_config)?
        .connect_components("dd_events_encode", "dd_out")?;
    Ok(())
}

async fn add_baseline_service_checks_pipeline_to_blueprint(
    blueprint: &mut TopologyBlueprint, components: &ComponentConfiguration,
) -> Result<(), GenericError> {
    let dd_service_checks_config = BufferedIncrementalConfiguration::from_encoder_builder(
        DatadogServiceChecksConfiguration::from_native(components.service_checks.datadog_encoder.clone()),
    );
    blueprint
        .add_encoder("dd_service_checks_encode", dd_service_checks_config)?
        .connect_components("dd_service_checks_encode", "dd_out")?;
    Ok(())
}

async fn add_baseline_traces_pipeline_to_blueprint(
    blueprint: &mut TopologyBlueprint, components: &ComponentConfiguration, env_provider: &ADPEnvironmentProvider,
) -> Result<(), GenericError> {
    let dd_traces_config = DatadogTraceConfiguration::from_native(components.traces.encoder.clone())
        .with_environment_provider(env_provider.clone())
        .await?;
    let trace_sampler_config = TraceSamplerConfiguration::from_native(components.traces.sampler.rate * 100.0);
    let dd_traces_enrich_config = ChainedConfiguration::default()
        .with_transform_builder("ottl_filter", OttlFilterConfiguration::from_native(Default::default()))
        .with_transform_builder(
            "ottl_transform",
            OttlTransformConfiguration::from_native(Default::default()),
        )
        .with_transform_builder("apm_onboarding", ApmOnboardingConfiguration)
        .with_transform_builder(
            "trace_obfuscation",
            TraceObfuscationConfiguration::from_native(components.traces.obfuscation.clone()),
        )
        .with_transform_builder("trace_sampler", trace_sampler_config);
    let apm_stats_transform_config = ApmStatsTransformConfiguration::from_native()
        .with_environment_provider(env_provider.clone())
        .await?;
    let dd_apm_stats_encoder =
        DatadogApmStatsEncoderConfiguration::from_native(components.metrics.apm_stats_encoder.clone())
            .with_environment_provider(env_provider.clone())
            .await?;

    blueprint
        .add_transform("traces_enrich", dd_traces_enrich_config)?
        .add_transform("dd_apm_stats", apm_stats_transform_config)?
        .add_encoder("dd_stats_encode", dd_apm_stats_encoder)?
        .add_encoder("dd_traces_encode", dd_traces_config)?
        .connect_components("traces_enrich", ["dd_apm_stats", "dd_traces_encode"])?
        .connect_components("dd_apm_stats", "dd_stats_encode")?
        .connect_components(["dd_traces_encode", "dd_stats_encode"], "dd_out")?;

    Ok(())
}

async fn add_dsd_pipeline_to_blueprint(
    blueprint: &mut TopologyBlueprint, components: &ComponentConfiguration, handles: &DynamicConfigHandles,
    env_provider: &ADPEnvironmentProvider,
) -> Result<DogStatsDControlSurface, GenericError> {
    let dsd_config = DogStatsDConfiguration::from_native(components.dogstatsd.source.clone())
        .with_run_path(None)
        .with_workload_provider(env_provider.workload().clone())
        .with_capture_entity_resolver(env_provider.workload().clone());
    let dsd_prefix_filter_configuration =
        DogStatsDPrefixFilterConfiguration::from_native(handles.dogstatsd_prefix_filter.clone());
    let dsd_mapper_config = DogStatsDMapperConfiguration::from_native(components.dogstatsd.mapper.clone());
    let dsd_enrich_config =
        ChainedConfiguration::default().with_transform_builder("dogstatsd_mapper", dsd_mapper_config);
    let dsd_tag_filterlist_config = TagFilterlistConfiguration::from_native(handles.dogstatsd_tag_filterlist.clone());
    let dsd_agg_config = AggregateConfiguration::from_native(components.dogstatsd.aggregate.clone());
    let dsd_post_agg_filter_config =
        DogStatsDPostAggregateFilterConfiguration::from_native(handles.dogstatsd_post_aggregate_filter.clone());
    let events_enrich_config = ChainedConfiguration::default().with_transform_builder(
        "host_enrichment",
        HostEnrichmentConfiguration::from_environment_provider(env_provider.clone()),
    );
    let service_checks_enrich_config = ChainedConfiguration::default().with_transform_builder(
        "host_enrichment",
        HostEnrichmentConfiguration::from_environment_provider(env_provider.clone()),
    );
    let dsd_debug_log_config = DogStatsDDebugLogConfiguration::from_native(
        handles.dogstatsd_debug_log.clone(),
        PlatformSettings::get_default_dogstatsd_log_file_path(),
    )?;
    let dsd_stats_config = DogStatsDStatisticsConfiguration::new();

    let stats_api_handler = dsd_stats_config.api_handler();
    let capture_api_handler = dsd_config.capture_api_handler();
    let replay_api_handler = dsd_config.replay_api_handler();

    blueprint
        .add_source("dsd_in", dsd_config)?
        .add_transform("dsd_prefix_filter", dsd_prefix_filter_configuration)?
        .add_transform("dsd_enrich", dsd_enrich_config)?
        .add_transform("dsd_tag_filterlist", dsd_tag_filterlist_config)?
        .add_transform("dsd_agg", dsd_agg_config)?
        .add_transform("dsd_post_agg_filter", dsd_post_agg_filter_config)?
        .add_transform("events_enrich", events_enrich_config)?
        .add_transform("service_checks_enrich", service_checks_enrich_config)?
        .add_destination("dsd_stats_out", dsd_stats_config)?
        .connect_components_in_order([
            "dsd_in.metrics",
            "dsd_enrich",
            "dsd_prefix_filter",
            "dsd_tag_filterlist",
            "dsd_agg",
            "dsd_post_agg_filter",
            "metrics_enrich",
        ])?
        .connect_components_in_order(["dsd_in.events", "events_enrich", "dd_events_encode"])?
        .connect_components_in_order([
            "dsd_in.service_checks",
            "service_checks_enrich",
            "dd_service_checks_encode",
        ])?
        .connect_components("dsd_in.metrics", "dsd_stats_out")?;

    if dsd_debug_log_config.enabled() {
        blueprint
            .add_destination("dsd_debug_log_out", dsd_debug_log_config)?
            .connect_components("dsd_in.metrics", "dsd_debug_log_out")?;
    }
    Ok(DogStatsDControlSurface {
        stats_api_handler,
        capture_api_handler,
        replay_api_handler,
    })
}

fn add_otlp_pipeline_to_blueprint(
    blueprint: &mut TopologyBlueprint, components: &ComponentConfiguration, dp_config: &DataPlaneConfiguration,
    env_provider: &ADPEnvironmentProvider,
) -> Result<(), GenericError> {
    if dp_config.otlp().proxy().enabled() {
        let core_agent_otlp_grpc_endpoint = dp_config.otlp().proxy().core_agent_otlp_grpc_endpoint().to_string();
        info!(proxy_grpc_endpoint = %core_agent_otlp_grpc_endpoint, "OTLP proxy mode enabled.");

        let local_agent_otlp_forwarder_config =
            OtlpForwarderConfiguration::from_native(core_agent_otlp_grpc_endpoint, 5003);

        blueprint
            .add_relay(
                "otlp_relay_in",
                OtlpRelayConfiguration::from_native(components.otlp.relay.clone()),
            )?
            .add_forwarder("local_agent_otlp_out", local_agent_otlp_forwarder_config)?;

        if dp_config.otlp().proxy().proxy_metrics() {
            blueprint.connect_components("otlp_relay_in.metrics", "local_agent_otlp_out")?;
        }
        if dp_config.otlp().proxy().proxy_logs() {
            blueprint.connect_components("otlp_relay_in.logs", "local_agent_otlp_out")?;
        }

        if dp_config.otlp().proxy().proxy_traces() {
            blueprint.connect_components("otlp_relay_in.traces", "local_agent_otlp_out")?;
        } else {
            blueprint
                .add_decoder(
                    "otlp_traces_decode",
                    OtlpDecoderConfiguration::from_native(components.otlp.decoder.clone()),
                )?
                .connect_components_in_order(["otlp_relay_in.traces", "otlp_traces_decode", "traces_enrich"])?;
        }
    } else {
        info!("OTLP proxy mode disabled. OTLP signals will be handled natively.");

        let otlp_config = OtlpConfiguration::from_native(components.otlp.source.clone())
            .with_workload_provider(env_provider.workload().clone());

        blueprint
            .add_source("otlp_in", otlp_config)?
            .connect_components("otlp_in.metrics", "metrics_enrich")?
            .connect_components("otlp_in.logs", "dd_logs_encode")?
            .connect_components("otlp_in.traces", "traces_enrich")?;
    }
    Ok(())
}

fn parse_io_listen_address(value: &str, default_scheme: &str) -> Result<ListenAddress, GenericError> {
    let raw = if value.contains("://") {
        value.to_string()
    } else {
        format!("{default_scheme}://{value}")
    };
    ListenAddress::try_from(raw.as_str()).map_err(|e| generic_error!("Invalid listen address `{}`: {}", value, e))
}

#[allow(dead_code)]
fn native_listen_to_io(address: &NativeListenAddress, default_port: u16) -> Result<ListenAddress, GenericError> {
    match address {
        NativeListenAddress::Disabled => Ok(ListenAddress::any_tcp(default_port)),
        NativeListenAddress::Tcp(value) => parse_io_listen_address(value, "tcp"),
        NativeListenAddress::Udp(value) => parse_io_listen_address(value, "udp"),
        NativeListenAddress::Unix(value) => parse_io_listen_address(value, "unix"),
    }
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
