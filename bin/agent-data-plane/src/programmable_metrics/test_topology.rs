//! Running topologies for tests: the primary topology as `run.rs` builds it, with the legacy or the plan-driven metrics
//! path, and with its I/O replaced by test doubles at the same positions.
//!
//! [`TestTopology::start`] builds the blueprint with `create_legacy_topology` or `create_topology`, then replaces
//! components by ID, keeping every connection and admission point (see `TopologyBlueprint::replace_source` and its
//! siblings):
//!
//! - every source, and the trace sampler, which emits metrics of its own, with a scripted component that sends each
//!   batch the test feeds it, as one buffer, on its `metrics` output;
//! - DogStatsD aggregation, when asked, with native aggregation on a test clock ([`Aggregation::Clocked`]): it admits
//!   every metric at [`ADMITTED_AT`] and flushes once, when its input ends, at [`FLUSHED_AT`], past every bucket, so
//!   that what it emits does not depend on when a test runs;
//! - every metrics encoder, and the DogStatsD client telemetry destination, with a collector that records each buffer
//!   it receives and encodes nothing;
//! - every forwarder with one that drops what it receives, so nothing leaves the process.
//!
//! Everything else runs as it does in production: the regions (with the gateways, on the plan-driven path), the native
//! transforms (with the gateways, on the legacy path), aggregation unless replaced, and, for the plan-driven path, the
//! installer.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex as StdMutex},
    time::Duration,
};

use agent_data_plane_config::SalukiConfiguration;
use agent_data_plane_config_system::ConfigurationSystem;
use arc_swap::ArcSwap;
use async_trait::async_trait;
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
        payload::PayloadType,
    },
    health::{Health, HealthRegistry},
    observability::ComponentMetricsExt as _,
    runtime::{state::ResourceRegistry, Supervisor, SupervisorError},
    topology::{BlueprintSnapshot, ComponentSnapshot, DataType, EventsBuffer, OutputDefinition, OutputName},
};
use saluki_error::{generic_error, GenericError};
use saluki_metric_kernels::aggregate::{AggregateCore, AggregateParams, AggregateTelemetry};
use saluki_metric_plan::{equivalence::OutputPoints, ir::OutputId};
use saluki_metrics::MetricsBuilder;
use tokio::{
    sync::{mpsc, oneshot, Semaphore},
    task::JoinHandle,
    time::Instant,
};

use super::{
    installer::{BootTopology, InstallerWorker, PlanStatus},
    test_support::{environment_provider, programmable_metrics},
};
use crate::{
    cli::run::{create_legacy_topology, create_topology, dogstatsd_aggregate_configuration},
    config::remote_agent_client_configuration,
};

/// The longest a test waits for a topology to deliver, to start, or to stop.
pub(super) const PATIENCE: Duration = Duration::from_secs(60);

/// The time, in Unix seconds, at which clocked aggregation admits every metric: inside one bucket of every allowed
/// window.
pub(super) const ADMITTED_AT: u64 = 1_800_000_005;

/// The time, in Unix seconds, at which clocked aggregation flushes, when its input ends: past the end of the bucket
/// [`ADMITTED_AT`] falls in, for any window, and of every timestamped value the tests send.
pub(super) const FLUSHED_AT: u64 = 1_900_000_000;

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
}

/// One buffer an output received.
#[derive(Clone, Debug)]
pub(super) struct Delivery {
    /// The component the buffer reached: an encoder, or the DogStatsD client telemetry destination.
    pub output: String,

    /// When the buffer arrived.
    pub at: Instant,

    /// The generation the buffer's header carries.
    pub generation: Option<u64>,

    /// The buffer's metrics that the options say to keep: all of them, unless a test only needs some.
    pub metrics: Vec<Metric>,
}

/// What a test does to the plan-driven metrics path between its boot and the construction of the topology.
pub(super) type BeforeTopology = Box<dyn FnOnce(&mut BootTopology)>;

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

    /// Which metrics the collectors keep in their deliveries; every one by default.
    pub keep: fn(&Metric) -> bool,

    /// What the topology waits for after it is built and before it starts its components, as `run.rs` waits for the
    /// environment to be ready; nothing by default.
    pub environment_ready: Option<futures::future::BoxFuture<'static, ()>>,
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
            keep: |_| true,
            environment_ready: None,
        }
    }
}

/// A running test topology.
pub(super) struct TestTopology {
    feeds: BTreeMap<String, mpsc::UnboundedSender<Vec<Event>>>,
    deliveries: mpsc::UnboundedReceiver<Delivery>,
    received: Vec<Delivery>,
    status: Option<Arc<ArcSwap<PlanStatus>>>,
    installer: Option<InstallerWorker>,
    gates: BTreeMap<String, Arc<Semaphore>>,
    snapshot: BlueprintSnapshot,
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
            keep,
            environment_ready,
        } = options;
        let config = SalukiConfiguration::clone(&system.config());
        let env = environment_provider(&config).await;
        let (mut blueprint, installer) = match path {
            MetricsPath::Legacy => {
                let client = (!config.control.standalone_mode)
                    .then(|| remote_agent_client_configuration(&config).expect("the client configuration resolves"));
                let (blueprint, _) = create_legacy_topology(system, client.as_ref(), &env, &registry)
                    .await
                    .expect("the legacy topology builds");
                (blueprint, None)
            }
            MetricsPath::Plan => {
                let mut metrics = programmable_metrics(&config, system.live(|config| config), &registry)
                    .expect("a metrics pipeline is required");
                if let Some(before_topology) = before_topology {
                    before_topology(metrics.boot_topology());
                }
                let (blueprint, _) = create_topology(system, &env, &registry, Some(&mut metrics))
                    .await
                    .expect("the plan-driven topology builds");
                (blueprint, Some(metrics.into_installer()))
            }
        };

        let snapshot = blueprint.snapshot().expect("the blueprint is not running yet");
        let (deliveries_tx, deliveries) = mpsc::unbounded_channel();
        let mut feeds = BTreeMap::new();
        let gates = gated
            .into_iter()
            .map(|output| (output, Arc::new(Semaphore::new(0))))
            .collect::<BTreeMap<_, _>>();
        let collector = |id: &str| Collector {
            output: id.to_string(),
            deliveries: deliveries_tx.clone(),
            gate: gates.get(id).cloned(),
            keep,
        };
        for component in snapshot.components() {
            let id = component.id().to_string();
            match component.component_type() {
                ComponentType::Source => {
                    let (feed, batches) = mpsc::unbounded_channel();
                    blueprint
                        .replace_source(&id, ScriptedSourceBuilder::new(component, batches))
                        .expect("a scripted source declares what it replaces");
                    feeds.insert(format!("{}.metrics", id), feed);
                }
                ComponentType::Transform if METRIC_EMITTING_TRANSFORMS.contains(&id.as_str()) => {
                    let (feed, batches) = mpsc::unbounded_channel();
                    blueprint
                        .replace_transform(&id, ScriptedTransformBuilder::new(component, batches))
                        .expect("a scripted transform declares what it replaces");
                    feeds.insert(format!("{}.metrics", id), feed);
                }
                ComponentType::Transform if id == "dsd_agg" && aggregation == Aggregation::Clocked => {
                    blueprint
                        .replace_transform(&id, ClockedAggregateBuilder::new(&config))
                        .expect("clocked aggregation declares what it replaces");
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
                ComponentType::Forwarder => {
                    let payloads = match component.input() {
                        Some(DataType::Payload(payloads)) => payloads,
                        other => panic!("forwarder `{}` accepts {:?}", id, other),
                    };
                    blueprint
                        .replace_forwarder(&id, DroppingForwarderBuilder { payloads })
                        .expect("a dropping forwarder declares what it replaces");
                }
                _ => {}
            }
        }
        drop(deliveries_tx);

        blueprint
            .with_health_registry(health.clone())
            .with_memory_limiter(MemoryLimiter::noop())
            .with_resource_registry(ResourceRegistry::new())
            .with_ambient_worker_pool();
        if let Some(ready) = environment_ready {
            blueprint.with_environment_readiness(ready);
        }
        let status = installer.as_ref().map(|installer| installer.status());
        let mut supervisor = Supervisor::new("test-topology").expect("the supervisor is created");
        supervisor.add_worker(health.worker());
        supervisor.add_worker(blueprint);
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
            deliveries,
            received: Vec::new(),
            status,
            installer,
            gates,
            snapshot,
            shutdown: Some(shutdown),
            task,
        }
    }

    /// Returns the blueprint of the topology, as it was built, before any component was replaced.
    pub(super) fn snapshot(&self) -> &BlueprintSnapshot {
        &self.snapshot
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

    /// Opens the gate of `output`: its collector starts reading what was sent to it.
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
            .send(metrics.into_iter().map(Event::Metric).collect())
            .expect("the scripted component runs until its feed closes");
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

/// Returns the metrics as one buffer.
fn buffer_of(events: Vec<Event>) -> EventsBuffer {
    let mut buffer = EventsBuffer::default();
    for event in events {
        assert!(buffer.try_push(event).is_none(), "a fed batch fits one buffer");
    }
    buffer
}

/// Builds a source that sends each batch it is fed on its `metrics` output, and declares the outputs of the source it
/// replaces.
struct ScriptedSourceBuilder {
    outputs: Vec<OutputDefinition<EventType>>,
    batches: StdMutex<Option<mpsc::UnboundedReceiver<Vec<Event>>>>,
}

impl ScriptedSourceBuilder {
    fn new(replaced: &ComponentSnapshot, batches: mpsc::UnboundedReceiver<Vec<Event>>) -> Self {
        Self {
            outputs: event_outputs(replaced),
            batches: StdMutex::new(Some(batches)),
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
        Ok(Box::new(ScriptedSource { batches }))
    }
}

impl MemoryBounds for ScriptedSourceBuilder {
    fn specify_bounds(&self, _builder: &mut MemoryBoundsBuilder) {}
}

struct ScriptedSource {
    batches: mpsc::UnboundedReceiver<Vec<Event>>,
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
                    Some(events) => context.dispatcher().dispatch_named("metrics", buffer_of(events)).await?,
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

/// Builds a transform that sends each batch it is fed on its `metrics` output and drops its input, in place of a
/// transform that emits metrics of its own, such as the trace sampler.
struct ScriptedTransformBuilder {
    input: EventType,
    outputs: Vec<OutputDefinition<EventType>>,
    batches: StdMutex<Option<mpsc::UnboundedReceiver<Vec<Event>>>>,
}

impl ScriptedTransformBuilder {
    fn new(replaced: &ComponentSnapshot, batches: mpsc::UnboundedReceiver<Vec<Event>>) -> Self {
        let input = match replaced.input() {
            Some(DataType::Event(input)) => input,
            other => panic!("transform `{}` accepts {:?}", replaced.id(), other),
        };
        Self {
            input,
            outputs: event_outputs(replaced),
            batches: StdMutex::new(Some(batches)),
        }
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
        Ok(Box::new(ScriptedTransform { batches }))
    }
}

impl MemoryBounds for ScriptedTransformBuilder {
    fn specify_bounds(&self, _builder: &mut MemoryBoundsBuilder) {}
}

struct ScriptedTransform {
    batches: mpsc::UnboundedReceiver<Vec<Event>>,
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
                    Some(events) => context.dispatcher().dispatch_named("metrics", buffer_of(events)).await?,
                    None => feed_open = false,
                },
            }
        }
        Ok(())
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
        let Some(gate) = &self.gate else { return };
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

    fn collect(&self, buffer: EventsBuffer) {
        let at = Instant::now();
        let generation = buffer.header().generation_id().map(|generation| generation.get());
        let metrics = buffer
            .into_iter()
            .filter_map(Event::try_into_metric)
            .filter(|metric| (self.keep)(metric))
            .collect();
        let _ = self.deliveries.send(Delivery {
            output: self.output.clone(),
            at,
            generation,
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

/// Builds a forwarder that drops every payload it receives.
struct DroppingForwarderBuilder {
    payloads: PayloadType,
}

#[async_trait]
impl ForwarderBuilder for DroppingForwarderBuilder {
    fn input_payload_type(&self) -> PayloadType {
        self.payloads
    }

    async fn build(&self, _context: BuildContext) -> Result<Box<dyn Forwarder + Send>, GenericError> {
        Ok(Box::new(DroppingForwarder))
    }
}

impl MemoryBounds for DroppingForwarderBuilder {
    fn specify_bounds(&self, _builder: &mut MemoryBoundsBuilder) {}
}

struct DroppingForwarder;

#[async_trait]
impl Forwarder for DroppingForwarder {
    async fn run(self: Box<Self>, mut context: ForwarderContext) -> Result<(), GenericError> {
        let mut health = context.take_health_handle();
        health.mark_ready();
        loop {
            tokio::select! {
                _ = health.live() => {}
                payloads = context.payloads().next() => if payloads.is_none() {
                    return Ok(());
                },
            }
        }
    }
}
