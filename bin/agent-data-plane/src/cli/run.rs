use std::path::PathBuf;
use std::{
    collections::HashSet,
    time::{Duration, Instant},
};

use agent_data_plane_config::{ComponentConfiguration, ControlConfiguration};
use agent_data_plane_config_system::{
    Attachments, DynamicConfigHandles, EnvConfig, LoadedConfigurationSystem, RemoteAgentRegistration,
};
use argh::FromArgs;
use datadog_agent_config::classifier::{ConfigClassifier, Pipeline, PipelineAffinity, Severity, SupportLevel};
use resource_accounting::{ComponentBounds, ComponentRegistry};
use saluki_app::{
    accounting::{initialize_memory_bounds, MemoryBoundsConfiguration},
    bootstrap::BootstrapGuard,
    metrics::emit_startup_metrics,
};
use saluki_components::{
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
use saluki_core::health::HealthRegistry;
use saluki_core::runtime::{RestartMode, RestartStrategy, Supervisor};
use saluki_core::topology::TopologyBlueprint;
use saluki_env::EnvironmentProvider as _;
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use tracing::{debug, error, info, trace, warn};

use crate::{
    components::{
        apm_onboarding::ApmOnboardingConfiguration,
        dogstatsd_post_aggregate_filter::DogStatsDPostAggregateFilterConfiguration,
        dogstatsd_prefix_filter::DogStatsDPrefixFilterConfiguration, host_tags::HostTagsConfiguration,
        ottl_filter_processor::OttlFilterConfiguration, ottl_transform_processor::OttlTransformConfiguration,
        tag_filterlist::TagFilterlistConfiguration,
    },
    internal::{
        create_internal_supervisor, env::ADPEnvironmentProvider, remote_agent::RemoteAgentServices,
        DogStatsDControlSurface, TopologyControlSurfaces,
    },
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
    started: Instant, loaded: LoadedConfigurationSystem, bootstrap_guard: &mut BootstrapGuard,
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

    // Build the remote-agent registration the config-system uses when connecting to the Agent. The
    // gRPC service names and the callback endpoint (the secure API listen address) are local
    // decisions, so they come from the typed bootstrap slice before runtime authority exists. In
    // local-snapshot mode the registration is unused.
    let secure_api_listen_address = loaded
        .bootstrap()
        .datadog
        .local_api()
        .secure_api_listen_address
        .clone()
        .unwrap_or_else(|| "tcp://0.0.0.0:5101".to_string());
    let registration = RemoteAgentRegistration {
        pid: std::process::id(),
        display_name: app_details.full_name().to_string(),
        flavor: app_details.full_name().replace(' ', "_").to_lowercase(),
        api_endpoint: secure_api_listen_address,
        service_names: RemoteAgentServices::service_names(),
    };

    // Start the runtime. In stream mode this connects to the Agent and awaits the first config
    // snapshot; in local-snapshot mode it translates the retained local snapshot. After this call the
    // raw bootstrap snapshot is gone -- only typed outputs remain.
    let mut started_config = loaded
        .start_runtime(registration)
        .await
        .error_context("Failed to start the configuration system runtime.")?;

    let saluki = started_config.saluki();
    let control = saluki.control.clone();
    let components = saluki.components.clone();
    let handles = started_config
        .dynamic_handles()
        .ok_or_else(|| generic_error!("Dynamic configuration handles were already taken."))?;
    let attachments = started_config.attachments();
    let env_config = started_config.env_config();
    let view_producer = started_config.view_producer();

    // Split the dynamic log-level handle off the bundle for the control plane's dynamic log-level
    // worker; the rest of the bundle is consumed by topology construction. `ScopedConfig` is
    // clone-able, so the split is a clone observing the same channel, and the original is replaced
    // with a fixed handle to keep the bundle whole for the topology.
    let log_level_handle = handles.log_level.clone();

    // Reload logging from the runtime configuration's log level once the authoritative config is
    // online (it may differ from the bootstrap-phase value).
    let runtime_log_level = log_level_handle.current();
    let logging_bootstrap = agent_data_plane_config::LoggingBootstrap {
        log_level: Some(runtime_log_level),
        ..Default::default()
    };
    match crate::internal::logging::LoggingConfigurationTranslator::translate(&logging_bootstrap) {
        Ok(logging_config) => {
            if let Err(e) = bootstrap_guard.logging_mut().reload(logging_config).await {
                warn!(error = %e, "Failed to reload logging from runtime configuration; continuing with bootstrap logging settings.");
            }
        }
        Err(e) => {
            warn!(error = %e, "Failed to translate runtime logging configuration; continuing with bootstrap logging settings.")
        }
    }

    if !control.standalone_mode && !control.enabled {
        info!("Agent Data Plane is not enabled. Exiting.");
        return Ok(());
    }

    let active_pipelines = active_pipelines(&control);
    check_and_warn_config(&env_config, &active_pipelines).error_context("Incompatible configuration detected.")?;

    // Set up all of the building blocks for building our topologies and launching internal processes.
    let component_registry = ComponentRegistry::default();
    let health_registry = HealthRegistry::new();
    let (env_provider, maybe_env_supervisor) = ADPEnvironmentProvider::from_typed(
        &env_config,
        &components.workload.config,
        &control,
        attachments.as_ref(),
        &component_registry,
        &health_registry,
    )
    .await?;

    // Create the blueprint for our primary topology.
    let (mut blueprint, control_surfaces) = create_topology(
        &control,
        &components,
        handles,
        &attachments,
        &env_config,
        &env_provider,
        &component_registry,
    )
    .await?;

    // Build the remote-agent gRPC services from the typed attachments, when connected to the Agent.
    let remote_agent_services = match attachments.as_ref() {
        Some(attachments) => Some(RemoteAgentServices::from_attachments(attachments).await),
        None => None,
    };

    // Create the internal supervisor which drives our control plane and internal observability.
    let mut internal_supervisor = create_internal_supervisor(
        &control,
        &env_config,
        &component_registry,
        health_registry.clone(),
        control_surfaces,
        remote_agent_services,
        bootstrap_guard.logging().controller(),
        log_level_handle,
        view_producer,
    )
    .await
    .error_context("Failed to create internal supervisor.")?;

    // Run memory bounds validation to ensure that we can launch the topology with our configured memory limit, if any.
    let bounds_config = MemoryBoundsConfiguration::try_from_config(env_config.raw())?;
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

    // Assemble the full supervision tree and run it.
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

    // Once the topology is healthy, log readiness and emit our startup metrics.
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

/// Returns the set of [`Pipeline`] variants that are active based on our configuration.
fn active_pipelines(control: &ControlConfiguration) -> HashSet<Pipeline> {
    let mut s = HashSet::new();
    if control.dogstatsd.enabled() {
        s.insert(Pipeline::DogStatsD);
    }
    if control.checks.enabled() {
        s.insert(Pipeline::Checks);
    }
    if control.otlp.enabled {
        s.insert(Pipeline::Otlp);
    }
    if control.traces_pipeline_required() {
        s.insert(Pipeline::Traces);
    }
    s
}

/// Check the resolved configuration against the config registry for incompatibilities.
///
/// Classifies each flattened key in the runtime source map with the config registry `Classifier`.
/// Returns an `Error` if one or more high severity incompatibility is discovered. Emits warnings for
/// less severe incompatibilities. Keys are only considered incompatible when they have non-default
/// values and the pipelines they affect are active.
fn check_and_warn_config(env_config: &EnvConfig, active_pipelines: &HashSet<Pipeline>) -> Result<(), GenericError> {
    let classifier = ConfigClassifier::new();
    let mut high_severity_incompatibilities = 0u32;
    debug!("Analyzing configuration.");
    for (key, val) in env_config
        .raw()
        .flattened_keys()
        .error_context("Unable to flatten configuration into a list of dot-separated keys.")?
    {
        // Get the classification. The classifier returns None if the config key is invalid or not-applicable to ADP.
        let Some(classification) = classifier.classify(&key, &val) else {
            continue;
        };

        // Ignore it if none of the affected pipelines are active.
        if !is_a_pipeline_affected(active_pipelines, &classification.pipeline_affinity) {
            continue;
        }

        // The Agent populates default values into the config, so we do not consider keys with default values.
        if classification.is_default {
            trace!(key = %key, "Configuration key has a default value.");
            continue;
        }

        match classification.support_level {
            SupportLevel::Incompatible(Severity::Low) => debug!("Low-severity incompatible key detected. Proceeding."),
            SupportLevel::Partial => {
                warn!(key = %key, "Partially supported configuration key. See documentation for details. Proceeding.")
            }
            SupportLevel::Incompatible(Severity::Medium) => {
                warn!(key = %key, "Unsupported configuration key. Proceeding.")
            }
            SupportLevel::Incompatible(Severity::High) => {
                error!(key = %key, "Unsupported configuration key with non-default value. ADP cannot run safely with \
                this setting.");
                high_severity_incompatibilities += 1;
            }
            SupportLevel::Ignored | SupportLevel::Unrecognized => {
                trace!(key = %key, "Configuration key not-applicable. Silently ignoring.")
            }
        }
    }

    if high_severity_incompatibilities > 0 {
        return Err(generic_error!(
            "{high_severity_incompatibilities} incompatible configuration detected. ADP cannot start. Review error \
            logs for details."
        ));
    }

    Ok(())
}

/// Returns `true` if at least one of the `active_pipelines` is affected based on `pipeline_affinity`.
fn is_a_pipeline_affected(active_pipelines: &HashSet<Pipeline>, pipeline_affinity: &PipelineAffinity) -> bool {
    match pipeline_affinity {
        PipelineAffinity::Pipelines(affected_pipelines) => {
            for affected_pipeline in *affected_pipelines {
                if active_pipelines.contains(affected_pipeline) {
                    return true;
                }
            }
            false
        }
        PipelineAffinity::CrossCutting => true,
    }
}

#[allow(clippy::too_many_arguments)]
async fn create_topology(
    control: &ControlConfiguration, components: &ComponentConfiguration, handles: DynamicConfigHandles,
    attachments: &Option<Attachments>, env_config: &EnvConfig, env_provider: &ADPEnvironmentProvider,
    component_registry: &ComponentRegistry,
) -> Result<(TopologyBlueprint, TopologyControlSurfaces), GenericError> {
    let mut blueprint = TopologyBlueprint::new("primary", component_registry);
    blueprint.with_shutdown_timeout(control.stop_timeout);

    let mut control_surfaces = TopologyControlSurfaces::default();

    // If no data pipelines are enabled, then there's nothing for us to do.
    if !control.data_pipelines_enabled() {
        return Err(generic_error!("No data pipelines are enabled. Exiting."));
    }

    // Create our baseline pipelines if necessary. The forwarder is created if any baseline pipeline
    // that emits to Datadog is required; it receives the dynamic forwarder handle (for API-key
    // refresh).
    if control.metrics_pipeline_required()
        || control.logs_pipeline_required()
        || control.events_pipeline_required()
        || control.service_checks_pipeline_required()
        || control.traces_pipeline_required()
    {
        let dd_forwarder_config = DatadogForwarderConfiguration::from_native(handles.forwarder.clone());
        blueprint.add_forwarder("dd_out", dd_forwarder_config)?;
    }

    if control.metrics_pipeline_required() {
        add_baseline_metrics_pipeline_to_blueprint(
            &mut blueprint,
            components,
            control,
            &handles,
            attachments,
            env_provider,
        )
        .await?;
    }

    if control.logs_pipeline_required() {
        add_baseline_logs_pipeline_to_blueprint(&mut blueprint, components).await?;
    }

    if control.events_pipeline_required() {
        add_baseline_events_pipeline_to_blueprint(&mut blueprint, components).await?;
    }

    if control.service_checks_pipeline_required() {
        add_baseline_service_checks_pipeline_to_blueprint(&mut blueprint, components).await?;
    }

    if control.traces_pipeline_required() {
        add_baseline_traces_pipeline_to_blueprint(&mut blueprint, components, env_config, env_provider).await?;
    }

    // Now we move on to our actual data pipelines.
    if control.checks.enabled() {
        add_checks_pipeline_to_blueprint(&mut blueprint, components).await?;
    }

    if control.dogstatsd.enabled() {
        let dsd_control_surface =
            add_dsd_pipeline_to_blueprint(&mut blueprint, components, &handles, env_provider).await?;
        control_surfaces.attach_dogstatsd(dsd_control_surface);
    }

    if control.otlp.enabled {
        add_otlp_pipeline_to_blueprint(&mut blueprint, components, control, env_provider)?;
    }

    Ok((blueprint, control_surfaces))
}

async fn add_checks_pipeline_to_blueprint(
    blueprint: &mut TopologyBlueprint, components: &ComponentConfiguration,
) -> Result<(), GenericError> {
    let checks_config = ChecksIPCConfiguration::from_native(components.checks.ipc.clone());

    blueprint
        .add_source("checks_ipc_in", checks_config)?
        .connect_components("checks_ipc_in.metrics", "metrics_enrich")?
        .connect_components("checks_ipc_in.logs", "dd_logs_encode")?
        .connect_components("checks_ipc_in.events", "dd_events_encode")?
        .connect_components("checks_ipc_in.service_checks", "dd_service_checks_encode")?;

    Ok(())
}

async fn add_baseline_metrics_pipeline_to_blueprint(
    blueprint: &mut TopologyBlueprint, components: &ComponentConfiguration, control: &ControlConfiguration,
    handles: &DynamicConfigHandles, attachments: &Option<Attachments>, env_provider: &ADPEnvironmentProvider,
) -> Result<(), GenericError> {
    // Create the back half of the metrics processing pipeline.
    let host_enrichment_config = HostEnrichmentConfiguration::from_environment_provider(env_provider.clone());
    let mut metrics_enrich_config =
        ChainedConfiguration::default().with_transform_builder("host_enrichment", host_enrichment_config);

    if !control.standalone_mode {
        // Host tags require the Agent connection; the host-tags attachment client is only present in
        // stream mode. In standalone/local mode there is no client, so host-tags enrichment is skipped.
        if let Some(attachments) = attachments.as_ref() {
            let host_tags_config = HostTagsConfiguration::from_native(
                attachments.host_tags.client(),
                components.workload.config.host_tags_expected_duration,
            );
            metrics_enrich_config = metrics_enrich_config.with_transform_builder("host_tags", host_tags_config);
        }
    }

    let dd_metrics_config = DatadogMetricsConfiguration::from_native(components.metrics.datadog_encoder.clone());

    blueprint
        .add_transform("metrics_enrich", metrics_enrich_config)?
        .add_encoder("dd_metrics_encode", dd_metrics_config)?
        .connect_components_in_order(["metrics_enrich", "dd_metrics_encode", "dd_out"])?;

    add_mrf_metrics_pipeline_to_blueprint(blueprint, components, handles)?;

    Ok(())
}

fn add_mrf_metrics_pipeline_to_blueprint(
    blueprint: &mut TopologyBlueprint, components: &ComponentConfiguration, handles: &DynamicConfigHandles,
) -> Result<(), GenericError> {
    let mrf = &components.metrics.multi_region_failover;

    // The MRF metrics branch is only wired when a static MRF endpoint override is configured: both an
    // API key and a resolved `dd_url` are required. (`site` alone does not yield a metrics URL here;
    // the operator must set `multi_region_failover.dd_url`.)
    let Some((mrf_dd_url, mrf_api_key)) = mrf_metrics_endpoint_override(mrf) else {
        if mrf.enabled {
            warn!(
                "Multi-Region Failover is enabled, but the MRF API key and endpoint are required for metrics \
                 forwarding. The MRF metrics branch will not be wired, and primary forwarding will continue. Restart \
                 ADP after configuring the static MRF endpoint settings."
            );
        }
        return Ok(());
    };

    let mrf_gateway_config = MrfMetricsGatewayConfiguration::from_native(handles.multi_region_failover.clone());
    let mrf_metrics_config = DatadogMetricsConfiguration::from_native(components.metrics.datadog_encoder.clone());

    let mrf_forwarder_config = DatadogForwarderConfiguration::from_native(handles.forwarder.clone())
        .with_endpoint_override(mrf_dd_url, mrf_api_key);

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

/// Returns the `(dd_url, api_key)` MRF metrics endpoint override if fully configured.
///
/// Requires both `multi_region_failover.api_key` and `multi_region_failover.dd_url` to be set. This
/// mirrors the previous behavior of requiring a resolved static endpoint before wiring the MRF
/// metrics branch.
fn mrf_metrics_endpoint_override(mrf: &saluki_component_config::forwarder::MrfConfig) -> Option<(String, String)> {
    match (mrf.dd_url.as_ref(), mrf.api_key.as_ref()) {
        (Some(dd_url), Some(api_key)) if !dd_url.is_empty() && !api_key.is_empty() => {
            Some((dd_url.clone(), api_key.clone()))
        }
        _ => None,
    }
}

async fn add_baseline_logs_pipeline_to_blueprint(
    blueprint: &mut TopologyBlueprint, components: &ComponentConfiguration,
) -> Result<(), GenericError> {
    let dd_logs_config = BufferedIncrementalConfiguration::from_encoder_builder(DatadogLogsConfiguration::from_native(
        components.logs.encoder.clone(),
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
        DatadogEventsConfiguration::from_native(components.events.encoder.clone()),
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
        DatadogServiceChecksConfiguration::from_native(components.service_checks.encoder.clone()),
    );

    blueprint
        .add_encoder("dd_service_checks_encode", dd_service_checks_config)?
        .connect_components("dd_service_checks_encode", "dd_out")?;

    Ok(())
}

async fn add_baseline_traces_pipeline_to_blueprint(
    blueprint: &mut TopologyBlueprint, components: &ComponentConfiguration, env_config: &EnvConfig,
    env_provider: &ADPEnvironmentProvider,
) -> Result<(), GenericError> {
    let dd_traces_config = DatadogTraceConfiguration::from_native(components.traces.encoder.clone())
        .with_environment_provider(env_provider.clone())
        .await?;
    let trace_obfuscation_config = TraceObfuscationConfiguration::from_native(&components.traces.obfuscation);
    let trace_sampler_config = TraceSamplerConfiguration::from_native(&components.traces.sampler);
    // The OTTL trace processors are Saluki-schema-only knobs with no home in `SalukiConfiguration`;
    // their typed config is read from the runtime source map via the confined `EnvConfig` accessor.
    let ottl_filter_config = OttlFilterConfiguration::from_native(
        env_config
            .get_typed("ottl_filter_config")
            .error_context("Failed to read `ottl_filter_config`.")?
            .unwrap_or_default(),
    );
    let ottl_transform_config = OttlTransformConfiguration::from_native(
        env_config
            .get_typed("ottl_transform_config")
            .error_context("Failed to read `ottl_transform_config`.")?
            .unwrap_or_default(),
    );
    let dd_traces_enrich_config = ChainedConfiguration::default()
        .with_transform_builder("ottl_filter", ottl_filter_config)
        .with_transform_builder("ottl_transform", ottl_transform_config)
        .with_transform_builder("apm_onboarding", ApmOnboardingConfiguration)
        .with_transform_builder("trace_obfuscation", trace_obfuscation_config)
        .with_transform_builder("trace_sampler", trace_sampler_config);
    let apm_stats_transform_config =
        ApmStatsTransformConfiguration::from_native(&components.metrics.apm_stats_transform)
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
        .with_workload_provider(env_provider.workload().clone())
        .with_capture_entity_resolver(env_provider.workload().clone());
    let dsd_prefix_filter_configuration =
        DogStatsDPrefixFilterConfiguration::from_native(handles.prefix_filter.clone());
    let dsd_mapper_config = DogStatsDMapperConfiguration::from_native(&components.dogstatsd.mapper);
    let dsd_enrich_config =
        ChainedConfiguration::default().with_transform_builder("dogstatsd_mapper", dsd_mapper_config);
    let dsd_tag_filterlist_config = TagFilterlistConfiguration::from_native(handles.tag_filterlist.clone());
    let dsd_agg_config = AggregateConfiguration::from_native(&components.dogstatsd.aggregate);
    let dsd_post_agg_filter_config =
        DogStatsDPostAggregateFilterConfiguration::from_native(handles.post_aggregate_filter.clone());
    let events_enrich_config = ChainedConfiguration::default().with_transform_builder(
        "host_enrichment",
        HostEnrichmentConfiguration::from_environment_provider(env_provider.clone()),
    );
    let service_checks_enrich_config = ChainedConfiguration::default().with_transform_builder(
        "host_enrichment",
        HostEnrichmentConfiguration::from_environment_provider(env_provider.clone()),
    );
    let dsd_debug_log_config = DogStatsDDebugLogConfiguration::from_native(handles.debug_log.clone());
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
    blueprint: &mut TopologyBlueprint, components: &ComponentConfiguration, control: &ControlConfiguration,
    env_provider: &ADPEnvironmentProvider,
) -> Result<(), GenericError> {
    if control.otlp.proxy.enabled {
        let core_agent_otlp_grpc_endpoint = control.otlp.proxy.core_agent_otlp_grpc_endpoint.clone();
        let proxy_metrics = control.otlp.proxy.proxy_metrics;
        let proxy_logs = control.otlp.proxy.proxy_logs;
        let proxy_traces = control.otlp.proxy.proxy_traces;

        info!(
            proxy_grpc_endpoint = %core_agent_otlp_grpc_endpoint,
            proxy_metrics,
            proxy_logs,
            proxy_traces,
            "OTLP proxy mode enabled. Select OTLP payloads will be proxied to the Core Agent."
        );

        let otlp_relay_config = OtlpRelayConfiguration::from_native(&components.otlp.relay);
        let otlp_decoder_config = OtlpDecoderConfiguration::from_native(&components.otlp.decoder);

        let local_agent_otlp_forwarder_config =
            OtlpForwarderConfiguration::from_native(&components.otlp.forwarder, core_agent_otlp_grpc_endpoint);

        blueprint
            .add_relay("otlp_relay_in", otlp_relay_config)?
            .add_forwarder("local_agent_otlp_out", local_agent_otlp_forwarder_config)?
            .connect_components(["otlp_relay_in.metrics", "otlp_relay_in.logs"], "local_agent_otlp_out")?;

        if control.otlp.proxy.proxy_traces {
            blueprint.connect_components("otlp_relay_in.traces", "local_agent_otlp_out")?;
        } else {
            blueprint
                .add_decoder("otlp_traces_decode", otlp_decoder_config)?
                .connect_components_in_order(["otlp_relay_in.traces", "otlp_traces_decode", "traces_enrich"])?;
        }
    } else {
        info!("OTLP proxy mode disabled. OTLP signals will be handled natively.");

        let otlp_config = OtlpConfiguration::from_native(&components.otlp.source)
            .with_workload_provider(env_provider.workload().clone());

        blueprint
            .add_source("otlp_in", otlp_config)?
            .connect_components("otlp_in.metrics", "metrics_enrich")?
            .connect_components("otlp_in.logs", "dd_logs_encode")?
            .connect_components("otlp_in.traces", "traces_enrich")?;
    }
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
