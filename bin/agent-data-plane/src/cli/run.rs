use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Instant,
};

use agent_data_plane_config::{
    defaults::DEFAULT_ENCODER_FLUSH_TIMEOUT,
    domains::multi_region_failover,
    shared::{Secrets, SharedConfiguration},
    SalukiConfiguration,
};
use agent_data_plane_config_system::{ConfigurationSystem, LoadedConfiguration};
use argh::FromArgs;
use bytesize::ByteSize;
use datadog_agent_apm::{sampling::TraceSamplingSubscription, semantics::SemanticRegistryProvider};
use datadog_agent_commons::{ipc::config::RemoteAgentClientConfiguration, platform::PlatformSettings};
use datadog_agent_runtime::{
    env::RemoteAgentEnvironmentProvider,
    facade::{Initialized, RemoteAgent},
};
use saluki_app::bootstrap::BootstrapGuard;
use saluki_components::{
    config::{
        AgentEnv, AutoscalingFailoverConfiguration, ClusterAgentConfiguration, MetricsEndpointRoutingConfiguration,
    },
    decoders::otlp::OtlpDecoderConfiguration,
    destinations::{
        DogStatsDClientTelemetryConfiguration, DogStatsDDebugLogConfiguration, DogStatsDStatisticsConfiguration,
    },
    encoders::{
        BufferedIncrementalConfiguration, DatadogApmStatsEncoderConfiguration, DatadogEventsConfiguration,
        DatadogLogsConfiguration, DatadogServiceChecksConfiguration,
    },
    forwarders::{
        ClusterAgentForwarderConfiguration, DatadogForwarderConfiguration, OtlpForwarderConfiguration,
        RoutingTargetCatalog,
    },
    relays::otlp::OtlpRelayConfiguration,
    sources::{
        ChecksIPCConfiguration, DogStatsDCaptureAPIHandler, DogStatsDCaptureControl, DogStatsDConfiguration,
        DogStatsDReplayAPIHandler, DogStatsDReplayControl, EnablePayloadsConfiguration, ListenerRequirement,
        OriginEnrichmentConfiguration, OtlpConfiguration,
    },
    transforms::{
        aggregate_context_snapshot_channel, AggregateConfiguration, AutoscalingFailoverGatewayConfiguration,
        ChainedConfiguration, DogStatsDMapperConfiguration, DogStatsDMapperProfile, DogStatsDMetricMapping,
        HistogramConfiguration, HostEnrichmentConfiguration, ReplaceRule, TraceObfuscationConfiguration,
        TraceTagReplacerConfiguration,
    },
};
use saluki_core::{runtime::Supervisor, topology::TopologyBlueprint};
use saluki_env::{features, EnvironmentProvider as _, HostProvider as _};
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use saluki_io::net::ListenAddress;
use stringtheory::MetaString;
use tracing::{debug, info, warn};

use crate::{
    component_inputs::{
        apm_relay_configuration, apm_stats_configuration, datadog_metrics_configuration, forwarder_configuration,
        metric_mirroring, metric_name_filter, mrf_configuration, obfuscation_configuration, origin_tag_cardinality,
        otlp_context_settings, otlp_metrics_settings, otlp_receiver, otlp_traces_settings, ottl_error_mode,
        resolve_static_tags, tag_filter_entries, tag_value_allowlist, trace_configuration, trace_sampler_configuration,
    },
    components::{
        apm_onboarding::ApmOnboardingConfiguration, dogstatsd_no_agg_split::DogStatsDNoAggSplitConfiguration,
        dogstatsd_post_aggregate_filter::DogStatsDPostAggregateFilterConfiguration,
        dogstatsd_prefix_filter::DogStatsDPrefixFilterConfiguration, host_tags::HostTagsConfiguration,
        liveness::LivenessConfiguration, metric_filter::MetricFilterConfiguration,
        ottl_filter_processor::OttlFilterConfiguration, ottl_transform_processor::OttlTransformConfiguration,
        tag_filterlist::TagFilterlistConfiguration,
    },
    config::DataPlaneConfiguration,
    dogstatsd_contexts::DogStatsDContextDumpAPIHandler,
    internal::{
        create_control_plane_supervisor, logging::get_logging_translator, AdpLocalConfig, AgentDataPlane,
        DogStatsDControlSurface, TopologyControlSurfaces,
    },
    remote_config::{self, RemoteConfigSubscriptions},
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
    started: Instant, local_config: LoadedConfiguration, bootstrap_guard: &mut BootstrapGuard,
    bootstrap_supervisor: Supervisor,
) -> Result<(), GenericError> {
    // Resolve the authoritative configuration: connected mode registers as a remote agent and waits for the Core
    // Agent's configuration, while standalone mode treats the local sources as authoritative.
    let agent = RemoteAgent::<AgentDataPlane>::new(AdpLocalConfig::new(local_config), get_logging_translator().clone())
        .with_start_time(started)
        .start(bootstrap_guard, bootstrap_supervisor)
        .await?;

    let (enabled, active_pipelines) = {
        let config = agent.config().system().config();
        let dp = DataPlaneConfiguration::from_configuration(&config);
        (dp.enabled(), dp.active_pipelines())
    };

    if !agent.is_standalone() && !enabled {
        info!("Agent Data Plane is not enabled. Exiting.");
        return Ok(());
    }

    agent
        .config()
        .system()
        .check_compatibility(&active_pipelines)
        .error_context("Incompatible configuration detected.")?;

    // Set up all of the building blocks for building our topologies and launching internal processes.
    let mut agent = agent.initialize().await?;

    let rc_products = remote_config::enabled_products(&agent.config().system().config());
    let rc_subscriptions = RemoteConfigSubscriptions::subscribe(rc_products, &mut agent)
        .await
        .error_context("Failed to set up Remote Configuration.")?;

    // Create the blueprint for our primary topology.
    let (blueprint, control_surfaces) = create_topology(&agent, rc_subscriptions).await?;

    // Create the control plane, and then run: the remote agent builds the rest of the supervisors, and runs memory
    // bounds validation to ensure that we can launch the topology with our configured memory limit, if any.
    let control_plane =
        create_control_plane_supervisor(&agent, control_surfaces, bootstrap_guard.logging().controller())
            .await
            .error_context("Failed to create internal supervisor.")?;
    agent.run(blueprint, control_plane).await
}

async fn create_topology(
    agent: &RemoteAgent<AgentDataPlane, Initialized>, rc_subscriptions: RemoteConfigSubscriptions,
) -> Result<(TopologyBlueprint, TopologyControlSurfaces), GenericError> {
    let config_system = agent.config().system();
    let config = config_system.config();
    let dp = DataPlaneConfiguration::from_configuration(&config);
    let mut blueprint = TopologyBlueprint::new("primary", agent.component_registry());
    blueprint.with_shutdown_timeout(dp.stop_timeout());
    let shared = config.shared.clone();

    let mut control_surfaces = TopologyControlSurfaces::default();
    let RemoteConfigSubscriptions {
        trace_sampling,
        semantic_registry,
    } = rc_subscriptions;

    // If no data pipelines are enabled, then there's nothing for us to do.
    if !dp.data_pipelines_enabled() {
        return Err(generic_error!("No data pipelines are enabled. Exiting."));
    }

    let remote_agent_client_config = agent.client_configuration();
    let env_provider = agent.env_provider();

    // Create our baseline pipelines if necessary.
    //
    // We check if the "metrics" or "logs" pipeline is required, which represent the basic components necessary to
    // forward metrics and logs to Datadog. This means that if either are enabled, we always create the forwarder, but
    // we additionally create metrics- and logs-specific components connected to that forwarder depending on which of
    // the baseline pipelines are required.
    //
    // Notably, we _don't_ need either of these if all we're doing is running the OTLP pipeline in proxy mode, which
    // is the only reason we're differentiating here. Connected mode is the exception: liveness deliberately
    // provisions metric and service-check output paths even for proxy-only topologies. The APM relay needs the
    // forwarder alone, as it has nothing to feed the traces pipeline.
    let mut routing_targets = None;
    if dp.metrics_pipeline_required()
        || dp.logs_pipeline_required()
        || dp.events_pipeline_required()
        || dp.service_checks_pipeline_required()
        || dp.traces_pipeline_required()
        || dp.apm_enabled()
    {
        // The forwarder and every metrics encoder that addresses its payloads to specific endpoints share one catalog,
        // so that both sides agree on what each routing target identifier means.
        let catalog = Arc::new(RoutingTargetCatalog::new(
            &shared.endpoints.primary_endpoint(),
            &shared.endpoints.additional_endpoints,
        )?);
        let dd_forwarder_config = DatadogForwarderConfiguration::new(
            forwarder_configuration(&shared),
            config_system.live(|config| &config.shared.endpoints.api_key),
            config_system.live(|config| &config.shared.endpoints.additional_endpoints),
            config_system.live(|config| &config.shared.secrets).map(Secrets::in_use),
        )
        .with_routing_targets(Arc::clone(&catalog));
        blueprint.add_forwarder("dd_out", dd_forwarder_config)?;
        routing_targets = Some(catalog);
    }

    if dp.metrics_pipeline_required() {
        let routing_targets =
            routing_targets.ok_or_else(|| generic_error!("The metrics pipeline requires the Datadog forwarder."))?;
        add_baseline_metrics_pipeline_to_blueprint(
            &mut blueprint,
            config_system,
            remote_agent_client_config,
            &shared,
            &routing_targets,
            env_provider,
        )
        .await?;
    }

    if dp.logs_pipeline_required() {
        add_baseline_logs_pipeline_to_blueprint(&mut blueprint, &shared)?;
    }

    if dp.events_pipeline_required() {
        add_baseline_events_pipeline_to_blueprint(&mut blueprint, &shared)?;
    }

    if dp.service_checks_pipeline_required() {
        add_baseline_service_checks_pipeline_to_blueprint(&mut blueprint, &shared)?;
    }

    if dp.traces_pipeline_required() {
        add_baseline_traces_pipeline_to_blueprint(
            &mut blueprint,
            config_system,
            env_provider,
            trace_sampling,
            semantic_registry.clone(),
        )
        .await?;
    }

    // Connected topologies emit liveness through the metric and service-check baselines created above. Standalone
    // OTLP proxy mode must not construct liveness or output paths that it does not otherwise need.
    if !dp.standalone_mode() {
        let add_container_tags = config_system.config().shared.basic_telemetry.add_container_tags;
        add_liveness_source_to_blueprint(&mut blueprint, env_provider, add_container_tags).await?;
    }

    // Now we move on to our actual data pipelines.
    if dp.checks_enabled() {
        add_checks_pipeline_to_blueprint(&mut blueprint, &config.domains.checks.ipc_endpoint, env_provider).await?;
    }

    if dp.dogstatsd_enabled() {
        let dsd_control_surface = add_dsd_pipeline_to_blueprint(&mut blueprint, config_system, env_provider).await?;
        control_surfaces.attach_dogstatsd(dsd_control_surface);
    }

    if dp.otlp_enabled() {
        add_otlp_pipeline_to_blueprint(&mut blueprint, config_system, env_provider, semantic_registry).await?;
    }

    if dp.apm_enabled() {
        add_apm_pipeline_to_blueprint(&mut blueprint, config_system, env_provider).await?;
    }

    Ok((blueprint, control_surfaces))
}

const LIVENESS_METRICS_DESTINATION: &str = "metrics_enrich";

async fn add_liveness_source_to_blueprint(
    blueprint: &mut TopologyBlueprint, env_provider: &RemoteAgentEnvironmentProvider, add_container_tags: bool,
) -> Result<(), GenericError> {
    let liveness_config = LivenessConfiguration::from_environment_provider(env_provider, add_container_tags).await?;

    blueprint
        .add_source("liveness_in", liveness_config)?
        .connect_components("liveness_in.metrics", LIVENESS_METRICS_DESTINATION)?
        .connect_components("liveness_in.service_checks", "dd_service_checks_encode")?;

    Ok(())
}

async fn add_checks_pipeline_to_blueprint(
    blueprint: &mut TopologyBlueprint, checks_ipc_endpoint: &str, env_provider: &RemoteAgentEnvironmentProvider,
) -> Result<(), GenericError> {
    let grpc_endpoint = ListenAddress::try_from(checks_ipc_endpoint)
        .map_err(|error| generic_error!("Invalid checks IPC endpoint `{checks_ipc_endpoint}`: {error}"))?;
    let default_hostname = env_provider
        .host()
        .get_hostname()
        .await
        .error_context("Failed to get default hostname for Checks IPC source.")?;
    let checks_config = ChecksIPCConfiguration::new(grpc_endpoint, default_hostname);

    blueprint
        .add_source("checks_ipc_in", checks_config)?
        .connect_components("checks_ipc_in.metrics", "metrics_enrich")?
        .connect_components("checks_ipc_in.logs", "dd_logs_encode")?
        .connect_components("checks_ipc_in.events", "dd_events_encode")?
        .connect_components("checks_ipc_in.service_checks", "dd_service_checks_encode")?;

    Ok(())
}

async fn add_baseline_metrics_pipeline_to_blueprint(
    blueprint: &mut TopologyBlueprint, config_system: &ConfigurationSystem,
    remote_agent_client_config: Option<&RemoteAgentClientConfiguration>, shared: &SharedConfiguration,
    routing_targets: &Arc<RoutingTargetCatalog>, env_provider: &RemoteAgentEnvironmentProvider,
) -> Result<(), GenericError> {
    // Create the back half of the metrics processing pipeline.
    let host_enrichment_config = HostEnrichmentConfiguration::from_environment_provider(env_provider.clone());
    let mut metrics_enrich_config =
        ChainedConfiguration::default().with_transform_builder("host_enrichment", host_enrichment_config);

    let config = config_system.config();
    let dp = DataPlaneConfiguration::from_configuration(&config);
    if !dp.standalone_mode() {
        let client_config = remote_agent_client_config
            .ok_or_else(|| generic_error!("Remote Agent client configuration is required in connected mode."))?;
        let host_tags_config = HostTagsConfiguration::new(client_config.clone(), shared.tags.expected_tags_duration);
        if host_tags_config.enabled() {
            metrics_enrich_config = metrics_enrich_config.with_transform_builder("host_tags", host_tags_config);
        }
    }

    let endpoint_routing = MetricsEndpointRoutingConfiguration::new(
        &config.domains.metrics_endpoint_routing.metric_allowlists,
        &config.domains.metrics_endpoint_routing.metric_prefix_allowlists,
        &shared.endpoints.primary_endpoint(),
        &shared.endpoints.additional_endpoints,
    )?;
    blueprint.add_transform("metrics_enrich", metrics_enrich_config)?;

    add_metrics_output_pipelines_to_blueprint(blueprint, shared, routing_targets, &endpoint_routing)?;
    add_mrf_metrics_pipeline_to_blueprint(blueprint, config_system, shared, &config.domains.multi_region_failover)?;
    add_autoscaling_failover_metrics_pipeline_to_blueprint(blueprint, shared)?;

    Ok(())
}

fn add_mrf_metrics_pipeline_to_blueprint(
    blueprint: &mut TopologyBlueprint, config_system: &ConfigurationSystem, shared: &SharedConfiguration,
    mrf: &multi_region_failover::Domain,
) -> Result<(), GenericError> {
    let mrf_config = mrf_configuration(mrf);

    let Some((mrf_dd_url, mrf_api_key)) = mrf_config.metrics_endpoint_override() else {
        if mrf_config.is_enabled() {
            warn!(
                "Multi-Region Failover is enabled, but multi_region_failover.api_key and one of \
                 multi_region_failover.dd_url or multi_region_failover.site are required for metrics forwarding. The \
                 MRF metrics branch will not be wired, and primary forwarding will continue. Restart ADP after \
                 configuring the static MRF endpoint settings."
            );
        }

        return Ok(());
    };

    let mrf_gateway_config = MetricFilterConfiguration::for_mrf(
        mrf_config.is_enabled(),
        config_system
            .live(|config| &config.domains.multi_region_failover.metric_mirroring)
            .map(metric_mirroring),
    );
    let mrf_metrics_config = datadog_metrics_configuration(shared).with_metrics_endpoint_override(mrf_dd_url.clone());

    let mrf_forwarder_config = DatadogForwarderConfiguration::for_endpoint_override(
        forwarder_configuration(shared),
        mrf_dd_url,
        mrf_api_key,
        config_system.live(|config| &config.domains.multi_region_failover.api_key),
        config_system.live(|config| &config.shared.secrets).map(Secrets::in_use),
    );

    blueprint
        .add_transform("mrf_metrics_gateway", mrf_gateway_config)?
        .add_encoder("mrf_metrics_encode", mrf_metrics_config)?
        .add_forwarder("mrf_dd_out", mrf_forwarder_config)?
        .connect_components_in_order([
            "metrics_enrich",
            "mrf_metrics_gateway",
            "mrf_metrics_encode",
            "mrf_dd_out",
        ])?;

    Ok(())
}

// Build both sides of metric filtering together: the ordinary stream excludes selected endpoints, and each
// filtered stream targets only its policy's endpoints. This applies to both series and sketches.
fn add_metrics_output_pipelines_to_blueprint(
    blueprint: &mut TopologyBlueprint, shared: &SharedConfiguration, catalog: &Arc<RoutingTargetCatalog>,
    routing: &MetricsEndpointRoutingConfiguration,
) -> Result<(), GenericError> {
    let mut dd_metrics_config = datadog_metrics_configuration(shared);
    if !routing.selected_endpoints().is_empty() {
        dd_metrics_config = dd_metrics_config.with_routing(Arc::clone(catalog), routing.unfiltered_targets(catalog));
    }
    blueprint
        .add_encoder("dd_metrics_encode", dd_metrics_config)?
        .connect_components_in_order(["metrics_enrich", "dd_metrics_encode", "dd_out"])?;

    // Each policy group needs unique component IDs; indices keep endpoint names out of those IDs.
    for (index, policy) in routing.policy_groups().iter().enumerate() {
        let filter_id = format!("metrics_routing_filter_{index}");
        let encoder_id = format!("metrics_routing_encode_{index}");
        let filter_config = MetricFilterConfiguration::for_allowlist(
            policy.metric_allowlist.clone(),
            policy.metric_prefix_allowlist.clone(),
        );
        let metrics_config =
            datadog_metrics_configuration(shared).with_routing(Arc::clone(catalog), policy.targets(catalog));

        blueprint
            .add_transform(filter_id.as_str(), filter_config)?
            .add_encoder(encoder_id.as_str(), metrics_config)?
            .connect_components_in_order(["metrics_enrich", filter_id.as_str(), encoder_id.as_str(), "dd_out"])?;
    }

    Ok(())
}

fn add_autoscaling_failover_metrics_pipeline_to_blueprint(
    blueprint: &mut TopologyBlueprint, shared: &SharedConfiguration,
) -> Result<(), GenericError> {
    let af_config = AutoscalingFailoverConfiguration::new(
        shared.autoscaling_failover.enabled,
        shared.autoscaling_failover.metrics.clone(),
    );
    let cluster_agent = &shared.cluster_agent;
    let ca_config = ClusterAgentConfiguration {
        enabled: cluster_agent.enabled,
        url: cluster_agent.url.clone(),
        auth_token: cluster_agent.auth_token.clone(),
        kubernetes_service_name: cluster_agent.kubernetes_service_name.clone(),
    };

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
    let af_metrics_config = datadog_metrics_configuration(shared).with_v2_series_only();
    let cluster_agent_forwarder_config =
        ClusterAgentForwarderConfiguration::new(forwarder_configuration(shared), ca_url, ca_token)
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

fn add_baseline_logs_pipeline_to_blueprint(
    blueprint: &mut TopologyBlueprint, shared: &SharedConfiguration,
) -> Result<(), GenericError> {
    // Create the back half of the logs processing pipeline.
    let compression = &shared.endpoints.compression;
    // TODO: the buffered encoders ignore `metrics_encoding.flush_timeout` and always use its default.
    let dd_logs_config = BufferedIncrementalConfiguration::new(
        DatadogLogsConfiguration::new(&compression.compressor_kind, compression.effective_zstd_level()),
        DEFAULT_ENCODER_FLUSH_TIMEOUT,
    );

    blueprint
        // Components.
        .add_encoder("dd_logs_encode", dd_logs_config)?
        // Logs.
        .connect_components("dd_logs_encode", "dd_out")?;

    Ok(())
}

fn add_baseline_events_pipeline_to_blueprint(
    blueprint: &mut TopologyBlueprint, shared: &SharedConfiguration,
) -> Result<(), GenericError> {
    let metrics_encoding = &shared.metrics_encoding;
    let compression = &shared.endpoints.compression;
    // TODO: the buffered encoders ignore `metrics_encoding.flush_timeout` and always use its default.
    let dd_events_config = BufferedIncrementalConfiguration::new(
        DatadogEventsConfiguration {
            max_payload_size: metrics_encoding.max_payload_size,
            max_uncompressed_payload_size: metrics_encoding.max_uncompressed_payload_size,
            compressor_kind: compression.compressor_kind.clone(),
            zstd_level: compression.effective_zstd_level(),
            log_payloads: metrics_encoding.log_payloads,
        },
        DEFAULT_ENCODER_FLUSH_TIMEOUT,
    );

    blueprint
        .add_encoder("dd_events_encode", dd_events_config)?
        .connect_components("dd_events_encode", "dd_out")?;

    Ok(())
}

fn add_baseline_service_checks_pipeline_to_blueprint(
    blueprint: &mut TopologyBlueprint, shared: &SharedConfiguration,
) -> Result<(), GenericError> {
    let metrics_encoding = &shared.metrics_encoding;
    let compression = &shared.endpoints.compression;
    // TODO: the buffered encoders ignore `metrics_encoding.flush_timeout` and always use its default.
    let dd_service_checks_config = BufferedIncrementalConfiguration::new(
        DatadogServiceChecksConfiguration {
            max_payload_size: metrics_encoding.max_payload_size,
            max_uncompressed_payload_size: metrics_encoding.max_uncompressed_payload_size,
            compressor_kind: compression.compressor_kind.clone(),
            zstd_level: compression.effective_zstd_level(),
            log_payloads: metrics_encoding.log_payloads,
        },
        DEFAULT_ENCODER_FLUSH_TIMEOUT,
    );

    blueprint
        .add_encoder("dd_service_checks_encode", dd_service_checks_config)?
        .connect_components("dd_service_checks_encode", "dd_out")?;

    Ok(())
}

async fn add_baseline_traces_pipeline_to_blueprint(
    blueprint: &mut TopologyBlueprint, config_system: &ConfigurationSystem,
    env_provider: &RemoteAgentEnvironmentProvider, trace_sampling: Option<TraceSamplingSubscription>,
    semantic_registry: SemanticRegistryProvider,
) -> Result<(), GenericError> {
    let config = config_system.config();
    let agent_env = AgentEnv::resolve(
        config.domains.traces.apm_env.as_deref(),
        config.domains.traces.env.as_deref(),
        &config.shared.tags.tags,
        &config.shared.tags.extra_tags,
    );
    let dd_traces_config = trace_configuration(
        &config.domains.traces,
        &config.domains.otlp.traces,
        &config.shared,
        agent_env.clone(),
    )
    .with_environment_provider(env_provider.clone())
    .await?;
    let trace_sampler_config = trace_sampler_configuration(
        &config.domains.traces,
        &config.domains.otlp.traces,
        agent_env.clone(),
        trace_sampling.unwrap_or_default(),
    );

    let trace_obfuscation_config = TraceObfuscationConfiguration {
        config: obfuscation_configuration(&config.domains.traces.obfuscation),
    };
    let trace_tag_replacer_config = TraceTagReplacerConfiguration::new(
        config
            .domains
            .traces
            .replace_tags
            .iter()
            .cloned()
            .map(|rule| ReplaceRule {
                name: rule.name,
                pattern: rule.pattern,
                repl: rule.repl,
            })
            .collect(),
    );

    let ottl_filter = &config.domains.traces.ottl_filter;
    let ottl_filter_config = OttlFilterConfiguration::new(
        ottl_error_mode(ottl_filter.error_mode),
        ottl_filter.span_conditions.clone(),
    );

    let ottl_transform = &config.domains.traces.ottl_transform;
    let ottl_transform_config = OttlTransformConfiguration::new(
        ottl_error_mode(ottl_transform.error_mode),
        ottl_transform.trace_statements.clone(),
    );

    let dd_traces_enrich_config = ChainedConfiguration::default()
        .with_transform_builder("ottl_filter", ottl_filter_config)
        .with_transform_builder("ottl_transform", ottl_transform_config)
        .with_transform_builder("apm_onboarding", ApmOnboardingConfiguration)
        .with_transform_builder("trace_obfuscation", trace_obfuscation_config)
        .with_transform_builder("trace_tag_replacer", trace_tag_replacer_config);
    let apm_stats_transform_config =
        apm_stats_configuration(&config.domains.traces, agent_env.clone(), semantic_registry)
            .with_environment_provider(env_provider.clone())
            .await?;
    let dd_apm_stats_encoder =
        DatadogApmStatsEncoderConfiguration::new(config.shared.metrics_encoding.flush_timeout, agent_env)
            .with_environment_provider(env_provider.clone())
            .await?;

    blueprint
        .add_transform("traces_enrich", dd_traces_enrich_config)?
        .add_transform("trace_sampler", trace_sampler_config)?
        .add_transform("dd_apm_stats", apm_stats_transform_config)?
        .add_encoder("dd_stats_encode", dd_apm_stats_encoder)?
        .add_encoder("dd_traces_encode", dd_traces_config)?
        .connect_components("traces_enrich", "trace_sampler")?
        .connect_components("trace_sampler", ["dd_apm_stats", "dd_traces_encode"])?
        // The sampler reports its sampling telemetry as metric events through this dedicated
        // output, delivering the metrics through the metrics pipeline to the backend.
        .connect_components("trace_sampler.metrics", "metrics_enrich")?
        .connect_components("dd_apm_stats", "dd_stats_encode")?
        .connect_components(["dd_traces_encode", "dd_stats_encode"], "dd_out")?;

    Ok(())
}

/// Subdirectory of `run_path` that holds DogStatsD capture files when no capture path is configured.
const DOGSTATSD_CAPTURE_DIR: &str = "dsd_capture";

/// Resolves the directory that DogStatsD capture files are written to by default.
///
/// An unset capture path falls back to `run_path` plus `dsd_capture`. When neither is available, the source starts
/// without a default and a capture session must name its own path.
fn dogstatsd_capture_path(capture_path: &Path, run_path: &Option<PathBuf>) -> PathBuf {
    if capture_path.parent().is_some() {
        return capture_path.to_path_buf();
    }

    match run_path {
        Some(run_path) => run_path.join(DOGSTATSD_CAPTURE_DIR),
        None => {
            debug!(
                "`dogstatsd_capture_path` and `run_path` were empty. Default DogStatsD capture path is unavailable."
            );
            PathBuf::new()
        }
    }
}

/// Builds the DogStatsD source configuration from the typed model and the running environment.
///
/// The caller creates `capture_control` and `replay_control` so that it keeps the handles the source binds to, which is
/// how the capture and replay API surfaces reach the running source.
fn dogstatsd_source_configuration(
    config: &SalukiConfiguration, env_provider: &RemoteAgentEnvironmentProvider, default_hostname: MetaString,
    capture_control: DogStatsDCaptureControl, replay_control: DogStatsDReplayControl,
) -> DogStatsDConfiguration {
    let dogstatsd = &config.domains.dogstatsd;
    let listeners = &dogstatsd.listeners;
    let contexts = &dogstatsd.contexts;
    let origin = &dogstatsd.origin;

    let mut additional_tags = dogstatsd.tags.clone();
    additional_tags.extend(resolve_static_tags(
        &config.shared.static_tags,
        &config.shared.tags,
        features::is_ecs_fargate(),
    ));

    // One provider serves both roles: the full workload lookup used for origin enrichment, and the narrow live-PID
    // lookup used to pin a sender entity before packet processing is deferred.
    let workload_provider = Arc::new(env_provider.workload().clone());

    DogStatsDConfiguration {
        default_hostname,
        buffer_size: listeners.buffer_size,
        buffer_count: listeners.buffer_count,
        buffer_count_max: listeners.buffer_count_max,
        workers_count: listeners.workers_count,
        port: listeners.port,
        socket_receive_buffer_size: listeners.so_rcvbuf,
        tcp_port: listeners.tcp_port,
        statsd_forward_host: listeners.forward_host.as_deref().map(MetaString::from),
        statsd_forward_port: listeners.forward_port,
        socket_path: listeners.socket.clone(),
        socket_stream_path: listeners.stream_socket.clone(),
        stream_log_too_big: listeners.stream_log_too_big,
        // The Agent skips a listener that can't be created; `dogstatsd_require_listener` only decides whether running
        // with none is fatal.
        listener_requirement: if listeners.require_listener {
            ListenerRequirement::AtLeastOne
        } else {
            ListenerRequirement::Optional
        },
        pipe_name: listeners.pipe_name.clone(),
        windows_pipe_security_descriptor: listeners.windows_pipe_security_descriptor.clone(),
        disable_verbose_logs: dogstatsd.debug_log.disable_verbose_logs,
        eol_required: listeners.eol_required.clone(),
        bind_host: config.shared.bind_host.value.clone(),
        non_local_traffic: listeners.non_local_traffic,
        autoscale_udp_listeners: listeners.autoscale_udp_listeners,
        allow_context_heap_allocations: contexts.allow_context_heap_allocs,
        no_aggregation_pipeline_support: dogstatsd.aggregation.no_aggregation_pipeline,
        context_string_interner_entry_count: contexts.string_interner_size,
        context_string_interner_size_bytes: contexts.string_interner_size_bytes.map(ByteSize::b),
        cached_contexts_limit: contexts.cached_contexts_limit,
        cached_tagsets_limit: contexts.cached_tagsets_limit,
        context_expiry_seconds: dogstatsd.aggregation.context_expiry_seconds,
        permissive_decoding: listeners.permissive_decoding,
        minimum_sample_rate: contexts.minimum_sample_rate,
        enable_payloads: EnablePayloadsConfiguration {
            series: dogstatsd.enable_payloads.series,
            sketches: dogstatsd.enable_payloads.sketches,
            events: dogstatsd.enable_payloads.events,
            service_checks: dogstatsd.enable_payloads.service_checks,
        },
        origin_enrichment: OriginEnrichmentConfiguration {
            enabled: origin.detection,
            entity_id_precedence: origin.entity_id_precedence,
            tag_cardinality: origin_tag_cardinality(origin.tag_cardinality),
            origin_detection_unified: origin.unified,
            origin_detection_optout: origin.optout_enabled,
            origin_detection_client: origin.detection_client,
        },
        origin_telemetry_enabled: dogstatsd.telemetry.origin_breakdown,
        workload_provider: Some(workload_provider.clone()),
        capture_entity_resolver: Some(workload_provider),
        additional_tags,
        capture_path: dogstatsd_capture_path(&listeners.capture_path, &config.shared.run_path),
        capture_depth: listeners.capture_depth,
        capture_control,
        replay_control,
    }
}

async fn add_dsd_pipeline_to_blueprint(
    blueprint: &mut TopologyBlueprint, config_system: &ConfigurationSystem,
    env_provider: &RemoteAgentEnvironmentProvider,
) -> Result<DogStatsDControlSurface, GenericError> {
    // We're creating the "front half" of the DogStatsD pipeline, which deals solely with accepting DogStatsD payloads,
    // and enriching/processing them in DSD-specific ways, relevant to how the Datadog Agent is expected to behave.
    //
    //                                                 ┌─────────────────────┐
    //                              metrics            │      DogStatsD      │
    //               ┌ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ │       (source)      │ ─ ─ ─ ─ ─ ─ ─ ┐
    //               │                 │               └─────────────────────┘               │
    //               │                 │                          │                          │
    //               │                 │                          │ service checks           │ events
    //               │                 ▼                          ▼                          ▼
    //               │      ┌─────────────────────┐    ┌─────────────────────┐    ┌─────────────────────┐
    //               │      │     DSD Enrich      │    │     DSD Service     │    │     DSD Events      │
    //               │      │ (chained transform) │    │    Checks (encoder) │    │      (encoder)      │
    //               │      │┌───────────────────┐│    └─────────────────────┘    └─────────────────────┘
    //               │      ││    DSD Mapper     ││               │                          │
    //               │      │└───────────────────┘│               │                          │
    //               │      └─────────────────────┘               │                          │
    //               │                 │                          │                          │
    //               │                 ▼                          │                          │
    //               │      ┌─────────────────────┐               │                          │
    //               │      │  DSD Prefix/Filter  │               │                          │
    //               │      │     (transform)     │               │                          │
    //               │      └─────────────────────┘               │                          │
    //               │                 │                          │                          │
    //               │                 │        ┌ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ┐           │
    //               │                 └ ─ ─ ─▶ │        Metrics Pipeline       │           │
    //               │                          │  (aggregate, enrich, encode)  │           │
    //               │                          └ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ┘           │
    //               │                                       │                               │
    //               ▼                                       ▼                               ▼
    //    ┌─────────────────────┐    ┌ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ┐
    //    │      DSD Stats      │    │                           Forwarder                             │
    //    │    (destination)    │    │                       (Datadog Platform)                        │
    //    └─────────────────────┘    └ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ┘

    let default_hostname = env_provider
        .host()
        .get_hostname()
        .await
        .error_context("Failed to get default hostname for DogStatsD source.")?;
    let typed = config_system.config();
    let dsd_capture_control = DogStatsDCaptureControl::default();
    let dsd_replay_control = DogStatsDReplayControl::default();
    let dsd_config = dogstatsd_source_configuration(
        &typed,
        env_provider,
        default_hostname.into(),
        dsd_capture_control.clone(),
        dsd_replay_control.clone(),
    );
    let prefix_filter = &typed.domains.dogstatsd.prefix_filter;
    let dsd_prefix_filter_configuration = DogStatsDPrefixFilterConfiguration::new(
        prefix_filter.metric_namespace.clone(),
        prefix_filter.metric_namespace_blocklist.clone(),
        config_system
            .live(|config| &config.domains.dogstatsd.metric_filter)
            .map(metric_name_filter),
    );
    let mapper = &typed.domains.dogstatsd.mapper;
    let mapper_profiles = mapper
        .profiles
        .iter()
        .map(|profile| DogStatsDMapperProfile {
            name: profile.name.clone(),
            prefix: profile.prefix.clone(),
            mappings: profile
                .mappings
                .iter()
                .map(|mapping| DogStatsDMetricMapping {
                    metric_match: mapping.metric_match.clone(),
                    match_type: mapping.match_type.clone(),
                    name: mapping.name.clone(),
                    tags: mapping.tags.clone(),
                })
                .collect(),
        })
        .collect();
    let dsd_mapper_config =
        DogStatsDMapperConfiguration::new(mapper.string_interner_size_bytes, mapper.cache_size, mapper_profiles);
    let dsd_enrich_config =
        ChainedConfiguration::default().with_transform_builder("dogstatsd_mapper", dsd_mapper_config);
    let dsd_tag_filterlist_config = TagFilterlistConfiguration::new(
        config_system
            .live(|config| &config.domains.dogstatsd.tag_filterlist)
            .map(|entries| tag_filter_entries(entries)),
        &tag_value_allowlist(&typed.domains.dogstatsd.tag_value_allowlist),
        typed.domains.dogstatsd.aggregation.aggregator_tag_filter_cache_capacity,
    );
    let aggregation = &typed.domains.dogstatsd.aggregation;
    let histogram = &typed.shared.metrics_encoding.histogram;
    let dsd_hist_config = HistogramConfiguration::try_new(
        &histogram.aggregates,
        &histogram.percentiles,
        histogram.copy_to_distribution,
        histogram.copy_to_distribution_prefix.clone(),
    )
    .error_context("Failed to configure histogram aggregation.")?;
    let (dsd_context_snapshot_handle, dsd_context_snapshot_receiver) = aggregate_context_snapshot_channel();
    let dsd_agg_config = AggregateConfiguration {
        window_duration_seconds: aggregation.window_duration_seconds,
        primary_flush_interval: aggregation.flush_interval,
        context_limit: aggregation.context_limit,
        flush_open_windows: aggregation.flush_open_windows,
        counter_expiry_seconds: aggregation.counter_expiry_seconds,
        hist_config: dsd_hist_config,
        context_snapshot_receiver: dsd_context_snapshot_receiver,
    };
    let dsd_no_agg_split_config = DogStatsDNoAggSplitConfiguration::new(aggregation.passthrough_idle_flush_timeout);
    let dsd_post_agg_filter_config = DogStatsDPostAggregateFilterConfiguration::new(
        config_system
            .live(|config| &config.domains.dogstatsd.metric_filter)
            .map(metric_name_filter),
        &histogram.aggregates,
        &histogram.percentiles,
    )
    .error_context("Failed to configure DogStatsD post-aggregate filter transform.")?;
    let events_enrich_config = ChainedConfiguration::default().with_transform_builder(
        "host_enrichment",
        HostEnrichmentConfiguration::from_environment_provider(env_provider.clone()),
    );
    let service_checks_enrich_config = ChainedConfiguration::default().with_transform_builder(
        "host_enrichment",
        HostEnrichmentConfiguration::from_environment_provider(env_provider.clone()),
    );

    // Resolve the platform default log path when unset.
    let debug_log = &typed.domains.dogstatsd.debug_log;
    let debug_log_file = debug_log
        .log_file
        .clone()
        .unwrap_or_else(PlatformSettings::get_default_dogstatsd_log_file_path);
    if debug_log_file.to_str().is_none() {
        return Err(generic_error!(
            "dogstatsd_log_file must be valid UTF-8, got '{}'",
            debug_log_file.display()
        ));
    }

    let dsd_debug_log_config = DogStatsDDebugLogConfiguration {
        metrics_stats_enabled: config_system.live(|config| &config.domains.dogstatsd.debug_log.metrics_stats_enable),
        log_file: debug_log_file,
        log_file_max_size: debug_log.log_file_max_size,
        log_file_max_rolls: debug_log.log_file_max_rolls,
    };
    let dsd_stats_config = DogStatsDStatisticsConfiguration::new();

    let stats_api_handler = dsd_stats_config.api_handler();
    let capture_api_handler = DogStatsDCaptureAPIHandler::new(dsd_capture_control);
    let replay_api_handler = DogStatsDReplayAPIHandler::new(dsd_replay_control);
    let context_dump_api_handler = DogStatsDContextDumpAPIHandler::new(
        vec![dsd_context_snapshot_handle],
        typed.shared.run_path.clone().unwrap_or_default(),
    );

    blueprint
        // Components.
        .add_source("dsd_in", dsd_config)?
        .add_transform("dsd_prefix_filter", dsd_prefix_filter_configuration)?
        .add_transform("dsd_enrich", dsd_enrich_config)?
        .add_transform("dsd_tag_filterlist", dsd_tag_filterlist_config)?
        .add_transform("dsd_agg", dsd_agg_config)?
        .add_transform("dsd_post_agg_filter", dsd_post_agg_filter_config)?
        .add_transform("events_enrich", events_enrich_config)?
        .add_transform("service_checks_enrich", service_checks_enrich_config)?
        .add_destination("dsd_stats_out", dsd_stats_config)?
        .add_destination("dsd_client_telemetry_out", DogStatsDClientTelemetryConfiguration)?
        // Events.
        .connect_components_in_order(["dsd_in.events", "events_enrich", "dd_events_encode"])?
        // Service checks.
        .connect_components_in_order([
            "dsd_in.service_checks",
            "service_checks_enrich",
            "dd_service_checks_encode",
        ])?
        // DogStatsD Stats.
        .connect_components("dsd_in.metrics", "dsd_stats_out")?
        // Post-aggregation client telemetry for RAR/COAT.
        .connect_components("dsd_post_agg_filter", "dsd_client_telemetry_out")?;

    // Metrics.
    //
    // `dsd_no_agg_split` only earns a place in the pipeline when the no-aggregation pipeline is enabled. With it
    // disabled, every metric is aggregated, so the split would have nothing to route out of the aggregation path and
    // would sit in the hottest part of the process as a pure forwarder.
    if aggregation.no_aggregation_pipeline {
        blueprint
            .add_transform("dsd_no_agg_split", dsd_no_agg_split_config)?
            .connect_components_in_order([
                "dsd_in.metrics",
                "dsd_enrich",
                "dsd_prefix_filter",
                "dsd_no_agg_split",
                "dsd_tag_filterlist",
                "dsd_agg",
                "dsd_post_agg_filter",
                "metrics_enrich",
            ])?
            // Timestamped metrics skip tag filtering and aggregation, rejoining the pipeline after the aggregate
            // transform.
            .connect_components("dsd_no_agg_split.passthrough", "dsd_post_agg_filter")?;
    } else {
        blueprint.connect_components_in_order([
            "dsd_in.metrics",
            "dsd_enrich",
            "dsd_prefix_filter",
            "dsd_tag_filterlist",
            "dsd_agg",
            "dsd_post_agg_filter",
            "metrics_enrich",
        ])?;
    }

    if debug_log.logging_enabled {
        blueprint
            // DogStatsD debug log.
            .add_destination("dsd_debug_log_out", dsd_debug_log_config)?
            .connect_components("dsd_in.metrics", "dsd_debug_log_out")?;
    }
    Ok(DogStatsDControlSurface {
        stats_api_handler,
        capture_api_handler,
        replay_api_handler,
        context_dump_api_handler,
    })
}

async fn add_apm_pipeline_to_blueprint(
    blueprint: &mut TopologyBlueprint, config_system: &ConfigurationSystem,
    env_provider: &RemoteAgentEnvironmentProvider,
) -> Result<(), GenericError> {
    let config = config_system.config();
    // In standalone mode there is no workload provider, so the resolver finds nothing and the relay forwards requests
    // without a container ID
    let dd_traces_relay = apm_relay_configuration(
        &config.domains.apm,
        &config.shared,
        datadog_agent_config::is_containerized(),
        Some(Arc::new(env_provider.workload().clone())),
    );
    blueprint
        .add_relay("dd_traces_relay", dd_traces_relay)?
        // The relay has no output path of its own, but a topology can't hold a component with no connections, so it
        // connects to the forwarder, which is always created when APM is enabled.
        .connect_components("dd_traces_relay.traces", "dd_out")?;
    Ok(())
}

async fn add_otlp_pipeline_to_blueprint(
    blueprint: &mut TopologyBlueprint, config_system: &ConfigurationSystem,
    env_provider: &RemoteAgentEnvironmentProvider, semantic_registry: SemanticRegistryProvider,
) -> Result<(), GenericError> {
    let config = config_system.config();
    let dp = DataPlaneConfiguration::from_configuration(&config);
    if dp.otlp_proxy_enabled() {
        let core_agent_otlp_grpc_endpoint = config.domains.otlp.proxy.grpc_endpoint.clone();
        let proxy_metrics = config.domains.otlp.proxy.metrics_enabled;
        let proxy_logs = config.domains.otlp.proxy.logs_enabled;
        let proxy_traces = config.domains.otlp.proxy.traces_enabled;

        info!(
            proxy_grpc_endpoint = %core_agent_otlp_grpc_endpoint,
            proxy_metrics,
            proxy_logs,
            proxy_traces,
            "OTLP proxy mode enabled. Select OTLP payloads will be proxied to the Core Agent."
        );

        let config = config_system.config();
        let otlp_relay_config = OtlpRelayConfiguration::new(otlp_receiver(&config.domains.otlp.receiver));
        let otlp_decoder_config = OtlpDecoderConfiguration::new(otlp_traces_settings(&config.domains.otlp.traces))
            .with_max_resource_len(config.domains.traces.max_resource_len)
            .with_semantic_registry(semantic_registry);

        let local_agent_otlp_forwarder_config =
            OtlpForwarderConfiguration::new(config.domains.otlp.traces.internal_port, core_agent_otlp_grpc_endpoint);

        blueprint
            // Components.
            .add_relay("otlp_relay_in", otlp_relay_config)?
            .add_forwarder("local_agent_otlp_out", local_agent_otlp_forwarder_config)?
            // Metrics and logs directly to the forwarders.
            .connect_components(["otlp_relay_in.metrics", "otlp_relay_in.logs"], "local_agent_otlp_out")?;

        if proxy_traces {
            blueprint.connect_components("otlp_relay_in.traces", "local_agent_otlp_out")?;
        } else {
            blueprint
                .add_decoder("otlp_traces_decode", otlp_decoder_config)?
                // Traces to decoder, then to the trace pipeline: obfuscation, enrichment, encoding, stats, forwarding.
                .connect_components_in_order(["otlp_relay_in.traces", "otlp_traces_decode", "traces_enrich"])?;
        }
    } else {
        info!("OTLP proxy mode disabled. OTLP signals will be handled natively.");

        let default_hostname = env_provider
            .host()
            .get_hostname()
            .await
            .error_context("Failed to get default hostname for OTLP source.")?;
        let config = config_system.config();
        let static_tags = resolve_static_tags(
            &config.shared.static_tags,
            &config.shared.tags,
            features::is_ecs_fargate(),
        );
        let otlp = &config.domains.otlp;
        let otlp_config = OtlpConfiguration {
            default_hostname: default_hostname.into(),
            receiver: otlp_receiver(&otlp.receiver),
            metrics: otlp_metrics_settings(&otlp.metrics),
            traces_enabled: otlp.traces.enabled,
            traces: otlp_traces_settings(&otlp.traces),
            contexts: otlp_context_settings(&otlp.contexts),
            max_resource_len: config.domains.traces.max_resource_len,
            semantic_registry,
            workload_provider: Arc::new(env_provider.workload().clone()),
        }
        .with_static_metric_tags(static_tags);

        blueprint
            // Components.
            .add_source("otlp_in", otlp_config)?
            // Metrics, logs, and traces.
            //
            // We send OTLP metrics directly to the enrichment stage of the metrics pipeline, skipping aggregation,
            // to avoid transforming counters into rates.
            .connect_components("otlp_in.metrics", "metrics_enrich")?
            .connect_components("otlp_in.logs", "dd_logs_encode")?
            .connect_components("otlp_in.traces", "traces_enrich")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use http::{Request, StatusCode};
    use http_body_util::{BodyExt as _, Empty};
    use hyper::body::Bytes;
    use saluki_api::{response::Response, APIHandler as _};
    use saluki_components::transforms::{
        aggregate_context_snapshot_channel_for_test, AggregateContextSnapshotEntry, AggregateMetricType,
    };
    use saluki_core::data_model::event::metric::context::Context;
    use stringtheory::MetaString;
    use tower::ServiceExt as _;

    use crate::dogstatsd_contexts::DogStatsDContextDumpAPIHandler;

    const CONTEXT_DUMP_FILENAME: &str = "dogstatsd_contexts.json.zstd";
    const CONTEXT_DUMP_ROUTE: &str = crate::dogstatsd_contexts::CONTEXT_DUMP_ROUTE;

    #[test]
    fn dogstatsd_capture_path_falls_back_to_run_path() {
        let run_path = PathBuf::from("/my/little/run_path");

        assert_eq!(
            super::dogstatsd_capture_path(Path::new(""), &Some(run_path.clone())),
            run_path.join("dsd_capture")
        );
    }

    #[test]
    fn dogstatsd_capture_path_keeps_an_explicit_path() {
        let capture_path = PathBuf::from("/custom/path/to/capture");

        assert_eq!(
            super::dogstatsd_capture_path(&capture_path, &Some(PathBuf::from("/my/little/run_path"))),
            capture_path
        );
    }

    #[test]
    fn dogstatsd_capture_path_is_empty_without_a_run_path() {
        assert_eq!(super::dogstatsd_capture_path(Path::new(""), &None), PathBuf::new());
    }

    #[tokio::test]
    async fn context_dump_handler_uses_supplied_run_path_and_owner() {
        let run_directory = tempfile::tempdir().expect("run directory should be created");
        let (snapshot_handle, mut snapshot_responder) = aggregate_context_snapshot_channel_for_test();

        let handler = DogStatsDContextDumpAPIHandler::new(vec![snapshot_handle], run_directory.path());
        let owner = tokio::spawn(async move {
            snapshot_responder
                .respond(vec![snapshot_entry("from.supplied.aggregate")])
                .await
        });

        let response = send(&handler, context_dump_post()).await;

        assert_eq!(response.status(), StatusCode::OK);
        let artifact_path = run_directory.path().join(CONTEXT_DUMP_FILENAME);
        assert_eq!(
            response_body(response).await,
            serde_json::to_string(&artifact_path).unwrap()
        );
        assert!(artifact_path.is_file());
        owner.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn context_dump_handler_keeps_empty_run_path_empty_until_publication() {
        let cwd_artifact = std::env::current_dir().unwrap().join(CONTEXT_DUMP_FILENAME);
        assert!(!cwd_artifact.exists(), "test requires no pre-existing cwd artifact");

        for run_path in [None, Some(Path::new(""))] {
            let run_path = run_path.map(Path::to_path_buf).unwrap_or_default();
            let (snapshot_handle, mut snapshot_responder) = aggregate_context_snapshot_channel_for_test();
            let handler = DogStatsDContextDumpAPIHandler::new(vec![snapshot_handle], run_path);
            let owner = tokio::spawn(async move { snapshot_responder.respond(Vec::new()).await });

            let response = send(&handler, context_dump_post()).await;

            assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
            assert!(!cwd_artifact.exists());
            owner.await.unwrap().unwrap();
        }
    }

    fn context_dump_post() -> Request<Empty<Bytes>> {
        Request::builder()
            .method("POST")
            .uri(CONTEXT_DUMP_ROUTE)
            .body(Empty::new())
            .unwrap()
    }

    async fn send(handler: &DogStatsDContextDumpAPIHandler, request: Request<Empty<Bytes>>) -> Response {
        handler
            .generate_routes()
            .with_state(handler.generate_initial_state())
            .oneshot(request)
            .await
            .unwrap()
    }

    async fn response_body(response: Response) -> String {
        String::from_utf8(response.into_body().collect().await.unwrap().to_bytes().to_vec()).unwrap()
    }

    fn snapshot_entry(name: &'static str) -> AggregateContextSnapshotEntry {
        AggregateContextSnapshotEntry::for_test(
            Context::from_static_name(name),
            AggregateMetricType::Gauge,
            MetaString::empty(),
        )
    }
}
