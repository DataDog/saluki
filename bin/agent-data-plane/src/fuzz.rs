#![allow(dead_code)]
use std::sync::LazyLock;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use arbitrary::{Arbitrary, Unstructured};
use barkus_core::{generate::decode, ir::GrammarIr, profile::Profile};
use bytes::Bytes;
use futures::FutureExt as _;
use libfuzzer_sys::fuzz_target;
use memory_accounting::ComponentRegistry;
use nix::sys::signal as nix_signal;
use rand::{rngs::SmallRng, SeedableRng};
use saluki_app::{
    bootstrap::AppBootstrapper,
    memory::{initialize_memory_bounds, MemoryBoundsConfiguration},
    metrics::emit_startup_metrics,
};
use saluki_components::{
    config::{DatadogRemapper, KEY_ALIASES},
    decoders::otlp::OtlpDecoderConfiguration,
    destinations::DogStatsDStatisticsConfiguration,
    encoders::{
        BufferedIncrementalConfiguration, DatadogEventsConfiguration, DatadogMetricsConfiguration,
        DatadogServiceChecksConfiguration,
    },
    forwarders::{DatadogConfiguration, OtlpForwarderConfiguration},
    relays::otlp::OtlpRelayConfiguration,
    sources::{DogStatsDConfiguration, OtlpConfiguration},
    transforms::{
        AggregateConfiguration, ChainedConfiguration, DogStatsDMapperConfiguration, DogStatsDPrefixFilterConfiguration,
        HostEnrichmentConfiguration, HostTagsConfiguration,
    },
};
use saluki_config::{ConfigurationLoader, GenericConfiguration};
use saluki_core::health::HealthRegistry;
use saluki_core::runtime::SupervisorError;
use saluki_core::topology::TopologyBlueprint;
use saluki_env::EnvironmentProvider as _;
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use tokio::{
    net::UdpSocket,
    process::Command,
    select,
    time::{self, interval},
};
use tracing::{error, info, warn};

pub(crate) mod components;
pub(crate) mod config;
pub(crate) mod env_provider;
pub(crate) mod internal;
pub(crate) mod state;

use crate::{
    components::tag_filterlist::TagFilterlistConfiguration,
    internal::{create_internal_supervisor, platform::PlatformSettings},
};
use crate::{config::DataPlaneConfiguration, env_provider::ADPEnvironmentProvider};

const DD_YAML_CONFIG: &str = "bin/agent-data-plane/fuzz_datadog_config.yaml";

// TODO: fuzz entry modeled after the content of that command
// extract stuff to get a function that gives us enough to build the topology
// get rid of the supervisor - we don't need it
// the topology should shut itself down after a fixed delay
// its shutdown sequence will include a flush to the intake ("flush open bucket")
// ( or a sleep of 17secs after sending the payloads )
// tokio runtime
// spawn topology
// call the network calls to send packets to the topology
/// Entrypoint for the `run` commands.
pub async fn handle_run_command(
    shutdown: tokio::sync::oneshot::Receiver<()>, started: Instant,
) -> Result<(), GenericError> {
    let bootstrap_config = ConfigurationLoader::default()
        .with_key_aliases(KEY_ALIASES)
        .from_yaml(&DD_YAML_CONFIG)
        .error_context("Failed to load Datadog Agent configuration file during bootstrap.")?
        .add_providers([DatadogRemapper::new()])
        .from_environment(PlatformSettings::get_env_var_prefix())
        .error_context("Environment variable prefix should not be empty.")?
        .with_default_secrets_resolution()
        .await
        .error_context("Failed to load secrets resolution configuration during bootstrap.")?
        .bootstrap_generic();

    let _bs_guard = AppBootstrapper::from_configuration(&bootstrap_config)?
        .bootstrap()
        .await?;
    info!("Agent Data Plane starting...");

    // Load our "bootstrap" configuration.
    //
    // If remote agent mode is enabled, we'll register as a remote agent, which will unlock the ability to receive
    // configuration updates from the Core Agent, which we'll use to build our final, updated configuration. Otherwise,
    // we keep the bootstrap configuration and use it as-is.
    let bootstrap_dp_config = DataPlaneConfiguration::from_configuration(&bootstrap_config)
        .error_context("Failed to load data plane configuration.")?;

    let in_standalone_mode = bootstrap_dp_config.standalone_mode();
    // simpl: no remote loading, only the local config
    assert!(in_standalone_mode);
    assert!(!bootstrap_dp_config.remote_agent_enabled());
    assert!(!bootstrap_dp_config.use_new_config_stream_endpoint());

    let (config, dp_config) = (bootstrap_config, bootstrap_dp_config);

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

    // Create the internal supervisor (control plane + observability)
    let mut internal_supervisor = create_internal_supervisor(
        &config,
        &dp_config,
        &component_registry,
        health_registry.clone(),
        env_provider,
        dsd_stats_config,
        None,
    )
    .await
    .error_context("Failed to create internal supervisor.")?;

    // Create shutdown channel for the internal supervisor - we'll drive it in the main select loop
    let (internal_shutdown_tx, internal_shutdown_rx) = tokio::sync::oneshot::channel();
    let internal_supervisor_fut = internal_supervisor.run_with_shutdown(internal_shutdown_rx).fuse();
    tokio::pin!(internal_supervisor_fut);

    // Run memory bounds validation to ensure that we can launch the topology with our configured memory limit, if any.
    let bounds_config = MemoryBoundsConfiguration::try_from_config(&config)?;
    let memory_limiter = initialize_memory_bounds(bounds_config, component_registry.root())?;

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

    let mut topology_failed = false;
    let mut internal_supervisor_failed = false;
    select! {
        result = &mut internal_supervisor_fut => {
            match result {
                Err(SupervisorError::FailedToInitialize { child_name, source }) => {
                    error!(child_name, "Internal supervisor failed to initialize: {}. Shutting down...", source);
                    internal_supervisor_failed = true;
                }
                // If we haven't hit an initialization error -- which implies an error we can't really recover from --
                // then just log for now, until we fully migrate everything over to the supervisor-based approach and
                // can dial in our supervisor configuration.
                //
                // For right now, this matches the previous behavior where the process would exit if we couldn't
                // configure/spawn the control plane or internal observability pipeline, but the process is unaffected
                // if either of those components fail at _runtime_.
                Err(e) => {
                    warn!("Internal supervisor exited: {}", e);
                }
                Ok(()) => {
                    warn!("Internal supervisor exited unexpectedly.");
                }
            }
        }
        _ = shutdown => {info!("shutdown request");},
        _ = running_topology.wait_for_unexpected_finish() => {
            error!("Topology component unexpectedly finished. Shutting down...");
            topology_failed = true;
        },
        _ = tokio::signal::ctrl_c() => {
            info!("Received SIGINT, shutting down...");
        }
    }

    // Shutdown the primary topology
    let topology_result = running_topology.shutdown_with_timeout(Duration::from_secs(30)).await;

    // Signal the internal supervisor to shutdown (if still running) and drive it to completion.
    // If the supervisor already exited (i.e., the select! above matched its branch), both the send
    // and await resolve immediately — the send is a no-op and the future is already complete.
    let _ = internal_shutdown_tx.send(());
    let _ = internal_supervisor_fut.await;

    // Figure out the final "result" of this run: did something fail? did we stop cleanly?
    //
    // We prefer to return errors from the topology failing over the internal supervisor failing, since that matters
    // more in terms of understanding the state of the process when it exited.
    match topology_result {
        Ok(()) => {
            if topology_failed {
                warn!("Topology shutdown complete despite error(s).");
            } else {
                info!("Topology shutdown successfully.");
            }

            if internal_supervisor_failed {
                Err(generic_error!("Internal supervisor failed to initialize."))
            } else {
                Ok(())
            }
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
    if dp_config.metrics_pipeline_required()
        || dp_config.logs_pipeline_required()
        || dp_config.traces_pipeline_required()
    {
        let dd_forwarder_config =
            DatadogConfiguration::from_configuration(config).error_context("Failed to configure Datadog forwarder.")?;
        blueprint.add_forwarder("dd_out", dd_forwarder_config)?;
    }

    if dp_config.metrics_pipeline_required() {
        add_baseline_metrics_pipeline_to_blueprint(&mut blueprint, config, dp_config, env_provider).await?;
    }

    if dp_config.logs_pipeline_required() {
        error!("[Fuzz] logs not supported");
    }

    if dp_config.traces_pipeline_required() {
        error!("[Fuzz] traces not supported");
    }

    // Now we move on to our actual data pipelines.
    if dp_config.dogstatsd().enabled() {
        add_dsd_pipeline_to_blueprint(&mut blueprint, config, env_provider, dsd_stats_config).await?;
    }

    if dp_config.otlp().enabled() {
        add_otlp_pipeline_to_blueprint(&mut blueprint, config, dp_config, env_provider)?;
    }

    Ok(blueprint)
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
        let host_tags_config = HostTagsConfiguration::from_configuration(config).await?;
        metrics_enrich_config = metrics_enrich_config.with_transform_builder("host_tags", host_tags_config);
    }

    let dd_metrics_config = DatadogMetricsConfiguration::from_configuration(config)
        .error_context("Failed to configure Datadog Metrics encoder.")?;

    blueprint
        // Components.
        .add_transform("metrics_enrich", metrics_enrich_config)?
        .add_encoder("dd_metrics_encode", dd_metrics_config)?
        // Metrics.
        .connect_component("dd_metrics_encode", ["metrics_enrich"])?
        // Forwarding.
        .connect_component("dd_out", ["dd_metrics_encode"])?;

    Ok(())
}

async fn add_dsd_pipeline_to_blueprint(
    blueprint: &mut TopologyBlueprint, config: &GenericConfiguration, env_provider: &ADPEnvironmentProvider,
    dsd_stats_config: DogStatsDStatisticsConfiguration,
) -> Result<(), GenericError> {
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
    //               │      │  DSD Prefix/Filter  │    │ DSD Service Checks  │    │     DSD Events      │
    //               │      │     (transform)     │    │      (encoder)      │    │      (encoder)      │
    //               │      └─────────────────────┘    └─────────────────────┘    └─────────────────────┘
    //               │                 │                          │                          │
    //               │                 ▼                          │                          └─ ─ ─ ┐
    //               │      ┌─────────────────────┐               └ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ┐   │
    //               │      │     DSD Enrich      │                                             │   │
    //               │      │ (chained transform) │        ┌ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ┐    │   │
    //               │      │┌───────────────────┐│        │        Metrics Pipeline       │    │   │
    //               │      ││    DSD Mapper     ││ ─ ─ ─▶ │  (aggregate, enrich, encode)  │    │   │
    //               │      │└───────────────────┘│        └ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ┘    │   │
    //               │      └─────────────────────┘                       │                     │   │
    //               │                                                    │                     │   │
    //               ▼                                                    ▼                     ▼   ▼
    //    ┌─────────────────────┐    ┌ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ┐
    //    │      DSD Stats      │    │                           Forwarder                             │
    //    │    (destination)    │    │                       (Datadog Platform)                        │
    //    └─────────────────────┘    └ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ┘

    let dsd_config = DogStatsDConfiguration::from_configuration(config)
        .error_context("Failed to configure DogStatsD source.")?
        .with_workload_provider(env_provider.workload().clone());
    let dsd_prefix_filter_configuration = DogStatsDPrefixFilterConfiguration::from_configuration(config)?;
    let dsd_mapper_config = DogStatsDMapperConfiguration::from_configuration(config)?;
    let dsd_enrich_config =
        ChainedConfiguration::default().with_transform_builder("dogstatsd_mapper", dsd_mapper_config);
    let dsd_tag_filterlist_config = TagFilterlistConfiguration::from_configuration(config)
        .error_context("Failed to configure metric tag filterlist transform.")?;
    let dsd_agg_config =
        AggregateConfiguration::from_configuration(config).error_context("Failed to configure aggregate transform.")?;
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
        .add_transform("dsd_tag_filterlist", dsd_tag_filterlist_config)?
        .add_transform("dsd_agg", dsd_agg_config)?
        .add_encoder("dd_events_encode", dd_events_config)?
        .add_encoder("dd_service_checks_encode", dd_service_checks_config)?
        .add_destination("dsd_stats_out", dsd_stats_config)?
        // Metrics.
        .connect_component("dsd_prefix_filter", ["dsd_in.metrics"])?
        .connect_component("dsd_enrich", ["dsd_prefix_filter"])?
        .connect_component("dsd_tag_filterlist", ["dsd_enrich"])?
        .connect_component("dsd_agg", ["dsd_tag_filterlist"])?
        .connect_component("metrics_enrich", ["dsd_agg"])?
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
            .connect_component("local_agent_otlp_out", ["otlp_relay_in.metrics", "otlp_relay_in.logs"])?;

        if dp_config.otlp().proxy().proxy_traces() {
            blueprint.connect_component("local_agent_otlp_out", ["otlp_relay_in.traces"])?;
        } else {
            blueprint
                .add_decoder("otlp_traces_decode", otlp_decoder_config)?
                // Traces to decoder, then to the trace pipeline: obfuscation, enrichment, encoding, stats, forwarding.
                .connect_component("otlp_traces_decode", ["otlp_relay_in.traces"])?
                .connect_component("traces_enrich", ["otlp_traces_decode"])?;
        }
    } else {
        info!("OTLP proxy mode disabled. OTLP signals will be handled natively.");

        let otlp_config =
            OtlpConfiguration::from_configuration(config)?.with_workload_provider(env_provider.workload().clone());

        blueprint
            // Components.
            .add_source("otlp_in", otlp_config)?
            // Metrics, logs, and traces.
            //
            // We send OTLP metrics directly to the enrichment stage of the metrics pipeline, skipping aggregation,
            // to avoid transforming counters into rates.
            .connect_component("metrics_enrich", ["otlp_in.metrics"])?
            .connect_component("dd_logs_encode", ["otlp_in.logs"])?
            .connect_component("traces_enrich", ["otlp_in.traces"])?;
    }
    Ok(())
}

// ----- END OF SALUKI-LIKE CODE ---
// below is Greg messing around

const DSD_PORT: u16 = 3000;
const INTAKE_PORT: u16 = 2049;
const SALUKI_PATH: &str = "/Users/gregoire.roussel/dd/saluki";

async fn intake(shutdown: tokio::sync::oneshot::Receiver<()>) -> Result<(), GenericError> {
    let mut cmd = Command::new("target/release/datadog-intake")
        .kill_on_drop(true)
        .spawn()?;
    shutdown.await?;

    info!("SIGTERM intake");
    let maybe_intake_pid = cmd.id();
    if let Some(intake_pid) = maybe_intake_pid {
        let nix_pid = nix::unistd::Pid::from_raw(intake_pid as i32);
        nix_signal::kill(nix_pid, nix_signal::SIGTERM)?;
    }
    if let Err(_) = tokio::time::timeout(Duration::from_secs(5), cmd.wait()).await {
        warn!("SIGKILL intake");
        cmd.kill().await?;
        cmd.wait().await?;
    }
    Ok(())
}

static GRAMMAR_SINGLE: LazyLock<GrammarIr> = LazyLock::new(|| {
    let grammar_path: PathBuf = Path::new(SALUKI_PATH).join("fuzz/dogstatsd_offset.ebnf");
    let grammar = std::fs::read_to_string(grammar_path).unwrap();
    barkus_ebnf::compile(&grammar).unwrap()
});

static PROFILE: LazyLock<Profile> = LazyLock::new(|| Profile::default());

fn metric_line_from_raw(raw: &[u8]) -> Result<(u64, Bytes), GenericError> {
    let split_index = memchr::memchr(b';', raw).ok_or_else(|| generic_error!("missing ';'"))?;
    let ts = std::str::from_utf8(&raw[..split_index])?.parse::<u64>()?;
    let body = Bytes::copy_from_slice(&raw[split_index + 1..]);
    Ok((ts, body.into()))
}

fn aggregate_metric_lines(unsorted_packets: Vec<(u64, Bytes)>) -> Vec<(u64, Vec<Bytes>)> {
    let mut groups: BTreeMap<u64, Vec<Bytes>> = BTreeMap::new();
    unsorted_packets.into_iter().for_each(|(ts, packet)| {
        groups.entry(ts).or_default().push(packet);
    });
    groups.into_iter().collect()
}

fn generate_corpus_random() -> Result<DogStatsDInput, GenericError> {
    let seed = Some(1234u64);
    let count = 100;

    let mut rng: SmallRng = match seed {
        Some(s) => SmallRng::seed_from_u64(s),
        None => rand::make_rng(),
    };

    let generated_packets: Vec<(u64, Bytes)> = (0..count)
        .map(|_| {
            let (ast, _tape, _map) = barkus_core::generate::generate(&GRAMMAR_SINGLE, &PROFILE, &mut rng)
                .map_err(|e| GenericError::msg("Barkus - failed to generate").context(e))?;

            let ast_bytes = ast.serialize();
            let (ts, packet) = metric_line_from_raw(&ast_bytes)?;
            Ok((ts, packet))
        })
        .collect::<Result<_, GenericError>>()?;
    Ok(DogStatsDInput {
        messages: aggregate_metric_lines(generated_packets),
    })
}

static GRAMMAR_MULTILINE: LazyLock<GrammarIr> = LazyLock::new(|| {
    let grammar_path = Path::new(SALUKI_PATH).join("fuzz/dogstatsd_multi_offset.ebnf");
    let grammar = std::fs::read_to_string(grammar_path).unwrap();
    barkus_ebnf::compile(&grammar).unwrap()
});

#[derive(Debug)]
struct DogStatsDInput {
    messages: Vec<(u64, Vec<Bytes>)>,
}

impl<'a> Arbitrary<'a> for DogStatsDInput {
    fn arbitrary(u: &mut Unstructured<'a>) -> arbitrary::Result<Self> {
        let tape = u.bytes(u.len())?;
        let profile = Profile::default();
        let (ast, _) = decode(&*GRAMMAR_SINGLE, &profile, tape).map_err(|_| arbitrary::Error::IncorrectFormat)?;
        let text = ast.serialize();
        let message_lines: Vec<(u64, Bytes)> = text
            .split(|&b| b == b'\n')
            .filter(|s| !s.is_empty())
            .map(metric_line_from_raw)
            .collect::<Result<_, _>>()
            .map_err(|_| arbitrary::Error::IncorrectFormat)?;
        let aggregated_messages = aggregate_metric_lines(message_lines);

        Ok(DogStatsDInput {
            messages: aggregated_messages,
        })
    }
}

async fn inject_plan(plan: Vec<(u64, Vec<Bytes>)>) -> Result<(), GenericError> {
    let socket = UdpSocket::bind("0.0.0.0:0").await.expect("failed to bind UDP socket");
    socket
        .connect(("127.0.0.1", DSD_PORT))
        .await
        .unwrap_or_else(|e| panic!("failed to connect to port {}: {}", DSD_PORT, e));

    let start = Instant::now();
    let mut maybe_prev_ts = None;
    for (ts, packets) in &plan {
        // timestamp X is sent at second X/10
        if let Some(prev_ts) = maybe_prev_ts {
            assert!(prev_ts < ts);
        }
        maybe_prev_ts = Some(ts);
        let target = start + Duration::from_millis(ts * 10);
        if target > Instant::now() {
            tokio::time::sleep_until(target.into()).await;
        }
        for packet in packets {
            if let Err(e) = socket.send(packet).await {
                eprintln!("send error: {e}");
            }
        }
    }
    Ok(())
}

async fn inner(corpus: DogStatsDInput) {
    let (st1_tx, st1_rx) = tokio::sync::oneshot::channel();
    let saluki_handle = tokio::spawn(handle_run_command(st1_rx, Instant::now()));
    let (st2_tx, st2_rx) = tokio::sync::oneshot::channel();
    let intake_handle = tokio::spawn(intake(st2_rx));
    time::sleep(Duration::from_secs(2)).await;

    // run injection
    info!("Starting injection");
    inject_plan(corpus.messages).await.unwrap();
    time::sleep(Duration::from_secs(10)).await;

    // trigger shutdown
    info!("Trigger shutdown");
    st1_tx.send(()).unwrap();
    saluki_handle.await.unwrap().unwrap();

    info!("waiting on intake");
    st2_tx.send(()).unwrap();
    intake_handle.await.unwrap().unwrap();
}

fn main() {
    println!("hello!");
    let corpus = generate_corpus_random().unwrap();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(inner(corpus));
    println!("goodbye!");
}

// static CODEC: LazyLock<DogStatsDCodec> =
//     LazyLock::new(|| DogStatsDCodec::from_configuration(DogStatsDCodecConfiguration::default()));

fuzz_target!(|input: DogStatsDInput| {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(inner(input));
});
