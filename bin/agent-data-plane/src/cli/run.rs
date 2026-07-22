use std::{
    collections::HashSet,
    env,
    path::PathBuf,
    time::{Duration, Instant},
};

use agent_data_plane_config_system::{ConfigurationSystem, EnvPrecedence, LoadedConfiguration};
use argh::FromArgs;
use datadog_agent_commons::{ipc::config::IpcAuthConfiguration, platform::PlatformSettings};
use datadog_agent_config::classifier::{ConfigClassifier, Pipeline, PipelineAffinity, Severity, SupportLevel};
use saluki_app::{
    accounting::{initialize_memory_bounds, MemoryBoundsConfiguration},
    bootstrap::BootstrapGuard,
    metrics::emit_startup_metrics,
};
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
        AggregateConfiguration, AggregateContextSnapshotHandle, ApmStatsTransformConfiguration,
        AutoscalingFailoverGatewayConfiguration, ChainedConfiguration, DogStatsDMapperConfiguration,
        HostEnrichmentConfiguration, MrfMetricsGatewayConfiguration, TraceObfuscationConfiguration,
        TraceSamplerConfiguration,
    },
};
use saluki_config::GenericConfiguration;
use saluki_core::accounting::{ComponentBounds, ComponentRegistry};
use saluki_core::health::HealthRegistry;
use saluki_core::runtime::{RestartMode, RestartStrategy, Supervisor, SupervisorError};
use saluki_core::topology::TopologyBlueprint;
use saluki_env::{EnvironmentProvider as _, HostProvider as _};
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
    dogstatsd_contexts::DogStatsDContextDumpAPIHandler,
    internal::{
        create_internal_supervisor, logging::LoggingConfigurationTranslator, remote_agent::RemoteAgentBootstrap,
        DogStatsDControlSurface, TopologyControlSurfaces,
    },
};
use crate::{config::DataPlaneConfiguration, internal::env::ADPEnvironmentProvider};

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
    started: Instant, config_path: PathBuf, bootstrap_guard: &mut BootstrapGuard, bootstrap_supervisor: Supervisor,
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

    // Load the local configuration sources (file + environment) once. The configuration system owns
    // the loader wiring; we supply the file path and the environment precedence. Environment
    // variables sit above the file so ADP-specific overrides (log level, etc.) win.
    let loaded = LoadedConfiguration::load(&config_path, EnvPrecedence::AfterFile)
        .await
        .error_context("Failed to load configuration.")?;

    // A static snapshot of the local sources, used only for decisions the Agent stream can never
    // change: whether we run standalone, and the remote-agent registration parameters.
    let bootstrap_dp_config = DataPlaneConfiguration::from_configuration(&loaded.raw_config())
        .error_context("Failed to load data plane configuration.")?;
    let standalone = bootstrap_dp_config.standalone_mode();

    // Resolve the authoritative configuration. Connected mode registers as a remote agent and takes
    // the Core Agent's config stream as authority; standalone mode treats the local sources as
    // authoritative. Both terminals block until the first configuration is received, deserialized,
    // and translated, failing the boot if it cannot be -- the strict startup gate.
    let (config_sys, ra_bootstrap) = if standalone {
        let config_sys = loaded
            .standalone()
            .await
            .error_context("Failed to load configuration.")?;
        (config_sys, None)
    } else {
        // Blocks until the Core Agent acknowledges registration.
        let ra_bootstrap = RemoteAgentBootstrap::from_configuration(&loaded.raw_config(), &bootstrap_dp_config)
            .await
            .error_context("Failed to bootstrap remote agent state.")?;

        // The configuration system owns the config stream: it reads `ConfigUpdate`s directly to
        // build the typed model and forwards them to the legacy compat map. `create_config_stream`
        // stays on `ra_bootstrap`, which we keep to build the status/flare/telemetry services below.
        let config_stream = ra_bootstrap.create_config_stream();

        info!("Waiting for initial configuration from Datadog Agent...");
        let config_sys = loaded
            .run(config_stream)
            .await
            .error_context("Failed to load initial configuration from the Datadog Agent.")?;
        info!("Initial configuration received.");

        (config_sys, Some(ra_bootstrap))
    };

    // Hand un-migrated components the resolved configuration as the legacy `GenericConfiguration`
    // they still read from. Raw reads go through `config`; typed/live reads go through `config_sys`.
    let dp_config = DataPlaneConfiguration::from_configuration(&config_sys.raw_map())
        .error_context("Failed to load data plane configuration.")?;

    // Connected mode may have replaced the bootstrap-phase settings with the Agent's authoritative
    // config, so reload logging to match. Standalone resolves the same local sources seen at
    // bootstrap, making a reload redundant.
    if !standalone {
        match LoggingConfigurationTranslator::translate(&config_sys.raw_map()) {
            Ok(logging_config) => {
                if let Err(e) = bootstrap_guard.logging_mut().reload(logging_config).await {
                    warn!(
                        error = %e,
                        "Failed to reload logging from Agent configuration; continuing with bootstrap logging settings."
                    );
                }
            }
            Err(e) => warn!(
                error = %e,
                "Failed to translate logging configuration from Agent; continuing with bootstrap logging settings."
            ),
        }
    }

    if !standalone && !dp_config.enabled() {
        info!("Agent Data Plane is not enabled. Exiting.");
        return Ok(());
    }

    let active_pipelines = active_pipelines(&dp_config);
    check_and_warn_config(&config_sys, &active_pipelines).error_context("Incompatible configuration detected.")?;

    // Set up all of the building blocks for building our topologies and launching internal processes.
    let component_registry = ComponentRegistry::default();
    let health_registry = HealthRegistry::new();
    let (env_provider, maybe_env_supervisor) =
        ADPEnvironmentProvider::from_configuration(&config_sys, &dp_config, &component_registry, &health_registry)
            .await?;

    // Create the blueprint for our primary topology.
    let (mut blueprint, control_surfaces) =
        create_topology(&config_sys, &dp_config, &env_provider, &component_registry).await?;

    // Create the internal supervisor which drives our control plane and internal observability.
    let mut internal_supervisor = create_internal_supervisor(
        &config_sys,
        &dp_config,
        &component_registry,
        health_registry.clone(),
        control_surfaces,
        ra_bootstrap,
        bootstrap_guard.logging().controller(),
        config_sys.current_handle(),
    )
    .await
    .error_context("Failed to create internal supervisor.")?;

    // Run memory bounds validation to ensure that we can launch the topology with our configured memory limit, if any.
    let bounds_config = MemoryBoundsConfiguration::try_from_config(&config_sys.raw_map())?;
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
    //
    // We run our internal supervisor (control plane, environment provider, etc) and our topology supervisor
    // side-by-side, which means everyone has access to the same dataspace. This is crucial for allowing processes
    // in the topology supervisor to (eventually) interact with components in the control plane, and vise versa.
    blueprint
        .with_health_registry(health_registry.clone())
        .with_memory_limiter(memory_limiter)
        .with_environment_readiness(env_provider.wait_for_ready());

    // Acquire a readiness handle before handing the blueprint off to the supervisor. This waits until the topology has
    // registered its components in the health registry and they've all reported ready, rather than racing the supervisor
    // and potentially observing an empty/already-ready registry before the topology's components even exist.
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
        // If the topology is torn down before it ever becomes ready (for example, shutdown during startup), `wait`
        // returns `false` and we skip reporting readiness.
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
        // The shutdown ran to completion, but one or more components ignored graceful shutdown and had to be forcefully
        // aborted after exceeding the shutdown timeout. Surface that plainly instead of reporting success -- but don't
        // treat it as a fatal error: the process did stop, and mapping it through the error path would mislabel it as a
        // boot/setup failure (and trip the boot assertion in `run_inner`).
        Err(SupervisorError::ShutdownTimedOut { aborted }) => {
            warn!(
                aborted,
                "Agent Data Plane shut down uncleanly: {aborted} component(s) had to be forcefully stopped after \
                 exceeding the shutdown timeout. Check the preceding warnings for which components were affected."
            );
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
fn active_pipelines(dp_config: &DataPlaneConfiguration) -> HashSet<Pipeline> {
    let mut s = HashSet::new();
    if dp_config.dogstatsd().enabled() {
        s.insert(Pipeline::DogStatsD);
    }
    if dp_config.checks().enabled() {
        s.insert(Pipeline::Checks);
    }
    if dp_config.otlp().enabled() {
        s.insert(Pipeline::Otlp);
    }
    if dp_config.traces_pipeline_required() {
        s.insert(Pipeline::Traces);
    }
    s
}

/// Check the resolved configuration against the config registry for incompatibilities.
///
/// Classifies each flattened key in `config` with the config registry `Classifier`. Returns an
/// `Error` if one or more high severity incompatibility is discovered. Emits warnings for less
/// severe incompatibilities. Keys are only considered incompatible when they have non-default
/// values and the pipelines they affect are active.
///
/// # Input
///
/// - `config`: the state of our configuration which we will flatten and consider all keys from.
/// - `active_pipelines`: the list of pipelines that are enabled based on the configuration.
///
/// # Error
///
/// To provide a better debugging experience in the presence of multiple high-severity incompatible
/// keys, all keys are checked before returning. The error reports the count of incompatible keys;
/// individual keys are logged at error level during iteration.
///
fn check_and_warn_config(
    config: &ConfigurationSystem, active_pipelines: &HashSet<Pipeline>,
) -> Result<(), GenericError> {
    let classifier = ConfigClassifier::new();
    let mut high_severity_incompatibilities = 0u32;
    debug!("Analyzing configuration.");
    // TODO: transfer this functionality to the config system
    let config = config.raw_map();
    for (key, val) in config
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
                    // We found an active pipeline that is in the affected list. Early return true.
                    return true;
                }
            }
            // We checked all affected pipelines against those that are active and none matched.
            false
        }
        PipelineAffinity::CrossCutting => true,
    }
}

async fn create_topology(
    config_system: &ConfigurationSystem, dp_config: &DataPlaneConfiguration, env_provider: &ADPEnvironmentProvider,
    component_registry: &ComponentRegistry,
) -> Result<(TopologyBlueprint, TopologyControlSurfaces), GenericError> {
    let mut blueprint = TopologyBlueprint::new("primary", component_registry);
    blueprint.with_shutdown_timeout(dp_config.stop_timeout());

    let mut control_surfaces = TopologyControlSurfaces::default();

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
    if dp_config.metrics_pipeline_required()
        || dp_config.logs_pipeline_required()
        || dp_config.events_pipeline_required()
        || dp_config.service_checks_pipeline_required()
        || dp_config.traces_pipeline_required()
    {
        let dd_forwarder_config = DatadogForwarderConfiguration::from_configuration(&config_system.raw_map())
            .error_context("Failed to configure Datadog forwarder.")?;
        blueprint.add_forwarder("dd_out", dd_forwarder_config)?;
    }

    if dp_config.metrics_pipeline_required() {
        add_baseline_metrics_pipeline_to_blueprint(&mut blueprint, &config_system.raw_map(), dp_config, env_provider)
            .await?;
    }

    if dp_config.logs_pipeline_required() {
        add_baseline_logs_pipeline_to_blueprint(&mut blueprint, &config_system.raw_map()).await?;
    }

    if dp_config.events_pipeline_required() {
        add_baseline_events_pipeline_to_blueprint(&mut blueprint, &config_system.raw_map()).await?;
    }

    if dp_config.service_checks_pipeline_required() {
        add_baseline_service_checks_pipeline_to_blueprint(&mut blueprint, &config_system.raw_map()).await?;
    }

    if dp_config.traces_pipeline_required() {
        add_baseline_traces_pipeline_to_blueprint(&mut blueprint, &config_system.raw_map(), env_provider).await?;
    }

    // Now we move on to our actual data pipelines.
    if dp_config.checks().enabled() {
        add_checks_pipeline_to_blueprint(&mut blueprint, &config_system.raw_map(), env_provider).await?;
    }

    if dp_config.dogstatsd().enabled() {
        let dsd_control_surface =
            add_dsd_pipeline_to_blueprint(&mut blueprint, &config_system.raw_map(), config_system, env_provider)
                .await?;
        control_surfaces.attach_dogstatsd(dsd_control_surface);
    }

    if dp_config.otlp().enabled() {
        add_otlp_pipeline_to_blueprint(&mut blueprint, &config_system.raw_map(), dp_config, env_provider).await?;
    }

    Ok((blueprint, control_surfaces))
}

async fn add_checks_pipeline_to_blueprint(
    blueprint: &mut TopologyBlueprint, config: &GenericConfiguration, env_provider: &ADPEnvironmentProvider,
) -> Result<(), GenericError> {
    let default_hostname = env_provider
        .host()
        .get_hostname()
        .await
        .error_context("Failed to get default hostname for Checks IPC source.")?;
    let checks_config = ChecksIPCConfiguration::from_configuration(config)?.with_default_hostname(default_hostname);

    blueprint
        .add_source("checks_ipc_in", checks_config)?
        .connect_components("checks_ipc_in.metrics", "metrics_enrich")?
        .connect_components("checks_ipc_in.logs", "dd_logs_encode")?
        .connect_components("checks_ipc_in.events", "dd_events_encode")?
        .connect_components("checks_ipc_in.service_checks", "dd_service_checks_encode")?;

    Ok(())
}

async fn add_baseline_metrics_pipeline_to_blueprint(
    blueprint: &mut TopologyBlueprint, config: &GenericConfiguration, dp_config: &DataPlaneConfiguration,
    env_provider: &ADPEnvironmentProvider,
) -> Result<(), GenericError> {
    // Create the back half of the metrics processing pipeline.
    let host_enrichment_config = HostEnrichmentConfiguration::from_environment_provider(env_provider.clone());
    let mut metrics_enrich_config =
        ChainedConfiguration::default().with_transform_builder("host_enrichment", host_enrichment_config);

    if !dp_config.standalone_mode() {
        let host_tags_config = HostTagsConfiguration::from_configuration(config)?;
        if host_tags_config.enabled() {
            metrics_enrich_config = metrics_enrich_config.with_transform_builder("host_tags", host_tags_config);
        }
    }

    let dd_metrics_config = DatadogMetricsConfiguration::from_configuration(config)
        .error_context("Failed to configure Datadog Metrics encoder.")?;

    blueprint
        // Components.
        .add_transform("metrics_enrich", metrics_enrich_config)?
        .add_encoder("dd_metrics_encode", dd_metrics_config)?
        // Metrics, then forwarding.
        .connect_components_in_order(["metrics_enrich", "dd_metrics_encode", "dd_out"])?;

    add_mrf_metrics_pipeline_to_blueprint(blueprint, config)?;
    add_autoscaling_failover_metrics_pipeline_to_blueprint(blueprint, config)?;

    Ok(())
}

fn add_mrf_metrics_pipeline_to_blueprint(
    blueprint: &mut TopologyBlueprint, config: &GenericConfiguration,
) -> Result<(), GenericError> {
    let mrf_config = MrfConfiguration::from_configuration(config)
        .error_context("Failed to configure Multi-Region Failover metrics pipeline.")?;

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

    let mrf_gateway_config = MrfMetricsGatewayConfiguration::new(mrf_config.clone(), config.clone());
    let mrf_metrics_config = DatadogMetricsConfiguration::from_configuration(config)
        .error_context("Failed to configure Multi-Region Failover Datadog Metrics encoder.")?;

    let mrf_forwarder_config = DatadogForwarderConfiguration::from_configuration(config)
        .map(|config| {
            config.with_endpoint_override_and_api_key_refresh_config_path(
                mrf_dd_url,
                mrf_api_key,
                "multi_region_failover.api_key",
            )
        })
        .error_context("Failed to configure Multi-Region Failover Datadog forwarder.")?;

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
        .connect_components("dd_logs_encode", "dd_out")?;

    Ok(())
}

async fn add_baseline_events_pipeline_to_blueprint(
    blueprint: &mut TopologyBlueprint, config: &GenericConfiguration,
) -> Result<(), GenericError> {
    let dd_events_config = DatadogEventsConfiguration::from_configuration(config)
        .map(BufferedIncrementalConfiguration::from_encoder_builder)
        .error_context("Failed to configure Datadog Events encoder.")?;

    blueprint
        .add_encoder("dd_events_encode", dd_events_config)?
        .connect_components("dd_events_encode", "dd_out")?;

    Ok(())
}

async fn add_baseline_service_checks_pipeline_to_blueprint(
    blueprint: &mut TopologyBlueprint, config: &GenericConfiguration,
) -> Result<(), GenericError> {
    let dd_service_checks_config = DatadogServiceChecksConfiguration::from_configuration(config)
        .map(BufferedIncrementalConfiguration::from_encoder_builder)
        .error_context("Failed to configure Datadog Service Checks encoder.")?;

    blueprint
        .add_encoder("dd_service_checks_encode", dd_service_checks_config)?
        .connect_components("dd_service_checks_encode", "dd_out")?;

    Ok(())
}

async fn add_baseline_traces_pipeline_to_blueprint(
    blueprint: &mut TopologyBlueprint, config: &GenericConfiguration, env_provider: &ADPEnvironmentProvider,
) -> Result<(), GenericError> {
    let dd_traces_config = DatadogTraceConfiguration::from_configuration(config)
        .error_context("Failed to configure Datadog Traces encoder.")?
        .with_environment_provider(env_provider.clone())
        .await?;
    let trace_obfuscation_config = TraceObfuscationConfiguration::from_apm_configuration(config)?;
    let trace_sampler_config = TraceSamplerConfiguration::from_configuration(config)
        .error_context("Failed to configure Trace Sampler transform.")?;
    let ottl_filter_config = OttlFilterConfiguration::from_configuration(config)
        .error_context("Failed to configure OTTL filter processor.")?;
    let ottl_transform_config = OttlTransformConfiguration::from_configuration(config)
        .error_context("Failed to configure OTTL transform processor.")?;
    let dd_traces_enrich_config = ChainedConfiguration::default()
        .with_transform_builder("ottl_filter", ottl_filter_config)
        .with_transform_builder("ottl_transform", ottl_transform_config)
        .with_transform_builder("apm_onboarding", ApmOnboardingConfiguration)
        .with_transform_builder("trace_obfuscation", trace_obfuscation_config)
        .with_transform_builder("trace_sampler", trace_sampler_config);
    let apm_stats_transform_config = ApmStatsTransformConfiguration::from_configuration(config)
        .error_context("Failed to configure APM Stats transform.")?
        .with_environment_provider(env_provider.clone())
        .await?;
    let dd_apm_stats_encoder = DatadogApmStatsEncoderConfiguration::from_configuration(config)
        .error_context("Failed to configure Datadog APM Stats encoder.")?
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

async fn build_dogstatsd_context_dump_api_handler(
    config: &GenericConfiguration, snapshot_handle: AggregateContextSnapshotHandle,
) -> Result<DogStatsDContextDumpAPIHandler, GenericError> {
    let ipc_config = IpcAuthConfiguration::from_configuration(config)?;
    let auth_token_path = ipc_config.auth_token_file_path();
    let auth_token = tokio::fs::read(&auth_token_path).await.map_err(|error| {
        generic_error!(
            "Failed to read Agent authentication token from file '{}' ({}).",
            auth_token_path.display(),
            error.kind()
        )
    })?;
    let run_path = config
        .try_get_typed::<PathBuf>("run_path")
        .error_context("Failed to read configured `run_path` for DogStatsD context dumps.")?
        .unwrap_or_default();

    DogStatsDContextDumpAPIHandler::new(auth_token, vec![snapshot_handle], run_path)
        .error_context("Failed to configure DogStatsD context dump API handler.")
}

async fn add_dsd_pipeline_to_blueprint(
    blueprint: &mut TopologyBlueprint,
    config: &GenericConfiguration,
    // Threaded in ready for the typed-config component cutover (aggregate/debug-log); unused until
    // those components read from it, hence the leading underscore.
    _config_system: &ConfigurationSystem,
    env_provider: &ADPEnvironmentProvider,
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
    let dsd_config = DogStatsDConfiguration::from_configuration(config)
        .error_context("Failed to configure DogStatsD source.")?
        .with_default_hostname(default_hostname)
        .with_workload_provider(env_provider.workload().clone())
        .with_capture_entity_resolver(env_provider.workload().clone());
    let dsd_prefix_filter_configuration = DogStatsDPrefixFilterConfiguration::from_configuration(config)?;
    let dsd_mapper_config = DogStatsDMapperConfiguration::from_configuration(config)?;
    let dsd_enrich_config =
        ChainedConfiguration::default().with_transform_builder("dogstatsd_mapper", dsd_mapper_config);
    let dsd_tag_filterlist_config = TagFilterlistConfiguration::from_configuration(config)
        .error_context("Failed to configure metric tag filterlist transform.")?;
    let dsd_agg_config =
        AggregateConfiguration::from_configuration(config).error_context("Failed to configure aggregate transform.")?;
    let dsd_context_snapshot_handle = dsd_agg_config.context_snapshot_handle();
    let dsd_post_agg_filter_config = DogStatsDPostAggregateFilterConfiguration::from_configuration(config)
        .error_context("Failed to configure DogStatsD post-aggregate filter transform.")?;
    let events_enrich_config = ChainedConfiguration::default().with_transform_builder(
        "host_enrichment",
        HostEnrichmentConfiguration::from_environment_provider(env_provider.clone()),
    );
    let service_checks_enrich_config = ChainedConfiguration::default().with_transform_builder(
        "host_enrichment",
        HostEnrichmentConfiguration::from_environment_provider(env_provider.clone()),
    );
    let dsd_debug_log_config = DogStatsDDebugLogConfiguration::from_configuration(
        config,
        PlatformSettings::get_default_dogstatsd_log_file_path(),
    )
    .error_context("Failed to configure DogStatsD debug log destination.")?;
    let dsd_stats_config = DogStatsDStatisticsConfiguration::new();

    let stats_api_handler = dsd_stats_config.api_handler();
    let capture_api_handler = dsd_config.capture_api_handler();
    let replay_api_handler = dsd_config.replay_api_handler();
    let context_dump_api_handler =
        build_dogstatsd_context_dump_api_handler(config, dsd_context_snapshot_handle).await?;

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
        // Metrics.
        .connect_components_in_order([
            "dsd_in.metrics",
            "dsd_enrich",
            "dsd_prefix_filter",
            "dsd_tag_filterlist",
            "dsd_agg",
            "dsd_post_agg_filter",
            "metrics_enrich",
        ])?
        // Events.
        .connect_components_in_order(["dsd_in.events", "events_enrich", "dd_events_encode"])?
        // Service checks.
        .connect_components_in_order([
            "dsd_in.service_checks",
            "service_checks_enrich",
            "dd_service_checks_encode",
        ])?
        // DogStatsD Stats.
        .connect_components("dsd_in.metrics", "dsd_stats_out")?;

    if dsd_debug_log_config.enabled() {
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

async fn add_otlp_pipeline_to_blueprint(
    blueprint: &mut TopologyBlueprint, config: &GenericConfiguration, dp_config: &DataPlaneConfiguration,
    env_provider: &ADPEnvironmentProvider,
) -> Result<(), GenericError> {
    if dp_config.otlp().proxy().enabled() {
        let core_agent_otlp_grpc_endpoint = dp_config.otlp().proxy().core_agent_otlp_grpc_endpoint().to_string();
        let proxy_metrics = dp_config.otlp().proxy().proxy_metrics();
        let proxy_logs = dp_config.otlp().proxy().proxy_logs();
        let proxy_traces = dp_config.otlp().proxy().proxy_traces();

        info!(
            proxy_grpc_endpoint = %core_agent_otlp_grpc_endpoint,
            proxy_metrics,
            proxy_logs,
            proxy_traces,
            "OTLP proxy mode enabled. Select OTLP payloads will be proxied to the Core Agent."
        );

        let otlp_relay_config = OtlpRelayConfiguration::from_configuration(config)?;
        let otlp_decoder_config = OtlpDecoderConfiguration::from_configuration(config)?;

        let local_agent_otlp_forwarder_config =
            OtlpForwarderConfiguration::from_configuration(config, core_agent_otlp_grpc_endpoint)?;

        blueprint
            // Components.
            .add_relay("otlp_relay_in", otlp_relay_config)?
            .add_forwarder("local_agent_otlp_out", local_agent_otlp_forwarder_config)?
            // Metrics and logs directly to the forwarders.
            .connect_components(["otlp_relay_in.metrics", "otlp_relay_in.logs"], "local_agent_otlp_out")?;

        if dp_config.otlp().proxy().proxy_traces() {
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
        let mut otlp_config = OtlpConfiguration::from_configuration(config)?
            .with_default_hostname(default_hostname)
            .with_workload_provider(env_provider.workload().clone());
        if let Ok(grpc_endpoint) = env::var("DD_DATA_PLANE_OTLP_RECEIVER_PROTOCOLS_GRPC_ENDPOINT") {
            otlp_config = otlp_config.with_grpc_endpoint(grpc_endpoint);
        }
        if let Ok(http_endpoint) = env::var("DD_DATA_PLANE_OTLP_RECEIVER_PROTOCOLS_HTTP_ENDPOINT") {
            otlp_config = otlp_config.with_http_endpoint(http_endpoint);
        }

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

#[cfg(test)]
mod tests {
    use std::{fs, path::Path, sync::Mutex, time::Duration};

    use async_trait::async_trait;
    use http::{header::AUTHORIZATION, Request, StatusCode};
    use http_body_util::{BodyExt as _, Empty};
    use hyper::body::Bytes;
    use saluki_api::{response::Response, APIHandler as _};
    use saluki_components::transforms::{
        aggregate_context_snapshot_channel_for_test, AggregateConfiguration, AggregateContextSnapshotEntry,
        AggregateMetricType, ChainedConfiguration, DogStatsDMapperConfiguration,
    };
    use saluki_config::{config_from, GenericConfiguration};
    use saluki_context::Context;
    use saluki_core::{
        accounting::{ComponentRegistry, MemoryBounds, MemoryBoundsBuilder, MemoryLimiter},
        components::{
            destinations::{Destination, DestinationBuilder, DestinationContext},
            sources::{Source, SourceBuilder, SourceContext},
            ComponentContext,
        },
        data_model::event::{metric::Metric, Event, EventType},
        health::HealthRegistry,
        runtime::Supervisor,
        topology::{OutputDefinition, TopologyBlueprint},
    };
    use saluki_error::{generic_error, GenericError};
    use serde_json::json;
    use stringtheory::MetaString;
    use tokio::sync::{mpsc, oneshot};
    use tower::ServiceExt as _;

    use super::build_dogstatsd_context_dump_api_handler;
    use crate::{
        components::{
            dogstatsd_prefix_filter::DogStatsDPrefixFilterConfiguration, tag_filterlist::TagFilterlistConfiguration,
        },
        dogstatsd_contexts::DogStatsDContextDumpAPIHandler,
    };

    const AUTH_TOKEN: &[u8] = b"configured-agent-token";
    const CONTEXT_DUMP_FILENAME: &str = "dogstatsd_contexts.json.zstd";
    const CONTEXT_DUMP_ROUTE: &str = "/dogstatsd/contexts/dump";

    #[tokio::test]
    async fn retained_context_identity_follows_dogstatsd_post_processing() {
        tokio::time::timeout(Duration::from_secs(5), async {
            let config = config_from(json!({
                "dogstatsd_mapper_profiles": [{
                    "name": "retained-context-test",
                    "prefix": "raw.requests.",
                    "mappings": [{
                        "match": "raw.requests.*",
                        "name": "mapped.requests",
                        "tags": { "route": "$1" }
                    }]
                }],
                "statsd_metric_namespace": "tenant",
                "statsd_metric_namespace_blocklist": [],
                "metric_filterlist": ["tenant.raw.blocked"],
                "metric_filterlist_match_prefix": false,
                "metric_tag_filterlist": [{
                    "metric_name": "tenant.mapped.requests",
                    "action": "exclude",
                    "tags": ["remove"]
                }],
                "aggregate_flush_interval": { "secs": 60, "nanos": 0 }
            }))
            .await;

            let mapper =
                DogStatsDMapperConfiguration::from_configuration(&config).expect("mapper configuration should parse");
            let mapper_chain = ChainedConfiguration::default().with_transform_builder("dogstatsd_mapper", mapper);
            let prefix_filter = DogStatsDPrefixFilterConfiguration::from_configuration(&config)
                .expect("prefix filter configuration should parse");
            let tag_filter =
                TagFilterlistConfiguration::from_configuration(&config).expect("tag filter configuration should parse");
            let aggregate =
                AggregateConfiguration::from_configuration(&config).expect("aggregate configuration should parse");
            let snapshot_handle = aggregate.context_snapshot_handle();

            let (events_tx, events_rx) = mpsc::channel(2);
            let source = ControlledMetricSourceBuilder {
                events: Mutex::new(Some(events_rx)),
                outputs: vec![OutputDefinition::default_output(EventType::Metric)],
            };
            let component_registry = ComponentRegistry::default();
            let mut blueprint = TopologyBlueprint::new("dogstatsd_retained_identity", &component_registry);
            blueprint
                .add_source("source", source)
                .expect("controlled source should be accepted")
                .add_transform("mapper", mapper_chain)
                .expect("mapper should be accepted")
                .add_transform("prefix_filter", prefix_filter)
                .expect("prefix filter should be accepted")
                .add_transform("tag_filter", tag_filter)
                .expect("tag filter should be accepted")
                .add_transform("aggregate", aggregate)
                .expect("aggregate should be accepted")
                .add_destination("destination", DrainingMetricDestinationBuilder)
                .expect("draining destination should be accepted");
            blueprint
                .connect_components_in_order([
                    "source",
                    "mapper",
                    "prefix_filter",
                    "tag_filter",
                    "aggregate",
                    "destination",
                ])
                .expect("DogStatsD post-processing topology should connect");
            blueprint
                .with_health_registry(HealthRegistry::new())
                .with_memory_limiter(MemoryLimiter::noop())
                .with_ambient_worker_pool();

            let mut supervisor =
                Supervisor::new("dogstatsd-retained-identity").expect("test supervisor should be created");
            supervisor.add_worker(blueprint);
            let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
            let topology_task = tokio::spawn(async move { supervisor.run_with_shutdown(shutdown_rx).await });

            events_tx
                .send(Event::Metric(Metric::counter("raw.blocked", 1.0)))
                .await
                .expect("controlled source should accept the blocked metric");
            let input_context = Context::from_static_parts("raw.requests.checkout", &["keep:client", "remove:secret"]);
            events_tx
                .send(Event::Metric(Metric::counter(input_context.clone(), 1.0)))
                .await
                .expect("controlled source should accept the retained metric");

            let expected_context =
                Context::from_static_parts("tenant.mapped.requests", &["keep:client", "route:checkout"]);
            let snapshot = loop {
                let snapshot = snapshot_handle
                    .snapshot()
                    .await
                    .expect("running aggregate should fulfill snapshots");
                if snapshot.iter().any(|entry| entry.context() == &expected_context) {
                    break snapshot;
                }
                tokio::task::yield_now().await;
            };

            assert_eq!(snapshot.len(), 1);
            assert_eq!(snapshot[0].context(), &expected_context);
            assert_eq!(snapshot[0].metric_type(), AggregateMetricType::Counter);
            assert_eq!(snapshot[0].context().tags().len(), 2);
            assert_eq!(
                snapshot[0]
                    .context()
                    .tags()
                    .get_single_tag("keep")
                    .and_then(|tag| tag.value()),
                Some("client")
            );
            assert_eq!(
                snapshot[0]
                    .context()
                    .tags()
                    .get_single_tag("route")
                    .and_then(|tag| tag.value()),
                Some("checkout")
            );
            assert!(snapshot[0].context().tags().get_single_tag("remove").is_none());
            assert!(snapshot.iter().all(|entry| entry.context() != &input_context));
            assert!(snapshot
                .iter()
                .all(|entry| { !matches!(entry.context().name().as_ref(), "raw.blocked" | "tenant.raw.blocked") }));

            drop(events_tx);
            drop(snapshot_handle);
            shutdown_tx.send(()).expect("test topology should still be running");
            let topology_result = topology_task.await.expect("topology task should not panic");
            assert!(
                topology_result.is_ok(),
                "topology should stop cleanly: {topology_result:?}"
            );
        })
        .await
        .expect("DogStatsD retained identity test should complete without hanging");
    }

    #[tokio::test]
    async fn context_dump_handler_reads_configured_credentials_and_run_path_and_uses_supplied_owner() {
        let run_directory = tempfile::tempdir().expect("run directory should be created");
        let token_file = run_directory.path().join("auth_token");
        fs::write(&token_file, AUTH_TOKEN).expect("token should be written");
        let config = context_dump_config(Some(run_directory.path()), &token_file).await;
        let (snapshot_handle, mut snapshot_responder) = aggregate_context_snapshot_channel_for_test();

        let handler = build_dogstatsd_context_dump_api_handler(&config, snapshot_handle)
            .await
            .expect("configured handler should build");
        let owner = tokio::spawn(async move {
            snapshot_responder
                .respond(vec![snapshot_entry("from.supplied.aggregate")])
                .await
        });

        let response = send(&handler, authorized_post()).await;

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
    async fn context_dump_handler_reports_missing_and_unreadable_token_paths_with_io_kind() {
        let directory = tempfile::tempdir().expect("temporary directory should be created");
        let cases = [directory.path().join("missing-token"), directory.path().to_owned()];

        for token_path in cases {
            let expected_kind = tokio::fs::read(&token_path)
                .await
                .expect_err("token fixture should be unreadable")
                .kind();
            let config = context_dump_config(Some(directory.path()), &token_path).await;
            let (snapshot_handle, _snapshot_responder) = aggregate_context_snapshot_channel_for_test();

            let error = match build_dogstatsd_context_dump_api_handler(&config, snapshot_handle).await {
                Ok(_) => panic!("unreadable token should fail handler construction"),
                Err(error) => error,
            };
            let message = format!("{error:#}");
            assert!(message.contains(&token_path.display().to_string()), "{message}");
            assert!(message.contains(&expected_kind.to_string()), "{message}");
        }
    }

    #[tokio::test]
    async fn context_dump_handler_rejects_empty_and_invalid_raw_token_contents() {
        let directory = tempfile::tempdir().expect("temporary directory should be created");
        let token_file = directory.path().join("auth_token");

        for token in [b"".as_slice(), b"token-with-newline\n".as_slice()] {
            fs::write(&token_file, token).expect("token fixture should be written");
            let config = context_dump_config(Some(directory.path()), &token_file).await;
            let (snapshot_handle, _snapshot_responder) = aggregate_context_snapshot_channel_for_test();

            if build_dogstatsd_context_dump_api_handler(&config, snapshot_handle)
                .await
                .is_ok()
            {
                panic!("invalid raw token should fail closed");
            }
        }
    }

    #[tokio::test]
    async fn context_dump_handler_keeps_missing_and_empty_run_path_empty_until_authorized_publication() {
        let directory = tempfile::tempdir().expect("temporary directory should be created");
        let token_file = directory.path().join("auth_token");
        fs::write(&token_file, AUTH_TOKEN).expect("token should be written");
        let cwd_artifact = std::env::current_dir().unwrap().join(CONTEXT_DUMP_FILENAME);
        assert!(!cwd_artifact.exists(), "test requires no pre-existing cwd artifact");

        for run_path in [None, Some(Path::new(""))] {
            let config = context_dump_config(run_path, &token_file).await;
            let (snapshot_handle, mut snapshot_responder) = aggregate_context_snapshot_channel_for_test();
            let handler = build_dogstatsd_context_dump_api_handler(&config, snapshot_handle)
                .await
                .expect("empty run path should not weaken authentication setup");
            let owner = tokio::spawn(async move { snapshot_responder.respond(Vec::new()).await });

            let response = send(&handler, authorized_post()).await;

            assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
            assert!(!cwd_artifact.exists());
            owner.await.unwrap().unwrap();
        }
    }

    struct ControlledMetricSource {
        events: mpsc::Receiver<Event>,
    }

    #[async_trait]
    impl Source for ControlledMetricSource {
        async fn run(mut self: Box<Self>, mut context: SourceContext) -> Result<(), GenericError> {
            let shutdown = context.take_shutdown_handle();
            tokio::pin!(shutdown);
            let mut events_open = true;

            loop {
                tokio::select! {
                    _ = &mut shutdown => break,
                    maybe_event = self.events.recv(), if events_open => match maybe_event {
                        Some(event) => context.dispatcher().dispatch_one(event).await?,
                        None => events_open = false,
                    }
                }
            }
            Ok(())
        }
    }

    struct ControlledMetricSourceBuilder {
        events: Mutex<Option<mpsc::Receiver<Event>>>,
        outputs: Vec<OutputDefinition<EventType>>,
    }

    #[async_trait]
    impl SourceBuilder for ControlledMetricSourceBuilder {
        fn outputs(&self) -> &[OutputDefinition<EventType>] {
            &self.outputs
        }

        async fn build(&self, _context: ComponentContext) -> Result<Box<dyn Source + Send>, GenericError> {
            let events = self
                .events
                .lock()
                .map_err(|_| generic_error!("controlled metric source receiver lock is poisoned"))?
                .take()
                .ok_or_else(|| generic_error!("controlled metric source receiver has already been taken"))?;
            Ok(Box::new(ControlledMetricSource { events }))
        }
    }

    impl MemoryBounds for ControlledMetricSourceBuilder {
        fn specify_bounds(&self, _builder: &mut MemoryBoundsBuilder) {}
    }

    struct DrainingMetricDestination;

    #[async_trait]
    impl Destination for DrainingMetricDestination {
        async fn run(self: Box<Self>, mut context: DestinationContext) -> Result<(), GenericError> {
            while context.events().next().await.is_some() {}
            Ok(())
        }
    }

    struct DrainingMetricDestinationBuilder;

    #[async_trait]
    impl DestinationBuilder for DrainingMetricDestinationBuilder {
        fn input_event_type(&self) -> EventType {
            EventType::Metric
        }

        async fn build(&self, _context: ComponentContext) -> Result<Box<dyn Destination + Send>, GenericError> {
            Ok(Box::new(DrainingMetricDestination))
        }
    }

    impl MemoryBounds for DrainingMetricDestinationBuilder {
        fn specify_bounds(&self, _builder: &mut MemoryBoundsBuilder) {}
    }

    async fn context_dump_config(run_path: Option<&Path>, token_path: &Path) -> GenericConfiguration {
        let mut values = json!({
            "auth_token_file_path": token_path,
        });
        if let Some(run_path) = run_path {
            values["run_path"] = json!(run_path);
        }
        config_from(values).await
    }

    fn authorized_post() -> Request<Empty<Bytes>> {
        Request::builder()
            .method("POST")
            .uri(CONTEXT_DUMP_ROUTE)
            .header(AUTHORIZATION, "Bearer configured-agent-token")
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
