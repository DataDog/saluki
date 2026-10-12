//! Running topologies for tests: the primary topology as `run.rs` builds it, with the legacy or the plan-driven metrics
//! path, and with its I/O replaced by test doubles at the same positions.
//!
//! [`TestTopology::start`] builds the blueprint with `create_legacy_topology` or `create_topology`, then replaces
//! components by ID, keeping every connection and admission point (see `TopologyBlueprint::replace_source` and its
//! siblings):
//!
//! - every source, and the trace sampler, which emits metrics of its own, with a scripted component that sends each
//!   batch the test feeds it, as one buffer, on its output of the batch's type (a source's `metrics`, `events`,
//!   `service_checks`, or `logs`; the sampler's `metrics`), and counts the sends that completed
//!   ([`TestTopology::sent`]); except the sources and the trace sampler the options keep real
//!   ([`Options::real_sources`], [`Options::real_trace_sampler`]), such as the DogStatsD source listening on a socket
//!   for a measurement;
//! - DogStatsD aggregation, when asked, with native aggregation on a test clock ([`Aggregation::Clocked`]): it admits
//!   every metric at [`ADMITTED_AT`] and flushes once, when its input ends, at [`FLUSHED_AT`], past every bucket, so
//!   that what it emits does not depend on when a test runs; or with a transform that reads nothing until its gate
//!   opens ([`Aggregation::Gated`]), or one that sends what the test feeds it ([`Aggregation::Scripted`]). The
//!   plan-driven path runs aggregation as a stateful kernel in its `dsd_agg` isolation slot: clocked aggregation then
//!   puts the admission point of `dsd_in.metrics` on a wall clock that reads [`ADMITTED_AT`], the slot on one that
//!   reads [`FLUSHED_AT`], and the flush interval at [`CLOCKED_FLUSH_INTERVAL`], so that the slot flushes once, when
//!   its input ends, exactly as clocked native aggregation does. Gated and scripted aggregation replace the slot's
//!   region, as they replace the native component on the legacy path: the scripted stand-in stamps what it sends as
//!   the slot stamps its time-driven output, so the regions after it take it as aggregation's;
//! - every metrics encoder, and the DogStatsD client telemetry destination, with a collector that records each buffer
//!   it receives and encodes nothing; or, with [`Encoders::Real`], the metrics encoders are kept, and the collector
//!   observes what each receives before it encodes it (see the `test-util` helpers `TopologyBlueprint::wrap_encoder`
//!   and `Consumer::observe`);
//! - every forwarder with one that drops what it receives, so nothing leaves the process, counting the metrics the
//!   payloads it drops carry ([`TestTopology::forwarded`]); on the plan-driven path, the delivery hosts' recipients
//!   with ones that send nothing and count the events they would have sent. With [`Recipients::Sending`], the
//!   forwarders and the recipients send as in production, to the endpoints the configuration names, such as recording
//!   intakes.
//!
//! Everything else runs as it does in production: the regions (with the gateways, on the plan-driven path), the native
//! transforms (with the gateways, on the legacy path), aggregation unless replaced, and, for the plan-driven path, the
//! installer.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        atomic::{AtomicU64, AtomicUsize, Ordering},
        Arc, Mutex as StdMutex,
    },
    time::Duration,
};

use agent_data_plane_config::SalukiConfiguration;
use agent_data_plane_config_system::ConfigurationSystem;
use arc_swap::ArcSwap;
use async_trait::async_trait;
use saluki_components::{encoders::encode_host::LeaseObserver, transforms::AggregateContextSnapshotEntry};
use saluki_core::{
    accounting::{ComponentRegistry, MemoryBounds, MemoryBoundsBuilder, MemoryLimiter},
    components::{
        destinations::{Destination, DestinationBuilder, DestinationContext},
        encoders::{Encoder, EncoderBuilder, EncoderContext},
        forwarders::{Forwarder, ForwarderBuilder, ForwarderContext},
        sources::{Source, SourceBuilder, SourceContext},
        transforms::{Transform, TransformBuilder, TransformContext},
        BuildContext, ComponentType,
    },
    data_model::{
        event::{metric::Metric, Event, EventType},
        payload::{Payload, PayloadType},
    },
    execution::{AdmissionTime, BatchHeader, EntryId, ManualWallClock, WallClock as _},
    health::{Health, HealthRegistry},
    observability::ComponentMetricsExt as _,
    runtime::{state::ResourceRegistry, Supervisor, SupervisorError},
    topology::{BlueprintSnapshot, ComponentSnapshot, DataType, EventsBuffer, OutputDefinition, OutputName},
};
use saluki_error::{generic_error, GenericError};
use saluki_metric_kernels::aggregate::{AggregateCore, AggregateParams, AggregateTelemetry};
use saluki_metric_plan::{
    compile::{EntryKey, SlotKind, Step},
    equivalence::OutputPoints,
    ir::OutputId,
    operators::OperatorRegistry,
};
use saluki_metrics::MetricsBuilder;
use tokio::{
    runtime::Handle,
    sync::{mpsc, oneshot, Semaphore},
    task::JoinHandle,
    time::Instant,
};

use super::{
    installer::{BootTopology, InstallerWorker, PlanStatus, ProgrammableMetrics},
    test_support::{environment_provider, init_test_tls, programmable_metrics_with_operators},
    topology::{aggregation_slot, HostDoubles},
};
use crate::{
    cli::run::{create_legacy_topology, create_topology, dogstatsd_aggregate_configuration},
    config::remote_agent_client_configuration,
    internal::TopologyControlSurfaces,
};

/// The longest a test waits for a topology to deliver, to start, or to stop.
pub(super) const PATIENCE: Duration = Duration::from_secs(60);

/// The time, in Unix seconds, at which clocked aggregation admits every metric: inside one bucket of every allowed
/// window.
pub(super) const ADMITTED_AT: u64 = 1_800_000_005;

/// The time, in Unix seconds, at which clocked aggregation flushes, when its input ends: past the end of the bucket
/// [`ADMITTED_AT`] falls in, for any window, and of every timestamped value the tests send.
pub(super) const FLUSHED_AT: u64 = 1_900_000_000;

/// The flush interval clocked aggregation in an isolation slot runs with: longer than any test, so that it flushes only
/// when its input ends.
pub(super) const CLOCKED_FLUSH_INTERVAL: Duration = Duration::from_secs(3_600);

/// The components whose output carries metrics into the metrics path, besides the sources: the trace sampler, which
/// reports its sampling telemetry as metrics.
const METRIC_EMITTING_TRANSFORMS: [&str; 1] = ["trace_sampler"];

/// Which metrics path the topology runs.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum MetricsPath {
    /// The legacy metrics path, from native components.
    Legacy,

    /// The plan-driven metrics path, with its installer.
    Plan,
}

/// What runs DogStatsD aggregation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Aggregation {
    /// The native aggregation component, on the wall clock and its flush timer.
    Native,

    /// Native aggregation on a test clock: it admits every metric at [`ADMITTED_AT`] and flushes once, at
    /// [`FLUSHED_AT`], when its input ends.
    Clocked,

    /// A transform that reads nothing until its gate, `dsd_agg`, [opens](TestTopology::open_gate), so that what is sent
    /// to aggregation backs up, and then sends what it reads on unchanged.
    Gated,

    /// A scripted transform that drops what it reads and sends each batch the test feeds it on `dsd_agg`, as
    /// aggregation's output.
    Scripted,
}

/// What runs in place of the metrics encoders.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Encoders {
    /// Collectors that record what each encoder would receive and encode nothing.
    Collecting,

    /// The real encoders, which encode and compress into payloads that the dropping forwarders discard; the collector
    /// observes what each encoder receives, recording only buffers with metrics the options keep, so that a
    /// measurement sees when a marker reaches an encoder while every encoder does its work.
    Real,
}

/// What the forwarders do with the payloads they receive: on the legacy path, the native forwarders; on the plan-driven
/// path, the delivery hosts' recipients.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Recipients {
    /// Forwarders that discard every payload and count its metrics; the delivery hosts run as in production, with
    /// recipients that send nothing.
    Discarding,

    /// The native forwarders, and the delivery hosts' recipients, sending every payload to the endpoints the
    /// configuration names, as in production: with [`Encoders::Real`], for tests that run recording intakes.
    Sending,
}

/// The component DogStatsD aggregation runs as, which also names the gate of gated aggregation and the feed of scripted
/// aggregation.
pub(super) const DSD_AGG: &str = "dsd_agg";

/// One buffer an output received.
#[derive(Clone, Debug)]
pub(super) struct Delivery {
    /// The component the buffer reached: an encoder, or the DogStatsD client telemetry destination.
    pub output: String,

    /// When the buffer arrived.
    pub at: Instant,

    /// The generation the buffer's header carries.
    pub generation: Option<u64>,

    /// The entry the buffer's header carries: for an encoder that one of an output's leases feeds, the lease.
    pub entry: Option<EntryId>,

    /// The buffer's metrics that the options say to keep: all of them, unless a test only needs some.
    pub metrics: Vec<Metric>,
}

/// What a test does to the plan-driven metrics path between its boot and the construction of the topology.
pub(super) type BeforeTopology = Box<dyn FnOnce(&mut BootTopology)>;

/// What a test does to the plan-driven metrics path once the topology is built and before its installer runs, such as
/// connecting an artifact channel.
pub(super) type BeforeInstall = Box<dyn FnOnce(&mut ProgrammableMetrics)>;

/// How a test topology is started.
pub(super) struct Options {
    /// Which metrics path to run.
    pub path: MetricsPath,

    /// What runs DogStatsD aggregation.
    pub aggregation: Aggregation,

    /// The registry the topology accounts its components' memory in.
    pub registry: ComponentRegistry,

    /// The health registry the topology registers its components with. Its worker runs beside the topology, probing
    /// liveness.
    pub health: HealthRegistry,

    /// The outputs whose collectors wait for their [gate](TestTopology::open_gate) to open before they read anything,
    /// so that what is sent to them backs up.
    pub gated: BTreeSet<String>,

    /// Whether the installer runs under the supervisor of the topology. Without it, the test takes the installer with
    /// [`TestTopology::take_installer`] and runs it itself.
    pub supervise_installer: bool,

    /// What to do to the plan-driven metrics path between its boot and the construction of the topology.
    pub before_topology: Option<BeforeTopology>,

    /// What to do to the plan-driven metrics path once the topology is built and before its installer runs.
    pub before_install: Option<BeforeInstall>,

    /// Which metrics the collectors keep in their deliveries; every one by default.
    pub keep: fn(&Metric) -> bool,

    /// What the topology waits for after it is built and before it starts its components, as `run.rs` waits for the
    /// environment to be ready; nothing by default.
    pub environment_ready: Option<futures::future::BoxFuture<'static, ()>>,

    /// The operators the plan-driven metrics path validates, composes, and compiles plans with: the native ones by
    /// default, which a test extends with operators of its own.
    pub operators: OperatorRegistry,

    /// The wall clock native aggregation reads on the legacy path, instead of the system's, with
    /// [`Aggregation::Native`]: its flush ticks keep the native timer, and read this clock, as a test that compares
    /// ticks needs. The plan-driven path's clocks are those of the boot topology (see
    /// [`before_topology`][Self::before_topology]).
    pub aggregation_clock: Option<ManualWallClock>,

    /// The sources that run as built, such as `dsd_in` listening on its configured sockets, rather than as scripted
    /// sources: they have no feed. None by default.
    pub real_sources: BTreeSet<String>,

    /// What runs in place of the metrics encoders: collectors by default.
    pub encoders: Encoders,

    /// Whether the trace sampler runs as built, sampling the traces fed to the OTLP source and sending them on to the
    /// traces encoder, rather than as a scripted transform that drops them: `false` by default.
    pub real_trace_sampler: bool,

    /// What the forwarders do with their payloads: discard them by default.
    pub recipients: Recipients,

    /// The runtime the topology runs compute-heavy subtasks of components on, such as the encoders' request builders,
    /// as ADP's global thread pool; the test's runtime by default.
    pub worker_pool: Option<Handle>,
}

impl Options {
    /// Returns the options of a topology running `path`, with `aggregation`, fresh registries, no gates, and the
    /// installer supervised.
    pub(super) fn new(path: MetricsPath, aggregation: Aggregation) -> Self {
        Self {
            path,
            aggregation,
            registry: ComponentRegistry::default(),
            health: HealthRegistry::new(),
            gated: BTreeSet::new(),
            supervise_installer: true,
            before_topology: None,
            before_install: None,
            keep: |_| true,
            environment_ready: None,
            operators: OperatorRegistry::native(),
            aggregation_clock: None,
            real_sources: BTreeSet::new(),
            encoders: Encoders::Collecting,
            recipients: Recipients::Discarding,
            real_trace_sampler: false,
            worker_pool: None,
        }
    }
}

/// The input a test feeds into one component output, and how many of the batches fed the component has sent.
#[derive(Clone)]
struct Feed {
    batches: mpsc::UnboundedSender<Vec<Event>>,
    sent: Arc<AtomicUsize>,
}

impl Feed {
    /// Returns a feed, with the receiving side of its batches and its count of sends for the scripted component that
    /// sends them.
    fn new() -> (Self, mpsc::UnboundedReceiver<Vec<Event>>, Arc<AtomicUsize>) {
        let (batches, receiver) = mpsc::unbounded_channel();
        let sent = Arc::new(AtomicUsize::new(0));
        let feed = Self {
            batches,
            sent: Arc::clone(&sent),
        };
        (feed, receiver, sent)
    }
}

/// A running test topology.
pub(super) struct TestTopology {
    feeds: BTreeMap<String, Feed>,
    forwarded: Arc<AtomicU64>,
    outputs: BTreeSet<String>,
    deliveries: mpsc::UnboundedReceiver<Delivery>,
    received: Vec<Delivery>,
    status: Option<Arc<ArcSwap<PlanStatus>>>,
    installer: Option<InstallerWorker>,
    gates: BTreeMap<String, Arc<Semaphore>>,
    snapshot: BlueprintSnapshot,
    control_surfaces: TopologyControlSurfaces,
    shutdown: Option<oneshot::Sender<()>>,
    task: JoinHandle<Result<(), SupervisorError>>,
}

impl TestTopology {
    /// Builds and starts the primary topology of `system`, as [described](self) in the module documentation.
    ///
    /// `extra` adds workers to the supervisor the topology runs under, such as the worker that applies configuration
    /// updates.
    pub(super) async fn start(
        system: &ConfigurationSystem, options: Options, extra: impl FnOnce(&mut Supervisor),
    ) -> Self {
        let Options {
            path,
            aggregation,
            registry,
            health,
            gated,
            supervise_installer,
            before_topology,
            before_install,
            keep,
            environment_ready,
            operators,
            aggregation_clock,
            real_sources,
            encoders,
            recipients,
            real_trace_sampler,
            worker_pool,
        } = options;
        assert!(
            recipients == Recipients::Discarding || encoders == Encoders::Real,
            "recipients send only payloads that real encoders build"
        );
        init_test_tls();
        let config = SalukiConfiguration::clone(&system.config());
        let env = environment_provider(&config).await;
        let (deliveries_tx, deliveries) = mpsc::unbounded_channel();
        let forwarded = Arc::new(AtomicU64::new(0));
        // The gates of the outputs that wait before they read: an output's own collector's, or, on the plan-driven
        // path, the gate of the encode host that runs the output's lease.
        let mut gates = gated
            .iter()
            .map(|output| (output.clone(), Arc::new(Semaphore::new(0))))
            .collect::<BTreeMap<_, _>>();
        if aggregation == Aggregation::Gated {
            gates.insert(DSD_AGG.to_string(), Arc::new(Semaphore::new(0)));
        }
        // The plan-driven path's outputs: its encode hosts' leases, which the hosts name, and its fixed destinations.
        let mut plan_outputs = None;
        // The encode hosts and delivery hosts the plan-driven path builds, which run as in production with doubles at
        // their edges, so that the installer controls them.
        let mut hosts = BTreeSet::new();
        let mut isolated_aggregation = false;
        // How the isolation slot of aggregation stamps its time-driven output, for a scripted stand-in.
        let mut aggregation_stamp = None;
        // The snapshot bridge, which `run.rs` runs beside the installer, answers the context dump API from the region
        // running aggregation.
        let mut bridge = None;
        let (mut blueprint, control_surfaces, installer) = match path {
            MetricsPath::Legacy => {
                let client = (!config.control.standalone_mode)
                    .then(|| remote_agent_client_configuration(&config).expect("the client configuration resolves"));
                let (blueprint, control_surfaces) = create_legacy_topology(system, client.as_ref(), &env, &registry)
                    .await
                    .expect("the legacy topology builds");
                (blueprint, control_surfaces, None)
            }
            MetricsPath::Plan => {
                let boot = |config: &SalukiConfiguration| {
                    programmable_metrics_with_operators(config, system.live(|config| config), &registry, operators)
                        .expect("a metrics pipeline is required")
                };
                let mut metrics = boot(&config);
                isolated_aggregation = aggregation_slot(&metrics.boot_topology().frame).is_some();
                if isolated_aggregation && aggregation == Aggregation::Clocked {
                    // The slot flushes only when its input ends; only the boot program has this flush interval.
                    let mut clocked = config.clone();
                    clocked.domains.dogstatsd.aggregation.flush_interval = CLOCKED_FLUSH_INTERVAL;
                    metrics = boot(&clocked);
                    let topology = metrics.boot_topology();
                    topology.state_admission_clock =
                        Some(ManualWallClock::new(AdmissionTime::from_unix_secs(ADMITTED_AT)));
                    topology.wall_clock = Arc::new(ManualWallClock::new(AdmissionTime::from_unix_secs(FLUSHED_AT)));
                }
                if isolated_aggregation {
                    aggregation_stamp = Some(time_driven_stamp(metrics.boot_topology()));
                }
                if let Some(before_topology) = before_topology {
                    before_topology(metrics.boot_topology());
                }
                let boot = metrics.boot_topology();
                let mut host_gates = BTreeMap::new();
                for output in &gated {
                    if let Some(encoder) = encode_host_of(boot, output) {
                        let gate = host_gates
                            .entry(encoder.to_string())
                            .or_insert_with(|| Arc::new(Semaphore::new(0)));
                        gates.insert(output.clone(), Arc::clone(gate));
                    }
                }
                for host in boot.hosts_at_boot.iter() {
                    hosts.insert(host.delivery.to_string());
                    hosts.insert(host.encoder.to_string());
                }
                plan_outputs = Some(frame_outputs(boot));
                boot.host_doubles = Some(HostDoubles {
                    observer: Arc::new(LeaseCollector {
                        deliveries: deliveries_tx.clone(),
                        keep,
                    }),
                    encode: encoders == Encoders::Real,
                    delivered: (recipients == Recipients::Discarding).then(|| Arc::clone(&forwarded)),
                    gates: host_gates,
                });
                let (blueprint, control_surfaces) = create_topology(system, &env, &registry, Some(&mut metrics))
                    .await
                    .expect("the plan-driven topology builds");
                if let Some(before_install) = before_install {
                    before_install(&mut metrics);
                }
                bridge = metrics.snapshot_bridge();
                (blueprint, control_surfaces, Some(metrics.into_installer()))
            }
        };

        let snapshot = blueprint.snapshot().expect("the blueprint is not running yet");
        let outputs = plan_outputs.unwrap_or_else(|| collected_outputs(&snapshot));
        let mut feeds = BTreeMap::new();
        let collector = |id: &str| Collector {
            output: id.to_string(),
            deliveries: deliveries_tx.clone(),
            gate: gates.get(id).cloned(),
            keep,
        };
        for component in snapshot.components() {
            let id = component.id().to_string();
            match component.component_type() {
                // The plan-driven path's hosts run with doubles at their edges.
                ComponentType::Encoder | ComponentType::Forwarder if hosts.contains(&id) => {}
                ComponentType::Source if real_sources.contains(&id) => {}
                ComponentType::Source => {
                    let (feed, batches, sent) = Feed::new();
                    let source = ScriptedSourceBuilder::new(component, batches, sent);
                    // One feed for every output: the source sends each batch on the output of its events' type.
                    for name in source.outputs.iter().filter_map(OutputDefinition::output_name) {
                        feeds.insert(format!("{}.{}", id, name), feed.clone());
                    }
                    feeds.insert(format!("{}.metrics", id), feed);
                    blueprint
                        .replace_source(&id, source)
                        .expect("a scripted source declares what it replaces");
                }
                ComponentType::Transform if id == "trace_sampler" && real_trace_sampler => {}
                ComponentType::Transform if METRIC_EMITTING_TRANSFORMS.contains(&id.as_str()) => {
                    let (feed, batches, sent) = Feed::new();
                    let output = OutputName::Given("metrics".into());
                    blueprint
                        .replace_transform(&id, ScriptedTransformBuilder::new(component, output, batches, sent))
                        .expect("a scripted transform declares what it replaces");
                    feeds.insert(format!("{}.metrics", id), feed);
                }
                // The kernel of aggregation in an isolation slot runs, clocked through its slot's clocks, unless gated
                // or scripted aggregation stands in for the slot's region.
                ComponentType::Transform if id == DSD_AGG && isolated_aggregation => match aggregation {
                    Aggregation::Native | Aggregation::Clocked => {}
                    Aggregation::Gated => {
                        let gate = gates.get(DSD_AGG).cloned().expect("gated aggregation has a gate");
                        blueprint
                            .replace_transform(&id, GatedTransformBuilder::new(component, gate))
                            .expect("gated aggregation declares what it replaces");
                    }
                    Aggregation::Scripted => {
                        let (feed, batches, sent) = Feed::new();
                        let output = first_output(component);
                        let stamp = aggregation_stamp.clone();
                        blueprint
                            .replace_transform(
                                &id,
                                ScriptedTransformBuilder::new(component, output, batches, sent).stamped(stamp),
                            )
                            .expect("scripted aggregation declares what it replaces");
                        feeds.insert(DSD_AGG.to_string(), feed);
                    }
                },
                ComponentType::Transform if id == DSD_AGG => match aggregation {
                    Aggregation::Native => {
                        if let Some(clock) = aggregation_clock.clone() {
                            let (native, _) =
                                dogstatsd_aggregate_configuration(&config).expect("aggregation is configured");
                            let reading = Arc::new(move || clock.now().as_unix_secs());
                            blueprint
                                .replace_transform(&id, native.with_clock(reading))
                                .expect("native aggregation on a test clock declares what it replaces");
                        }
                    }
                    Aggregation::Clocked => {
                        blueprint
                            .replace_transform(&id, ClockedAggregateBuilder::new(&config))
                            .expect("clocked aggregation declares what it replaces");
                    }
                    Aggregation::Gated => {
                        let gate = gates.get(DSD_AGG).cloned().expect("gated aggregation has a gate");
                        blueprint
                            .replace_transform(&id, GatedTransformBuilder::new(component, gate))
                            .expect("gated aggregation declares what it replaces");
                    }
                    Aggregation::Scripted => {
                        let (feed, batches, sent) = Feed::new();
                        blueprint
                            .replace_transform(
                                &id,
                                ScriptedTransformBuilder::new(component, OutputName::Default, batches, sent),
                            )
                            .expect("scripted aggregation declares what it replaces");
                        feeds.insert(DSD_AGG.to_string(), feed);
                    }
                },
                ComponentType::Encoder
                    if encoders == Encoders::Real && component.input() == Some(DataType::Event(EventType::Metric)) =>
                {
                    let collector = Arc::new(collector(&id));
                    blueprint
                        .wrap_encoder(&id, move |inner| {
                            Box::new(ObservedEncoder {
                                inner,
                                collector: Arc::clone(&collector),
                            })
                        })
                        .expect("the encoder exists");
                }
                ComponentType::Encoder if component.input() == Some(DataType::Event(EventType::Metric)) => {
                    let payloads = match component.outputs().first().map(|output| output.data_type()) {
                        Some(DataType::Payload(payloads)) => payloads,
                        other => panic!("encoder `{}` emits {:?}", id, other),
                    };
                    blueprint
                        .replace_encoder(
                            &id,
                            CollectingEncoderBuilder {
                                collector: collector(&id),
                                payloads,
                            },
                        )
                        .expect("a collecting encoder declares what it replaces");
                }
                ComponentType::Destination if id == "dsd_client_telemetry_out" => {
                    blueprint
                        .replace_destination(
                            &id,
                            CollectingDestinationBuilder {
                                collector: collector(&id),
                            },
                        )
                        .expect("a collecting destination declares what it replaces");
                }
                ComponentType::Forwarder if recipients == Recipients::Sending => {}
                ComponentType::Forwarder => {
                    let payloads = match component.input() {
                        Some(DataType::Payload(payloads)) => payloads,
                        other => panic!("forwarder `{}` accepts {:?}", id, other),
                    };
                    let forwarded = Arc::clone(&forwarded);
                    blueprint
                        .replace_forwarder(&id, DroppingForwarderBuilder { payloads, forwarded })
                        .expect("a dropping forwarder declares what it replaces");
                }
                _ => {}
            }
        }
        drop(deliveries_tx);

        blueprint
            .with_health_registry(health.clone())
            .with_memory_limiter(MemoryLimiter::noop())
            .with_resource_registry(ResourceRegistry::new());
        match worker_pool {
            Some(pool) => blueprint.with_explicit_worker_pool(pool),
            None => blueprint.with_ambient_worker_pool(),
        };
        if let Some(ready) = environment_ready {
            blueprint.with_environment_readiness(ready);
        }
        let status = installer.as_ref().map(|installer| installer.status());
        let mut supervisor = Supervisor::new("test-topology").expect("the supervisor is created");
        supervisor.add_worker(health.worker());
        supervisor.add_worker(blueprint);
        if let Some(bridge) = bridge {
            supervisor.add_worker(bridge);
        }
        let installer = match installer {
            Some(installer) if supervise_installer => {
                supervisor.add_worker(installer);
                None
            }
            installer => installer,
        };
        extra(&mut supervisor);
        let (shutdown, shutdown_rx) = oneshot::channel::<()>();
        let task = tokio::spawn(async move { supervisor.run_with_shutdown(shutdown_rx).await });

        Self {
            feeds,
            forwarded,
            outputs,
            deliveries,
            received: Vec::new(),
            status,
            installer,
            gates,
            snapshot,
            control_surfaces,
            shutdown: Some(shutdown),
            task,
        }
    }

    /// Returns the contexts DogStatsD aggregation holds, as the context dump API reads them: from the native component,
    /// or, on the plan-driven path, through the snapshot bridge from the region running aggregation. Returns `None` if
    /// the snapshot is unavailable, or without DogStatsD.
    pub(super) async fn context_dump(&self) -> Option<Vec<AggregateContextSnapshotEntry>> {
        self.control_surfaces.context_dump()?.snapshot().await
    }

    /// Returns the blueprint of the topology, as it was built, before any component was replaced.
    pub(super) fn snapshot(&self) -> &BlueprintSnapshot {
        &self.snapshot
    }

    /// Returns the outputs whose deliveries the topology records: on the legacy path, every metrics encoder and the
    /// DogStatsD client telemetry destination ([`collected_outputs`]); on the plan-driven path, every output bound to
    /// an encode host, as its leases name it, and the client telemetry destination.
    pub(super) fn outputs(&self) -> BTreeSet<String> {
        self.outputs.clone()
    }

    /// Returns the component outputs a test can feed metrics into, such as `dsd_in.metrics`.
    pub(super) fn feeds(&self) -> BTreeSet<String> {
        self.feeds.keys().cloned().collect()
    }

    /// Returns the installer's status, kept current, if the topology runs the plan-driven metrics path.
    pub(super) fn status(&self) -> Option<Arc<ArcSwap<PlanStatus>>> {
        self.status.clone()
    }

    /// Returns the installer, when the options said not to supervise it.
    pub(super) fn take_installer(&mut self) -> InstallerWorker {
        self.installer
            .take()
            .expect("the installer is not supervised, and not taken yet")
    }

    /// Opens the gate of `output`: its collector, or gated aggregation for `dsd_agg`, starts reading what was sent to
    /// it.
    pub(super) fn open_gate(&self, output: &str) {
        self.gates
            .get(output)
            .unwrap_or_else(|| panic!("`{}` has no gate", output))
            .add_permits(1);
    }

    /// Sends `metrics` as one buffer on the component output `feed`.
    ///
    /// # Panics
    ///
    /// Panics if the topology has no such feed, or if the metrics do not fit one buffer.
    pub(super) fn feed(&self, feed: &str, metrics: Vec<Metric>) {
        let sender = self
            .feeds
            .get(feed)
            .unwrap_or_else(|| panic!("the topology has no feed `{}`; it has {:?}", feed, self.feeds.keys()));
        sender
            .batches
            .send(metrics.into_iter().map(Event::Metric).collect())
            .expect("the scripted component runs until its feed closes");
    }

    /// Returns `true` if the topology has the feed `feed`.
    pub(super) fn has_feed(&self, feed: &str) -> bool {
        self.feeds.contains_key(feed)
    }

    /// Feeds one batch of events, of one type, into `feed`: a scripted source sends it on its output of that type, as
    /// [`feed`][Self::feed] does metrics.
    ///
    /// # Panics
    ///
    /// Panics if the topology has no such feed.
    pub(super) fn feed_events(&self, feed: &str, events: Vec<Event>) {
        let sender = self
            .feeds
            .get(feed)
            .unwrap_or_else(|| panic!("the topology has no feed `{}`; it has {:?}", feed, self.feeds.keys()));
        sender
            .batches
            .send(events)
            .expect("the scripted component runs until its feed closes");
    }

    /// Returns how many of the batches fed on `feed` its scripted component has sent so far. A send completes once
    /// every component the output is connected to has taken the batch into its input.
    ///
    /// # Panics
    ///
    /// Panics if the topology has no such feed.
    pub(super) fn sent(&self, feed: &str) -> usize {
        self.feeds
            .get(feed)
            .unwrap_or_else(|| panic!("the topology has no feed `{}`; it has {:?}", feed, self.feeds.keys()))
            .sent
            .load(Ordering::SeqCst)
    }

    /// Returns how many metrics the payloads the dropping forwarders discarded so far carried, in all.
    pub(super) fn forwarded(&self) -> u64 {
        self.forwarded.load(Ordering::Relaxed)
    }

    /// Returns the next buffer an output receives, without recording it, or `None` once the outputs have stopped.
    pub(super) async fn next_delivery(&mut self) -> Option<Delivery> {
        self.deliveries.recv().await
    }

    /// Waits until the outputs have received buffers for which `done` returns `true`, and returns every buffer
    /// received so far, in arrival order.
    pub(super) async fn wait_for(&mut self, what: &str, done: impl Fn(&[Delivery]) -> bool) -> &[Delivery] {
        let received = &mut self.received;
        let deliveries = &mut self.deliveries;
        let waited = tokio::time::timeout(PATIENCE, async {
            while !done(received) {
                match deliveries.recv().await {
                    Some(delivery) => received.push(delivery),
                    None => return false,
                }
            }
            true
        })
        .await;
        match waited {
            Ok(true) => &self.received,
            Ok(false) => panic!("the outputs stopped before {}: received {:?}", what, self.received),
            Err(_) => panic!("timed out waiting for {}: received {:?}", what, self.received),
        }
    }

    /// Waits at most `limit` until the outputs have received buffers for which `done` returns `true`, recording what
    /// arrives meanwhile, and returns whether they did: for something that may not happen.
    pub(super) async fn received_within(&mut self, limit: Duration, done: impl Fn(&[Delivery]) -> bool) -> bool {
        let received = &mut self.received;
        let deliveries = &mut self.deliveries;
        tokio::time::timeout(limit, async {
            while !done(received) {
                match deliveries.recv().await {
                    Some(delivery) => received.push(delivery),
                    None => return false,
                }
            }
            true
        })
        .await
        .unwrap_or(false)
    }

    /// Stops the topology: closes every feed, so the scripted components stop once they have sent what they were fed,
    /// then shuts the topology down, which drains it, and returns every buffer the outputs received, in arrival order.
    ///
    /// # Panics
    ///
    /// Panics if the topology does not stop cleanly in time.
    pub(super) async fn stop(mut self) -> Vec<Delivery> {
        self.feeds.clear();
        self.shutdown.take().expect("the topology runs").send(()).ok();
        let result = tokio::time::timeout(PATIENCE, &mut self.task)
            .await
            .expect("the topology stops in time")
            .expect("the supervisor does not panic");
        assert!(result.is_ok(), "the topology stops cleanly: {:?}", result);
        while let Some(delivery) = self.deliveries.recv().await {
            self.received.push(delivery);
        }
        self.received
    }

    /// Waits until the topology stops on its own, as it does when one of its components fails, without asking it to,
    /// and returns why it stopped, and every buffer the outputs received, in arrival order.
    ///
    /// # Panics
    ///
    /// Panics if the topology does not stop in time, or stops cleanly.
    pub(super) async fn failure(mut self) -> (SupervisorError, Vec<Delivery>) {
        let result = tokio::time::timeout(PATIENCE, &mut self.task)
            .await
            .expect("the topology stops in time")
            .expect("the supervisor does not panic");
        let error = match result {
            Ok(()) => panic!("the topology stopped cleanly"),
            Err(error) => error,
        };
        self.feeds.clear();
        while let Some(delivery) = self.deliveries.recv().await {
            self.received.push(delivery);
        }
        (error, self.received)
    }
}

/// Returns the points every output received in `deliveries`, keyed by output component.
pub(super) fn output_points(deliveries: &[Delivery]) -> OutputPoints {
    let mut points = OutputPoints::new();
    for delivery in deliveries {
        let output = OutputId::new(delivery.output.clone()).expect("component IDs are output IDs");
        for metric in &delivery.metrics {
            points.insert(&output, metric);
        }
    }
    points
}

/// Returns the outputs of the boot plan as the plan-driven path's doubles record them: every output bound to a
/// delivery host, by its ID, which its lease in the encode host names, and every fixed destination, by its component.
fn frame_outputs(boot: &BootTopology) -> BTreeSet<String> {
    boot.frame
        .outputs()
        .iter()
        .map(|output| match &output.binding {
            Some(_) => output.id.to_string(),
            None => boot
                .outputs
                .get(output.id.as_str())
                .map_or_else(|| output.id.to_string(), |native| native.component.clone()),
        })
        .collect()
}

/// Returns the encode host that runs the lease of the boot plan's `output`, if the output is bound to a delivery host.
fn encode_host_of(boot: &BootTopology, output: &str) -> Option<&'static str> {
    let output = boot.frame.outputs().iter().find(|decl| decl.id.as_str() == output)?;
    let host = output.binding.as_ref()?.host.as_str();
    boot.hosts_at_boot.get(host).map(|host| host.encoder)
}

/// Returns the outputs a topology declares in its snapshot as test doubles collect them: every metrics encoder, and
/// the DogStatsD client telemetry destination.
pub(super) fn collected_outputs(snapshot: &BlueprintSnapshot) -> BTreeSet<String> {
    snapshot
        .components()
        .iter()
        .filter(|component| match component.component_type() {
            ComponentType::Encoder => component.input() == Some(DataType::Event(EventType::Metric)),
            ComponentType::Destination => &**component.id() == "dsd_client_telemetry_out",
            _ => false,
        })
        .map(|component| component.id().to_string())
        .collect()
}

/// Returns the events a transform accepts.
fn event_input(component: &ComponentSnapshot) -> EventType {
    match component.input() {
        Some(DataType::Event(input)) => input,
        other => panic!("transform `{}` accepts {:?}", component.id(), other),
    }
}

/// Returns the event outputs a component declares, as output definitions.
fn event_outputs(component: &ComponentSnapshot) -> Vec<OutputDefinition<EventType>> {
    component
        .outputs()
        .iter()
        .map(|output| {
            let DataType::Event(events) = output.data_type() else {
                panic!("`{}` declares a payload output", output.id());
            };
            match output.name() {
                OutputName::Default => OutputDefinition::default_output(events),
                OutputName::Given(name) => OutputDefinition::named_output(name.to_string(), events),
            }
        })
        .collect()
}

/// Returns the name of a component's first output, which the stand-ins that replace it send on.
fn first_output(component: &ComponentSnapshot) -> OutputName {
    component
        .outputs()
        .first()
        .map(|output| output.name().clone())
        .unwrap_or(OutputName::Default)
}

/// Returns the header the isolation slot of aggregation stamps on its time-driven output in the boot generation: its
/// output point, the boot generation, and the entry its `StateOutput` program leads to in the region after it, at
/// [`FLUSHED_AT`].
fn time_driven_stamp(boot: &BootTopology) -> BatchHeader {
    let slot = aggregation_slot(&boot.frame).expect("aggregation runs in an isolation slot");
    let SlotKind::Isolation { output_point, .. } = &slot.kind else {
        unreachable!("aggregation's slot is an isolation slot");
    };
    let entry = boot.programs[slot.id.get()]
        .entries()
        .iter()
        .filter(|entry| matches!(entry.key, EntryKey::StateOutput(_)))
        .flat_map(|entry| &entry.steps)
        .find_map(|step| match step {
            Step::Exit { entry, .. } => entry.region(),
            _ => None,
        })
        .expect("aggregation's time-driven output leads to a region");
    BatchHeader {
        admitted_at: Some(AdmissionTime::from_unix_secs(FLUSHED_AT)),
        admission_point: Some(*output_point),
        epoch: 0,
        generation: Some(boot.generation.clone()),
        entry: Some(entry),
    }
}

/// Returns the events as one buffer, sized to them.
fn buffer_of(events: Vec<Event>) -> EventsBuffer {
    let mut buffer = EventsBuffer::with_capacity(events.len());
    for event in events {
        assert!(buffer.try_push(event).is_none(), "a fed batch fits one buffer");
    }
    buffer
}

/// Builds a source that sends each batch it is fed on its output of the batch's event type (`metrics`, `events`,
/// `service_checks`, `logs`, or `traces`), counting each send once it completes in `sent`, and declares the outputs of
/// the source it replaces.
struct ScriptedSourceBuilder {
    outputs: Vec<OutputDefinition<EventType>>,
    batches: StdMutex<Option<mpsc::UnboundedReceiver<Vec<Event>>>>,
    sent: Arc<AtomicUsize>,
}

impl ScriptedSourceBuilder {
    fn new(replaced: &ComponentSnapshot, batches: mpsc::UnboundedReceiver<Vec<Event>>, sent: Arc<AtomicUsize>) -> Self {
        Self {
            outputs: event_outputs(replaced),
            batches: StdMutex::new(Some(batches)),
            sent,
        }
    }
}

#[async_trait]
impl SourceBuilder for ScriptedSourceBuilder {
    fn outputs(&self) -> &[OutputDefinition<EventType>] {
        &self.outputs
    }

    async fn build(&self, _context: BuildContext) -> Result<Box<dyn Source + Send>, GenericError> {
        let batches = self
            .batches
            .lock()
            .map_err(|_| generic_error!("poisoned"))?
            .take()
            .ok_or_else(|| generic_error!("the scripted source was already built"))?;
        Ok(Box::new(ScriptedSource {
            batches,
            sent: Arc::clone(&self.sent),
        }))
    }
}

impl MemoryBounds for ScriptedSourceBuilder {
    fn specify_bounds(&self, _builder: &mut MemoryBoundsBuilder) {}
}

struct ScriptedSource {
    batches: mpsc::UnboundedReceiver<Vec<Event>>,
    sent: Arc<AtomicUsize>,
}

#[async_trait]
impl Source for ScriptedSource {
    async fn run(mut self: Box<Self>, mut context: SourceContext) -> Result<(), GenericError> {
        let mut health = context.take_health_handle();
        health.mark_ready();
        let shutdown = context.take_shutdown_handle();
        tokio::pin!(shutdown);

        // Everything fed is sent, whether or not shutdown has begun, until the feed closes.
        loop {
            tokio::select! {
                _ = health.live() => {}
                batch = self.batches.recv() => match batch {
                    Some(events) => {
                        let output = match events.first() {
                            Some(Event::EventD(_)) => "events",
                            Some(Event::ServiceCheck(_)) => "service_checks",
                            Some(Event::Log(_)) => "logs",
                            Some(Event::Trace(_)) => "traces",
                            _ => "metrics",
                        };
                        context.dispatcher().dispatch_named(output, buffer_of(events)).await?;
                        self.sent.fetch_add(1, Ordering::SeqCst);
                    }
                    None => break,
                },
            }
        }
        loop {
            tokio::select! {
                _ = health.live() => {}
                _ = &mut shutdown => return Ok(()),
            }
        }
    }
}

/// Builds a transform that sends each batch it is fed on `output` and drops its input, counting each send once it
/// completes in `sent`, in place of a transform that emits metrics of its own, such as the trace sampler, or of
/// aggregation.
struct ScriptedTransformBuilder {
    input: EventType,
    outputs: Vec<OutputDefinition<EventType>>,
    output: OutputName,
    batches: StdMutex<Option<mpsc::UnboundedReceiver<Vec<Event>>>>,
    sent: Arc<AtomicUsize>,
    stamp: Option<BatchHeader>,
}

impl ScriptedTransformBuilder {
    fn new(
        replaced: &ComponentSnapshot, output: OutputName, batches: mpsc::UnboundedReceiver<Vec<Event>>,
        sent: Arc<AtomicUsize>,
    ) -> Self {
        Self {
            input: event_input(replaced),
            outputs: event_outputs(replaced),
            output,
            batches: StdMutex::new(Some(batches)),
            sent,
            stamp: None,
        }
    }

    /// Stamps every buffer the transform sends with `stamp`, as a region stamps what it sends.
    fn stamped(mut self, stamp: Option<BatchHeader>) -> Self {
        self.stamp = stamp;
        self
    }
}

#[async_trait]
impl TransformBuilder for ScriptedTransformBuilder {
    fn input_event_type(&self) -> EventType {
        self.input
    }

    fn outputs(&self) -> &[OutputDefinition<EventType>] {
        &self.outputs
    }

    async fn build(&self, _context: BuildContext) -> Result<Box<dyn Transform + Send>, GenericError> {
        let batches = self
            .batches
            .lock()
            .map_err(|_| generic_error!("poisoned"))?
            .take()
            .ok_or_else(|| generic_error!("the scripted transform was already built"))?;
        Ok(Box::new(ScriptedTransform {
            output: self.output.clone(),
            batches,
            sent: Arc::clone(&self.sent),
            stamp: self.stamp.clone(),
        }))
    }
}

impl MemoryBounds for ScriptedTransformBuilder {
    fn specify_bounds(&self, _builder: &mut MemoryBoundsBuilder) {}
}

struct ScriptedTransform {
    output: OutputName,
    batches: mpsc::UnboundedReceiver<Vec<Event>>,
    sent: Arc<AtomicUsize>,
    stamp: Option<BatchHeader>,
}

#[async_trait]
impl Transform for ScriptedTransform {
    async fn run(mut self: Box<Self>, mut context: TransformContext) -> Result<(), GenericError> {
        let mut health = context.take_health_handle();
        health.mark_ready();
        let (mut input_open, mut feed_open) = (true, true);
        while input_open || feed_open {
            tokio::select! {
                _ = health.live() => {}
                events = context.events().next(), if input_open => input_open = events.is_some(),
                batch = self.batches.recv(), if feed_open => match batch {
                    Some(events) => {
                        let mut buffer = buffer_of(events);
                        if let Some(stamp) = &self.stamp {
                            buffer.set_header(stamp.clone());
                        }
                        match &self.output {
                            OutputName::Default => context.dispatcher().dispatch(buffer).await?,
                            OutputName::Given(name) => context.dispatcher().dispatch_named(name, buffer).await?,
                        }
                        self.sent.fetch_add(1, Ordering::SeqCst);
                    }
                    None => feed_open = false,
                },
            }
        }
        Ok(())
    }
}

/// Builds a transform that reads nothing until its gate opens, answering liveness probes meanwhile, and then sends
/// every buffer it reads on the first output of the component it replaces unchanged, until its input closes.
struct GatedTransformBuilder {
    input: EventType,
    outputs: Vec<OutputDefinition<EventType>>,
    output: OutputName,
    gate: Arc<Semaphore>,
}

impl GatedTransformBuilder {
    fn new(replaced: &ComponentSnapshot, gate: Arc<Semaphore>) -> Self {
        Self {
            input: event_input(replaced),
            outputs: event_outputs(replaced),
            output: first_output(replaced),
            gate,
        }
    }
}

#[async_trait]
impl TransformBuilder for GatedTransformBuilder {
    fn input_event_type(&self) -> EventType {
        self.input
    }

    fn outputs(&self) -> &[OutputDefinition<EventType>] {
        &self.outputs
    }

    async fn build(&self, _context: BuildContext) -> Result<Box<dyn Transform + Send>, GenericError> {
        Ok(Box::new(GatedTransform {
            output: self.output.clone(),
            gate: Arc::clone(&self.gate),
        }))
    }
}

impl MemoryBounds for GatedTransformBuilder {
    fn specify_bounds(&self, _builder: &mut MemoryBoundsBuilder) {}
}

struct GatedTransform {
    output: OutputName,
    gate: Arc<Semaphore>,
}

#[async_trait]
impl Transform for GatedTransform {
    async fn run(self: Box<Self>, mut context: TransformContext) -> Result<(), GenericError> {
        let mut health = context.take_health_handle();
        health.mark_ready();
        wait_for_gate(&self.gate, &mut health).await;
        loop {
            tokio::select! {
                _ = health.live() => {}
                events = context.events().next() => match (events, &self.output) {
                    (Some(events), OutputName::Default) => context.dispatcher().dispatch(events).await?,
                    (Some(events), OutputName::Given(name)) => context.dispatcher().dispatch_named(name, events).await?,
                    (None, _) => return Ok(()),
                },
            }
        }
    }
}

/// Waits until `gate` opens, answering liveness probes meanwhile.
async fn wait_for_gate(gate: &Semaphore, health: &mut Health) {
    loop {
        tokio::select! {
            _ = health.live() => {}
            permit = gate.acquire() => {
                permit.expect("gates are never closed").forget();
                return;
            }
        }
    }
}

/// Builds native aggregation on a test clock, configured as the native aggregation component would be.
struct ClockedAggregateBuilder {
    params: AggregateParams,
    outputs: [OutputDefinition<EventType>; 1],
}

impl ClockedAggregateBuilder {
    fn new(config: &SalukiConfiguration) -> Self {
        let (native, _) = dogstatsd_aggregate_configuration(config).expect("aggregation is configured");
        Self {
            params: AggregateParams {
                window: native.window_duration_seconds,
                context_limit: native.context_limit,
                counter_expiry: native.counter_expiry_seconds.and_then(std::num::NonZeroU64::new),
                flush_open_windows: native.flush_open_windows,
                histograms: native.hist_config.clone(),
            },
            outputs: [OutputDefinition::default_output(EventType::Metric)],
        }
    }
}

#[async_trait]
impl TransformBuilder for ClockedAggregateBuilder {
    fn input_event_type(&self) -> EventType {
        EventType::Metric
    }

    fn outputs(&self) -> &[OutputDefinition<EventType>] {
        &self.outputs
    }

    async fn build(&self, context: BuildContext) -> Result<Box<dyn Transform + Send>, GenericError> {
        let telemetry = AggregateTelemetry::new(&MetricsBuilder::from_component_context(context.component_context()));
        Ok(Box::new(ClockedAggregate {
            core: AggregateCore::new(self.params.clone(), telemetry),
        }))
    }
}

impl MemoryBounds for ClockedAggregateBuilder {
    fn specify_bounds(&self, _builder: &mut MemoryBoundsBuilder) {}
}

struct ClockedAggregate {
    core: AggregateCore,
}

#[async_trait]
impl Transform for ClockedAggregate {
    async fn run(mut self: Box<Self>, mut context: TransformContext) -> Result<(), GenericError> {
        let mut health = context.take_health_handle();
        health.mark_ready();
        loop {
            tokio::select! {
                _ = health.live() => {}
                events = context.events().next() => match events {
                    Some(events) => {
                        self.core.admit(ADMITTED_AT, events.into_iter().filter_map(Event::try_into_metric));
                    }
                    None => break,
                },
            }
        }

        let mut flushed = Vec::new();
        self.core
            .flush_into(FLUSHED_AT, true, |metric| flushed.push(Event::Metric(metric)));
        let mut dispatcher = context.dispatcher().buffered()?;
        for event in flushed {
            dispatcher.push(event).await?;
        }
        dispatcher.flush().await?;
        Ok(())
    }
}

/// Where a collecting test double reports what it receives, and the gate it waits for, if any.
#[derive(Clone)]
struct Collector {
    output: String,
    deliveries: mpsc::UnboundedSender<Delivery>,
    gate: Option<Arc<Semaphore>>,
    keep: fn(&Metric) -> bool,
}

impl Collector {
    /// Waits until the collector's gate opens, if it has one, answering liveness probes meanwhile.
    async fn wait_for_gate(&self, health: &mut Health) {
        if let Some(gate) = &self.gate {
            wait_for_gate(gate, health).await;
        }
    }

    /// Records the metrics of `buffer` that the options keep, without taking the buffer, if there are any.
    fn observe(&self, buffer: &EventsBuffer) {
        let metrics = buffer
            .into_iter()
            .filter_map(Event::try_as_metric)
            .filter(|metric| (self.keep)(metric))
            .cloned()
            .collect::<Vec<_>>();
        if metrics.is_empty() {
            return;
        }
        let _ = self.deliveries.send(Delivery {
            output: self.output.clone(),
            at: Instant::now(),
            generation: buffer.header().generation_id().map(|generation| generation.get()),
            entry: buffer.header().entry,
            metrics,
        });
    }

    fn collect(&self, buffer: EventsBuffer) {
        let at = Instant::now();
        let generation = buffer.header().generation_id().map(|generation| generation.get());
        let entry = buffer.header().entry;
        let metrics = buffer
            .into_iter()
            .filter_map(Event::try_into_metric)
            .filter(|metric| (self.keep)(metric))
            .collect();
        let _ = self.deliveries.send(Delivery {
            output: self.output.clone(),
            at,
            generation,
            entry,
            metrics,
        });
    }
}

/// Records what the plan-driven path's encode hosts route to each lease, as the delivery of the lease's output.
struct LeaseCollector {
    deliveries: mpsc::UnboundedSender<Delivery>,
    keep: fn(&Metric) -> bool,
}

impl LeaseObserver for LeaseCollector {
    fn observe(&self, output: &str, buffer: &EventsBuffer) {
        let metrics = buffer
            .into_iter()
            .filter_map(Event::try_as_metric)
            .filter(|metric| (self.keep)(metric))
            .cloned()
            .collect::<Vec<_>>();
        let _ = self.deliveries.send(Delivery {
            output: output.to_string(),
            at: Instant::now(),
            generation: buffer.header().generation_id().map(|generation| generation.get()),
            entry: buffer.header().entry,
            metrics,
        });
    }
}

/// Builds an encoder that records every buffer it receives and encodes nothing.
struct CollectingEncoderBuilder {
    collector: Collector,
    payloads: PayloadType,
}

#[async_trait]
impl EncoderBuilder for CollectingEncoderBuilder {
    fn input_event_type(&self) -> EventType {
        EventType::Metric
    }

    fn output_payload_type(&self) -> PayloadType {
        self.payloads
    }

    async fn build(&self, _context: BuildContext) -> Result<Box<dyn Encoder + Send>, GenericError> {
        Ok(Box::new(CollectingEncoder {
            collector: self.collector.clone(),
        }))
    }
}

impl MemoryBounds for CollectingEncoderBuilder {
    fn specify_bounds(&self, _builder: &mut MemoryBoundsBuilder) {}
}

struct CollectingEncoder {
    collector: Collector,
}

#[async_trait]
impl Encoder for CollectingEncoder {
    async fn run(self: Box<Self>, mut context: EncoderContext) -> Result<(), GenericError> {
        let mut health = context.take_health_handle();
        health.mark_ready();
        self.collector.wait_for_gate(&mut health).await;
        loop {
            tokio::select! {
                _ = health.live() => {}
                events = context.events().next() => match events {
                    Some(events) => self.collector.collect(events),
                    None => return Ok(()),
                },
            }
        }
    }
}

/// A real encoder whose input a collector observes: the encoder receives every buffer as it would, after the collector
/// has recorded the metrics of it that the options keep.
struct ObservedEncoder {
    inner: Box<dyn Encoder + Send>,
    collector: Arc<Collector>,
}

#[async_trait]
impl Encoder for ObservedEncoder {
    async fn run(self: Box<Self>, mut context: EncoderContext) -> Result<(), GenericError> {
        let collector = self.collector;
        context.events().observe(move |buffer| collector.observe(buffer));
        self.inner.run(context).await
    }
}

/// Builds a destination that records every buffer it receives.
struct CollectingDestinationBuilder {
    collector: Collector,
}

#[async_trait]
impl DestinationBuilder for CollectingDestinationBuilder {
    fn input_event_type(&self) -> EventType {
        EventType::Metric
    }

    async fn build(&self, _context: BuildContext) -> Result<Box<dyn Destination + Send>, GenericError> {
        Ok(Box::new(CollectingDestination {
            collector: self.collector.clone(),
        }))
    }
}

impl MemoryBounds for CollectingDestinationBuilder {
    fn specify_bounds(&self, _builder: &mut MemoryBoundsBuilder) {}
}

struct CollectingDestination {
    collector: Collector,
}

#[async_trait]
impl Destination for CollectingDestination {
    async fn run(self: Box<Self>, mut context: DestinationContext) -> Result<(), GenericError> {
        let mut health = context.take_health_handle();
        health.mark_ready();
        self.collector.wait_for_gate(&mut health).await;
        loop {
            tokio::select! {
                _ = health.live() => {}
                events = context.events().next() => match events {
                    Some(events) => self.collector.collect(events),
                    None => return Ok(()),
                },
            }
        }
    }
}

/// Builds a forwarder that drops every payload it receives, adding the metrics each HTTP payload carries to
/// `forwarded`.
struct DroppingForwarderBuilder {
    payloads: PayloadType,
    forwarded: Arc<AtomicU64>,
}

#[async_trait]
impl ForwarderBuilder for DroppingForwarderBuilder {
    fn input_payload_type(&self) -> PayloadType {
        self.payloads
    }

    async fn build(&self, _context: BuildContext) -> Result<Box<dyn Forwarder + Send>, GenericError> {
        Ok(Box::new(DroppingForwarder {
            forwarded: Arc::clone(&self.forwarded),
        }))
    }
}

impl MemoryBounds for DroppingForwarderBuilder {
    fn specify_bounds(&self, _builder: &mut MemoryBoundsBuilder) {}
}

struct DroppingForwarder {
    forwarded: Arc<AtomicU64>,
}

#[async_trait]
impl Forwarder for DroppingForwarder {
    async fn run(self: Box<Self>, mut context: ForwarderContext) -> Result<(), GenericError> {
        let mut health = context.take_health_handle();
        health.mark_ready();
        loop {
            tokio::select! {
                _ = health.live() => {}
                payload = context.payloads().next() => match payload {
                    Some(Payload::Http(payload)) => {
                        let (metadata, _) = payload.into_parts();
                        self.forwarded.fetch_add(metadata.event_count() as u64, Ordering::Relaxed);
                    }
                    Some(_) => {}
                    None => return Ok(()),
                },
            }
        }
    }
}
