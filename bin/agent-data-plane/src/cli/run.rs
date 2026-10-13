use std::{
    num::NonZeroU16,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant, SystemTime},
};

use agent_data_plane_config::{
    control::MemoryMode,
    domains::{dogstatsd, multi_region_failover},
    shared::SharedConfiguration,
    SalukiConfiguration,
};
use agent_data_plane_config_system::{ConfigurationSystem, LoadedConfiguration};
use argh::FromArgs;
use bytesize::ByteSize;
#[cfg(test)]
use datadog_agent_commons::ipc::config::RemoteAgentClientConfiguration;
use datadog_agent_commons::platform::PlatformSettings;
use saluki_app::{
    accounting::{initialize_memory_bounds, memory_grant, MemoryBoundsConfiguration, MemoryMode as AppMemoryMode},
    bootstrap::BootstrapGuard,
    metrics::emit_startup_metrics,
    util::wait_for_shutdown_signal,
};
use saluki_common::resource_tracking::set_cpu_time_sampling;
use saluki_components::{
    config::{
        metrics_endpoint_routing::EndpointAllowlistGroup, AutoscalingFailoverConfiguration,
        MetricsEndpointRoutingConfiguration, MrfConfiguration,
    },
    decoders::otlp::OtlpDecoderConfiguration,
    destinations::{DogStatsDDebugLogConfiguration, DogStatsDStatisticsConfiguration},
    encoders::{
        BufferedIncrementalConfiguration, DatadogApmStatsEncoderConfiguration, DatadogEventsConfiguration,
        DatadogLogsConfiguration, DatadogMetricsConfiguration, DatadogServiceChecksConfiguration,
        DatadogTraceConfiguration, MetricsEndpointRouting,
    },
    forwarders::{DatadogForwarderConfiguration, OtlpForwarderConfiguration},
    relays::otlp::OtlpRelayConfiguration,
    sources::{
        dogstatsd_metric_lane_output, ChecksIPCConfiguration, DogStatsDCaptureAPIHandler, DogStatsDCaptureControl,
        DogStatsDConfiguration, DogStatsDReplayAPIHandler, DogStatsDReplayControl, EnablePayloadsConfiguration,
        OriginEnrichmentConfiguration, OtlpConfiguration,
    },
    transforms::{
        AggregateContextSnapshotHandle, ApmStatsTransformConfiguration, ChainedConfiguration,
        DogStatsDMapperConfiguration, DogStatsDMapperProfile, DogStatsDMetricMapping, HostEnrichmentConfiguration,
        ReplaceRule, TraceObfuscationConfiguration, TraceSamplerConfiguration, TraceTagReplacerConfiguration,
    },
};
use saluki_core::{
    accounting::{ComponentBounds, ComponentRegistry},
    data_model::origin::OriginTagCardinality,
    health::HealthRegistry,
    runtime::{state::ResourceRegistry, RestartMode, RestartStrategy, Supervisor, SupervisorError},
    support::SubsystemIdentifier,
    topology::{topology_identifier, worker_pool_threads, TopologyBlueprint},
};
use saluki_env::{features, EnvironmentProvider as _, HostProvider as _};
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use saluki_io::net::ListenAddress;
use stringtheory::MetaString;
use tracing::{debug, info, warn};

use crate::{
    components::{
        apm_onboarding::ApmOnboardingConfiguration, liveness::LivenessConfiguration,
        ottl_filter_processor::OttlFilterConfiguration, ottl_transform_processor::OttlTransformConfiguration,
        static_tags::resolve_static_tags,
    },
    dogstatsd_contexts::DogStatsDContextDumpAPIHandler,
    internal::{
        create_internal_supervisor, logging::LoggingConfigurationTranslator, remote_agent::RemoteAgentBootstrap,
        ConfigUpdatesWorker, DogStatsDControlSurface, TopologyControlSurfaces,
    },
    programmable_metrics::{
        api::ControlSurfaceWorker,
        channel_stub::{ClientInfo, StubChannelConfig, StubChannelWorker},
        installer::{BootCapture, ProgrammableMetrics},
        lanes::{
            cpu_time_sampling, log_booted_lanes, LaneBound, LaneCounts, LaneFit, LaneSettings, CPU_TIME_SAMPLING_ENV,
        },
        topology::PlanTopology,
    },
};
use crate::{
    config::{remote_agent_client_configuration, DataPlaneConfiguration},
    internal::env::ADPEnvironmentProvider,
};

// The legacy metrics path, built from native components: a differential reference for the plan-driven one.
#[cfg(test)]
mod legacy;

/// The name of the primary topology, which every component's identity starts with.
pub(crate) const PRIMARY_TOPOLOGY: &str = "primary";

/// Runs the data plane.
#[derive(FromArgs, Debug)]
#[argh(subcommand, name = "run")]
pub struct RunCommand {
    /// path to the PID file
    #[argh(option, short = 'p', long = "pidfile")]
    pub pid_file: Option<PathBuf>,
}

/// Entrypoint for the `run` commands.
///
/// `started` and `started_at` are the monotonic and the wall-clock time at which the process started.
pub async fn handle_run_command(
    started: Instant, started_at: SystemTime, local_config: LoadedConfiguration, bootstrap_guard: &mut BootstrapGuard,
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

    // Use local sources for decisions required before the Agent configuration stream is available.
    let bootstrap_dp_config = DataPlaneConfiguration::from_configuration(local_config.local());
    let standalone = bootstrap_dp_config.standalone_mode();

    // Resolve the authoritative configuration. Connected mode registers as a remote agent and takes
    // the Core Agent's config stream as authority; standalone mode treats the local sources as
    // authoritative. Both terminals block until the first configuration is received, deserialized,
    // and translated, failing the boot if it cannot be -- the strict startup gate.
    let (config_sys, ra_bootstrap, config_updates) = if standalone {
        let config_sys = local_config
            .standalone()
            .await
            .error_context("Failed to load configuration.")?;
        (config_sys, None, None)
    } else {
        // Blocks until the Core Agent acknowledges registration.
        let client_config = remote_agent_client_configuration(local_config.local())?;
        let ra_bootstrap = RemoteAgentBootstrap::new(&client_config, &bootstrap_dp_config)
            .await
            .error_context("Failed to bootstrap remote agent state.")?;

        // The configuration system owns the config stream: it reads `ConfigUpdate`s directly to
        // build the typed model. `create_config_stream` stays on `ra_bootstrap`, which we keep to
        // build the status/flare/telemetry services below.
        let config_stream = ra_bootstrap.create_config_stream();

        info!("Waiting for initial configuration from Datadog Agent...");
        let (config_sys, config_updates) = local_config
            .run(config_stream)
            .await
            .error_context("Failed to load initial configuration from the Datadog Agent.")?;
        info!("Initial configuration received.");

        (config_sys, Some(ra_bootstrap), Some(config_updates))
    };

    // Connected mode may have replaced the bootstrap-phase settings with the Agent's authoritative
    // config, so reload logging to match. Standalone resolves the same local sources seen at
    // bootstrap, making a reload redundant.
    if !standalone {
        match LoggingConfigurationTranslator::translate(&config_sys.config().control.logging) {
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

    let (enabled, active_pipelines) = {
        let config = config_sys.config();
        let dp = DataPlaneConfiguration::from_configuration(&config);
        (dp.enabled(), dp.active_pipelines())
    };

    if !standalone && !enabled {
        info!("Agent Data Plane is not enabled. Exiting.");
        return Ok(());
    }

    config_sys
        .check_compatibility(&active_pipelines)
        .error_context("Incompatible configuration detected.")?;

    // The local stub of the programmable metrics artifact channel, if configured. A stub URL that is not an `http://`
    // URL of a loopback address fails startup, as soon as the configuration is loaded.
    let artifact_channel_stub =
        StubChannelConfig::from_configuration(&config_sys.config().control.programmable_metrics.artifact_channel)?;

    let remote_agent_client_config = if standalone {
        None
    } else {
        Some(remote_agent_client_configuration(&config_sys.config())?)
    };

    // Set up all of the building blocks for building our topologies and launching internal processes.
    let component_registry = ComponentRegistry::default();
    let health_registry = HealthRegistry::new();
    let resource_registry = ResourceRegistry::new();
    let (env_provider, maybe_env_supervisor) = ADPEnvironmentProvider::from_configuration(
        standalone,
        config_sys.config().control.ipc.remote_agent_string_interner_size_bytes,
        &config_sys.config().shared.environment,
        remote_agent_client_config.as_ref(),
        &component_registry,
        &health_registry,
    )
    .await?;

    // Lower the metrics configuration into the default program, which the metrics path of the topology is built from,
    // and prepare the installer that keeps it current as the configuration changes. The lanes the metrics path's slots
    // run on are fixed now, from the configuration and the primary runtime's workers.
    let workers = saluki_app::util::get_async_runtime_parallelism().unwrap_or(1);
    let lane_settings = LaneSettings::from_config(&config_sys.config(), workers);
    let metrics_capture = ProgrammableMetrics::capture(
        started_at,
        &config_sys,
        &env_provider,
        remote_agent_client_config.as_ref(),
    )
    .await?;
    let mut programmable_metrics = boot_metrics(metrics_capture.as_ref(), lane_settings.counts, &component_registry)?;

    // Create the blueprint for our primary topology.
    let (blueprint, control_surfaces) = create_topology(
        &config_sys,
        &env_provider,
        &component_registry,
        programmable_metrics.as_mut(),
    )
    .await?;

    // In `strict` memory mode, automatic lanes that would not fit the memory grant are lowered, and the topology is
    // built again on the lanes that fit.
    let bounds_config = memory_bounds_configuration(&config_sys);
    let fitted = fit_lanes_to_memory(
        &LaneFitInputs {
            config_system: &config_sys,
            env_provider: &env_provider,
            component_registry: &component_registry,
            bounds: &bounds_config,
            settings: &lane_settings,
            capture: metrics_capture.as_ref(),
        },
        (blueprint, control_surfaces, programmable_metrics),
    )
    .await?;
    let (mut blueprint, control_surfaces, mut programmable_metrics) = fitted;
    let lanes = programmable_metrics
        .as_ref()
        .map_or(LaneCounts::OFF, ProgrammableMetrics::boot_lanes);
    // Resource groups read their CPU time on a sampled entry only, on lanes, where entries multiply.
    let sampling = cpu_time_sampling(std::env::var(CPU_TIME_SAMPLING_ENV).ok().as_deref(), lanes.lanes_on());
    set_cpu_time_sampling(sampling);
    if programmable_metrics.is_some() {
        log_booted_lanes(&config_sys.config(), &lanes, workers, sampling);
    }
    // On lanes, compute-heavy subtasks, such as an encode host's builders that lanes feed, run on a global pool that
    // grows with the workers.
    if lanes.lanes_on() {
        blueprint.with_dedicated_worker_pool(worker_pool_threads(workers));
    }

    // Create the root supervisor up front so that a handle to the whole supervision tree can be handed to the
    // control plane below, which serves it over the API. Nothing is added to it until the rest of the tree is built.
    let root_restart_strategy = RestartStrategy::new(RestartMode::OneForOne, 0, Duration::from_secs(5));
    let mut root_supervisor = Supervisor::new("adp-root")?.with_restart_strategy(root_restart_strategy);

    // Create the internal supervisor which drives our control plane and internal observability.
    let mut internal_supervisor = create_internal_supervisor(
        &config_sys,
        &component_registry,
        health_registry.clone(),
        control_surfaces,
        ra_bootstrap,
        bootstrap_guard.logging().controller(),
        config_sys.current_handle(),
        root_supervisor.tree_handle(),
    )
    .await
    .error_context("Failed to create internal supervisor.")?;

    // Run memory bounds validation to ensure that we can launch the topology with our configured memory limit, if any.
    let memory_bounds = initialize_memory_bounds(bounds_config, component_registry.root())?;
    let memory_limiter = memory_bounds.limiter;
    // What a generation or a resource transaction adds after boot is admitted against what verification left.
    if let Some(programmable_metrics) = programmable_metrics.as_mut() {
        programmable_metrics.admit_against(memory_bounds.verified);
    }

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
        .with_resource_registry(resource_registry.clone())
        .with_environment_readiness(env_provider.wait_for_ready());

    // Acquire a readiness handle before handing the blueprint off to the supervisor. This waits until the topology has
    // registered its components in the health registry and they've all reported ready, rather than racing the supervisor
    // and potentially observing an empty/already-ready registry before the topology's components even exist.
    let topology_ready = blueprint.topology_ready();

    root_supervisor.add_worker(bootstrap_supervisor);
    internal_supervisor.add_worker(resource_registry.worker());

    // Apply configuration updates from the Datadog Agent. Until the supervision tree starts, updates stay queued in the
    // Agent configuration stream and are not applied.
    //
    // A restart of the worker recovers from a panic, but not from a closed configuration stream, which never reopens:
    // the restarted worker fails again at once. The failure then escalates and stops ADP, rather than leaving ADP to run
    // on a configuration that can no longer change.
    if let Some(config_updates) = config_updates {
        internal_supervisor.add_worker(ConfigUpdatesWorker::new(config_updates));
    }

    // Install a new generation of the metrics plan whenever a configuration change or a policy artifact changes it. The
    // installer's state lives outside the worker, so a restart of the worker continues where it stopped. The control
    // surface on the privileged API serves the installer's status, and the stub of the artifact channel, if
    // configured, delivers policy artifacts to it. When DogStatsD aggregation runs in a region, the snapshot bridge
    // answers the context dump API from that region; its requests, too, live outside the worker.
    match programmable_metrics {
        Some(mut programmable_metrics) => {
            internal_supervisor.add_worker(ControlSurfaceWorker::new(programmable_metrics.status()));
            if let Some(stub) = artifact_channel_stub {
                let transport = programmable_metrics.connect_artifact_channel();
                internal_supervisor.add_worker(StubChannelWorker::new(stub, ClientInfo::for_process(), transport));
            }
            if let Some(bridge) = programmable_metrics.snapshot_bridge() {
                internal_supervisor.add_worker(bridge);
            }
            if let Some(lanes) = programmable_metrics.lane_telemetry() {
                internal_supervisor.add_worker(lanes);
            }
            internal_supervisor.add_worker(programmable_metrics.into_installer());
        }
        None if artifact_channel_stub.is_some() => {
            warn!(
                "The programmable metrics artifact channel stub is configured, but no metrics pipeline is enabled: \
                 the stub is not polled."
            );
        }
        None => {}
    }

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
    match root_supervisor.run_with_shutdown(wait_for_shutdown_signal()).await {
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

/// Returns the memory bounds configuration of the process, as `config_sys` configures it.
fn memory_bounds_configuration(config_sys: &ConfigurationSystem) -> MemoryBoundsConfiguration {
    let control = &config_sys.config().control;
    MemoryBoundsConfiguration {
        memory_limit: control.memory_limit.map(ByteSize::b),
        memory_slop_factor: control.memory_slop_factor,
        enable_global_limiter: control.enable_global_limiter,
        memory_mode: match control.memory_mode {
            MemoryMode::Disabled => AppMemoryMode::Disabled,
            MemoryMode::Permissive => AppMemoryMode::Permissive,
            MemoryMode::Strict => AppMemoryMode::Strict,
        },
    }
}

/// Prepares the programmable metrics path from what boot captured, on `lanes`, or `None` without a metrics pipeline.
pub(crate) fn boot_metrics(
    capture: Option<&BootCapture>, lanes: LaneCounts, component_registry: &ComponentRegistry,
) -> Result<Option<ProgrammableMetrics>, GenericError> {
    match capture {
        Some(capture) => ProgrammableMetrics::boot(capture, lanes, component_registry),
        None => Ok(None),
    }
}

/// The blueprint of the primary topology, its control surfaces, and the programmable metrics path it was built with.
pub(crate) type BuiltTopology = (TopologyBlueprint, TopologyControlSurfaces, Option<ProgrammableMetrics>);

/// What fitting the metrics path's lanes to the memory grant reads.
pub(crate) struct LaneFitInputs<'a> {
    pub(crate) config_system: &'a ConfigurationSystem,
    pub(crate) env_provider: &'a ADPEnvironmentProvider,
    pub(crate) component_registry: &'a ComponentRegistry,
    pub(crate) bounds: &'a MemoryBoundsConfiguration,
    pub(crate) settings: &'a LaneSettings,
    pub(crate) capture: Option<&'a BootCapture>,
}

/// Fits the lanes of the metrics path to the memory grant in `strict` memory mode, returning the topology to run: the
/// one built, or, when its automatic lanes do not fit the grant while fewer would, one built again on fewer lanes (see
/// [`LaneSettings::fit`]).
///
/// The firm bound of a candidate's lanes is measured by building the blueprint of its topology into a registry of its
/// own, which nothing runs: everything outside the topology, registered already, is the same at every lane count. The
/// room a live change's overlap takes beside it is the candidate's largest kernel-instance credit of one slot
/// ([`ProgrammableMetrics::instance_overlap_bytes`]). Without a grant, outside `strict` mode, or without lanes, the
/// built topology runs as built.
///
/// # Errors
///
/// Returns an error if the counts the configuration sets do not fit the grant, or if a topology cannot be built.
pub(crate) async fn fit_lanes_to_memory(
    inputs: &LaneFitInputs<'_>, built: BuiltTopology,
) -> Result<BuiltTopology, GenericError> {
    let LaneFitInputs {
        config_system,
        env_provider,
        component_registry,
        bounds,
        settings,
        capture,
    } = inputs;
    let Some(capture) = capture.filter(|_| bounds.memory_mode == AppMemoryMode::Strict) else {
        return Ok(built);
    };
    let Some(grant) = memory_grant(bounds)? else {
        return Ok(built);
    };
    let grant = u64::try_from(grant.effective_limit_bytes()).unwrap_or(u64::MAX);
    let topology = topology_identifier(PRIMARY_TOPOLOGY);
    let everything = component_registry.as_bounds();
    let built_topology = subtree_firm_bytes(&everything, &topology);
    let outside = (everything.total_firm_limit_bytes() as u64).saturating_sub(built_topology);
    let asked = settings.counts;
    let overlap_of =
        |metrics: Option<&ProgrammableMetrics>| metrics.map_or(0, ProgrammableMetrics::instance_overlap_bytes);
    let asked_overlap = overlap_of(built.2.as_ref());
    let fit = settings
        .fit(grant, async |counts| {
            if counts == asked {
                return Ok(LaneBound {
                    firm: outside.saturating_add(built_topology),
                    overlap: asked_overlap,
                });
            }
            let scratch = ComponentRegistry::default();
            let mut metrics = ProgrammableMetrics::boot(capture, counts, &scratch)?;
            create_topology(config_system, env_provider, &scratch, metrics.as_mut()).await?;
            Ok(LaneBound {
                firm: outside.saturating_add(subtree_firm_bytes(&scratch.as_bounds(), &topology)),
                overlap: overlap_of(metrics.as_ref()),
            })
        })
        .await?;
    let LaneFit::Lowered(counts) = fit else {
        return Ok(built);
    };
    // The topology built first is dropped unrun, with what it registered.
    drop(built);
    component_registry.bounds_builder(&topology).reset();
    let mut metrics = ProgrammableMetrics::boot(capture, counts, component_registry)?;
    let (blueprint, control_surfaces) =
        create_topology(config_system, env_provider, component_registry, metrics.as_mut()).await?;
    Ok((blueprint, control_surfaces, metrics))
}

/// Returns the firm bytes of the component `id` and everything below it in `bounds`, or zero if it has none.
fn subtree_firm_bytes(bounds: &ComponentBounds, id: &SubsystemIdentifier) -> u64 {
    let path = id.to_string();
    let mut current = bounds;
    for segment in path.split('.') {
        match current
            .subcomponents()
            .into_iter()
            .find(|(name, _)| name.as_str() == segment)
        {
            Some((_, below)) => current = below,
            None => return 0,
        }
    }
    current.total_firm_limit_bytes() as u64
}

/// Creates the blueprint of the primary topology.
///
/// When the configuration requires a metrics pipeline, `programmable_metrics` is what [`ProgrammableMetrics::boot`]
/// prepared for it: the metrics path is built from the default program's physical frame. Everything else is built
/// from native components.
///
/// # Errors
///
/// Returns an error if a component cannot be configured, or if the metrics pipeline is required and no metrics path
/// was prepared.
pub(crate) async fn create_topology(
    config_system: &ConfigurationSystem, env_provider: &ADPEnvironmentProvider, component_registry: &ComponentRegistry,
    programmable_metrics: Option<&mut ProgrammableMetrics>,
) -> Result<(TopologyBlueprint, TopologyControlSurfaces), GenericError> {
    let metrics = match programmable_metrics {
        Some(programmable_metrics) => MetricsWiring::Plan(PlanTopology::new(programmable_metrics)),
        None => MetricsWiring::None,
    };
    assemble_topology(config_system, env_provider, component_registry, metrics).await
}

/// Creates the blueprint of the primary topology with the legacy metrics path, built from native components as before
/// the plan-driven topology: a differential reference for tests.
#[cfg(test)]
pub(crate) async fn create_legacy_topology(
    config_system: &ConfigurationSystem, remote_agent_client_config: Option<&RemoteAgentClientConfiguration>,
    env_provider: &ADPEnvironmentProvider, component_registry: &ComponentRegistry,
) -> Result<(TopologyBlueprint, TopologyControlSurfaces), GenericError> {
    let metrics = legacy::LegacyMetrics::new(config_system, remote_agent_client_config, env_provider);
    assemble_topology(
        config_system,
        env_provider,
        component_registry,
        MetricsWiring::Legacy(metrics),
    )
    .await
}

/// How the metrics path of the topology is built.
///
/// The pipelines that feed the metrics path, and the parts of the DogStatsD pipeline outside it, are built the same
/// way whatever builds the metrics path, through these hooks.
enum MetricsWiring<'a> {
    /// No metrics path was prepared: the configuration requires no metrics pipeline.
    None,

    /// The plan-driven metrics path, built from the default program's physical frame.
    Plan(PlanTopology<'a>),

    /// The legacy metrics path, built from native components: a differential reference for tests.
    #[cfg(test)]
    Legacy(legacy::LegacyMetrics<'a>),
}

impl MetricsWiring<'_> {
    /// Returns the configuration of `dd_out`, which every pipeline sends through: on the plan-driven metrics path, the
    /// delivery host the installer controls, and otherwise the native forwarder of the startup configuration's
    /// endpoints, as for a deployment without a metrics pipeline.
    fn dd_forwarder(
        &mut self, config_system: &ConfigurationSystem, shared: &SharedConfiguration, metrics: bool,
    ) -> Result<DatadogForwarderConfiguration, GenericError> {
        match self {
            Self::Plan(plan) if metrics => plan.dd_forwarder(shared, config_system),
            _ => Ok(DatadogForwarderConfiguration::from_configuration(
                shared,
                config_system.live(|config| &config.shared.endpoints.api_key),
                config_system.live(|config| &config.shared.endpoints.additional_endpoints),
                config_system.live(|config| &config.shared.secrets),
            )),
        }
    }

    /// Adds the metrics outputs, before the pipelines that feed the metrics path are added.
    async fn add_baseline(
        &mut self, blueprint: &mut TopologyBlueprint, config_system: &ConfigurationSystem, shared: &SharedConfiguration,
    ) -> Result<(), GenericError> {
        match self {
            Self::None => Err(generic_error!(
                "The metrics pipeline is required, but no metrics path was prepared for it; this is a bug."
            )),
            Self::Plan(plan) => plan.add_outputs(blueprint, config_system, shared),
            #[cfg(test)]
            Self::Legacy(legacy) => legacy.add_baseline(blueprint, config_system, shared).await,
        }
    }

    /// Registers a component output that carries metrics into the metrics path, returning the component to connect it
    /// to, if the metrics path does not connect it itself.
    ///
    /// The plan-driven metrics path connects every output its frame reads itself, and checks that the frame reads
    /// every output registered here.
    fn metrics_source(&mut self, output: &str) -> Option<&'static str> {
        match self {
            Self::None => None,
            Self::Plan(plan) => {
                plan.add_source(output);
                None
            }
            #[cfg(test)]
            Self::Legacy(_) => Some(legacy::METRICS_DESTINATION),
        }
    }

    /// Returns the number of lanes the DogStatsD source's metrics split into: the plan-driven metrics path's producer
    /// lanes, and one otherwise.
    fn dogstatsd_metric_lanes(&self) -> NonZeroU16 {
        match self {
            Self::Plan(plan) => plan.dogstatsd_metric_lanes(),
            Self::None => NonZeroU16::MIN,
            #[cfg(test)]
            Self::Legacy(_) => NonZeroU16::MIN,
        }
    }

    /// Adds the DogStatsD part of the metrics path, once the DogStatsD source exists, and returns the handle of
    /// DogStatsD aggregation's context snapshots.
    ///
    /// The plan-driven path builds no component here: aggregation runs as a kernel in its isolation slot, which the
    /// frame lays out. Only the legacy path, a test reference, builds the native DogStatsD transforms from the
    /// blueprint and the configuration.
    #[cfg_attr(
        not(test),
        expect(
            unused_variables,
            reason = "only the legacy metrics path, a test reference, builds DogStatsD components from these"
        )
    )]
    fn add_dogstatsd(
        &mut self, blueprint: &mut TopologyBlueprint, typed: &SalukiConfiguration,
    ) -> Result<AggregateContextSnapshotHandle, GenericError> {
        match self {
            Self::None => Err(generic_error!(
                "The DogStatsD pipeline is enabled, but no metrics path was prepared for it; this is a bug."
            )),
            Self::Plan(plan) => plan.add_dogstatsd(),
            #[cfg(test)]
            Self::Legacy(legacy) => legacy.add_dogstatsd(blueprint, typed),
        }
    }

    /// Completes the metrics path, once every component feeding it exists.
    fn finish(self, blueprint: &mut TopologyBlueprint) -> Result<(), GenericError> {
        match self {
            Self::None => Ok(()),
            Self::Plan(plan) => plan.finish(blueprint),
            #[cfg(test)]
            Self::Legacy(_) => Ok(()),
        }
    }
}

/// Connects `output`, which carries metrics, into the metrics path.
fn connect_metrics_source(
    blueprint: &mut TopologyBlueprint, metrics: &mut MetricsWiring<'_>, output: &str,
) -> Result<(), GenericError> {
    if let Some(destination) = metrics.metrics_source(output) {
        blueprint.connect_components(output, destination)?;
    }
    Ok(())
}

async fn assemble_topology(
    config_system: &ConfigurationSystem, env_provider: &ADPEnvironmentProvider, component_registry: &ComponentRegistry,
    mut metrics: MetricsWiring<'_>,
) -> Result<(TopologyBlueprint, TopologyControlSurfaces), GenericError> {
    let config = config_system.config();
    let dp = DataPlaneConfiguration::from_configuration(&config);
    let mut blueprint = TopologyBlueprint::new(PRIMARY_TOPOLOGY, component_registry);
    blueprint.with_shutdown_timeout(dp.stop_timeout());
    let shared = config.shared.clone();

    let mut control_surfaces = TopologyControlSurfaces::default();

    // If no data pipelines are enabled, then there's nothing for us to do.
    if !dp.data_pipelines_enabled() {
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
    // is the only reason we're differentiating here. Connected mode is the exception: liveness deliberately
    // provisions metric and service-check output paths even for proxy-only topologies.
    if dp.metrics_pipeline_required()
        || dp.logs_pipeline_required()
        || dp.events_pipeline_required()
        || dp.service_checks_pipeline_required()
        || dp.traces_pipeline_required()
    {
        let dd_forwarder_config = metrics.dd_forwarder(config_system, &shared, dp.metrics_pipeline_required())?;
        blueprint.add_forwarder("dd_out", dd_forwarder_config)?;
    }

    if dp.metrics_pipeline_required() {
        metrics.add_baseline(&mut blueprint, config_system, &shared).await?;
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
        add_baseline_traces_pipeline_to_blueprint(&mut blueprint, config_system, env_provider, &mut metrics).await?;
    }

    // Connected topologies emit liveness through the metric and service-check baselines created above. Standalone
    // OTLP proxy mode must not construct liveness or output paths that it does not otherwise need.
    if !dp.standalone_mode() {
        let add_container_tags = config_system.config().shared.basic_telemetry.add_container_tags;
        add_liveness_source_to_blueprint(&mut blueprint, env_provider, add_container_tags, &mut metrics).await?;
    }

    // Now we move on to our actual data pipelines.
    if dp.checks_enabled() {
        add_checks_pipeline_to_blueprint(
            &mut blueprint,
            &config.domains.checks.ipc_endpoint,
            env_provider,
            &mut metrics,
        )
        .await?;
    }

    if dp.dogstatsd_enabled() {
        let dsd_control_surface =
            add_dsd_pipeline_to_blueprint(&mut blueprint, config_system, env_provider, &mut metrics).await?;
        control_surfaces.attach_dogstatsd(dsd_control_surface);
    }

    if dp.otlp_enabled() {
        add_otlp_pipeline_to_blueprint(&mut blueprint, config_system, env_provider, &mut metrics).await?;
    }

    if dp.metrics_pipeline_required() {
        metrics.finish(&mut blueprint)?;
    }

    Ok((blueprint, control_surfaces))
}

async fn add_liveness_source_to_blueprint(
    blueprint: &mut TopologyBlueprint, env_provider: &ADPEnvironmentProvider, add_container_tags: bool,
    metrics: &mut MetricsWiring<'_>,
) -> Result<(), GenericError> {
    let liveness_config = LivenessConfiguration::from_environment_provider(env_provider, add_container_tags).await?;

    blueprint
        .add_source("liveness_in", liveness_config)?
        .connect_components("liveness_in.service_checks", "dd_service_checks_encode")?;
    connect_metrics_source(blueprint, metrics, "liveness_in.metrics")?;

    Ok(())
}

async fn add_checks_pipeline_to_blueprint(
    blueprint: &mut TopologyBlueprint, checks_ipc_endpoint: &str, env_provider: &ADPEnvironmentProvider,
    metrics: &mut MetricsWiring<'_>,
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
        .connect_components("checks_ipc_in.logs", "dd_logs_encode")?
        .connect_components("checks_ipc_in.events", "dd_events_encode")?
        .connect_components("checks_ipc_in.service_checks", "dd_service_checks_encode")?;
    connect_metrics_source(blueprint, metrics, "checks_ipc_in.metrics")?;

    Ok(())
}

/// Returns the encoder of the main metrics output: every endpoint except those an endpoint routing policy selects.
pub(crate) fn main_metrics_encoder(
    shared: &SharedConfiguration, routing: &MetricsEndpointRoutingConfiguration,
) -> DatadogMetricsConfiguration {
    let mut dd_metrics_config = DatadogMetricsConfiguration::from_configuration(shared);
    if !routing.selected_endpoints().is_empty() {
        dd_metrics_config = dd_metrics_config.with_endpoint_routing(MetricsEndpointRouting::AllExcept(
            routing.selected_endpoints().iter().cloned().collect(),
        ));
    }
    dd_metrics_config
}

/// Returns the encoder of an endpoint routing group's branch, which sends to the group's endpoints only.
///
/// The group's filter, which selects what its policy allows, is a gateway of the metrics path, in front of the encoder.
pub(crate) fn routing_group_encoder(
    shared: &SharedConfiguration, policy: &EndpointAllowlistGroup,
) -> DatadogMetricsConfiguration {
    DatadogMetricsConfiguration::from_configuration(shared)
        .with_endpoint_routing(MetricsEndpointRouting::Only(policy.endpoints.iter().cloned().collect()))
}

/// The native components of a metrics branch behind a gateway: the branch's encoder and its forwarder.
///
/// The gateway, which selects what reaches the branch, is part of the metrics path, in front of the encoder.
#[cfg(test)]
pub(crate) struct MetricsBranch<F> {
    /// The branch's encoder.
    pub(crate) encoder: DatadogMetricsConfiguration,

    /// The branch's forwarder.
    pub(crate) forwarder: F,
}

/// Returns the Multi-Region Failover metrics endpoint and key, if its static endpoint override resolves.
///
/// Logs a warning when Multi-Region Failover is enabled but the endpoint cannot be resolved: the plan-driven path's
/// `mrf_dd_out` has no recipient, and the legacy path no branch, until it can.
pub(crate) fn mrf_metrics_endpoint(mrf: &multi_region_failover::Domain) -> Option<(String, String)> {
    let mrf_config = MrfConfiguration::from_configuration(mrf);
    let endpoint = mrf_config.metrics_endpoint_override();
    if endpoint.is_none() && mrf_config.is_enabled() {
        warn!(
            "Multi-Region Failover is enabled, but multi_region_failover.api_key and one of \
             multi_region_failover.dd_url or multi_region_failover.site are required for metrics forwarding. The MRF \
             metrics branch is not wired, and primary forwarding continues. The branch is wired as soon as the MRF \
             endpoint settings are configured; no restart is needed."
        );
    }
    endpoint
}

/// Returns the Multi-Region Failover metrics branch, if it is wired: when its static endpoint override resolves.
///
/// Logs a warning when Multi-Region Failover is enabled but the branch cannot be wired.
#[cfg(test)]
pub(crate) fn mrf_metrics_branch(
    config_system: &ConfigurationSystem, shared: &SharedConfiguration, mrf: &multi_region_failover::Domain,
) -> Option<MetricsBranch<DatadogForwarderConfiguration>> {
    let (mrf_dd_url, mrf_api_key) = mrf_metrics_endpoint(mrf)?;

    let encoder =
        DatadogMetricsConfiguration::from_configuration(shared).with_metrics_endpoint_override(mrf_dd_url.clone());
    let forwarder = DatadogForwarderConfiguration::for_endpoint_override(
        shared,
        mrf_dd_url,
        mrf_api_key,
        config_system.live(|config| &config.domains.multi_region_failover.api_key),
        config_system.live(|config| &config.shared.secrets),
    );

    Some(MetricsBranch { encoder, forwarder })
}

/// Returns whether autoscaling failover requests its metrics branch, and the Cluster Agent's endpoint and token,
/// `cluster_agent`, are known.
///
/// Logs a warning when the branch is requested but the Cluster Agent cannot be reached: the plan-driven path's
/// `cluster_agent_out` has no recipient, and the legacy path no branch, until it can. `endpoint_at_startup` says
/// whether a Cluster Agent endpoint resolved at startup, which only a restart changes: with one, the branch is wired
/// as soon as forwarding to the Cluster Agent is enabled with a token; without one, only after a restart.
pub(crate) fn autoscaling_failover_wired(
    shared: &SharedConfiguration, cluster_agent: Option<&(String, String)>, endpoint_at_startup: bool,
) -> bool {
    let af_config = AutoscalingFailoverConfiguration::new(
        shared.autoscaling_failover.enabled,
        shared.autoscaling_failover.metrics.clone(),
    );
    if cluster_agent.is_none() && af_config.is_branch_requested() {
        if endpoint_at_startup {
            warn!(
                "autoscaling.failover is enabled, but cluster_agent.enabled and cluster_agent.auth_token are required. \
                 The autoscaling failover metrics branch is not wired, and primary forwarding continues. The branch \
                 is wired as soon as both are set; no restart is needed."
            );
        } else {
            warn!(
                "autoscaling.failover is enabled, but no Cluster Agent endpoint resolved at startup. Set \
                 cluster_agent.url, or provide Kubernetes service env vars for cluster_agent.kubernetes_service_name, \
                 and restart: the endpoint is resolved once, at startup. Until then the autoscaling failover metrics \
                 branch is not wired, and primary forwarding continues."
            );
        }
    }
    cluster_agent.is_some() && af_config.is_branch_requested()
}

/// Returns the autoscaling failover metrics branch, if it is wired: when autoscaling failover requests it, and the
/// Cluster Agent's endpoint and token, `cluster_agent`, are known.
///
/// Logs a warning when the branch is requested but the Cluster Agent cannot be reached.
///
/// # Errors
///
/// Returns an error if the Cluster Agent forwarder cannot be configured, such as for a token that cannot be sent.
#[cfg(test)]
pub(crate) fn autoscaling_failover_branch(
    shared: &SharedConfiguration, cluster_agent: Option<(String, String)>,
) -> Result<Option<MetricsBranch<saluki_components::forwarders::ClusterAgentForwarderConfiguration>>, GenericError> {
    if !autoscaling_failover_wired(shared, cluster_agent.as_ref(), cluster_agent.is_some()) {
        return Ok(None);
    }
    let Some((ca_url, ca_token)) = cluster_agent else {
        return Ok(None);
    };

    let encoder = DatadogMetricsConfiguration::from_configuration(shared).with_v2_series_only();
    let forwarder =
        saluki_components::forwarders::ClusterAgentForwarderConfiguration::from_configuration(shared, ca_url, ca_token)
            .error_context("Failed to configure Cluster Agent forwarder.")?;

    Ok(Some(MetricsBranch { encoder, forwarder }))
}

/// Returns the configuration of the native DogStatsD aggregation transform, and the handle to its context snapshots:
/// those of the legacy topology, which tests compare the plan-driven one against. The plan-driven topology runs
/// aggregation as a kernel in its isolation slot, configured by lowering.
///
/// # Errors
///
/// Returns an error if the configured histogram statistics cannot be computed.
#[cfg(test)]
pub(crate) fn dogstatsd_aggregate_configuration(
    typed: &SalukiConfiguration,
) -> Result<
    (
        saluki_components::transforms::AggregateConfiguration,
        AggregateContextSnapshotHandle,
    ),
    GenericError,
> {
    use saluki_components::transforms::{
        aggregate_context_snapshot_channel, AggregateConfiguration, HistogramConfiguration,
    };

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
    Ok((dsd_agg_config, dsd_context_snapshot_handle))
}

fn add_baseline_logs_pipeline_to_blueprint(
    blueprint: &mut TopologyBlueprint, shared: &SharedConfiguration,
) -> Result<(), GenericError> {
    // Create the back half of the logs processing pipeline.
    let compression = &shared.endpoints.compression;
    let dd_logs_config = BufferedIncrementalConfiguration::from_encoder_builder(DatadogLogsConfiguration::new(
        &compression.compressor_kind,
        compression.effective_zstd_level(),
    ));

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
    let dd_events_config = BufferedIncrementalConfiguration::from_encoder_builder(DatadogEventsConfiguration {
        max_payload_size: metrics_encoding.max_payload_size,
        max_uncompressed_payload_size: metrics_encoding.max_uncompressed_payload_size,
        compressor_kind: compression.compressor_kind.clone(),
        zstd_level: compression.effective_zstd_level(),
        log_payloads: metrics_encoding.log_payloads,
    });

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
    let dd_service_checks_config =
        BufferedIncrementalConfiguration::from_encoder_builder(DatadogServiceChecksConfiguration {
            max_payload_size: metrics_encoding.max_payload_size,
            max_uncompressed_payload_size: metrics_encoding.max_uncompressed_payload_size,
            compressor_kind: compression.compressor_kind.clone(),
            zstd_level: compression.effective_zstd_level(),
            log_payloads: metrics_encoding.log_payloads,
        });

    blueprint
        .add_encoder("dd_service_checks_encode", dd_service_checks_config)?
        .connect_components("dd_service_checks_encode", "dd_out")?;

    Ok(())
}

async fn add_baseline_traces_pipeline_to_blueprint(
    blueprint: &mut TopologyBlueprint, config_system: &ConfigurationSystem, env_provider: &ADPEnvironmentProvider,
    metrics: &mut MetricsWiring<'_>,
) -> Result<(), GenericError> {
    let config = config_system.config();
    let dd_traces_config = DatadogTraceConfiguration::from_configuration(
        &config.domains.traces,
        &config.domains.otlp.traces,
        &config.shared,
    )
    .with_environment_provider(env_provider.clone())
    .await?;
    let trace_sampler_config =
        TraceSamplerConfiguration::from_configuration(&config.domains.traces, &config.domains.otlp.traces);

    let trace_obfuscation_config =
        TraceObfuscationConfiguration::from_configuration(&config.domains.traces.obfuscation);
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

    let ottl_filter_config = OttlFilterConfiguration::from_configuration(&config.domains.traces.ottl_filter);

    let ottl_transform_config = OttlTransformConfiguration::from_configuration(&config.domains.traces.ottl_transform);

    let dd_traces_enrich_config = ChainedConfiguration::default()
        .with_transform_builder("ottl_filter", ottl_filter_config)
        .with_transform_builder("ottl_transform", ottl_transform_config)
        .with_transform_builder("apm_onboarding", ApmOnboardingConfiguration)
        .with_transform_builder("trace_obfuscation", trace_obfuscation_config)
        .with_transform_builder("trace_tag_replacer", trace_tag_replacer_config);
    let apm_stats_transform_config = ApmStatsTransformConfiguration::from_configuration(&config.domains.traces)
        .with_environment_provider(env_provider.clone())
        .await?;
    let dd_apm_stats_encoder =
        DatadogApmStatsEncoderConfiguration::from_configuration(&config.domains.traces, &config.shared)
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
        .connect_components("dd_apm_stats", "dd_stats_encode")?
        .connect_components(["dd_traces_encode", "dd_stats_encode"], "dd_out")?;
    // The sampler reports its sampling telemetry as metric events through this dedicated output, delivering the
    // metrics through the metrics pipeline to the backend.
    connect_metrics_source(blueprint, metrics, "trace_sampler.metrics")?;

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

/// Maps the configured tag cardinality onto the cardinality the context resolver understands.
fn origin_tag_cardinality(cardinality: dogstatsd::OriginTagCardinality) -> OriginTagCardinality {
    match cardinality {
        dogstatsd::OriginTagCardinality::None => OriginTagCardinality::None,
        dogstatsd::OriginTagCardinality::Low => OriginTagCardinality::Low,
        dogstatsd::OriginTagCardinality::Orchestrator => OriginTagCardinality::Orchestrator,
        dogstatsd::OriginTagCardinality::High => OriginTagCardinality::High,
    }
}

/// Builds the DogStatsD source configuration from the typed model and the running environment.
///
/// The caller creates `capture_control` and `replay_control` so that it keeps the handles the source binds to, which is
/// how the capture and replay API surfaces reach the running source.
fn dogstatsd_source_configuration(
    config: &SalukiConfiguration, env_provider: &ADPEnvironmentProvider, default_hostname: MetaString,
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
        pipe_name: listeners.pipe_name.clone(),
        windows_pipe_security_descriptor: listeners.windows_pipe_security_descriptor.clone(),
        disable_verbose_logs: dogstatsd.debug_log.disable_verbose_logs,
        eol_required: listeners.eol_required.clone(),
        bind_host: listeners.bind_host.clone(),
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
        // One lane until the metrics path splits the source's metrics into its producer lanes.
        metric_lanes: NonZeroU16::MIN,
    }
}

/// Builds the DogStatsD mapper configuration from the typed model.
pub(crate) fn dogstatsd_mapper_configuration(mapper: &dogstatsd::Mapper) -> DogStatsDMapperConfiguration {
    let profiles = mapper
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
    DogStatsDMapperConfiguration::new(mapper.string_interner_size_bytes, mapper.cache_size, profiles)
}

async fn add_dsd_pipeline_to_blueprint(
    blueprint: &mut TopologyBlueprint, config_system: &ConfigurationSystem, env_provider: &ADPEnvironmentProvider,
    metrics: &mut MetricsWiring<'_>,
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
    //               │      │  DSD pre-aggregation│    │     DSD Service     │    │     DSD Events      │
    //               │      │      (region)       │    │    Checks (encoder) │    │      (encoder)      │
    //               │      │ mapper, prefix and  │    └─────────────────────┘    └─────────────────────┘
    //               │      │ filter, tag filter  │               │                          │
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
    //
    // The metrics path, from the source's `metrics` output to the encoders, is the metrics plan's: the metrics wiring
    // adds it. Everything else is added here.

    let default_hostname = env_provider
        .host()
        .get_hostname()
        .await
        .error_context("Failed to get default hostname for DogStatsD source.")?;
    let typed = config_system.config();
    let dsd_capture_control = DogStatsDCaptureControl::default();
    let dsd_replay_control = DogStatsDReplayControl::default();
    // The source's metrics split into the metrics path's producer lanes, each an output of its own, which the taps
    // below read every one of.
    let metric_lanes = metrics.dogstatsd_metric_lanes();
    let dsd_config = dogstatsd_source_configuration(
        &typed,
        env_provider,
        default_hostname.into(),
        dsd_capture_control.clone(),
        dsd_replay_control.clone(),
    )
    .with_metric_lanes(metric_lanes);
    blueprint.add_source("dsd_in", dsd_config)?;
    let metric_outputs = (0..metric_lanes.get())
        .map(|lane| format!("dsd_in.{}", dogstatsd_metric_lane_output(lane, metric_lanes)))
        .collect::<Vec<_>>();
    let dsd_context_snapshot_handle = metrics.add_dogstatsd(blueprint, &typed)?;

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

    let dsd_debug_log_config = DogStatsDDebugLogConfiguration::new(
        config_system.live(|config| &config.domains.dogstatsd.debug_log.metrics_stats_enable),
        debug_log_file,
        debug_log.log_file_max_size,
        debug_log.log_file_max_rolls,
    );
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
        .add_transform("events_enrich", events_enrich_config)?
        .add_transform("service_checks_enrich", service_checks_enrich_config)?
        .add_destination("dsd_stats_out", dsd_stats_config)?
        // Events.
        .connect_components_in_order(["dsd_in.events", "events_enrich", "dd_events_encode"])?
        // Service checks.
        .connect_components_in_order([
            "dsd_in.service_checks",
            "service_checks_enrich",
            "dd_service_checks_encode",
        ])?;
    // DogStatsD Stats, a tap on every metric the source receives.
    for output in &metric_outputs {
        blueprint.connect_components(output.as_str(), "dsd_stats_out")?;
    }

    if debug_log.logging_enabled {
        // DogStatsD debug log, another tap.
        blueprint.add_destination("dsd_debug_log_out", dsd_debug_log_config)?;
        for output in &metric_outputs {
            blueprint.connect_components(output.as_str(), "dsd_debug_log_out")?;
        }
    }
    Ok(DogStatsDControlSurface {
        stats_api_handler,
        capture_api_handler,
        replay_api_handler,
        context_dump_api_handler,
    })
}

async fn add_otlp_pipeline_to_blueprint(
    blueprint: &mut TopologyBlueprint, config_system: &ConfigurationSystem, env_provider: &ADPEnvironmentProvider,
    metrics: &mut MetricsWiring<'_>,
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
        let otlp_relay_config = OtlpRelayConfiguration::from_configuration(&config.domains.otlp.receiver);
        let otlp_decoder_config = OtlpDecoderConfiguration::from_configuration(&config.domains.otlp.traces)
            .with_max_resource_len(config.domains.traces.max_resource_len);

        let local_agent_otlp_forwarder_config =
            OtlpForwarderConfiguration::from_configuration(&config.domains.otlp.traces, core_agent_otlp_grpc_endpoint);

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
        let otlp_config = OtlpConfiguration::from_configuration(&config.domains.otlp, env_provider.workload().clone())
            .with_max_resource_len(config.domains.traces.max_resource_len)
            .with_static_metric_tags(static_tags)
            .with_default_hostname(default_hostname);

        blueprint
            // Components.
            .add_source("otlp_in", otlp_config)?
            // Logs and traces.
            .connect_components("otlp_in.logs", "dd_logs_encode")?
            .connect_components("otlp_in.traces", "traces_enrich")?;
        // Metrics enter the metrics path after DogStatsD aggregation, at enrichment, to avoid transforming counters
        // into rates.
        connect_metrics_source(blueprint, metrics, "otlp_in.metrics")?;
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
    use std::{
        num::{NonZeroU64, NonZeroUsize},
        path::{Path, PathBuf},
        sync::Mutex,
        time::Duration,
    };

    use agent_data_plane_config::{
        domains::dogstatsd::{
            FilterAction, MetricFilter, MetricTagFilterEntry, MetricTagValueAllowlistEntry, TagValueMismatchAction,
        },
        Live,
    };
    use async_trait::async_trait;
    use http::{Request, StatusCode};
    use http_body_util::{BodyExt as _, Empty};
    use hyper::body::Bytes;
    use saluki_api::{response::Response, APIHandler as _};
    use saluki_components::transforms::{
        aggregate_context_snapshot_channel, aggregate_context_snapshot_channel_for_test, AggregateConfiguration,
        AggregateContextSnapshotEntry, AggregateMetricType, ChainedConfiguration, DogStatsDMapperConfiguration,
        DogStatsDMapperProfile, DogStatsDMetricMapping, HistogramConfiguration,
    };
    use saluki_core::{
        accounting::{ComponentRegistry, MemoryBounds, MemoryBoundsBuilder, MemoryLimiter},
        components::{
            destinations::{Destination, DestinationBuilder, DestinationContext},
            sources::{Source, SourceBuilder, SourceContext},
            BuildContext,
        },
        data_model::event::{
            metric::{context::Context, Metric},
            Event, EventType,
        },
        health::HealthRegistry,
        runtime::{state::ResourceRegistry, Supervisor},
        topology::{OutputDefinition, TopologyBlueprint},
    };
    use saluki_error::{generic_error, GenericError};
    use stringtheory::MetaString;
    use tokio::sync::{mpsc, oneshot};
    use tower::ServiceExt as _;

    use crate::{
        components::{
            dogstatsd_no_agg_split::DogStatsDNoAggSplitConfiguration,
            dogstatsd_prefix_filter::DogStatsDPrefixFilterConfiguration, tag_filterlist::TagFilterlistConfiguration,
        },
        dogstatsd_contexts::DogStatsDContextDumpAPIHandler,
    };

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
    async fn retained_context_identity_follows_dogstatsd_post_processing() {
        tokio::time::timeout(Duration::from_secs(5), async {
            let mapper = DogStatsDMapperConfiguration::new(
                NonZeroUsize::new(64 * 1024).expect("not zero"),
                1_000,
                vec![DogStatsDMapperProfile {
                    name: "retained-context-test".to_string(),
                    prefix: "raw.requests.".to_string(),
                    mappings: vec![DogStatsDMetricMapping {
                        metric_match: "raw.requests.*".to_string(),
                        match_type: String::new(),
                        name: "mapped.requests".to_string(),
                        tags: [("route".to_string(), "$1".to_string())].into(),
                    }],
                }],
            );
            let mapper_chain = ChainedConfiguration::default().with_transform_builder("dogstatsd_mapper", mapper);
            let prefix_filter = DogStatsDPrefixFilterConfiguration::new(
                "tenant".to_string(),
                Vec::new(),
                Live::new_fixed(MetricFilter {
                    values: vec!["tenant.raw.blocked".to_string()],
                    match_prefix: false,
                }),
            );
            let tag_filter = TagFilterlistConfiguration::new(
                Live::new_fixed(vec![MetricTagFilterEntry {
                    metric_name: "tenant.mapped.requests".to_string(),
                    action: FilterAction::Exclude,
                    tags: vec!["remove".to_string()],
                }]),
                &[MetricTagValueAllowlistEntry {
                    metric_prefix: "tenant.mapped.".to_string(),
                    tag_name: "customer_id".to_string(),
                    values: vec!["top-1".to_string()],
                    on_miss: TagValueMismatchAction::Remove,
                    replacement: "other".to_string(),
                }],
                0,
            )
            .expect("tag filter configuration should be valid");
            let hist_config = HistogramConfiguration::try_new(
                &[
                    "max".to_string(),
                    "median".to_string(),
                    "avg".to_string(),
                    "count".to_string(),
                ],
                &["0.95".to_string()],
                false,
                String::new(),
            )
            .expect("histogram settings should be valid");
            let (snapshot_handle, context_snapshot_receiver) = aggregate_context_snapshot_channel();
            let aggregate = AggregateConfiguration {
                window_duration_seconds: NonZeroU64::new(10).expect("not zero"),
                // Longer than the test, so the window stays open and contexts stay retained.
                primary_flush_interval: Duration::from_secs(60),
                context_limit: 1_000_000,
                flush_open_windows: false,
                counter_expiry_seconds: Some(300),
                hist_config,
                context_snapshot_receiver,
            };

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
                .with_resource_registry(ResourceRegistry::new())
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
            let input_context = Context::from_static_parts(
                "raw.requests.checkout",
                &[
                    "customer_id:top-1",
                    "customer_id:long-tail",
                    "keep:client",
                    "remove:secret",
                ],
            );
            events_tx
                .send(Event::Metric(Metric::counter(input_context.clone(), 1.0)))
                .await
                .expect("controlled source should accept the retained metric");

            let expected_context = Context::from_static_parts(
                "tenant.mapped.requests",
                &["customer_id:top-1", "keep:client", "route:checkout"],
            );
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
            assert_eq!(snapshot[0].context().tags().len(), 3);
            assert_eq!(
                snapshot[0]
                    .context()
                    .tags()
                    .get_single_tag("customer_id")
                    .and_then(|tag| tag.value()),
                Some("top-1")
            );
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
    async fn no_agg_split_excludes_timestamped_metrics_from_tag_filterlist() {
        tokio::time::timeout(Duration::from_secs(5), async {
            let no_agg_split = DogStatsDNoAggSplitConfiguration::new(
                Duration::from_millis(50),
                NonZeroU64::new(10).expect("not zero"),
            );
            let tag_filter = TagFilterlistConfiguration::new(
                Live::new_fixed(vec![MetricTagFilterEntry {
                    metric_name: "app.requests".to_string(),
                    action: FilterAction::Exclude,
                    tags: vec!["remove".to_string()],
                }]),
                &[],
                0,
            )
            .expect("tag filter configuration should be valid");
            let hist_config = HistogramConfiguration::try_new(&[], &[], false, String::new())
                .expect("histogram settings should be valid");
            let (snapshot_handle, context_snapshot_receiver) = aggregate_context_snapshot_channel();
            let aggregate = AggregateConfiguration {
                window_duration_seconds: NonZeroU64::new(10).expect("not zero"),
                // Longer than the test, so the window stays open and contexts stay retained.
                primary_flush_interval: Duration::from_secs(60),
                context_limit: 1_000_000,
                flush_open_windows: false,
                counter_expiry_seconds: Some(300),
                hist_config,
                context_snapshot_receiver,
            };

            let (events_tx, events_rx) = mpsc::channel(2);
            let source = ControlledMetricSourceBuilder {
                events: Mutex::new(Some(events_rx)),
                outputs: vec![OutputDefinition::default_output(EventType::Metric)],
            };
            let (passthrough_tx, mut passthrough_rx) = mpsc::unbounded_channel();
            let passthrough_destination = CollectingMetricDestinationBuilder { sender: passthrough_tx };

            let component_registry = ComponentRegistry::default();
            let mut blueprint = TopologyBlueprint::new("dogstatsd_no_agg_split_filterlist", &component_registry);
            blueprint
                .add_source("source", source)
                .expect("controlled source should be accepted")
                .add_transform("no_agg_split", no_agg_split)
                .expect("no-agg split should be accepted")
                .add_transform("tag_filter", tag_filter)
                .expect("tag filter should be accepted")
                .add_transform("aggregate", aggregate)
                .expect("aggregate should be accepted")
                .add_destination("aggregate_out", DrainingMetricDestinationBuilder)
                .expect("draining destination should be accepted")
                .add_destination("passthrough_out", passthrough_destination)
                .expect("collecting destination should be accepted");
            blueprint
                .connect_components_in_order(["source", "no_agg_split", "tag_filter", "aggregate", "aggregate_out"])
                .expect("no-agg split topology should connect")
                .connect_components("no_agg_split.passthrough", "passthrough_out")
                .expect("passthrough output should connect");
            blueprint
                .with_health_registry(HealthRegistry::new())
                .with_memory_limiter(MemoryLimiter::noop())
                .with_resource_registry(ResourceRegistry::new())
                .with_ambient_worker_pool();

            let mut supervisor =
                Supervisor::new("dogstatsd-no-agg-split-filterlist").expect("test supervisor should be created");
            supervisor.add_worker(blueprint);
            let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
            let topology_task = tokio::spawn(async move { supervisor.run_with_shutdown(shutdown_rx).await });

            let timestamped_context =
                Context::from_static_parts("app.requests", &["keep:client", "remove:secret", "which:timestamped"]);
            events_tx
                .send(Event::Metric(Metric::counter(
                    timestamped_context.clone(),
                    (1_700_000_000, 1.0),
                )))
                .await
                .expect("controlled source should accept the timestamped metric");

            let non_timestamped_context = Context::from_static_parts(
                "app.requests",
                &["keep:client", "remove:secret", "which:non_timestamped"],
            );
            events_tx
                .send(Event::Metric(Metric::counter(non_timestamped_context.clone(), 1.0)))
                .await
                .expect("controlled source should accept the non-timestamped metric");

            // The timestamped metric bypasses the tag filterlist entirely, so it should reach the passthrough
            // destination with its `remove` tag intact.
            let passthrough_metric = passthrough_rx
                .recv()
                .await
                .expect("passthrough destination should receive the timestamped metric");
            assert_eq!(
                passthrough_metric
                    .context()
                    .tags()
                    .get_single_tag("remove")
                    .and_then(|tag| tag.value()),
                Some("secret"),
                "timestamped metrics must not be filtered by the tag filterlist"
            );

            // The non-timestamped metric goes through the tag filterlist before aggregation, so its `remove` tag
            // should be stripped from the resulting aggregated context.
            let expected_context =
                Context::from_static_parts("app.requests", &["keep:client", "which:non_timestamped"]);
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
            assert!(snapshot.iter().all(|entry| entry.context() != &non_timestamped_context));
            assert_eq!(snapshot.len(), 1);
            assert!(snapshot[0].context().tags().get_single_tag("remove").is_none());

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
        .expect("no-agg split filterlist exclusion test should complete without hanging");
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

        async fn build(&self, _context: BuildContext) -> Result<Box<dyn Source + Send>, GenericError> {
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

        async fn build(&self, _context: BuildContext) -> Result<Box<dyn Destination + Send>, GenericError> {
            Ok(Box::new(DrainingMetricDestination))
        }
    }

    impl MemoryBounds for DrainingMetricDestinationBuilder {
        fn specify_bounds(&self, _builder: &mut MemoryBoundsBuilder) {}
    }

    struct CollectingMetricDestination {
        sender: mpsc::UnboundedSender<Metric>,
    }

    #[async_trait]
    impl Destination for CollectingMetricDestination {
        async fn run(self: Box<Self>, mut context: DestinationContext) -> Result<(), GenericError> {
            while let Some(events) = context.events().next().await {
                for event in events {
                    if let Some(metric) = event.try_into_metric() {
                        let _ = self.sender.send(metric);
                    }
                }
            }
            Ok(())
        }
    }

    struct CollectingMetricDestinationBuilder {
        sender: mpsc::UnboundedSender<Metric>,
    }

    #[async_trait]
    impl DestinationBuilder for CollectingMetricDestinationBuilder {
        fn input_event_type(&self) -> EventType {
            EventType::Metric
        }

        async fn build(&self, _context: BuildContext) -> Result<Box<dyn Destination + Send>, GenericError> {
            Ok(Box::new(CollectingMetricDestination {
                sender: self.sender.clone(),
            }))
        }
    }

    impl MemoryBounds for CollectingMetricDestinationBuilder {
        fn specify_bounds(&self, _builder: &mut MemoryBoundsBuilder) {}
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
