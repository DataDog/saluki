use std::{
    collections::HashSet,
    path::PathBuf,
    time::{Duration, Instant},
};

use argh::FromArgs;
use datadog_agent_commons::{ipc::client::RemoteAgentClient, platform::PlatformSettings};
use datadog_agent_config::classifier::{ConfigClassifier, Pipeline, PipelineAffinity, Severity, SupportLevel};
use datadog_agent_config::{DatadogRemapper, KEY_ALIASES};
use resource_accounting::{ComponentBounds, ComponentRegistry};
use saluki_app::{
    accounting::{initialize_memory_bounds, MemoryBoundsConfiguration},
    bootstrap::BootstrapGuard,
    metrics::emit_startup_metrics,
};
use saluki_component_config::{
    DatadogForwarderConfig, DogStatsDDebugLogConfig, DogStatsDPostAggregateFilterConfig, DogStatsDPrefixFilterConfig,
    EndpointConfig, MetricTagFilterEntry, MrfConfig, RetryConfig, ScopedConfig, TagFilterlistConfig, TlsClientConfig,
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
        AggregateConfiguration, ApmStatsTransformConfiguration, AutoscalingFailoverGatewayConfiguration,
        ChainedConfiguration, DogStatsDMapperConfiguration, HostEnrichmentConfiguration,
        MrfMetricsGatewayConfiguration, TraceObfuscationConfiguration, TraceSamplerConfiguration,
    },
};
use saluki_config_tools::{ConfigurationLoader, DurationString, GenericConfiguration};
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
    started: Instant, bootstrap_config: GenericConfiguration, bootstrap_guard: &mut BootstrapGuard,
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

    // Load our "bootstrap" configuration.
    //
    // If remote agent mode is enabled, we'll register as a remote agent, which will unlock the ability to receive
    // configuration updates from the Core Agent, which we'll use to build our final, updated configuration. Otherwise,
    // we keep the bootstrap configuration and use it as-is.
    let bootstrap_dp_config = DataPlaneConfiguration::from_configuration(&bootstrap_config)
        .error_context("Failed to load data plane configuration.")?;

    let in_standalone_mode = bootstrap_dp_config.standalone_mode();
    let remote_agent_enabled = bootstrap_dp_config.remote_agent_enabled();
    let use_new_config_stream_endpoint = bootstrap_dp_config.use_new_config_stream_endpoint();
    let should_bootstrap_remote_agent = !in_standalone_mode && (remote_agent_enabled || use_new_config_stream_endpoint);

    let ra_bootstrap = if should_bootstrap_remote_agent {
        let ra_bootstrap = RemoteAgentBootstrap::from_configuration(&bootstrap_config, &bootstrap_dp_config)
            .await
            .error_context("Failed to bootstrap remote agent state.")?;

        Some(ra_bootstrap)
    } else {
        None
    };

    let (config, dp_config) = match &ra_bootstrap {
        Some(ra_bootstrap) if use_new_config_stream_endpoint => {
            // Build a new configuration that uses the configuration sent by the control plane as the authoritative
            // configuration source, but with environment variables on top of that to allow for ADP-specific overriding: log
            // level, etc.
            let dynamic_config = ConfigurationLoader::default()
                .with_key_aliases(KEY_ALIASES)
                .add_providers([DatadogRemapper::new()])
                .from_environment(PlatformSettings::get_env_var_prefix())?
                .with_dynamic_configuration(ra_bootstrap.create_config_stream())
                .into_generic()
                .await?;

            info!("Waiting for initial configuration from Datadog Agent...");
            dynamic_config.ready().await;
            info!("Initial configuration received.");

            // Now that the Datadog Agent has supplied its authoritative configuration, reload the logging subsystem
            // so its destinations, format, and level reflect what the Agent specifies rather than the bootstrap-phase
            // defaults.
            match LoggingConfigurationTranslator::translate(&dynamic_config) {
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

            // Reload our data plane configuration based on the dynamic configuration.
            let dynamic_dp_config = DataPlaneConfiguration::from_configuration(&dynamic_config)
                .error_context("Failed to load data plane configuration.")?;

            (dynamic_config, dynamic_dp_config)
        }

        // If dynamic configuration is disabled, the bootstrap configuration is already the complete and final configuration.
        _ => (bootstrap_config, bootstrap_dp_config),
    };

    if !in_standalone_mode && !dp_config.enabled() {
        info!("Agent Data Plane is not enabled. Exiting.");
        return Ok(());
    }

    let active_pipelines = active_pipelines(&dp_config);
    check_and_warn_config(&config, &active_pipelines).error_context("Incompatible configuration detected.")?;

    // Set up all of the building blocks for building our topologies and launching internal processes.
    let component_registry = ComponentRegistry::default();
    let health_registry = HealthRegistry::new();
    let (env_provider, maybe_env_supervisor) =
        ADPEnvironmentProvider::from_configuration(&config, &dp_config, &component_registry, &health_registry).await?;

    // Create the blueprint for our primary topology.
    let (mut blueprint, control_surfaces) =
        create_topology(&config, &dp_config, &env_provider, &component_registry).await?;

    // Create the internal supervisor which drives our control plane and internal observability.
    let mut internal_supervisor = create_internal_supervisor(
        &config,
        &dp_config,
        &component_registry,
        health_registry.clone(),
        control_surfaces,
        ra_bootstrap,
        bootstrap_guard.logging().controller(),
    )
    .await
    .error_context("Failed to create internal supervisor.")?;

    // Run memory bounds validation to ensure that we can launch the topology with our configured memory limit, if any.
    let bounds_config = MemoryBoundsConfiguration::try_from_config(&config)?;
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
    config: &GenericConfiguration, active_pipelines: &HashSet<Pipeline>,
) -> Result<(), GenericError> {
    let classifier = ConfigClassifier::new();
    let mut high_severity_incompatibilities = 0u32;
    debug!("Analyzing configuration.");
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
    config: &GenericConfiguration, dp_config: &DataPlaneConfiguration, env_provider: &ADPEnvironmentProvider,
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
        let dd_forwarder_config = DatadogForwarderConfiguration::from_native(ScopedConfig::fixed(
            datadog_forwarder_config_from_raw(config).error_context("Failed to configure Datadog forwarder.")?,
        ));
        blueprint.add_forwarder("dd_out", dd_forwarder_config)?;
    }

    if dp_config.metrics_pipeline_required() {
        add_baseline_metrics_pipeline_to_blueprint(&mut blueprint, config, dp_config, env_provider).await?;
    }

    if dp_config.logs_pipeline_required() {
        add_baseline_logs_pipeline_to_blueprint(&mut blueprint, config).await?;
    }

    if dp_config.events_pipeline_required() {
        add_baseline_events_pipeline_to_blueprint(&mut blueprint, config).await?;
    }

    if dp_config.service_checks_pipeline_required() {
        add_baseline_service_checks_pipeline_to_blueprint(&mut blueprint, config).await?;
    }

    if dp_config.traces_pipeline_required() {
        add_baseline_traces_pipeline_to_blueprint(&mut blueprint, config, env_provider).await?;
    }

    // Now we move on to our actual data pipelines.
    if dp_config.checks().enabled() {
        add_checks_pipeline_to_blueprint(&mut blueprint, config).await?;
    }

    if dp_config.dogstatsd().enabled() {
        let dsd_control_surface = add_dsd_pipeline_to_blueprint(&mut blueprint, config, env_provider).await?;
        control_surfaces.attach_dogstatsd(dsd_control_surface);
    }

    if dp_config.otlp().enabled() {
        add_otlp_pipeline_to_blueprint(&mut blueprint, config, dp_config, env_provider)?;
    }

    Ok((blueprint, control_surfaces))
}

async fn add_checks_pipeline_to_blueprint(
    blueprint: &mut TopologyBlueprint, config: &GenericConfiguration,
) -> Result<(), GenericError> {
    let checks_config = config.as_typed::<ChecksIPCConfiguration>()?;

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
        let host_tags_config = host_tags_config_from_raw(config).await?;
        metrics_enrich_config = metrics_enrich_config.with_transform_builder("host_tags", host_tags_config);
    }

    let dd_metrics_config = config
        .as_typed::<DatadogMetricsConfiguration>()
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

async fn host_tags_config_from_raw(config: &GenericConfiguration) -> Result<HostTagsConfiguration, GenericError> {
    let client = RemoteAgentClient::from_configuration(config).await?;
    let expected_tags_duration = config
        .try_get_typed::<DurationString>("expected_tags_duration")?
        .map(|value| value.as_duration());
    Ok(HostTagsConfiguration::from_parts(client, expected_tags_duration))
}

fn ottl_filter_config_from_raw(config: &GenericConfiguration) -> Result<OttlFilterConfiguration, GenericError> {
    let config = config
        .try_get_typed::<crate::components::ottl_filter_processor::config::OttlFilterConfig>("ottl_filter_config")?
        .unwrap_or_default();
    Ok(OttlFilterConfiguration::from_native(config))
}

fn ottl_transform_config_from_raw(config: &GenericConfiguration) -> Result<OttlTransformConfiguration, GenericError> {
    let config = config
        .try_get_typed::<crate::components::ottl_transform_processor::config::OttlTransformConfig>(
            "ottl_transform_config",
        )?
        .unwrap_or_default();
    Ok(OttlTransformConfiguration::from_native(config))
}

fn dogstatsd_prefix_filter_config_from_raw(
    config: &GenericConfiguration,
) -> Result<DogStatsDPrefixFilterConfiguration, GenericError> {
    let mut native = DogStatsDPrefixFilterConfig::default();
    native.metric_prefix = config.try_get_typed("statsd_metric_namespace")?.unwrap_or_default();
    if let Some(blocklist) = config
        .try_get_typed("statsd_metric_namespace_blocklist")?
        .or(config.try_get_typed("statsd_metric_namespace_blacklist")?)
    {
        native.metric_prefix_blocklist = blocklist;
    }
    native.metric_filterlist = config.try_get_typed("metric_filterlist")?.unwrap_or_default();
    native.metric_filterlist_match_prefix = config.try_get_typed("metric_filterlist_match_prefix")?.unwrap_or(false);
    native.metric_blocklist = config.try_get_typed("statsd_metric_blocklist")?.unwrap_or_default();
    native.metric_blocklist_match_prefix = config
        .try_get_typed("statsd_metric_blocklist_match_prefix")?
        .unwrap_or(false);
    Ok(DogStatsDPrefixFilterConfiguration::from_native(ScopedConfig::fixed(
        native,
    )))
}

fn tag_filterlist_config_from_raw(config: &GenericConfiguration) -> Result<TagFilterlistConfiguration, GenericError> {
    let native = TagFilterlistConfig {
        entries: config
            .try_get_typed::<Vec<MetricTagFilterEntry>>("metric_tag_filterlist")?
            .unwrap_or_default(),
        cache_capacity: config
            .try_get_typed("data_plane.dogstatsd.aggregator_tag_filter_cache_capacity")?
            .unwrap_or(100_000),
    };
    Ok(TagFilterlistConfiguration::from_native(ScopedConfig::fixed(native)))
}

fn dogstatsd_debug_log_config_from_raw(
    config: &GenericConfiguration, default_log_file_path: PathBuf,
) -> Result<DogStatsDDebugLogConfiguration, GenericError> {
    let native = DogStatsDDebugLogConfig {
        metrics_stats_enabled: config.try_get_typed("dogstatsd_metrics_stats_enable")?.unwrap_or(false),
        logging_enabled: config.try_get_typed("dogstatsd_logging_enabled")?.unwrap_or(true),
        log_file: config
            .try_get_typed::<String>("dogstatsd_log_file")?
            .unwrap_or_default(),
        log_file_max_size_bytes: config
            .try_get_typed::<bytesize::ByteSize>("dogstatsd_log_file_max_size")?
            .unwrap_or_else(|| bytesize::ByteSize::mb(10))
            .as_u64(),
        log_file_max_rolls: config.try_get_typed("dogstatsd_log_file_max_rolls")?.unwrap_or(3),
    };
    DogStatsDDebugLogConfiguration::from_native(ScopedConfig::fixed(native), default_log_file_path)
}

fn dogstatsd_config_from_raw(config: &GenericConfiguration) -> Result<DogStatsDConfiguration, GenericError> {
    let run_path = config.try_get_typed::<PathBuf>("run_path")?;
    let config = config.as_typed::<DogStatsDConfiguration>()?.with_run_path(run_path);
    Ok(config)
}

fn dogstatsd_post_aggregate_filter_config_from_raw(
    config: &GenericConfiguration,
) -> Result<DogStatsDPostAggregateFilterConfiguration, GenericError> {
    let native = DogStatsDPostAggregateFilterConfig {
        metric_filterlist: config.try_get_typed("metric_filterlist")?.unwrap_or_default(),
        metric_filterlist_match_prefix: config.try_get_typed("metric_filterlist_match_prefix")?.unwrap_or(false),
        metric_blocklist: config.try_get_typed("statsd_metric_blocklist")?.unwrap_or_default(),
        metric_blocklist_match_prefix: config
            .try_get_typed("statsd_metric_blocklist_match_prefix")?
            .unwrap_or(false),
        histogram_aggregates: config
            .try_get_typed("histogram_aggregates")?
            .unwrap_or_else(|| DogStatsDPostAggregateFilterConfig::default().histogram_aggregates),
        histogram_percentiles: config
            .try_get_typed("histogram_percentiles")?
            .unwrap_or_else(|| DogStatsDPostAggregateFilterConfig::default().histogram_percentiles),
    };
    Ok(DogStatsDPostAggregateFilterConfiguration::from_native(
        ScopedConfig::fixed(native),
    ))
}

fn mrf_config_from_raw(config: &GenericConfiguration) -> Result<MrfConfig, GenericError> {
    let site = config
        .try_get_typed::<String>("multi_region_failover.site")?
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    let dd_url = config
        .try_get_typed::<String>("multi_region_failover.dd_url")?
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    let endpoint = dd_url.or_else(|| site.map(|site| format!("https://app.mrf.{site}")));
    let api_key = config
        .try_get_typed::<String>("multi_region_failover.api_key")?
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());

    Ok(MrfConfig {
        enabled: config.try_get_typed("multi_region_failover.enabled")?.unwrap_or(false),
        failover_metrics: config
            .try_get_typed("multi_region_failover.failover_metrics")?
            .unwrap_or(false),
        metric_allowlist: config
            .try_get_typed("multi_region_failover.metric_allowlist")?
            .unwrap_or_default(),
        api_key,
        endpoint,
    })
}

fn datadog_forwarder_config_from_raw(config: &GenericConfiguration) -> Result<DatadogForwarderConfig, GenericError> {
    let api_key = config.try_get_typed::<String>("api_key")?.unwrap_or_default();
    let dd_url = config
        .try_get_typed::<String>("dd_url")?
        .or(config.try_get_typed::<String>("url")?)
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| {
            let site = config
                .try_get_typed::<String>("site")
                .ok()
                .flatten()
                .filter(|value| !value.trim().is_empty())
                .unwrap_or_else(|| "datadoghq.com".to_string());
            format!("https://app.{site}")
        });

    let mut endpoints = vec![EndpointConfig { url: dd_url, api_key }];
    if let Some(additional) = config.try_get_typed::<serde_json::Value>("additional_endpoints")? {
        append_additional_endpoints(&mut endpoints, additional);
    }

    let storage_path = config
        .try_get_typed::<String>("forwarder_storage_path")?
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            config
                .try_get_typed::<PathBuf>("run_path")
                .ok()
                .flatten()
                .map(|path| path.join("transactions_to_retry").to_string_lossy().into_owned())
        })
        .unwrap_or_default();

    Ok(DatadogForwarderConfig {
        endpoints,
        endpoint_concurrency: config
            .try_get_typed::<usize>("forwarder_max_concurrent_requests")?
            .unwrap_or(10),
        request_timeout_millis: config.try_get_typed::<u64>("forwarder_timeout")?.unwrap_or(20) * 1000,
        allow_arbitrary_tags: config.try_get_typed("allow_arbitrary_tags")?.unwrap_or(false),
        tls: TlsClientConfig {
            verify: !config.try_get_typed("skip_ssl_validation")?.unwrap_or(false),
        },
        retry: RetryConfig {
            storage_path,
            max_disk_size_bytes: config
                .try_get_typed("forwarder_storage_max_size_in_bytes")?
                .unwrap_or(0),
            flush_to_disk_mem_ratio: config
                .try_get_typed("forwarder_flush_to_disk_mem_ratio")?
                .unwrap_or(0.5),
        },
    })
}

fn append_additional_endpoints(endpoints: &mut Vec<EndpointConfig>, value: serde_json::Value) {
    let value = match value {
        serde_json::Value::String(raw) => serde_json::from_str(&raw).unwrap_or(serde_json::Value::String(raw)),
        value => value,
    };
    let serde_json::Value::Object(map) = value else {
        return;
    };
    for (url, keys) in map {
        match keys {
            serde_json::Value::Array(keys) => {
                endpoints.extend(keys.into_iter().filter_map(|key| endpoint_from_json(&url, key)));
            }
            key => {
                if let Some(endpoint) = endpoint_from_json(&url, key) {
                    endpoints.push(endpoint);
                }
            }
        }
    }
}

fn endpoint_from_json(url: &str, key: serde_json::Value) -> Option<EndpointConfig> {
    let api_key = key.as_str()?.trim().to_string();
    (!api_key.is_empty()).then(|| EndpointConfig {
        url: url.to_string(),
        api_key,
    })
}

fn add_mrf_metrics_pipeline_to_blueprint(
    blueprint: &mut TopologyBlueprint, config: &GenericConfiguration,
) -> Result<(), GenericError> {
    let mrf_native_config =
        mrf_config_from_raw(config).error_context("Failed to configure Multi-Region Failover metrics pipeline.")?;
    let mrf_config = MrfConfiguration::from_native(mrf_native_config.clone());

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

    let mrf_gateway_config =
        MrfMetricsGatewayConfiguration::new(mrf_config.clone(), ScopedConfig::fixed(mrf_native_config));
    let mrf_metrics_config = config
        .as_typed::<DatadogMetricsConfiguration>()
        .error_context("Failed to configure Multi-Region Failover Datadog Metrics encoder.")?;

    let mrf_forwarder_config = DatadogForwarderConfiguration::from_native(ScopedConfig::fixed(
        datadog_forwarder_config_from_raw(config)
            .error_context("Failed to configure Multi-Region Failover Datadog forwarder.")?,
    ))
    .with_endpoint_override_and_api_key_refresh_config_path(
        mrf_dd_url,
        mrf_api_key,
        "multi_region_failover.api_key",
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
    let dd_logs_config = config
        .as_typed::<DatadogLogsConfiguration>()
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
    let dd_events_config = config
        .as_typed::<DatadogEventsConfiguration>()
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
    let dd_service_checks_config = config
        .as_typed::<DatadogServiceChecksConfiguration>()
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
    let dd_traces_config = DatadogTraceConfiguration::from_native(
        config
            .as_typed::<DatadogTraceConfiguration>()
            .error_context("Failed to configure Datadog Traces encoder.")?,
    )
    .with_environment_provider(env_provider.clone())
    .await?;
    let trace_obfuscation_config = TraceObfuscationConfiguration::new();
    let otlp_sampling_percentage = config
        .try_get_typed("otlp_config.traces.probabilistic_sampler.sampling_percentage")?
        .or(config.try_get_typed("otlp_config_traces_probabilistic_sampler_sampling_percentage")?)
        .unwrap_or(100.0);
    let trace_sampler_config = TraceSamplerConfiguration::from_native(otlp_sampling_percentage);
    let ottl_filter_config =
        ottl_filter_config_from_raw(config).error_context("Failed to configure OTTL filter processor.")?;
    let ottl_transform_config =
        ottl_transform_config_from_raw(config).error_context("Failed to configure OTTL transform processor.")?;
    let dd_traces_enrich_config = ChainedConfiguration::default()
        .with_transform_builder("ottl_filter", ottl_filter_config)
        .with_transform_builder("ottl_transform", ottl_transform_config)
        .with_transform_builder("apm_onboarding", ApmOnboardingConfiguration)
        .with_transform_builder("trace_obfuscation", trace_obfuscation_config)
        .with_transform_builder("trace_sampler", trace_sampler_config);
    let apm_stats_transform_config = ApmStatsTransformConfiguration::from_native()
        .with_environment_provider(env_provider.clone())
        .await?;
    let dd_apm_stats_encoder = DatadogApmStatsEncoderConfiguration::from_native(
        config
            .as_typed::<DatadogApmStatsEncoderConfiguration>()
            .error_context("Failed to configure Datadog APM Stats encoder.")?,
    )
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
    blueprint: &mut TopologyBlueprint, config: &GenericConfiguration, env_provider: &ADPEnvironmentProvider,
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

    let dsd_config = dogstatsd_config_from_raw(config)
        .error_context("Failed to configure DogStatsD source.")?
        .with_workload_provider(env_provider.workload().clone())
        .with_capture_entity_resolver(env_provider.workload().clone());
    let dsd_prefix_filter_configuration = dogstatsd_prefix_filter_config_from_raw(config)?;
    let dsd_mapper_config = config.as_typed::<DogStatsDMapperConfiguration>()?;
    let dsd_enrich_config =
        ChainedConfiguration::default().with_transform_builder("dogstatsd_mapper", dsd_mapper_config);
    let dsd_tag_filterlist_config =
        tag_filterlist_config_from_raw(config).error_context("Failed to configure metric tag filterlist transform.")?;
    let dsd_agg_config = config
        .as_typed::<AggregateConfiguration>()
        .error_context("Failed to configure aggregate transform.")?;
    let dsd_post_agg_filter_config = dogstatsd_post_aggregate_filter_config_from_raw(config)
        .error_context("Failed to configure DogStatsD post-aggregate filter transform.")?;
    let events_enrich_config = ChainedConfiguration::default().with_transform_builder(
        "host_enrichment",
        HostEnrichmentConfiguration::from_environment_provider(env_provider.clone()),
    );
    let service_checks_enrich_config = ChainedConfiguration::default().with_transform_builder(
        "host_enrichment",
        HostEnrichmentConfiguration::from_environment_provider(env_provider.clone()),
    );
    let dsd_debug_log_config =
        dogstatsd_debug_log_config_from_raw(config, PlatformSettings::get_default_dogstatsd_log_file_path())
            .error_context("Failed to configure DogStatsD debug log destination.")?;
    let dsd_stats_config = DogStatsDStatisticsConfiguration::new();

    let stats_api_handler = dsd_stats_config.api_handler();
    let capture_api_handler = dsd_config.capture_api_handler();
    let replay_api_handler = dsd_config.replay_api_handler();

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
    })
}

fn add_otlp_pipeline_to_blueprint(
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

        let otlp_relay_config = config.as_typed::<OtlpRelayConfiguration>()?;
        let otlp_decoder_config = config.as_typed::<OtlpDecoderConfiguration>()?;

        let core_agent_traces_internal_port = config
            .try_get_typed("otlp_config.traces.internal_port")?
            .unwrap_or(5003);
        let local_agent_otlp_forwarder_config =
            OtlpForwarderConfiguration::from_native(core_agent_otlp_grpc_endpoint, core_agent_traces_internal_port);

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

        let otlp_config = config
            .as_typed::<OtlpConfiguration>()?
            .with_workload_provider(env_provider.workload().clone());

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
