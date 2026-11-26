use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Instant,
};

use agent_data_plane_config::{
    defaults::DEFAULT_ENCODER_FLUSH_TIMEOUT,
    domains::{logs, multi_region_failover},
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
use futures::StreamExt as _;
use saluki_app::bootstrap::BootstrapGuard;
use saluki_common::sync::live::Live;
use saluki_components::{
    config::{
        AgentEnv, AutoscalingFailoverConfiguration, ClusterAgentConfiguration, MetricsEndpointRoutingConfiguration,
    },
    destinations::{
        BlackholeConfiguration, DogStatsDClientTelemetryConfiguration, DogStatsDDebugLogConfiguration,
        DogStatsDStatisticsConfiguration,
    },
    encoders::{
        BufferedIncrementalConfiguration, DatadogApmStatsEncoderConfiguration, DatadogEventsConfiguration,
        DatadogLogsConfiguration, DatadogServiceChecksConfiguration,
    },
    forwarders::{ClusterAgentForwarderConfiguration, DatadogForwarderConfiguration, RoutingTargetCatalog},
    sources::{
        ChecksIPCConfiguration, DogStatsDCaptureAPIHandler, DogStatsDCaptureControl, DogStatsDConfiguration,
        DogStatsDReplayAPIHandler, DogStatsDReplayControl, EnablePayloadsConfiguration, ListenerRequirement,
        OriginEnrichmentConfiguration, OtlpConfiguration, OtlpRoute, OtlpRoutes,
    },
    transforms::{
        aggregate_context_snapshot_channel, AggregateConfiguration, AutoscalingFailoverGatewayConfiguration,
        ChainedConfiguration, DogStatsDMapperConfiguration, DogStatsDMapperProfile, DogStatsDMetricMapping,
        HistogramConfiguration, HostEnrichmentConfiguration, LogsProcessingRulesConfiguration,
        TraceObfuscationConfiguration,
    },
};
use saluki_core::{
    runtime::Supervisor,
    topology::{EdgeCondition, TopologyBlueprint},
};
use saluki_env::{features, EnvironmentProvider as _, HostProvider as _};
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use saluki_io::net::ListenAddress;
use stringtheory::MetaString;
use tracing::{debug, error, info, warn};

use crate::{
    component_inputs::{
        apm_relay_configuration, apm_stats_configuration, datadog_metrics_configuration, forwarder_configuration,
        logs_processing_rules, metric_mirroring, metric_name_filter, mrf_configuration, obfuscation_configuration,
        origin_tag_cardinality, otlp_context_settings, otlp_debug_verbosity, otlp_metrics_settings, otlp_receiver,
        otlp_traces_settings, ottl_error_mode, resolve_static_tags, tag_filter_entries, tag_value_allowlist,
        trace_configuration, trace_sampler_configuration, trace_tag_replacer_configuration,
        traces_forwarder_configuration,
    },
    components::{
        apm_onboarding::ApmOnboardingConfiguration,
        dogstatsd_no_agg_split::DogStatsDNoAggSplitConfiguration,
        dogstatsd_post_aggregate_filter::{DogStatsDPostAggregateFilterConfiguration, CLIENT_TELEMETRY_OUTPUT},
        dogstatsd_prefix_filter::DogStatsDPrefixFilterConfiguration,
        host_tags::HostTagsConfiguration,
        liveness::LivenessConfiguration,
        metric_filter::MetricFilterConfiguration,
        ottl_filter_processor::OttlFilterConfiguration,
        ottl_transform_processor::OttlTransformConfiguration,
        tag_filterlist::TagFilterlistConfiguration,
    },
    config::{DataPlaneConfiguration, OtlpPlan},
    dogstatsd_contexts::DogStatsDContextDumpAPIHandler,
    internal::{
        create_control_plane_supervisor, logging::get_logging_translator, AdpLocalConfig, AgentDataPlane,
        DogStatsDAPIWorker,
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

    // Every pipeline decision below comes from this one configuration and check.
    let config = agent.config().system().config();
    let dp = DataPlaneConfiguration::from_configuration(&config);

    if !agent.is_standalone() && !dp.enabled() {
        info!("Agent Data Plane is not enabled. Exiting.");
        return Ok(());
    }

    let otlp_plan = OtlpPlan::resolve(&dp).await;

    agent
        .config()
        .system()
        .check_compatibility(&dp.active_pipelines(&otlp_plan))
        .error_context("Incompatible configuration detected.")?;

    // Set up all of the building blocks for building our topologies and launching internal processes.
    let mut agent = agent.initialize().await?;

    let rc_products = remote_config::enabled_products(&dp, &otlp_plan, &config.domains.remote_configuration);
    let rc_subscriptions = RemoteConfigSubscriptions::subscribe(rc_products, &mut agent)
        .await
        .error_context("Failed to set up Remote Configuration.")?;

    // Create the blueprint for our primary topology.
    let (blueprint, dogstatsd_api) = create_topology(&agent, &dp, &otlp_plan, rc_subscriptions).await?;

    // Create the control plane, and then run: the remote agent builds the rest of the supervisors, and runs memory
    // bounds validation to ensure that we can launch the topology with our configured memory limit, if any.
    let control_plane = create_control_plane_supervisor(&agent, dogstatsd_api, bootstrap_guard.logging().controller())
        .await
        .error_context("Failed to create internal supervisor.")?;
    agent.run(blueprint, control_plane).await
}

async fn create_topology(
    agent: &RemoteAgent<AgentDataPlane, Initialized>, dp: &DataPlaneConfiguration<'_>, otlp: &OtlpPlan,
    rc_subscriptions: RemoteConfigSubscriptions,
) -> Result<(TopologyBlueprint, Option<DogStatsDAPIWorker>), GenericError> {
    let config_system = agent.config().system();
    let config = config_system.config();
    let mut blueprint = TopologyBlueprint::new("primary", agent.component_registry());
    blueprint.with_shutdown_timeout(dp.stop_timeout());
    let shared = config.shared.clone();

    let RemoteConfigSubscriptions {
        trace_sampling,
        semantic_registry,
    } = rc_subscriptions;

    // Like the Agent, run without OTLP ingest rather than failing startup when no OTLP signal is enabled.
    if dp.otlp_enabled_without_signals() {
        error!(
            "OTLP ingest is enabled, but OTLP metrics, logs, and traces are all disabled. ADP won't start OTLP \
            ingest. Enable at least one of `otlp_config.metrics.enabled`, `otlp_config.traces.enabled`, or \
            `otlp_config.logs.enabled` together with `logs_enabled`."
        );
    }

    if let Some(block) = otlp.blocked() {
        error!(reason = %block, "ADP won't start OTLP ingest.");
    }

    // If no data pipelines are enabled, then there's nothing for us to do.
    if !dp.data_pipelines_enabled(otlp) {
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
    // Notably, we _don't_ need either of these if all we're doing is running the OTLP pipeline with every signal
    // proxied to the Agent, which is the only reason we're differentiating here. Connected mode is the exception:
    // liveness deliberately provisions metric and service-check output paths even for proxy-only topologies.
    let mut routing_targets = None;
    if dp.metrics_pipeline_required(otlp)
        || dp.logs_pipeline_required(otlp)
        || dp.events_pipeline_required()
        || dp.service_checks_pipeline_required(otlp)
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

    // Traces and APM stats don't share `dd_out`, so routing them elsewhere never affects metrics, logs, or events.
    if dp.traces_forwarder_required(otlp) {
        let dd_traces_forwarder_config = DatadogForwarderConfiguration::new(
            traces_forwarder_configuration(&shared),
            config_system.live(|config| &config.shared.endpoints.api_key),
            config_system.live(|config| &config.shared.endpoints.traces.additional_endpoints),
            config_system.live(|config| &config.shared.secrets).map(Secrets::in_use),
        );
        blueprint.add_forwarder("dd_traces_out", dd_traces_forwarder_config)?;
    }

    if dp.logs_pipeline_required(otlp) {
        add_baseline_logs_pipeline_to_blueprint(&mut blueprint, &shared, &config.domains.logs, env_provider).await?;
    }

    if dp.events_pipeline_required() {
        add_baseline_events_pipeline_to_blueprint(&mut blueprint, &shared)?;
    }

    if dp.service_checks_pipeline_required(otlp) {
        add_baseline_service_checks_pipeline_to_blueprint(&mut blueprint, &shared)?;
    }

    let mut metrics_inputs = Vec::new();
    if dp.traces_pipeline_required(otlp) {
        add_baseline_traces_pipeline_to_blueprint(
            &mut blueprint,
            config_system,
            env_provider,
            trace_sampling,
            semantic_registry.clone(),
        )
        .await?;
        // The sampler emits sampling telemetry on a dedicated output. Send those metrics through the metrics pipeline.
        metrics_inputs.push("trace_sampler.metrics");
    }

    // Connected topologies emit liveness through the metric and service-check baselines created above. Standalone
    // OTLP proxy mode must not construct liveness or output paths that it does not otherwise need.
    if !dp.standalone_mode() {
        let add_container_tags = config_system.config().shared.basic_telemetry.add_container_tags;
        add_liveness_source_to_blueprint(&mut blueprint, env_provider, add_container_tags).await?;
        metrics_inputs.push("liveness_in.metrics");
    }

    // Build the enabled inputs first so the shared metrics pipeline can connect their outputs together.
    if dp.checks_enabled() {
        add_checks_pipeline_to_blueprint(
            &mut blueprint,
            &config.domains.checks.ipc_endpoint,
            logs_pipeline_input(&config.domains.logs),
            env_provider,
        )
        .await?;
        metrics_inputs.push("checks_ipc_in.metrics");
    }

    let dogstatsd_api = if dp.dogstatsd_enabled() {
        metrics_inputs.push("dsd_post_agg_filter");
        Some(add_dsd_pipeline_to_blueprint(&mut blueprint, config_system, env_provider).await?)
    } else {
        None
    };

    if otlp.enabled() {
        add_otlp_pipeline_to_blueprint(
            &mut blueprint,
            config_system,
            env_provider,
            semantic_registry.clone(),
            otlp,
        )
        .await?;
        if otlp.routes().metrics == OtlpRoute::Internal {
            metrics_inputs.push("otlp_in.metrics");
        }
    }

    if dp.apm_enabled() {
        add_apm_pipeline_to_blueprint(
            &mut blueprint,
            config_system,
            env_provider,
            remote_agent_client_config,
            semantic_registry,
        )
        .await?;
    }

    // The shared back end routes and encodes metrics for all enabled producers.
    if dp.metrics_pipeline_required(otlp) {
        let routing_targets =
            routing_targets.ok_or_else(|| generic_error!("The metrics pipeline requires the Datadog forwarder."))?;
        add_baseline_metrics_pipeline_to_blueprint(
            &mut blueprint,
            config_system,
            remote_agent_client_config,
            &shared,
            &routing_targets,
            env_provider,
            &metrics_inputs,
        )?;
    }

    Ok((blueprint, dogstatsd_api))
}

async fn add_liveness_source_to_blueprint(
    blueprint: &mut TopologyBlueprint, env_provider: &RemoteAgentEnvironmentProvider, add_container_tags: bool,
) -> Result<(), GenericError> {
    let liveness_config = LivenessConfiguration::from_environment_provider(env_provider, add_container_tags).await?;

    blueprint
        .add_source("liveness_in", liveness_config)?
        .connect_components("liveness_in.service_checks", "dd_service_checks_encode")?;

    Ok(())
}

async fn add_checks_pipeline_to_blueprint(
    blueprint: &mut TopologyBlueprint, checks_ipc_endpoint: &str, logs_input: &str,
    env_provider: &RemoteAgentEnvironmentProvider,
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
        .connect_components("checks_ipc_in.logs", logs_input)?
        .connect_components("checks_ipc_in.events", "dd_events_encode")?
        .connect_components("checks_ipc_in.service_checks", "dd_service_checks_encode")?;

    Ok(())
}

fn add_baseline_metrics_pipeline_to_blueprint(
    blueprint: &mut TopologyBlueprint, config_system: &ConfigurationSystem,
    remote_agent_client_config: Option<&RemoteAgentClientConfiguration>, shared: &SharedConfiguration,
    routing_targets: &Arc<RoutingTargetCatalog>, env_provider: &RemoteAgentEnvironmentProvider,
    metrics_inputs: &[&str],
) -> Result<(), GenericError> {
    let config = config_system.config();
    let dp = DataPlaneConfiguration::from_configuration(&config);
    let metrics_outputs_from = if dp.standalone_mode() {
        // Metric contexts already carry their host. Standalone pipelines do not add Agent host tags.
        metrics_inputs
    } else {
        let client_config = remote_agent_client_config
            .ok_or_else(|| generic_error!("Remote Agent client configuration is required in connected mode."))?;
        let host_enrichment_config = HostEnrichmentConfiguration::from_environment_provider(env_provider.clone());
        let mut metrics_enrich_config =
            ChainedConfiguration::default().with_transform_builder("host_enrichment", host_enrichment_config);
        let host_tags_config = HostTagsConfiguration::new(client_config.clone(), shared.tags.expected_tags_duration);
        if host_tags_config.enabled() {
            metrics_enrich_config = metrics_enrich_config.with_transform_builder("host_tags", host_tags_config);
        }
        blueprint.add_transform("metrics_enrich", metrics_enrich_config)?;
        for input in metrics_inputs {
            blueprint.connect_components(input, "metrics_enrich")?;
        }
        &["metrics_enrich"]
    };

    let endpoint_routing = MetricsEndpointRoutingConfiguration::new(
        &config.domains.metrics_endpoint_routing.metric_allowlists,
        &config.domains.metrics_endpoint_routing.metric_prefix_allowlists,
        &shared.endpoints.primary_endpoint(),
        &shared.endpoints.additional_endpoints,
    )?;

    add_metrics_output_pipelines_to_blueprint(
        blueprint,
        shared,
        routing_targets,
        &endpoint_routing,
        metrics_outputs_from,
    )?;
    add_mrf_metrics_pipeline_to_blueprint(
        blueprint,
        config_system,
        shared,
        &config.domains.multi_region_failover,
        metrics_outputs_from,
    )?;
    add_autoscaling_failover_metrics_pipeline_to_blueprint(blueprint, shared, metrics_outputs_from)?;

    Ok(())
}

fn add_mrf_metrics_pipeline_to_blueprint(
    blueprint: &mut TopologyBlueprint, config_system: &ConfigurationSystem, shared: &SharedConfiguration,
    mrf: &multi_region_failover::Domain, metrics_outputs_from: &[&str],
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

    // Only send metrics to the MRF branch while metric mirroring is active. When MRF is disabled at startup, the
    // connection can never be active, so the branch is never built.
    let mirroring_config = config_system.live(|config| &config.domains.multi_region_failover.metric_mirroring);
    let mrf_gateway_config =
        MetricFilterConfiguration::for_mrf(mrf_config.is_enabled(), mirroring_config.map(metric_mirroring));
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
        .connect_components_in_order(["mrf_metrics_gateway", "mrf_metrics_encode", "mrf_dd_out"])?;
    for from in metrics_outputs_from {
        let mrf_condition = if mrf_config.is_enabled() {
            live_condition(&mirroring_config, |mirroring| mirroring.enabled)
        } else {
            EdgeCondition::fixed(false)
        };
        blueprint.connect_components_when(from, "mrf_metrics_gateway", mrf_condition)?;
    }

    Ok(())
}

// Build both sides of metric filtering together: the ordinary stream excludes selected endpoints, and each
// filtered stream targets only its policy's endpoints. This applies to both series and sketches.
fn add_metrics_output_pipelines_to_blueprint(
    blueprint: &mut TopologyBlueprint, shared: &SharedConfiguration, catalog: &Arc<RoutingTargetCatalog>,
    routing: &MetricsEndpointRoutingConfiguration, metrics_outputs_from: &[&str],
) -> Result<(), GenericError> {
    let mut dd_metrics_config = datadog_metrics_configuration(shared);
    if !routing.selected_endpoints().is_empty() {
        dd_metrics_config = dd_metrics_config.with_routing(Arc::clone(catalog), routing.unfiltered_targets(catalog));
    }
    blueprint
        .add_encoder("dd_metrics_encode", dd_metrics_config)?
        .connect_components("dd_metrics_encode", "dd_out")?;
    for from in metrics_outputs_from {
        blueprint.connect_components(from, "dd_metrics_encode")?;
    }

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
            .connect_components_in_order([filter_id.as_str(), encoder_id.as_str(), "dd_out"])?;
        for from in metrics_outputs_from {
            blueprint.connect_components(from, filter_id.as_str())?;
        }
    }

    Ok(())
}

fn add_autoscaling_failover_metrics_pipeline_to_blueprint(
    blueprint: &mut TopologyBlueprint, shared: &SharedConfiguration, metrics_outputs_from: &[&str],
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
        .connect_components_in_order(["af_metrics_gateway", "af_metrics_encode", "cluster_agent_out"])?;
    for from in metrics_outputs_from {
        blueprint.connect_components(from, "af_metrics_gateway")?;
    }

    Ok(())
}

/// The component that applies the logs processing rules.
const LOGS_PROCESS: &str = "logs_process";

/// Returns the component that check and OTLP logs enter the logs pipeline through.
///
/// Logs go through `logs_process` only when there are processing rules that apply to them, so that they don't pay for
/// an extra component otherwise. Invalid rules select `logs_process` too, though building the logs pipeline fails on
/// them first.
fn logs_pipeline_input(logs: &logs::Domain) -> &'static str {
    match logs.validate_processing_rules() {
        Ok(rules) if logs_processing_rules(&rules).is_empty() => "dd_logs_encode",
        _ => LOGS_PROCESS,
    }
}

async fn add_baseline_logs_pipeline_to_blueprint(
    blueprint: &mut TopologyBlueprint, shared: &SharedConfiguration, logs: &logs::Domain,
    env_provider: &RemoteAgentEnvironmentProvider,
) -> Result<(), GenericError> {
    // Like the Agent's logs agent, fail to start on invalid processing rules. Only the logs pipeline uses them, so the
    // other pipelines don't depend on them being valid.
    let processing_rules = logs_processing_rules(
        &logs
            .validate_processing_rules()
            .error_context("Invalid processing rules.")?,
    );

    // Create the back half of the logs processing pipeline.
    //
    // Like the Agent's logs processor, the encoder sends a log without a hostname with the Agent's hostname.
    let default_hostname = env_provider
        .host()
        .get_hostname()
        .await
        .error_context("Failed to get default hostname for logs.")?;
    let compression = &shared.endpoints.compression;
    // TODO(#3001): pass `shared.metrics_encoding.flush_timeout`; the default preserves existing behavior.
    let dd_logs_config = BufferedIncrementalConfiguration::new(
        DatadogLogsConfiguration::new(&compression.compressor_kind, compression.effective_zstd_level())
            .with_default_hostname(default_hostname),
        DEFAULT_ENCODER_FLUSH_TIMEOUT,
    );

    blueprint
        // Components.
        .add_encoder("dd_logs_encode", dd_logs_config)?
        // Logs.
        .connect_components("dd_logs_encode", "dd_out")?;

    if !processing_rules.is_empty() {
        let processing_rules = LogsProcessingRulesConfiguration::new(processing_rules);
        blueprint
            .add_transform(
                LOGS_PROCESS,
                ChainedConfiguration::default().with_transform_builder("processing_rules", processing_rules),
            )?
            .connect_components(LOGS_PROCESS, "dd_logs_encode")?;
    }

    Ok(())
}

fn add_baseline_events_pipeline_to_blueprint(
    blueprint: &mut TopologyBlueprint, shared: &SharedConfiguration,
) -> Result<(), GenericError> {
    let metrics_encoding = &shared.metrics_encoding;
    let compression = &shared.endpoints.compression;
    // TODO(#3001): pass `shared.metrics_encoding.flush_timeout`; the default preserves existing behavior.
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
    // TODO(#3001): pass `shared.metrics_encoding.flush_timeout`; the default preserves existing behavior.
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
    // The trace encoder and the stats transform look up container tags to fill in missing version data. Stats also
    // carry those tags as infra tags.
    let workload_provider = Arc::new(env_provider.workload().clone());
    let mut dd_traces_config = trace_configuration(
        &config.domains.traces,
        &config.domains.otlp.traces,
        &config.shared,
        agent_env.clone(),
    )
    .with_environment_provider(env_provider.clone())
    .await?;
    dd_traces_config.workload_provider = Some(workload_provider.clone());
    let trace_sampler_config = trace_sampler_configuration(
        &config.domains.traces,
        &config.domains.otlp.traces,
        agent_env.clone(),
        trace_sampling.unwrap_or_default(),
    );

    let trace_obfuscation_config = TraceObfuscationConfiguration::new(
        obfuscation_configuration(&config.domains.traces),
        config.domains.traces.obfuscation.sql.dbms.clone(),
    );
    let trace_tag_replacer_config = trace_tag_replacer_configuration(&config.domains.traces);

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
        .with_transform_builder("trace_obfuscation", trace_obfuscation_config)
        .with_transform_builder("trace_tag_replacer", trace_tag_replacer_config);
    let apm_onboarding_config =
        ChainedConfiguration::default().with_transform_builder("apm_onboarding", ApmOnboardingConfiguration);
    let mut apm_stats_transform_config =
        apm_stats_configuration(&config.domains.traces, agent_env.clone(), semantic_registry)
            .with_environment_provider(env_provider.clone())
            .await?;
    apm_stats_transform_config.workload_provider = Some(workload_provider);
    let dd_apm_stats_encoder =
        DatadogApmStatsEncoderConfiguration::new(config.shared.metrics_encoding.flush_timeout, agent_env)
            .with_environment_provider(env_provider.clone())
            .await?;

    blueprint
        .add_transform("traces_enrich", dd_traces_enrich_config)?
        .add_transform("trace_sampler", trace_sampler_config)?
        .add_transform("apm_onboarding", apm_onboarding_config)?
        .add_transform("dd_apm_stats", apm_stats_transform_config)?
        .add_encoder("dd_stats_encode", dd_apm_stats_encoder)?
        .add_encoder("dd_traces_encode", dd_traces_config)?
        // Onboarding runs after sampling, so a service is only recorded as seen once one of its traces is kept.
        .connect_components_in_order(["traces_enrich", "trace_sampler", "apm_onboarding"])?
        .connect_components("apm_onboarding", ["dd_apm_stats", "dd_traces_encode"])?
        .connect_components("dd_apm_stats", "dd_stats_encode")?
        .connect_components(["dd_traces_encode", "dd_stats_encode"], "dd_traces_out")?;

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
        queue_size: listeners.queue_size,
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
        set_timing_unit: listeners.set_timing_unit,
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
) -> Result<DogStatsDAPIWorker, GenericError> {
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
    let dsd_hist_config = HistogramConfiguration::new(
        &histogram.aggregates,
        &histogram.percentiles,
        histogram.copy_to_distribution,
        histogram.copy_to_distribution_prefix.clone(),
    );
    let (dsd_context_snapshot_handle, dsd_context_snapshot_receiver) = aggregate_context_snapshot_channel();
    let dsd_agg_config = AggregateConfiguration {
        window_duration_seconds: aggregation.window_duration_seconds,
        primary_flush_interval: aggregation.flush_interval,
        context_limit: aggregation.context_limit,
        flush_open_windows: aggregation.flush_open_windows,
        counter_expiry_seconds: aggregation.counter_expiry_seconds,
        hist_config: dsd_hist_config,
        origin_telemetry_enabled: typed.domains.dogstatsd.telemetry.origin_breakdown,
        context_snapshot_receiver: dsd_context_snapshot_receiver,
    };
    let dsd_no_agg_split_config = DogStatsDNoAggSplitConfiguration::new(aggregation.passthrough_idle_flush_timeout);
    let dsd_post_agg_filter_config = DogStatsDPostAggregateFilterConfiguration::new(
        config_system
            .live(|config| &config.domains.dogstatsd.metric_filter)
            .map(metric_name_filter),
        &histogram.aggregates,
        &histogram.percentiles,
    );
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
    let dsd_stats_collection_active = dsd_stats_config.collection_active();
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
        // DogStatsD Stats. Metrics are only used while a statistics collection is in progress, so only send them then.
        .connect_components_when(
            "dsd_in.metrics",
            "dsd_stats_out",
            EdgeCondition::from_watch(dsd_stats_collection_active),
        )?
        // Post-aggregation client telemetry for RAR/COAT.
        .connect_components(
            format!("dsd_post_agg_filter.{CLIENT_TELEMETRY_OUTPUT}"),
            "dsd_client_telemetry_out",
        )?;

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
        ])?;
    }

    // DogStatsD debug log. Metrics are only used while metrics stats logging is enabled, so only send them then. When
    // debug logging is disabled entirely, the connection can never be active, so the destination is never built.
    let debug_log_condition = if debug_log.logging_enabled {
        live_condition(&dsd_debug_log_config.metrics_stats_enabled, |enabled| *enabled)
    } else {
        EdgeCondition::fixed(false)
    };
    blueprint
        .add_destination("dsd_debug_log_out", dsd_debug_log_config)?
        .connect_components_when("dsd_in.metrics", "dsd_debug_log_out", debug_log_condition)?;
    Ok(DogStatsDAPIWorker {
        stats_api_handler,
        capture_api_handler,
        replay_api_handler,
        context_dump_api_handler,
    })
}

async fn add_apm_pipeline_to_blueprint(
    blueprint: &mut TopologyBlueprint, config_system: &ConfigurationSystem,
    env_provider: &RemoteAgentEnvironmentProvider, agent_client: Option<&RemoteAgentClientConfiguration>,
    semantic_registry: SemanticRegistryProvider,
) -> Result<(), GenericError> {
    let config = config_system.config();
    // In standalone mode there is no workload provider, so the resolver finds nothing and the relay forwards requests
    // without a container ID
    let workload = Arc::new(env_provider.workload().clone());
    let dd_traces_relay = apm_relay_configuration(
        &config,
        datadog_agent_config::is_containerized(),
        Some(workload.clone()),
        Some(workload),
        config_system.live(|config| &config.shared.endpoints.api_key),
        agent_client.cloned(),
        semantic_registry,
    );
    blueprint
        .add_relay("dd_traces_relay", dd_traces_relay)?
        // The relay has no output path of its own, but a topology can't hold a component with no connections, so it
        // connects to the traces forwarder, which is always created when APM is enabled.
        .connect_components("dd_traces_relay.traces", "dd_traces_out")?;
    Ok(())
}

async fn add_otlp_pipeline_to_blueprint(
    blueprint: &mut TopologyBlueprint, config_system: &ConfigurationSystem,
    env_provider: &RemoteAgentEnvironmentProvider, semantic_registry: SemanticRegistryProvider, otlp_plan: &OtlpPlan,
) -> Result<(), GenericError> {
    let config = config_system.config();
    let otlp = &config.domains.otlp;
    let routes = otlp_plan.routes();
    let proxy_destinations = if routes.any_proxied() {
        info!(
            metrics = ?routes.metrics,
            logs = ?routes.logs,
            traces = ?routes.traces,
            agent_receiver_grpc_endpoint = %otlp.proxy.grpc_endpoint,
            agent_receiver_http_endpoint = %otlp.proxy.http_endpoint,
            "Starting OTLP ingest. Proxied signals go to the Agent's OTLP receiver for the protocol they arrive on."
        );
        otlp_plan.proxy_destinations().cloned()
    } else {
        info!(metrics = ?routes.metrics, logs = ?routes.logs, traces = ?routes.traces, "Starting OTLP ingest.");
        None
    };
    let default_hostname = env_provider
        .host()
        .get_hostname()
        .await
        .error_context("Failed to get default hostname for OTLP source.")?;
    let static_tags = resolve_static_tags(
        &config.shared.static_tags,
        &config.shared.tags,
        features::is_ecs_fargate(),
    );
    let otlp_config = OtlpConfiguration {
        default_hostname: default_hostname.into(),
        receiver: otlp_receiver(&otlp.receiver),
        routes,
        proxy_destinations,
        metrics: otlp_metrics_settings(&otlp.metrics),
        traces: otlp_traces_settings(&otlp.traces, &config.domains.traces.features),
        contexts: otlp_context_settings(&otlp.contexts),
        debug_verbosity: otlp_debug_verbosity(&otlp.debug),
        max_resource_len: config.domains.traces.max_resource_len,
        semantic_registry,
        workload_provider: Arc::new(env_provider.workload().clone()),
    }
    .with_static_metric_tags(static_tags);

    blueprint.add_source("otlp_in", otlp_config)?;

    let edges = otlp_source_edges(routes, logs_pipeline_input(&config.domains.logs));
    if edges.iter().any(|(_, to)| *to == OTLP_BLACKHOLE) {
        blueprint.add_destination(OTLP_BLACKHOLE, BlackholeConfiguration)?;
    }
    for (from, to) in edges {
        blueprint.connect_components(from, to)?;
    }
    Ok(())
}

/// Destination that drops what the OTLP source sends it.
const OTLP_BLACKHOLE: &str = "otlp_out_blackhole";

/// Returns the OTLP source edges as `(output, component)` pairs, except for the internal metrics route.
///
/// Internal logs go to `logs_input`, the component that logs enter the logs pipeline through, and internal traces go
/// to the traces pipeline. The shared metrics pipeline connects the OTLP metrics output with the other producers. OTLP
/// metrics skip aggregation so their counters do not become rates. If no signal uses an internal pipeline, connect to
/// [`OTLP_BLACKHOLE`]. The topology requires a connected source.
fn otlp_source_edges(routes: OtlpRoutes, logs_input: &'static str) -> Vec<(&'static str, &'static str)> {
    let edges = [
        (routes.logs, "otlp_in.logs", logs_input),
        (routes.traces, "otlp_in.traces", "traces_enrich"),
    ]
    .into_iter()
    .filter(|(route, _, _)| *route == OtlpRoute::Internal)
    .map(|(_, from, to)| (from, to))
    .collect::<Vec<_>>();

    if edges.is_empty() && routes.metrics != OtlpRoute::Internal {
        vec![("otlp_in.metrics", OTLP_BLACKHOLE)]
    } else {
        edges
    }
}

/// Builds a connection condition from a live configuration value.
///
/// A value that never changes becomes a fixed condition, which the topology resolves when it's built.
fn live_condition<T>(live: &Live<T>, predicate: impl Fn(&T) -> bool + Send + 'static) -> EdgeCondition
where
    T: Clone + PartialEq + Send + 'static,
{
    let initial = predicate(live);
    if live.is_fixed() {
        return EdgeCondition::fixed(initial);
    }

    let updates = live.clone().into_stream().map(move |value| predicate(&value));
    EdgeCondition::dynamic(initial, updates)
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
    use tower::ServiceExt as _;

    use super::*;
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
        AggregateContextSnapshotEntry::for_test(Context::from_static_name(name), AggregateMetricType::Gauge)
    }

    #[test]
    fn otlp_source_edges_follow_the_routes() {
        use OtlpRoute::{Disabled, Internal, Proxied};

        let routes = |metrics, logs, traces| OtlpRoutes { metrics, logs, traces };
        let cases = [
            (
                "internal only",
                OtlpRoutes::ALL_INTERNAL,
                vec![("otlp_in.logs", "dd_logs_encode"), ("otlp_in.traces", "traces_enrich")],
            ),
            (
                "mixed",
                routes(Proxied, Disabled, Internal),
                vec![("otlp_in.traces", "traces_enrich")],
            ),
            (
                "metrics and logs internal",
                routes(Internal, Internal, Proxied),
                vec![("otlp_in.logs", "dd_logs_encode")],
            ),
            ("only metrics internal", routes(Internal, Proxied, Disabled), vec![]),
            (
                "all proxied",
                routes(Proxied, Proxied, Proxied),
                vec![("otlp_in.metrics", OTLP_BLACKHOLE)],
            ),
            (
                "proxied and disabled",
                routes(Proxied, Disabled, Proxied),
                vec![("otlp_in.metrics", OTLP_BLACKHOLE)],
            ),
        ];

        for (name, routes, expected) in cases {
            assert_eq!(otlp_source_edges(routes, "dd_logs_encode"), expected, "{name}");
        }
    }

    #[tokio::test]
    async fn otlp_source_edges_target_components_that_the_topology_builds() {
        use OtlpRoute::{Disabled, Internal, Proxied};

        let all = [Disabled, Internal, Proxied];
        for (metrics, logs, traces) in all.into_iter().flat_map(|m| {
            all.into_iter()
                .flat_map(move |l| all.into_iter().map(move |t| (m, l, t)))
        }) {
            for standalone_mode in [false, true] {
                let mut config = SalukiConfiguration::default();
                config.control.otlp = true;
                config.control.standalone_mode = standalone_mode;
                config.domains.otlp.receiver.grpc.endpoint = "localhost:4317".to_string();
                config.domains.otlp.proxy.grpc_endpoint = "127.0.0.1:4319".to_string();
                config.domains.otlp.receiver.metrics_enabled = metrics != Disabled;
                config.domains.otlp.receiver.logs_enabled = logs != Disabled;
                config.control.logs_enabled = logs != Disabled;
                config.domains.otlp.traces.enabled = traces != Disabled;
                config.domains.otlp.proxy.metrics_enabled = metrics == Proxied;
                config.domains.otlp.proxy.logs_enabled = logs == Proxied;
                config.domains.otlp.proxy.traces_enabled = traces == Proxied;
                let dp = DataPlaneConfiguration::from_configuration(&config);
                let otlp = OtlpPlan::resolve(&dp).await;

                let routes = OtlpRoutes { metrics, logs, traces };
                let row = format!("{routes:?}, standalone {standalone_mode}");
                assert_eq!(otlp.routes(), routes, "{row}");
                // Without a served signal, the topology doesn't add the OTLP source.
                if !otlp.enabled() {
                    continue;
                }
                if routes.metrics == Internal {
                    assert!(dp.metrics_pipeline_required(&otlp), "{row}: metrics output isn't built");
                }

                for (_, component) in otlp_source_edges(routes, logs_pipeline_input(&config.domains.logs)) {
                    let built = match component {
                        "dd_logs_encode" | LOGS_PROCESS => dp.logs_pipeline_required(&otlp),
                        "traces_enrich" => dp.traces_pipeline_required(&otlp),
                        // The OTLP pipeline adds the blackhole itself.
                        OTLP_BLACKHOLE => true,
                        _ => false,
                    };
                    assert!(built, "{row}: `{component}` isn't built");
                }
            }
        }
    }
}
