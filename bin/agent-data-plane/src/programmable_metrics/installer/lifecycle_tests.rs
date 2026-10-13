//! The artifact lifecycle through the core's own paths, in a running plan-driven topology on the connected
//! configuration path: the exit-gate suites of stage 5 (continuous input across installs, recovery, stale proposals,
//! refusals, transport failures, faults, and bounds over many installs).
//!
//! Every artifact is an edit of the export the status serves, delivered in a real envelope with its real hashes,
//! through the installer's end of an artifact channel whose transport is the test (or, in one test, the stub worker
//! polling an in-process stub server). Everything else is what runs in ADP: receipt, composition against the default
//! program last lowered, the installer core's lifecycle and acknowledgments, preparation in the regions, the commit,
//! supervision, and rollback.
//!
//! The configuration has one endpoint routing group, so that the policy region delivers to two outputs: the main
//! output, which receives every metric, and the routing group's, which receives `orders.total`. Every batch carries one
//! point of each of a few families, each a timestamped counter tagged with the batch's sequence number, so a context of
//! its own: the no-aggregation pipeline sends it past aggregation as it is, so that each point reaches each output at
//! most once, in a buffer whose header names the generation that processed it. Once a test has stopped the topology, it
//! checks each batch at every output against what the policy of the generation that processed it includes: every point
//! included exactly once, with that generation, and no point excluded.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex as StdMutex, OnceLock},
    time::Duration,
};

use axum::{extract::State, http::StatusCode as AxumStatus, response::IntoResponse as _, routing::post, Router};
use http::StatusCode;
use saluki_config::dynamic::ConfigUpdate;
use saluki_core::{
    data_model::{
        event::metric::{context::Context, Metric},
        tags::{Tag, TagSet},
    },
    execution::{Kernel, Progress, Turn},
    health::HealthRegistry,
    runtime::SupervisorError,
    topology::EventsBuffer,
};
use saluki_metric_plan::{
    channel::{ApplyState, ArtifactConfig, ArtifactConfigId, ArtifactProduct, ConfigAcknowledgment},
    equivalence::{check_capacity, CapacityVerdict},
    ir::{DecodedParams, ParamSpec, ParamType},
    limits,
    model::{Field, FieldSet, PortType},
    operators::{EffectContract, KernelSpec, OperatorDef, PerKind, PortMapping, Ports},
};
use saluki_metrics::test::TestRecorder;
use serde_json::{json, Value};
use tokio::net::TcpListener;

use super::*;
use crate::{
    internal::ConfigUpdatesWorker,
    programmable_metrics::{
        catalog::POST_ENRICH_SCOPE,
        channel_stub::{ClientInfo, StubChannelConfig, StubChannelWorker, POLL_PATH},
        test_support::{connected_configuration_system, facts},
        test_topology::{Aggregation, Delivery, MetricsPath, Options, TestTopology, PATIENCE},
    },
};

/// The configuration every test runs: one endpoint routing group, which routes `orders.total`.
const ROUTING: &str = r#"
dd_url: https://primary.example.com
additional_endpoints: {https://b.example.com: [key-b]}
experimental: {metrics_endpoint_routing: {metric_allowlist: {https://b.example.com: [orders.total]}}}
"#;

/// The main output, which the default program sends every metric to.
const MAIN: &str = "dd_metrics_encode";

/// The routing group's output, which the default program sends `orders.total` to.
const ROUTED: &str = "metrics_routing_encode_0";

/// The DogStatsD client telemetry tap, which receives every DogStatsD metric ahead of the policy region.
const TAP: &str = "dsd_client_telemetry_out";

/// A family no artifact in these tests touches: under every policy, it reaches the main output and the tap, and no
/// other output. The generation that processed a batch is the one its point of this family arrived with.
const KEPT: &str = "kept";

/// The family the default program routes to the routing group.
const ORDERS: &str = "orders.total";

/// A family the route change routes to the routing group instead.
const APP: &str = "app.requests";

/// A family the family drop removes from the main output.
const DEBUG: &str = "debug.noise";

/// One point of every family.
const FAMILIES: [&str; 4] = [KEPT, ORDERS, APP, DEBUG];

/// The config ID the tests deliver artifacts in.
const CONFIG_ID: &str = "demo";

// A test operator whose kernel panics on demand: no native kernel panics on its input.

const PANIC_PARAMS: &[ParamSpec] = &[ParamSpec::required("on", ParamType::String)];

/// `test_panic`: passes every metric through unchanged, and panics on a metric named as its `on` parameter says, as a
/// kernel with a bug would.
struct TestPanic;

impl OperatorDef for TestPanic {
    fn name(&self) -> &'static str {
        "test_panic"
    }

    fn summary(&self) -> &'static str {
        "Passes every metric through unchanged, and panics on a metric with the name it is given."
    }

    fn params(&self) -> &'static [ParamSpec] {
        PANIC_PARAMS
    }

    fn ports(&self, _params: &DecodedParams) -> Ports {
        Ports::single(PortType::ANY, PortMapping::IDENTITY)
    }

    fn contract(&self, _params: &DecodedParams) -> EffectContract {
        EffectContract {
            reads: PerKind::uniform(FieldSet::of(&[Field::Name])),
            ..EffectContract::passthrough()
        }
    }

    fn kernel(&self, params: &DecodedParams) -> Option<KernelSpec> {
        let on = params.str("on")?.to_string();
        Some(KernelSpec::new(move |_| Ok(Box::new(PanicKernel { on: on.clone() }))))
    }
}

struct PanicKernel {
    on: String,
}

impl Kernel for PanicKernel {
    fn consume(&mut self, _turn: &mut Turn<'_>, input: &mut EventsBuffer) -> Progress {
        for event in &*input {
            if event
                .try_as_metric()
                .is_some_and(|metric| metric.context().name() == self.on.as_str())
            {
                panic!("test_panic was told to panic on `{}`", self.on);
            }
        }
        Progress::Done
    }
}

/// The name a metric must have for the kernel of a `test_panicking_aggregate` node to panic.
const PANIC_STATE: &str = "panic.state";

/// Returns the native aggregation operator.
fn aggregation() -> &'static dyn OperatorDef {
    OperatorRegistry::native()
        .get(saluki_metric_plan::operators::names::AGGREGATE)
        .expect("aggregation is a native operator")
}

/// Native aggregation, under its own name, whose kernel panics on a metric named [`PANIC_STATE`] before it consumes
/// the batch, as a state owner with a bug would: everything else is native aggregation's, its parameters, ports,
/// contract, state, bounds, cost, and partition key. It replaces native aggregation in [`panicking_state_operators`],
/// so that the boot program's state owner runs it, since a state owner cannot change its operator across generations.
struct TestPanickingAggregate;

impl OperatorDef for TestPanickingAggregate {
    fn name(&self) -> &'static str {
        aggregation().name()
    }

    fn summary(&self) -> &'static str {
        "Aggregates as native aggregation does, and panics on a metric with the name a test gives it."
    }

    fn cost(&self, params: &DecodedParams) -> u32 {
        aggregation().cost(params)
    }

    fn params(&self) -> &'static [ParamSpec] {
        aggregation().params()
    }

    fn check_params(&self, params: &DecodedParams) -> Vec<saluki_metric_plan::ir::ParamError> {
        aggregation().check_params(params)
    }

    fn ports(&self, params: &DecodedParams) -> Ports {
        aggregation().ports(params)
    }

    fn contract(&self, params: &DecodedParams) -> EffectContract {
        aggregation().contract(params)
    }

    fn output_types(&self, params: &DecodedParams, ports: &Ports, inputs: &[PortType]) -> Vec<PortType> {
        aggregation().output_types(params, ports, inputs)
    }

    fn transition(&self, old: &DecodedParams, new: &DecodedParams) -> saluki_metric_plan::extension::TransitionClass {
        aggregation().transition(old, new)
    }

    fn specify_bounds(&self, params: &DecodedParams, builder: &mut saluki_core::accounting::MemoryBoundsBuilder) {
        aggregation().specify_bounds(params, builder);
    }

    fn specify_state_bounds(&self, params: &DecodedParams, builder: &mut saluki_core::accounting::MemoryBoundsBuilder) {
        aggregation().specify_state_bounds(params, builder);
    }

    fn kernel(&self, params: &DecodedParams) -> Option<KernelSpec> {
        let inner = aggregation().kernel(params)?;
        Some(KernelSpec::new(move |context| {
            Ok(Box::new(PanickingState {
                inner: inner.build(context)?,
            }))
        }))
    }

    fn lineage_params(&self, params: &DecodedParams) -> saluki_metric_plan::ir::Params {
        aggregation().lineage_params(params)
    }

    fn step_params(
        &self, params: &DecodedParams, filter: Option<Arc<dyn saluki_metric_kernels::aggregate::EmissionFilter>>,
    ) -> Option<Arc<dyn std::any::Any + Send + Sync>> {
        aggregation().step_params(params, filter)
    }

    fn flush_interval(&self, params: &DecodedParams) -> Option<Duration> {
        aggregation().flush_interval(params)
    }

    fn partition_key(&self) -> Option<saluki_metric_plan::operators::PartitionKey> {
        aggregation().partition_key()
    }
}

/// A state owner's kernel that panics, before it consumes a batch, on a metric named [`PANIC_STATE`], and otherwise
/// runs `inner`.
struct PanickingState {
    inner: Box<dyn Kernel>,
}

impl Kernel for PanickingState {
    fn consume(&mut self, turn: &mut Turn<'_>, input: &mut EventsBuffer) -> Progress {
        if input.into_iter().any(|event| {
            event
                .try_as_metric()
                .is_some_and(|metric| metric.context().name() == PANIC_STATE)
        }) {
            panic!("the test's aggregation was told to panic on `{}`", PANIC_STATE);
        }
        self.inner.consume(turn, input)
    }

    fn advance_time(&mut self, turn: &mut Turn<'_>) -> Progress {
        self.inner.advance_time(turn)
    }

    fn control(&mut self, turn: &mut Turn<'_>, control: &mut dyn std::any::Any) -> Progress {
        self.inner.control(turn, control)
    }

    fn finish(&mut self, turn: &mut Turn<'_>, policy: saluki_core::execution::DrainPolicy) -> Progress {
        self.inner.finish(turn, policy)
    }

    fn held(&self) -> saluki_core::execution::LossCount {
        self.inner.held()
    }

    fn publish_telemetry(&mut self) {
        self.inner.publish_telemetry();
    }
}

/// Returns the native operators and `test_panic`.
fn operators() -> OperatorRegistry {
    static OPERATORS: OnceLock<OperatorRegistry> = OnceLock::new();
    *OPERATORS.get_or_init(|| {
        let operators = OperatorRegistry::native()
            .iter()
            .chain([&TestPanic as &'static dyn OperatorDef])
            .collect::<Vec<_>>();
        OperatorRegistry::with_operators(Box::leak(operators.into_boxed_slice()))
    })
}

/// Returns the native operators, with aggregation's replaced by [`TestPanickingAggregate`], and `test_panic`.
fn panicking_state_operators() -> OperatorRegistry {
    static OPERATORS: OnceLock<OperatorRegistry> = OnceLock::new();
    *OPERATORS.get_or_init(|| {
        let operators = OperatorRegistry::native()
            .iter()
            .filter(|operator| operator.name() != aggregation().name())
            .chain([
                &TestPanickingAggregate as &'static dyn OperatorDef,
                &TestPanic as &'static dyn OperatorDef,
            ])
            .collect::<Vec<_>>();
        OperatorRegistry::with_operators(Box::leak(operators.into_boxed_slice()))
    })
}

// Artifacts: edits of the export.

/// Returns the edges of an artifact or plan document.
fn edges(document: &mut Value) -> &mut Vec<Value> {
    document["edges"].as_array_mut().expect("documents have edges")
}

/// Returns the nodes of an artifact or plan document.
fn nodes(document: &mut Value) -> &mut Vec<Value> {
    document["nodes"].as_array_mut().expect("documents have nodes")
}

/// Puts `node`, whose input port is `in` and whose port `out` continues, in front of the main output's `emit`.
fn in_front_of_the_main_output(document: &mut Value, node: Value, out: &str) {
    in_front_of(document, "metrics_policy_0.in", node, out);
}

/// Puts `node`, whose input port is `in` and whose port `out` continues, in front of the input port `port`
/// (`<node>.<port>`), which one edge leads to.
fn in_front_of(document: &mut Value, port: &str, node: Value, out: &str) {
    let id = node["id"].as_str().expect("nodes have IDs").to_string();
    let listed = edges(document).clone();
    let into = edges(document)
        .iter_mut()
        .find(|edge| edge["to"] == port)
        .unwrap_or_else(|| panic!("no edge leads to `{}`: {:?}", port, listed));
    into["to"] = json!(format!("{}.in", id));
    edges(document).push(json!({"from": format!("{}.{}", id, out), "to": port}));
    nodes(document).push(node);
}

/// Returns `export` with the routing group's filter matching the intake names that start with `prefix` in place of
/// the group's lists: a route change.
fn route(export: &Value, prefix: &str) -> Value {
    let mut artifact = export.clone();
    let filter = nodes(&mut artifact)
        .iter_mut()
        .find(|node| node["id"] == "metrics_routing_filter_0")
        .expect("the export has the routing group's filter");
    filter["params"]["predicate"] = json!({"intake_name": {"matches": {"prefix": true, "values": [prefix]}}});
    artifact
}

/// Returns `artifact` with the metrics whose names start with `prefix` dropped in front of the main output, by the
/// split `id`: a family drop.
fn drop_family(artifact: &Value, id: &str, prefix: &str) -> Value {
    let mut artifact = artifact.clone();
    let split = json!({
        "id": id, "op": "split",
        "params": {"predicate": {"name": {"matches": {"prefix": true, "values": [prefix]}}}}
    });
    in_front_of_the_main_output(&mut artifact, split, "false");
    let dropped = format!("{}_dropped", id);
    edges(&mut artifact).push(json!({"from": format!("{}.true", id), "to": format!("{}.in", dropped)}));
    nodes(&mut artifact).push(json!({"id": dropped, "op": "drop"}));
    artifact
}

/// Returns `artifact` with a `test_panic` node `id` in front of the main output, panicking on metrics named `on`.
fn faulty(artifact: &Value, id: &str, on: &str) -> Value {
    let mut artifact = artifact.clone();
    let node = json!({"id": id, "op": "test_panic", "params": {"on": on}});
    in_front_of_the_main_output(&mut artifact, node, "out");
    artifact
}

/// Returns the hash of an artifact document, as the core computes it on receipt.
fn artifact_hash(artifact: &Value) -> PlanHash {
    ReceivedArtifact::from_json(&artifact.to_string())
        .expect("the artifact is readable")
        .hash
}

/// Returns the config `id` delivering `artifact` as revision `revision`, in version `revision`, approved against
/// `approved_against` if given.
fn config_with(id: &str, revision: u64, artifact: &Value, approved_against: Option<PlanHash>) -> ArtifactConfig {
    let mut envelope = json!({"revision": revision, "artifact_hash": artifact_hash(artifact), "artifact": artifact});
    if let Some(hash) = approved_against {
        envelope["approved_against"] = json!({"active_plan_hash": hash});
    }
    let payload = envelope.to_string();
    ArtifactConfig {
        id: ArtifactConfigId::new(id),
        version: revision,
        sha256: ArtifactConfig::transport_hash(&payload),
        payload,
    }
}

/// Returns the config the tests deliver `artifact` in, as revision `revision`.
fn config(revision: u64, artifact: &Value) -> ArtifactConfig {
    config_with(CONFIG_ID, revision, artifact, None)
}

// Batches.

/// Returns batch `seq`: one timestamped counter of every name in `names`, each tagged with the sequence number.
fn batch(seq: usize, names: &[&str]) -> Vec<Metric> {
    names
        .iter()
        .map(|name| {
            let tags = TagSet::from_iter([Tag::from(format!("seq:{}", seq))]);
            Metric::counter(
                Context::from_parts(name.to_string(), tags),
                (1_700_000_000 + seq as u64, 1.0),
            )
        })
        .collect()
}

/// Returns the sequence number of the batch a metric belongs to.
fn seq_of(metric: &Metric) -> usize {
    metric
        .context()
        .tags()
        .into_iter()
        .find_map(|tag| tag.as_str().strip_prefix("seq:").and_then(|seq| seq.parse().ok()))
        .unwrap_or_else(|| panic!("`{}` belongs to no batch", metric.context().name()))
}

/// The generation of every buffer each point arrived in, by output, family, and batch.
type Arrivals = BTreeMap<(String, String, usize), Vec<Option<u64>>>;

/// Returns the generation of every buffer each point arrived in.
fn arrivals(deliveries: &[Delivery]) -> Arrivals {
    let mut arrivals = Arrivals::new();
    for delivery in deliveries {
        for metric in &delivery.metrics {
            arrivals
                .entry((
                    delivery.output.clone(),
                    metric.context().name().to_string(),
                    seq_of(metric),
                ))
                .or_default()
                .push(delivery.generation);
        }
    }
    arrivals
}

/// What a policy delivers of a batch: the families each output receives.
type Routing = BTreeMap<&'static str, BTreeSet<&'static str>>;

/// Returns what the default program delivers: every family to the main output and the tap, and `orders.total` to the
/// routing group.
fn default_routing() -> Routing {
    routing(&FAMILIES, &[ORDERS])
}

/// Returns a policy that delivers `main` to the main output, `routed` to the routing group, and every family to the
/// tap.
fn routing(main: &[&'static str], routed: &[&'static str]) -> Routing {
    BTreeMap::from([
        (MAIN, main.iter().copied().collect()),
        (ROUTED, routed.iter().copied().collect()),
        (TAP, FAMILIES.iter().copied().collect()),
    ])
}

/// What a stopped harness received.
struct Finished {
    arrivals: Arrivals,
    /// The names of every batch sent, by sequence number.
    sent: Vec<Vec<&'static str>>,
}

impl Finished {
    /// Returns the generation that processed batch `seq`: the one its `kept` point arrived at the main output with,
    /// if it arrived exactly once.
    fn generation(&self, seq: usize) -> Option<u64> {
        match self
            .arrivals
            .get(&(MAIN.to_string(), KEPT.to_string(), seq))
            .map(Vec::as_slice)
        {
            Some([Some(generation)]) => Some(*generation),
            _ => None,
        }
    }

    /// Returns the generation that processed each batch, in sequence order, panicking for a batch that did not arrive
    /// exactly once.
    fn generations(&self) -> Vec<u64> {
        self.generations_but(&BTreeSet::new())
    }

    /// Returns the generation that processed each batch but those in `lost`, in sequence order, panicking for a batch
    /// that did not arrive exactly once.
    fn generations_but(&self, lost: &BTreeSet<usize>) -> Vec<u64> {
        (0..self.sent.len())
            .filter(|seq| !lost.contains(seq))
            .map(|seq| {
                self.generation(seq).unwrap_or_else(|| {
                    panic!(
                        "batch {} reached the main output as {:?}, not exactly once",
                        seq,
                        self.arrivals.get(&(MAIN.to_string(), KEPT.to_string(), seq))
                    )
                })
            })
            .collect()
    }

    /// Checks every batch but those in `lost` against what the policy of the generation that processed it delivers,
    /// `routing(generation)`: at every output, each point of the batch it includes arrived exactly once, with that
    /// generation, and no point it excludes arrived.
    ///
    /// A batch in `lost` was lost to a panic in the policy region: only the tap, which a region of its own feeds ahead
    /// of the policy region, received it, each point exactly once.
    fn check(&self, lost: &BTreeSet<usize>, routing: impl Fn(u64) -> Routing) {
        let outputs = self
            .arrivals
            .keys()
            .map(|(output, _, _)| output.as_str())
            .chain([MAIN, ROUTED, TAP])
            .collect::<BTreeSet<_>>();
        for (seq, names) in self.sent.iter().enumerate() {
            if lost.contains(&seq) {
                let arrived = self
                    .arrivals
                    .iter()
                    .filter(|((output, _, arrived), _)| *arrived == seq && output != TAP)
                    .collect::<Vec<_>>();
                assert!(arrived.is_empty(), "batch {} was lost, but arrived: {:?}", seq, arrived);
                for name in names {
                    let tapped = self.arrivals.get(&(TAP.to_string(), name.to_string(), seq));
                    assert_eq!(tapped.map(Vec::len), Some(1), "batch {}: `{}` at the tap", seq, name);
                }
                continue;
            }
            let generation = self
                .generation(seq)
                .unwrap_or_else(|| panic!("batch {} did not arrive", seq));
            let delivered = routing(generation);
            for output in &outputs {
                for name in names {
                    let arrivals = self
                        .arrivals
                        .get(&(output.to_string(), name.to_string(), seq))
                        .cloned()
                        .unwrap_or_default();
                    let included = delivered.get(output).is_some_and(|families| families.contains(name));
                    let expected = if included { vec![Some(generation)] } else { Vec::new() };
                    assert_eq!(
                        arrivals, expected,
                        "batch {}, processed by generation {}: `{}` at `{}`",
                        seq, generation, name, output
                    );
                }
            }
        }
    }
}

// The harness.

/// What delivers snapshots to the installer.
enum Transport {
    /// The test, through the channel's publisher.
    Publisher,

    /// The stub worker, polling a stub server.
    Stub(Box<StubChannelConfig>),
}

/// How a [`Lifecycle`] harness starts.
struct Start {
    /// The options the test topology starts with.
    options: Options,

    /// The local configuration: [`ROUTING`], unless a test runs another, such as one with lanes.
    config: &'static str,

    /// The DogStatsD feeds batches go to, in turn: the source's metrics output, unless the test runs producer lanes.
    feeds: Vec<String>,

    /// What delivers snapshots.
    transport: Transport,

    /// The core's artifact paths, if not the core's own.
    paths: Option<Box<dyn ArtifactPaths>>,
}

impl Start {
    /// Returns how a harness starts with `options`, the test as the transport, and the core's own artifact paths.
    fn new(options: Options) -> Self {
        Self {
            options,
            config: ROUTING,
            feeds: vec!["dsd_in.metrics".to_string()],
            transport: Transport::Publisher,
            paths: None,
        }
    }
}

/// A running plan-driven topology of the [`ROUTING`] configuration, or of the one its start names, connected to an
/// artifact channel and to the core's own artifact paths.
struct Lifecycle {
    topology: TestTopology,
    status: Arc<ArcSwap<PlanStatus>>,
    transport: Option<TransportEnd>,
    /// The configuration stream, kept open: the worker that applies configuration updates fails once it closes.
    _stream: mpsc::Sender<ConfigUpdate>,
    /// The artifact of the `post_enrich` export, as the status served it at boot.
    export: Value,
    /// The default program of the configuration at boot, and its catalog.
    default_program: (Plan, AdpCatalog),
    /// The names of every batch sent, by sequence number.
    sent: Vec<Vec<&'static str>>,

    /// The DogStatsD feeds batches go to, in turn.
    feeds: Vec<String>,
}

impl Lifecycle {
    /// Starts the topology with the default options.
    async fn start() -> Self {
        Self::start_with(Start::new(Options::new(MetricsPath::Plan, Aggregation::Native))).await
    }

    /// Starts the topology as `start` says.
    async fn start_with(start: Start) -> Self {
        let Start {
            mut options,
            config,
            feeds,
            transport,
            paths,
        } = start;
        let (system, updates, stream) = connected_configuration_system(config).await;
        let config = SalukiConfiguration::clone(&system.config());
        let lowered = Lowering::new(Arc::new(config.clone()), facts(&config))
            .expect("the configuration lowers")
            .expect("a metrics pipeline is required")
            .lower(&config)
            .expect("the configuration lowers");

        let connected = Arc::new(StdMutex::new(None));
        let worker = Arc::new(StdMutex::new(None));
        options.before_install = Some(Box::new({
            let connected = Arc::clone(&connected);
            let worker = Arc::clone(&worker);
            move |metrics: &mut ProgrammableMetrics| {
                let end = metrics.connect_artifact_channel();
                match transport {
                    Transport::Publisher => *connected.lock().unwrap() = Some(end),
                    Transport::Stub(config) => {
                        *worker.lock().unwrap() = Some(StubChannelWorker::new(*config, ClientInfo::for_process(), end))
                    }
                }
                if let Some(paths) = paths {
                    metrics.with_artifact_paths(paths);
                }
            }
        }));
        let topology = TestTopology::start(&system, options, |supervisor| {
            supervisor.add_worker(ConfigUpdatesWorker::new(updates));
            if let Some(worker) = worker.lock().unwrap().take() {
                supervisor.add_worker(worker);
            }
        })
        .await;
        let status = topology.status().expect("the plan-driven path has an installer");
        let scope = ScopeName::new(POST_ENRICH_SCOPE).unwrap();
        let bundle = status.load().exports[&scope].clone().expect("the scope exports");
        let export = serde_json::to_value(&bundle.artifact).unwrap();
        let transport = connected.lock().unwrap().take();
        Self {
            topology,
            status,
            transport,
            _stream: stream,
            export,
            default_program: (lowered.plan, lowered.catalog),
            sent: Vec::new(),
            feeds,
        }
    }

    /// Delivers a snapshot of `configs`.
    fn deliver(&self, configs: Vec<ArtifactConfig>) {
        let transport = self.transport.as_ref().expect("the test is the transport");
        assert!(transport
            .publisher
            .publish(ArtifactSnapshot::new(ArtifactProduct::Policy, configs)));
    }

    /// Sends the next batch of `names` without waiting for it, and returns its sequence number: batch `seq` goes to the
    /// feed `seq mod feeds` of the harness's feeds.
    fn feed(&mut self, names: &[&'static str]) -> usize {
        let seq = self.sent.len();
        let feed = &self.feeds[seq % self.feeds.len()];
        self.topology.feed(feed, batch(seq, names));
        self.sent.push(names.to_vec());
        seq
    }

    /// Sends the next batch of every family, waits until its `kept` point reaches the main output, and returns the
    /// generation that processed it.
    async fn send(&mut self) -> Option<u64> {
        self.send_names(&FAMILIES).await
    }

    /// Sends the next batch of `names`, which must include `kept`, waits until its `kept` point reaches the main
    /// output, and returns the generation that processed it.
    async fn send_names(&mut self, names: &[&'static str]) -> Option<u64> {
        let seq = self.feed(names);
        let key = (MAIN.to_string(), KEPT.to_string(), seq);
        let deliveries = self
            .topology
            .wait_for(&format!("batch {}", seq), |deliveries| {
                arrivals(deliveries).contains_key(&key)
            })
            .await;
        arrivals(deliveries)[&key][0]
    }

    /// Sends batches of every family, 32 every millisecond, without waiting for any of them, until the status satisfies
    /// `done`, and then twenty more at once, and returns the status that satisfied it.
    async fn feed_until(&mut self, done: impl Fn(&PlanStatus) -> bool) -> PlanStatus {
        let started = tokio::time::Instant::now();
        let status = loop {
            let current = self.status.load_full();
            if done(&current) {
                break PlanStatus::clone(&current);
            }
            assert!(
                started.elapsed() < PATIENCE,
                "the installer's status never changed as expected: {}",
                current
            );
            for _ in 0..32 {
                self.feed(&FAMILIES);
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        };
        for _ in 0..20 {
            self.feed(&FAMILIES);
        }
        status
    }

    /// Waits until the status satisfies `done`, returning it.
    async fn status_where(&self, done: impl Fn(&PlanStatus) -> bool) -> PlanStatus {
        tokio::time::timeout(PATIENCE, async {
            loop {
                let current = self.status.load_full();
                if done(&current) {
                    return PlanStatus::clone(&current);
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .unwrap_or_else(|_| {
            panic!(
                "the installer's status never changed as expected: {}",
                self.status.load()
            )
        })
    }

    /// Stops the topology and returns what it received.
    async fn finish(self) -> Finished {
        let deliveries = self.topology.stop().await;
        Finished {
            arrivals: arrivals(&deliveries),
            sent: self.sent,
        }
    }

    /// Waits until the topology stops on its own, as it does when a component fails, and returns why, and what it
    /// received.
    async fn failure(self) -> (SupervisorError, Finished) {
        let (error, deliveries) = self.topology.failure().await;
        let finished = Finished {
            arrivals: arrivals(&deliveries),
            sent: self.sent,
        };
        (error, finished)
    }
}

/// Returns the catalog the [`ROUTING`] configuration lowers with.
async fn routing_catalog() -> AdpCatalog {
    let config = crate::programmable_metrics::test_support::configuration(ROUTING).await;
    Lowering::new(Arc::new(config.clone()), facts(&config))
        .expect("the configuration lowers")
        .expect("a metrics pipeline is required")
        .lower(&config)
        .expect("the configuration lowers")
        .catalog
}

// Reading the status.

fn active(status: &PlanStatus) -> Option<u64> {
    status.installer.active.as_ref().map(|active| active.id.get())
}

fn active_origin(status: &PlanStatus) -> Option<Origin> {
    status.installer.active.as_ref().map(|active| active.origin)
}

fn live(status: &PlanStatus) -> Vec<u64> {
    status
        .installer
        .live
        .iter()
        .map(|generation| generation.get())
        .collect()
}

fn record(status: &PlanStatus, generation: u64) -> Option<(GenerationState, Option<String>)> {
    status
        .installer
        .generations
        .iter()
        .find(|record| record.id.get() == generation)
        .map(|record| (record.state, record.reason.clone()))
}

/// Returns the acknowledgment of config `id`, as the installer last reported it.
fn ack<'a>(status: &'a PlanStatus, id: &str) -> Option<&'a ConfigAcknowledgment> {
    status
        .apply_states
        .config_states
        .iter()
        .find(|ack| ack.id == ArtifactConfigId::new(id))
}

/// Returns the state, revision, and generation of the acknowledgment of config `id`.
fn ack_of(status: &PlanStatus, id: &str) -> Option<(ApplyState, Option<u64>, Option<u64>)> {
    ack(status, id).map(|ack| (ack.state, ack.revision, ack.generation))
}

/// Returns the error of the acknowledgment of config `id`.
fn ack_error(status: &PlanStatus, id: &str) -> Option<String> {
    ack(status, id).and_then(|ack| ack.error.clone())
}

/// Returns the nodes of the active plan, without their provenance, and its edges.
fn structure(status: &PlanStatus) -> (Vec<(String, String, String)>, String) {
    let plan = &status.active.as_ref().expect("a generation is active").plan;
    let nodes = plan
        .nodes
        .iter()
        .map(|node| {
            let params = serde_json::to_string(&node.params).unwrap();
            (node.id.to_string(), node.op.to_string(), params)
        })
        .collect();
    (nodes, serde_json::to_string(&plan.edges).unwrap())
}

/// Returns the revision of the artifact in effect, as the status summary reports it.
fn active_revision(status: &PlanStatus) -> Option<u64> {
    status
        .apply_states
        .status
        .active_artifact
        .as_ref()
        .map(|artifact| artifact.revision)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn continuous_input_across_a_series_of_real_artifacts_delivers_every_point_the_policy_includes_exactly_once() {
    let mut harness = Lifecycle::start().await;
    let export = harness.export.clone();
    let route_change = route(&export, "app.");
    let family_drop = drop_family(&route_change, "drop_debug", "debug.");

    // Batches are sent one after another without waiting for any of them, so that every install, revocation, and
    // re-approval happens with batches in flight.
    harness.feed_until(|_| true).await;

    // The unmodified export composes to the default program that runs: no generation is created for it, and the
    // default program's generation serves it.
    harness.deliver(vec![config(1, &export)]);
    let no_op = harness
        .feed_until(|status| ack_of(status, CONFIG_ID) == Some((ApplyState::Active, Some(1), Some(1))))
        .await;
    assert_eq!((active(&no_op), active_revision(&no_op)), (Some(1), Some(1)));
    assert_eq!(no_op.installer.revision_high_water, Some(1));

    // A route change: the routing group receives `app.requests` in place of `orders.total`.
    harness.deliver(vec![config(2, &route_change)]);
    let routed = harness.feed_until(|status| active(status) == Some(2)).await;
    assert!(matches!(
        active_origin(&routed),
        Some(Origin::Artifact { revision: 2, .. })
    ));

    // A family drop, on top of the route change: the main output no longer receives `debug.noise`.
    harness.deliver(vec![config(3, &family_drop)]);
    let dropped = harness.feed_until(|status| active(status) == Some(3)).await;
    assert_eq!(
        dropped.installer.last_known_good.as_ref().map(|kept| kept.id.get()),
        Some(2)
    );

    // A revocation: the default program of the current configuration is installed again, and the revoked artifact's
    // acknowledgment reads `retired` once its generation has retired. The high-water mark stays.
    harness.deliver(Vec::new());
    let revoked = harness
        .feed_until(|status| {
            active(status) == Some(4)
                && ack_of(status, CONFIG_ID).is_some_and(|(state, ..)| state == ApplyState::Retired)
        })
        .await;
    assert!(matches!(active_origin(&revoked), Some(Origin::DefaultProgram { .. })));
    assert_eq!(*revoked.active.as_ref().unwrap().plan, harness.default_program.0);
    assert_eq!(
        ack_of(&revoked, CONFIG_ID),
        Some((ApplyState::Retired, Some(3), Some(3)))
    );
    assert_eq!(
        (active_revision(&revoked), revoked.installer.revision_high_water),
        (None, Some(3))
    );

    // The route change approved again under a higher revision: the same document, delivered anew after the
    // revocation, installs again as a generation of its own, which keeps the default program's to roll back to.
    harness.deliver(vec![config(4, &route_change)]);
    let reapproved = harness.feed_until(|status| active(status) == Some(5)).await;
    assert_eq!(
        ack_of(&reapproved, CONFIG_ID),
        Some((ApplyState::Active, Some(4), Some(5)))
    );
    assert_eq!(
        reapproved.installer.last_known_good.as_ref().map(|kept| kept.id.get()),
        Some(4)
    );
    // Its plan is the route change's, but for the provenance, which names the revision that approved it.
    assert_eq!(structure(&reapproved), structure(&routed));

    // The same document approved under a higher revision again, while it is in effect: acknowledged with the new
    // revision, without a generation, and the high-water mark moves.
    harness.deliver(vec![config(5, &route_change)]);
    let idempotent = harness
        .feed_until(|status| ack_of(status, CONFIG_ID) == Some((ApplyState::Active, Some(5), Some(5))))
        .await;
    assert_eq!(idempotent.installer.revision_high_water, Some(5));
    assert!(record(&idempotent, 6).is_none(), "{}", idempotent);
    let finished = harness.finish().await;

    // Every batch was processed by one generation, in the order the schedule held them, and every point arrived at
    // every output as that generation's policy says, exactly once.
    let mut order = finished.generations();
    order.dedup();
    assert_eq!(order, [1, 2, 3, 4, 5]);
    finished.check(&BTreeSet::new(), |generation| match generation {
        1 | 4 => default_routing(),
        2 | 5 => routing(&FAMILIES, &[APP]),
        3 => routing(&[KEPT, ORDERS, APP], &[APP]),
        other => panic!("generation {} processed a batch", other),
    });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn after_a_restart_the_default_program_runs_until_the_channel_delivers_again_and_then_the_artifact() {
    // The first process installs the route change.
    let mut first = Lifecycle::start().await;
    let snapshot = vec![config(7, &route(&first.export, "app."))];
    first.deliver(snapshot.clone());
    let installed = first.status_where(|status| active(status) == Some(2)).await;
    assert_eq!(first.send().await, Some(2));
    first
        .finish()
        .await
        .check(&BTreeSet::new(), |_| routing(&FAMILIES, &[APP]));

    // A new process starts with new installer state: until the channel delivers, nothing has been seen, which is what
    // it reports, and the default program runs.
    let mut second = Lifecycle::start().await;
    let reported = second.transport.as_ref().unwrap().apply_states.latest();
    assert_eq!(
        (
            reported.status.active_generation,
            reported.status.revision_high_water,
            reported.status.active_artifact.as_ref(),
            reported.config_states.len()
        ),
        (Some(1), None, None, 0)
    );
    second.feed_until(|_| true).await;

    // The channel delivers its snapshot again: the artifact installs anew, composed to the same plan as in the first
    // process, and is acknowledged as it was there.
    second.deliver(snapshot);
    let reinstalled = second.feed_until(|status| active(status) == Some(2)).await;
    assert_eq!(
        ack_of(&reinstalled, CONFIG_ID),
        Some((ApplyState::Active, Some(7), Some(2)))
    );
    assert_eq!(reinstalled.installer.revision_high_water, Some(7));
    assert_eq!(
        reinstalled.installer.active.as_ref().map(|active| active.plan_hash),
        installed.installer.active.as_ref().map(|active| active.plan_hash)
    );
    let finished = second.finish().await;
    let mut order = finished.generations();
    order.dedup();
    assert_eq!(order, [1, 2]);
    finished.check(&BTreeSet::new(), |generation| match generation {
        1 => default_routing(),
        _ => routing(&FAMILIES, &[APP]),
    });
}

/// Runs the installer worker as a supervisor would, until `shutdown` triggers or the task is aborted.
fn run_installer(installer: &Arc<InstallerWorker>, shutdown: ShutdownHandle) -> impl std::future::Future<Output = ()> {
    let installer = Arc::clone(installer);
    async move {
        let run = match installer.initialize(shutdown).await {
            Ok(run) => run,
            Err(_) => panic!("the installer initializes"),
        };
        run.await.expect("the installer stops cleanly");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_restarted_installer_worker_keeps_the_high_water_mark_and_the_acknowledgments() {
    let mut options = Options::new(MetricsPath::Plan, Aggregation::Native);
    options.supervise_installer = false;
    let mut harness = Lifecycle::start_with(Start::new(options)).await;
    let installer = Arc::new(harness.topology.take_installer());
    let route_change = route(&harness.export, "app.");
    let family_drop = drop_family(&harness.export, "drop_debug", "debug.");

    // The first run installs the route change, then stops abruptly, as a worker aborted at an await point does.
    let first = tokio::spawn(run_installer(&installer, ShutdownHandle::noop()));
    harness.deliver(vec![config(2, &route_change)]);
    harness
        .status_where(|status| ack_of(status, CONFIG_ID) == Some((ApplyState::Active, Some(2), Some(2))))
        .await;
    first.abort();
    assert!(first.await.unwrap_err().is_cancelled());
    assert_eq!(harness.send().await, Some(2));

    // The second run continues with the core the first one left: the high-water mark and the acknowledgment are as
    // they were, so another artifact under the same revision, or a lower one, is stale.
    let (coordinator, shutdown) = ShutdownHandle::paired();
    let second = tokio::spawn(run_installer(&installer, shutdown));
    let stale = |revision: u64| {
        format!(
            "revision {} is stale: revision 2 has already been seen, and revisions must increase",
            revision
        )
    };
    for revision in [2, 1] {
        harness.deliver(vec![config(revision, &family_drop)]);
        let rejected = harness
            .status_where(|status| ack_of(status, CONFIG_ID) == Some((ApplyState::Rejected, Some(revision), None)))
            .await;
        assert_eq!(ack_error(&rejected, CONFIG_ID), Some(stale(revision)));
        assert_eq!(
            (active(&rejected), rejected.installer.revision_high_water),
            (Some(2), Some(2))
        );
        assert_eq!(harness.send().await, Some(2));
    }

    // A newer revision installs.
    harness.deliver(vec![config(3, &family_drop)]);
    harness.status_where(|status| active(status) == Some(3)).await;
    assert_eq!(harness.send().await, Some(3));
    coordinator.shutdown();
    tokio::time::timeout(PATIENCE, second).await.unwrap().unwrap();
    let finished = harness.finish().await;
    finished.check(&BTreeSet::new(), |generation| match generation {
        2 => routing(&FAMILIES, &[APP]),
        3 => routing(&[KEPT, ORDERS, APP], &[ORDERS]),
        other => panic!("generation {} processed a batch", other),
    });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stale_revisions_are_rejected_and_a_redelivery_or_a_reapproval_acknowledges_the_current_state() {
    let mut harness = Lifecycle::start().await;
    let route_change = route(&harness.export, "app.");
    let family_drop = drop_family(&harness.export, "drop_debug", "debug.");
    let (routed, dropped) = (artifact_hash(&route_change), artifact_hash(&family_drop));
    harness.deliver(vec![config(5, &route_change)]);
    harness.status_where(|status| active(status) == Some(2)).await;
    assert_eq!(harness.send().await, Some(2));

    let stale = |revision: u64, high_water: u64| {
        Some(format!(
            "revision {} is stale: revision {} has already been seen, and revisions must increase",
            revision, high_water
        ))
    };
    // Each delivery, and the acknowledgment, high-water mark, and revision in effect it leaves: (revision, artifact),
    // and (state, generation, error, artifact hash), and the mark and the revision.
    let cases = [
        // Another artifact under the revision that set the mark is stale, and so is one below it.
        (
            (5, &family_drop),
            (ApplyState::Rejected, None, stale(5, 5), dropped),
            (5, 5),
        ),
        (
            (4, &family_drop),
            (ApplyState::Rejected, None, stale(4, 5), dropped),
            (5, 5),
        ),
        // The artifact in effect delivered again at the revision that set the mark acknowledges its current state.
        ((5, &route_change), (ApplyState::Active, Some(2), None, routed), (5, 5)),
        // Approved again under a newer revision: acknowledged as active under that revision, which moves the mark and
        // is the revision in effect from now on.
        ((6, &route_change), (ApplyState::Active, Some(2), None, routed), (6, 6)),
        // A revision equal to the mark is stale for another artifact than the one that set it.
        (
            (6, &family_drop),
            (ApplyState::Rejected, None, stale(6, 6), dropped),
            (6, 6),
        ),
        // The artifact in effect below the mark is stale too: only a redelivery at the mark is idempotent.
        (
            (5, &route_change),
            (ApplyState::Rejected, None, stale(5, 6), routed),
            (6, 6),
        ),
    ];
    for ((revision, artifact), (state, generation, error, hash), (high_water, in_effect)) in cases {
        harness.deliver(vec![config(revision, artifact)]);
        let expected = (state, Some(revision), generation, error.clone(), Some(hash));
        let acknowledged = harness
            .status_where(|status| {
                ack(status, CONFIG_ID).map(|ack| {
                    (
                        ack.state,
                        ack.revision,
                        ack.generation,
                        ack.error.clone(),
                        ack.artifact_hash,
                    )
                }) == Some(expected.clone())
            })
            .await;
        // Nothing else changes: the artifact's generation keeps running, nothing new is prepared, and nothing is
        // reported degraded.
        assert_eq!(
            (
                active(&acknowledged),
                live(&acknowledged),
                acknowledged.installer.revision_high_water,
                active_revision(&acknowledged)
            ),
            (Some(2), vec![1, 2], Some(high_water), Some(in_effect)),
            "revision {}",
            revision
        );
        assert!(acknowledged.installer.preparing.is_none() && record(&acknowledged, 3).is_none());
        assert_eq!(
            (
                acknowledged.last_error.as_ref(),
                acknowledged.apply_states.status.degraded.as_ref()
            ),
            (None, None)
        );
        // What the transport reports is the status.
        assert_eq!(
            *harness.transport.as_ref().unwrap().apply_states.latest(),
            acknowledged.apply_states
        );
        assert_eq!(harness.send().await, Some(2));
    }
    let finished = harness.finish().await;
    finished.check(&BTreeSet::new(), |_| routing(&FAMILIES, &[APP]));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_artifact_on_a_stale_base_or_review_runs_and_is_reported_stale() {
    let mut harness = Lifecycle::start().await;
    let route_change = route(&harness.export, "app.");
    let family_drop = drop_family(&harness.export, "drop_debug", "debug.");

    // An artifact whose base program hash is not the current export's, as one derived from an earlier export: it
    // installs, and runs reported stale.
    let mut stale_base = route_change.clone();
    stale_base["base"]["program_hash"] = json!(PlanHash::of_bytes(b"an earlier default program"));
    harness.deliver(vec![config(1, &stale_base)]);
    let installed = harness.status_where(|status| active(status) == Some(2)).await;
    let summary = &installed.apply_states.status;
    assert_eq!((summary.stale_base, summary.stale_review), (true, false));
    assert_eq!(
        ack_of(&installed, CONFIG_ID),
        Some((ApplyState::Active, Some(1), Some(2)))
    );
    assert!(installed.to_string().contains(", stale base"), "{}", installed);
    assert_eq!(harness.send().await, Some(2));

    // An artifact on the current base, approved against the plan that runs: neither is stale.
    let running = installed.installer.active.as_ref().unwrap().plan_hash;
    harness.deliver(vec![config_with(CONFIG_ID, 2, &family_drop, Some(running))]);
    let fresh = harness.status_where(|status| active(status) == Some(3)).await;
    let summary = &fresh.apply_states.status;
    assert_eq!((summary.stale_base, summary.stale_review), (false, false));
    assert_eq!(harness.send().await, Some(3));

    // An artifact approved against another plan than the one that ran when it arrived: it installs, with its review
    // reported stale.
    let elsewhere = PlanHash::of_bytes(b"a plan the approver saw elsewhere");
    harness.deliver(vec![config_with(CONFIG_ID, 3, &route_change, Some(elsewhere))]);
    let reviewed = harness.status_where(|status| active(status) == Some(4)).await;
    let summary = &reviewed.apply_states.status;
    assert_eq!((summary.stale_base, summary.stale_review), (false, true));
    assert_eq!(summary.degraded, None);
    assert!(reviewed.to_string().contains(", stale review"), "{}", reviewed);
    assert_eq!(harness.send().await, Some(4));
    let finished = harness.finish().await;
    finished.check(&BTreeSet::new(), |generation| match generation {
        2 | 4 => routing(&FAMILIES, &[APP]),
        3 => routing(&[KEPT, ORDERS, APP], &[ORDERS]),
        other => panic!("generation {} processed a batch", other),
    });
}

/// One delivery the core refuses.
struct Refused {
    /// What the delivery is.
    what: &'static str,

    /// The configs of the snapshot delivering it at `revision`, edited from the export `export`.
    configs: fn(export: &Value, revision: u64) -> Vec<ArtifactConfig>,

    /// The error the config `demo` is acknowledged with, given the export and the revision.
    error: fn(export: &Value, revision: u64) -> String,

    /// Whether its envelope is read, so that its acknowledgment names its revision.
    read: bool,

    /// Whether it is authentic, so that its revision moves the high-water mark although it is refused.
    authentic: bool,
}

/// Returns `artifact` with the base's fingerprint of `output` set to `fingerprint`, or removed.
fn with_fingerprint(artifact: &Value, output: &str, fingerprint: Option<PlanHash>) -> Value {
    let mut artifact = artifact.clone();
    let outputs = artifact["base"]["outputs"]
        .as_object_mut()
        .expect("the base records outputs");
    match fingerprint {
        Some(fingerprint) => outputs.insert(output.to_string(), json!(fingerprint)),
        None => outputs.remove(output),
    };
    artifact
}

/// Returns the deliveries the core refuses, and how.
fn refused() -> Vec<Refused> {
    vec![
        Refused {
            what: "an output whose fingerprint is not the one the host declares",
            configs: |export, revision| {
                let other = PlanHash::of_bytes(b"the output as bound elsewhere");
                vec![config(revision, &with_fingerprint(export, MAIN, Some(other)))]
            },
            error: |_, _| {
                "The artifact emits to output `dd_metrics_encode`, which now delivers elsewhere than the artifact's \
                 base records. Rebase the artifact on the current export."
                    .to_string()
            },
            read: true,
            authentic: true,
        },
        Refused {
            what: "an output the base does not record",
            configs: |export, revision| vec![config(revision, &with_fingerprint(export, ROUTED, None))],
            error: |_, _| {
                "Artifact node `metrics_routing_filter_0_true` emits to output `metrics_routing_encode_0`, which the \
                 artifact's base does not record. Add the output's fingerprint from the export to `base.outputs`."
                    .to_string()
            },
            read: true,
            authentic: true,
        },
        Refused {
            // A state owner composes like any node; its parameters are validation's to judge.
            what: "a state owner without its parameters",
            configs: |export, revision| {
                let mut artifact = export.clone();
                in_front_of_the_main_output(&mut artifact, json!({"id": "agg", "op": "aggregate"}), "out");
                vec![config(revision, &artifact)]
            },
            error: |_, _| {
                let missing = [
                    "window",
                    "flush_interval",
                    "context_limit",
                    "counter_expiry",
                    "flush_open_windows",
                    "histogram_aggregates",
                    "histogram_percentiles",
                    "copy_to_distribution",
                    "copy_to_distribution_prefix",
                ]
                .map(|param| {
                    format!(
                        "error[valid_parameters] node `policy/agg`: `aggregate` parameter `{}`: missing required \
                         parameter",
                        param
                    )
                });
                format!("the plan is not valid: {}", missing.join("; "))
            },
            read: true,
            authentic: true,
        },
        Refused {
            // A state owner with every parameter, where the frame cannot run it: the policy scope runs ahead of no
            // isolation slot, so placement refuses it with the capability it lacks.
            what: "a state owner in the policy scope",
            configs: |export, revision| {
                let mut artifact = export.clone();
                let node = json!({"id": "agg", "op": "aggregate", "params": {
                    "window": "10s",
                    "flush_interval": "10s",
                    "context_limit": 1000,
                    "counter_expiry": "5m",
                    "flush_open_windows": false,
                    "histogram_aggregates": ["max", "count"],
                    "histogram_percentiles": [95],
                    "copy_to_distribution": false,
                    "copy_to_distribution_prefix": "",
                }});
                in_front_of_the_main_output(&mut artifact, node, "out");
                vec![config(revision, &artifact)]
            },
            error: |_, _| {
                "The candidate plan cannot run in the physical structure built at boot: node `policy/agg`: it is a \
                 state owner, which runs only in an isolation slot that the region its input comes from has an exit \
                 to, and the topology built at boot has no such isolation slot for `metrics_enrich`: a new exit from \
                 a region to an isolation slot requires stage 10 frame changes: the physical structure is built from \
                 the default program at boot, so a restart does not add it."
                    .to_string()
            },
            read: true,
            authentic: true,
        },
        Refused {
            what: "a payload damaged in transit",
            configs: |export, revision| {
                let mut config = config(revision, export);
                config.payload.push(' ');
                vec![config]
            },
            error: |export, revision| {
                let damaged = config(revision, export);
                format!(
                    "transport hash mismatch: the config declares sha256 {}, but its payload hashes to {}",
                    damaged.sha256,
                    ArtifactConfig::transport_hash(&format!("{} ", damaged.payload))
                )
            },
            read: false,
            authentic: false,
        },
        Refused {
            what: "an artifact hash that is not the artifact's",
            configs: |export, revision| {
                let payload = json!({
                    "revision": revision, "artifact_hash": PlanHash::of_bytes(b"another document"), "artifact": export
                })
                .to_string();
                vec![ArtifactConfig {
                    id: ArtifactConfigId::new(CONFIG_ID),
                    version: revision,
                    sha256: ArtifactConfig::transport_hash(&payload),
                    payload,
                }]
            },
            error: |export, _| {
                format!(
                    "artifact hash mismatch: the envelope declares {}, but the artifact as received hashes to {}; \
                     approval binds to the hash of the exact document",
                    PlanHash::of_bytes(b"another document"),
                    artifact_hash(export)
                )
            },
            read: true,
            authentic: false,
        },
        Refused {
            what: "a snapshot of two configs for one scope",
            configs: |export, revision| {
                vec![
                    config(revision, export),
                    config_with("other", revision, &route(export, "app."), None),
                ]
            },
            error: |_, _| {
                "the policy snapshot holds 2 configs (`demo`, `other`) for scope `post_enrich`, but one artifact \
                 replaces a scope: every config for it is rejected until the snapshot holds one"
                    .to_string()
            },
            read: false,
            authentic: false,
        },
    ]
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn deliveries_the_core_refuses_are_acknowledged_with_the_reason_and_leave_what_runs() {
    let mut harness = Lifecycle::start().await;
    let export = harness.export.clone();
    let mut revision = 0;
    let mut high_water = None;

    // Each refusal, first while the default program runs, as at boot, then while an artifact runs.
    for phase in ["the default program", "an artifact"] {
        let (running, in_effect) = match phase {
            "the default program" => (1, None),
            _ => {
                revision += 1;
                harness.deliver(vec![config(revision, &route(&export, "app."))]);
                harness.status_where(|status| active(status) == Some(2)).await;
                high_water = Some(revision);
                (2, Some(revision))
            }
        };
        for case in refused() {
            revision += 1;
            harness.deliver((case.configs)(&export, revision));
            let refused = harness
                .status_where(|status| {
                    ack(status, CONFIG_ID)
                        .is_some_and(|ack| ack.version == revision && ack.state == ApplyState::Rejected)
                })
                .await;
            let context = format!("{} while {} runs", case.what, phase);
            let acknowledged = ack(&refused, CONFIG_ID).unwrap();
            assert_eq!(
                (
                    acknowledged.revision,
                    acknowledged.generation,
                    acknowledged.error.clone()
                ),
                (
                    case.read.then_some(revision),
                    None,
                    Some((case.error)(&export, revision))
                ),
                "{}",
                context
            );
            if case.authentic {
                high_water = Some(revision);
            }
            // What runs is unchanged: no generation is prepared, nothing is reported degraded, and the next batch is
            // processed as before.
            assert_eq!(
                (
                    active(&refused),
                    active_revision(&refused),
                    refused.installer.revision_high_water
                ),
                (Some(running), in_effect, high_water),
                "{}",
                context
            );
            assert!(
                refused.installer.preparing.is_none() && record(&refused, running + 1).is_none(),
                "{}: {}",
                context,
                refused
            );
            assert_eq!(
                (
                    refused.last_error.as_ref(),
                    refused.apply_states.status.degraded.as_ref()
                ),
                (None, None),
                "{}",
                context
            );
            assert_eq!(harness.send().await, Some(running), "{}", context);
        }
    }
    // Both configs of the snapshot of two were rejected, the last time while the artifact ran.
    let last = harness.status.load_full();
    assert_eq!(ack(&last, "other").map(|ack| ack.state), Some(ApplyState::Rejected));
    let finished = harness.finish().await;
    finished.check(&BTreeSet::new(), |generation| match generation {
        1 => default_routing(),
        _ => routing(&FAMILIES, &[APP]),
    });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_delivery_whose_recorded_output_is_not_declared_waits_for_it_and_leaves_what_runs() {
    let mut harness = Lifecycle::start().await;
    let mut artifact = with_fingerprint(&harness.export, "gone_encode", Some(PlanHash::of_bytes(b"gone")));
    let emit = nodes(&mut artifact)
        .iter_mut()
        .find(|node| node["id"] == "metrics_routing_filter_0_true")
        .expect("the routing group's emit");
    emit["params"]["output"] = json!("gone_encode");
    harness.deliver(vec![config(1, &artifact)]);
    // A new artifact whose base records an output the host does not declare waits for it, as one published before the
    // resources artifact declaring its output does: its revision is seen, and what runs keeps running.
    let waiting = harness
        .status_where(|status| {
            ack(status, CONFIG_ID).is_some_and(|ack| ack.version == 1 && ack.state == ApplyState::Pending)
        })
        .await;
    let acknowledged = ack(&waiting, CONFIG_ID).unwrap();
    assert_eq!(
        (
            acknowledged.revision,
            acknowledged.generation,
            acknowledged.error.as_deref()
        ),
        (
            Some(1),
            None,
            Some(
                "waiting for output `gone_encode`: the artifact's base records it, and the host does not declare \
                 it; the artifact is installed once the output is declared"
            )
        )
    );
    assert_eq!(
        (
            active(&waiting),
            active_revision(&waiting),
            waiting.installer.revision_high_water
        ),
        (Some(1), None, Some(1))
    );
    assert!(waiting.installer.preparing.is_none() && record(&waiting, 2).is_none());
    assert_eq!(harness.send().await, Some(1));
    let finished = harness.finish().await;
    finished.check(&BTreeSet::new(), |_| default_routing());
}

/// How an in-process stub server answers a poll: a status and a body.
type StubAnswer = (u16, String);

/// What an in-process stub server shares with its handler.
#[derive(Clone)]
struct StubState {
    answer: Arc<StdMutex<StubAnswer>>,
    polls: Arc<StdMutex<Vec<Value>>>,
}

async fn answer_poll(State(state): State<StubState>, body: axum::body::Bytes) -> axum::response::Response {
    state
        .polls
        .lock()
        .unwrap()
        .push(serde_json::from_slice(&body).unwrap_or(Value::Null));
    let (status, body) = state.answer.lock().unwrap().clone();
    (AxumStatus::from_u16(status).unwrap(), body).into_response()
}

/// An in-process stub server on a loopback port, which answers every poll as told and records each poll's body.
struct Stub {
    state: StubState,
    url: String,
    server: tokio::task::JoinHandle<()>,
}

impl Stub {
    /// Starts a stub that answers `503 Service Unavailable` until told otherwise.
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let state = StubState {
            answer: Arc::new(StdMutex::new((503, "not yet".to_string()))),
            polls: Arc::new(StdMutex::new(Vec::new())),
        };
        let router = Router::new()
            .route(POLL_PATH, post(answer_poll))
            .with_state(state.clone());
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        Self { state, url, server }
    }

    /// Answers every poll from now on with `status` and `body`.
    fn answer(&self, status: u16, body: impl Into<String>) {
        *self.state.answer.lock().unwrap() = (status, body.into());
    }

    /// Answers every poll from now on with a policy snapshot of `configs`.
    fn serve(&self, configs: &[ArtifactConfig]) {
        let configs = configs
            .iter()
            .map(|config| serde_json::to_value(config).unwrap())
            .collect::<Vec<_>>();
        self.answer(
            200,
            json!({"protocol_version": 1, "products": {"policy": configs}}).to_string(),
        );
    }

    /// Waits until the stub has received `count` more polls, and returns the body of the last.
    async fn polls(&self, count: usize) -> Value {
        let target = self.state.polls.lock().unwrap().len() + count;
        tokio::time::timeout(PATIENCE, async {
            loop {
                if let Some(poll) = self.state.polls.lock().unwrap().get(target - 1) {
                    return poll.clone();
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .expect("the stub worker keeps polling")
    }
}

impl Drop for Stub {
    fn drop(&mut self) {
        self.server.abort();
    }
}

/// Returns the acknowledgment of config `id` a poll's body carries: its state, revision, and generation.
fn polled_ack(poll: &Value, id: &str) -> Option<(String, Option<u64>, Option<u64>)> {
    poll["config_states"]
        .as_array()?
        .iter()
        .find(|ack| ack["id"] == id)
        .map(|ack| {
            (
                ack["state"].as_str().unwrap_or_default().to_string(),
                ack["revision"].as_u64(),
                ack["generation"].as_u64(),
            )
        })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_stub_worker_installs_what_the_stub_serves_and_keeps_it_through_transport_failures() {
    let stub = Stub::start().await;
    let channel = StubChannelConfig::new(&stub.url, Duration::from_millis(20)).expect("the stub is on loopback");
    let mut start = Start::new(Options::new(MetricsPath::Plan, Aggregation::Native));
    start.transport = Transport::Stub(Box::new(channel));
    let mut harness = Lifecycle::start_with(start).await;
    let snapshot = [config(1, &route(&harness.export, "app."))];

    // Until the stub answers, every poll is a transport failure, and the default program runs.
    let unanswered = stub.polls(2).await;
    assert_eq!(unanswered["status"]["active_generation"], 1);
    assert_eq!(harness.send().await, Some(1));

    // The stub serves the route change: the worker publishes it, and the next polls carry its acknowledgment.
    stub.serve(&snapshot);
    harness.status_where(|status| active(status) == Some(2)).await;
    let mut acknowledged = stub.polls(1).await;
    while polled_ack(&acknowledged, CONFIG_ID) != Some(("active".to_string(), Some(1), Some(2))) {
        acknowledged = stub.polls(1).await;
    }
    assert_eq!(acknowledged["status"]["active_artifact"]["revision"], 1);
    assert_eq!(harness.send().await, Some(2));

    // Transport failures keep the feed: the artifact keeps running, and polling continues.
    let failures: [StubAnswer; 4] = [
        (500, "the stub failed".to_string()),
        (200, "not JSON".to_string()),
        (200, json!({"protocol_version": 1}).to_string()),
        (
            200,
            json!({"protocol_version": 2, "products": {"policy": []}}).to_string(),
        ),
    ];
    for (status, body) in failures {
        stub.answer(status, body.clone());
        let poll = stub.polls(3).await;
        let current = harness.status.load_full();
        assert_eq!(
            (active(&current), live(&current), ack_of(&current, CONFIG_ID)),
            (Some(2), vec![1, 2], Some((ApplyState::Active, Some(1), Some(2)))),
            "{} {}",
            status,
            body
        );
        assert_eq!(
            polled_ack(&poll, CONFIG_ID),
            Some(("active".to_string(), Some(1), Some(2)))
        );
        assert_eq!(harness.send().await, Some(2));
    }

    // The same snapshot again changes nothing; an empty one revokes.
    stub.serve(&snapshot);
    stub.polls(3).await;
    assert_eq!(live(&harness.status.load_full()), [1, 2]);
    stub.serve(&[]);
    let revoked = harness.status_where(|status| active(status) == Some(3)).await;
    assert!(matches!(active_origin(&revoked), Some(Origin::DefaultProgram { .. })));
    assert_eq!(harness.send().await, Some(3));
    let finished = harness.finish().await;
    finished.check(&BTreeSet::new(), |generation| match generation {
        2 => routing(&FAMILIES, &[APP]),
        _ => default_routing(),
    });
}

/// The name a metric must have for the first artifact's `test_panic` node to panic.
const PANIC_A: &str = "panic.a";

/// The name a metric must have for the second artifact's `test_panic` node to panic.
const PANIC_B: &str = "panic.b";

/// The name a metric must have for the native `test_panic` node of [`NativeFault`] to panic.
const PANIC_NATIVE: &str = "panic.native";

/// The core's own artifact paths, except that every default program lowered after boot is edited by `edit`, which
/// gives it a native kernel with a bug, which no native kernel has on demand: by default
/// ([`native_fault_in_front_of_the_main_output`]), a native `test_panic` node in front of the main output, which
/// panics on metrics named [`PANIC_NATIVE`].
struct NativeFault {
    catalog: AdpCatalog,
    edit: fn(&mut Value),
}

impl NativeFault {
    /// Returns the paths that give every default program lowered after boot a native `test_panic` node in front of the
    /// main output.
    fn in_front_of_the_main_output(catalog: AdpCatalog) -> Self {
        Self {
            catalog,
            edit: native_fault_in_front_of_the_main_output,
        }
    }
}

/// Puts a native `test_panic` node, which panics on metrics named [`PANIC_NATIVE`], in front of the main output of the
/// plan `plan`, where host enrichment's region runs it.
fn native_fault_in_front_of_the_main_output(plan: &mut Value) {
    let node = json!({
        "id": "native_fault", "op": "test_panic", "params": {"on": PANIC_NATIVE},
        "provenance": {"native": "metrics_enrich"}
    });
    in_front_of_the_main_output(plan, node, "out");
}

impl ArtifactPaths for NativeFault {
    fn on_snapshot(
        &mut self, core: &mut InstallerCore, snapshot: &ArtifactSnapshot, composer: &dyn Composer,
    ) -> Vec<Action> {
        core.on_snapshot(snapshot, composer)
    }

    fn on_default_program(
        &mut self, core: &mut InstallerCore, candidate: Candidate, composer: &dyn Composer,
    ) -> (CandidateId, Vec<Action>) {
        let mut plan = serde_json::to_value(&*candidate.plan).unwrap();
        (self.edit)(&mut plan);
        let plan = Plan::from_value(plan).expect("the default program with a native fault is a plan");
        let report = Validator::new(&operators(), &self.catalog).validate(&plan);
        let candidate = Candidate {
            plan: Arc::new(plan),
            report,
            origin: candidate.origin,
        };
        core.on_default_program(candidate, composer)
    }

    fn apply_states(
        &self, core: &InstallerCore, bases: &BTreeMap<ScopeName, PlanHash>, degraded: Option<String>,
    ) -> ApplyStates {
        core.apply_states_with_bases(bases, degraded)
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_artifact_that_panics_falls_back_to_last_known_good_and_then_to_the_default_program_which_fails_fast() {
    let mut options = Options::new(MetricsPath::Plan, Aggregation::Native);
    options.operators = operators();
    let mut start = Start::new(options);
    start.paths = Some(Box::new(NativeFault::in_front_of_the_main_output(
        routing_catalog().await,
    )));
    let mut harness = Lifecycle::start_with(start).await;
    let first = faulty(&harness.export, "faulty", PANIC_A);
    let second = faulty(&harness.export, "faulty", PANIC_B);
    harness.deliver(vec![config(1, &first)]);
    harness.status_where(|status| active(status) == Some(2)).await;
    harness.deliver(vec![config(2, &second)]);
    let installed = harness
        .status_where(|status| active(status) == Some(3) && live(status) == [2, 3])
        .await;
    assert_eq!(
        installed.installer.last_known_good.as_ref().map(|kept| kept.id.get()),
        Some(2)
    );
    assert_eq!(harness.send().await, Some(3));
    let mut lost = BTreeSet::new();

    // Within its panic budget, the second artifact's generation survives each panic: the region loses the batch that
    // panicked, as a native component's restart would, rebuilds its kernels, and processes the next batch.
    for _ in 0..3 {
        lost.insert(harness.feed(&[KEPT, PANIC_B]));
        assert_eq!(harness.send().await, Some(3));
    }

    // The fourth panic within a minute exceeds the budget: the generation fails, and the installer rolls back to the
    // generation kept as last-known-good, which runs the first artifact. The process keeps running.
    lost.insert(harness.feed(&[KEPT, PANIC_B]));
    let rolled_back = harness.status_where(|status| active(status) == Some(2)).await;
    let reason = "it exceeded its panic budget, with 4 panics, the last in node `policy/faulty`";
    assert_eq!(
        record(&rolled_back, 3),
        Some((GenerationState::Failed, Some(reason.to_string())))
    );
    assert_eq!(
        ack_of(&rolled_back, CONFIG_ID),
        Some((ApplyState::Failed, Some(2), Some(3)))
    );
    assert_eq!(ack_error(&rolled_back, CONFIG_ID).as_deref(), Some(reason));
    assert_eq!(
        (active_revision(&rolled_back), rolled_back.installer.revision_high_water),
        (Some(1), Some(2))
    );
    // The first artifact passes what the second panicked on.
    assert_eq!(harness.send_names(&[KEPT, PANIC_B]).await, Some(2));

    // The first artifact fails the same way. Nothing is kept to roll back to, so the default program is lowered from
    // the configuration and installed.
    for _ in 0..3 {
        lost.insert(harness.feed(&[KEPT, PANIC_A]));
        assert_eq!(harness.send().await, Some(2));
    }
    lost.insert(harness.feed(&[KEPT, PANIC_A]));
    let fallen_back = harness.status_where(|status| active(status) == Some(4)).await;
    assert!(matches!(
        active_origin(&fallen_back),
        Some(Origin::DefaultProgram { .. })
    ));
    assert_eq!(
        record(&fallen_back, 2),
        Some((GenerationState::Failed, Some(reason.to_string())))
    );
    assert_eq!(
        (
            active_revision(&fallen_back),
            fallen_back.installer.last_known_good.as_ref()
        ),
        (None, None)
    );
    assert_eq!(harness.send_names(&[KEPT, PANIC_A, PANIC_B]).await, Some(4));
    assert_eq!(harness.send().await, Some(4));

    // The default program's panic budget is zero: its first panic fails the region, which fails the topology, as a
    // native component's failure does, instead of failing the generation and rolling back. (The test's supervisor then
    // tries to start the topology again, which a topology cannot do; ADP's root supervisor restarts nothing, and the
    // process exits.)
    let status = Arc::clone(&harness.status);
    let fatal = harness.feed(&[KEPT, PANIC_NATIVE]);
    let (error, mut finished) = harness.failure().await;
    assert!(
        matches!(&error, SupervisorError::FailedToInitialize { child_name, .. } if child_name == "primary"),
        "{:?}",
        error
    );
    let failed = status.load_full();
    assert_eq!(
        (active(&failed), record(&failed, 4).map(|(state, _)| state)),
        (Some(4), Some(GenerationState::Active)),
        "{}",
        failed
    );
    let reached = finished
        .arrivals
        .keys()
        .filter(|(output, _, seq)| *seq == fatal && output != TAP)
        .collect::<Vec<_>>();
    assert!(reached.is_empty(), "the fatal batch reached {:?}", reached);
    finished.sent.truncate(fatal);

    // Every batch before the fatal one arrived exactly once, but those lost to the panics, which only the tap, ahead of
    // the policy region, received.
    let everything = [KEPT, ORDERS, APP, DEBUG, PANIC_A, PANIC_B];
    finished.check(&lost, |_| {
        BTreeMap::from([
            (MAIN, everything.into_iter().collect()),
            (ROUTED, BTreeSet::from([ORDERS])),
            (TAP, everything.into_iter().collect()),
        ])
    });
    let mut order = finished.generations_but(&lost);
    order.dedup();
    assert_eq!(order, [3, 2, 4]);
}

/// The core's own artifact paths, except that every artifact is composed with a native `test_panic` node in front of
/// the main output, which panics on metrics named [`PANIC_NATIVE`]: a native kernel with a bug in the region that runs
/// the artifact's nodes, as host enrichment runs beside the policy region.
struct NativeFaultBesideArtifacts {
    catalog: AdpCatalog,
}

/// Composes as the host's composer does, and adds the native `test_panic` node of [`NativeFaultBesideArtifacts`].
struct WithNativeFault<'a> {
    composer: &'a dyn Composer,
    catalog: &'a AdpCatalog,
}

impl Composer for WithNativeFault<'_> {
    fn compose_all(&self, artifacts: &[(&ReceivedArtifact, &Approval)]) -> Result<Composed, ComposeError> {
        let composed = self.composer.compose_all(artifacts)?;
        let mut plan = serde_json::to_value(&composed.plan).unwrap();
        let node = json!({
            "id": "native_fault", "op": "test_panic", "params": {"on": PANIC_NATIVE},
            "provenance": {"native": "metrics_enrich"}
        });
        in_front_of_the_main_output(&mut plan, node, "out");
        let plan = Plan::from_value(plan).expect("the composed plan with a native fault is a plan");
        let report = Validator::new(&operators(), self.catalog).validate(&plan);
        Ok(Composed {
            plan,
            report,
            ..composed
        })
    }
}

impl ArtifactPaths for NativeFaultBesideArtifacts {
    fn on_snapshot(
        &mut self, core: &mut InstallerCore, snapshot: &ArtifactSnapshot, composer: &dyn Composer,
    ) -> Vec<Action> {
        let composer = WithNativeFault {
            composer,
            catalog: &self.catalog,
        };
        core.on_snapshot(snapshot, &composer)
    }

    fn on_default_program(
        &mut self, core: &mut InstallerCore, candidate: Candidate, composer: &dyn Composer,
    ) -> (CandidateId, Vec<Action>) {
        let composer = WithNativeFault {
            composer,
            catalog: &self.catalog,
        };
        core.on_default_program(candidate, &composer)
    }

    fn apply_states(
        &self, core: &InstallerCore, bases: &BTreeMap<ScopeName, PlanHash>, degraded: Option<String>,
    ) -> ApplyStates {
        core.apply_states_with_bases(bases, degraded)
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_native_node_that_panics_beside_an_artifact_fails_the_topology_and_is_not_charged_to_the_artifact() {
    let mut options = Options::new(MetricsPath::Plan, Aggregation::Native);
    options.operators = operators();
    let mut start = Start::new(options);
    start.paths = Some(Box::new(NativeFaultBesideArtifacts {
        catalog: routing_catalog().await,
    }));
    let mut harness = Lifecycle::start_with(start).await;
    // The artifact's own `test_panic` node and the native one both run in the region of the policy decision.
    let artifact = faulty(&drop_family(&harness.export, "drop_debug", "debug."), "faulty", PANIC_A);
    harness.deliver(vec![config(1, &artifact)]);
    let installed = harness.status_where(|status| active(status) == Some(2)).await;
    let plan = &installed.active.as_ref().expect("a generation is active").plan;
    let frame = Frame::compute_with_hosts(
        &harness.default_program.0,
        &operators(),
        &slot_name,
        &crate::programmable_metrics::test_support::frame_hosts(),
    )
    .expect("the boot plan lays out");
    let placement = place_unvalidated(plan, &operators(), &frame).expect("the plan places");
    let slot_of = |node: &str| {
        placement
            .slots()
            .iter()
            .position(|slot| slot.nodes.iter().any(|id| id.to_string() == node))
            .unwrap_or_else(|| panic!("`{}` is not placed", node))
    };
    assert_eq!(slot_of("native_fault"), slot_of("policy/faulty"));
    assert_eq!(harness.send().await, Some(2));

    // A panic in the artifact's node is charged to the artifact's budget: the region survives it.
    let lost = BTreeSet::from([harness.feed(&[KEPT, PANIC_A])]);
    assert_eq!(harness.send().await, Some(2));

    // A panic in the native node fails the region at once, as the native component's failure does, although the
    // artifact's budget has room: the artifact is not failed or rolled back, and the topology stops.
    let status = Arc::clone(&harness.status);
    let fatal = harness.feed(&[KEPT, PANIC_NATIVE]);
    let (error, mut finished) = harness.failure().await;
    assert!(
        matches!(&error, SupervisorError::FailedToInitialize { child_name, .. } if child_name == "primary"),
        "{:?}",
        error
    );
    let failed = status.load_full();
    assert_eq!(
        (active(&failed), record(&failed, 2).map(|(state, _)| state)),
        (Some(2), Some(GenerationState::Active)),
        "{}",
        failed
    );
    assert_eq!(ack_of(&failed, CONFIG_ID), Some((ApplyState::Active, Some(1), Some(2))));
    let reached = finished
        .arrivals
        .keys()
        .filter(|(output, _, seq)| *seq == fatal && output != TAP)
        .collect::<Vec<_>>();
    assert!(reached.is_empty(), "the fatal batch reached {:?}", reached);
    finished.sent.truncate(fatal);

    // Every batch before the fatal one arrived exactly once, as the artifact delivers it, but the one lost to the
    // artifact's panic, which only the tap, ahead of the policy region, received.
    let everything = [KEPT, ORDERS, APP, DEBUG, PANIC_A];
    finished.check(&lost, |generation| match generation {
        2 => BTreeMap::from([
            (MAIN, BTreeSet::from([KEPT, ORDERS, APP])),
            (ROUTED, BTreeSet::from([ORDERS])),
            (TAP, everything.into_iter().collect()),
        ]),
        other => panic!("generation {} processed a batch", other),
    });
}

/// Delivers install `install` of `artifacts`, in turn, each in a config of its own, and waits until it is the active
/// generation and the generation before it the only other one alive; then sends a batch, which it processes.
async fn install_in_turn(harness: &mut Lifecycle, artifacts: &[Value; 2], install: u64) -> PlanStatus {
    let id = format!("demo-{}", install);
    let artifact = &artifacts[(install % 2) as usize];
    harness.deliver(vec![config_with(&id, install, artifact, None)]);
    let generation = install + 1;
    let installed = harness
        .status_where(|status| active(status) == Some(generation) && live(status) == [generation - 1, generation])
        .await;
    assert_eq!(
        ack_of(&installed, &id),
        Some((ApplyState::Active, Some(install), Some(generation)))
    );
    assert_eq!(harness.send().await, Some(generation));
    installed
}

#[tokio::test]
async fn a_hundred_installs_leave_health_accounting_metric_series_and_acknowledgments_bounded() {
    use super::fault_tests::{bounds_tree, health_components, ready, resource_groups, series, wait_until_healthy};

    let recorder = TestRecorder::default();
    let _recorder = metrics::set_default_local_recorder(&recorder);
    let mut options = Options::new(MetricsPath::Plan, Aggregation::Native);
    options.operators = operators();
    let (health, registry) = (options.health.clone(), options.registry.clone());
    let mut harness = Lifecycle::start_with(Start::new(options)).await;
    // Two artifacts installed in turn: each drops a family and runs a kernel of its own, which never panics.
    let artifacts = [
        faulty(&drop_family(&harness.export, "drop_app", "app."), "kernel", "never.a"),
        faulty(
            &drop_family(&harness.export, "drop_debug", "debug."),
            "kernel",
            "never.b",
        ),
    ];
    let registrations = |health: &HealthRegistry| {
        health_components(health)
            .into_iter()
            .map(|(component, _, ready)| (component, ready))
            .collect::<BTreeSet<_>>()
    };

    // Once each has been installed, everything an install registers exists.
    for install in 1..=2 {
        install_in_turn(&mut harness, &artifacts, install).await;
    }
    wait_until_healthy(&health).await;
    let before = (
        registrations(&health),
        resource_groups(),
        bounds_tree(&registry.as_bounds()),
        series(&recorder),
    );

    // A hundred installs more, each in a config of its own: the acknowledgments kept are those of the latest config,
    // of the one kept to roll back to, and of the sixteen configs most recently retired; the generation records, those
    // of the two generations alive and of the sixteen most recently final.
    const INSTALLS: u64 = 100;
    for install in 3..=INSTALLS + 2 {
        let installed = install_in_turn(&mut harness, &artifacts, install).await;
        let acknowledgments = installed.apply_states.config_states.len();
        let records = installed.installer.generations.len();
        assert!(
            acknowledgments <= 18,
            "install {}: {} acknowledgments",
            install,
            acknowledgments
        );
        assert!(records <= 18, "install {}: {} generation records", install, records);
    }
    wait_until_healthy(&health).await;
    let after = (
        registrations(&health),
        resource_groups(),
        bounds_tree(&registry.as_bounds()),
        series(&recorder),
    );
    assert_eq!(after.0, before.0, "health registrations");
    assert_eq!(after.1, before.1, "resource groups");
    assert_eq!(after.2, before.2, "memory bounds");
    assert_eq!(
        (
            after.3.difference(&before.3).collect::<Vec<_>>(),
            before.3.difference(&after.3).collect::<Vec<_>>()
        ),
        (Vec::<&String>::new(), Vec::<&String>::new()),
        "metric series added and removed"
    );
    assert_eq!(ready(&health).await, StatusCode::OK);

    let last = harness.status.load_full();
    let states = last
        .apply_states
        .config_states
        .iter()
        .map(|ack| (ack.id.to_string(), ack.state))
        .collect::<BTreeMap<_, _>>();
    let expected = (INSTALLS + 2 - 17..=INSTALLS + 2)
        .map(|install| {
            let state = match INSTALLS + 2 - install {
                0 => ApplyState::Active,
                1 => ApplyState::Prepared,
                _ => ApplyState::Retired,
            };
            (format!("demo-{}", install), state)
        })
        .collect::<BTreeMap<_, _>>();
    assert_eq!(states, expected);
    assert_eq!(last.installer.generations.len(), 18);
    assert_eq!(last.installer.revision_high_water, Some(INSTALLS + 2));
    assert_eq!(
        recorder.gauge(metrics::Key::from_name(
            crate::programmable_metrics::telemetry::PLAN_LIVE_GENERATIONS
        )),
        Some(2.0)
    );
    let finished = harness.finish().await;
    finished.check(&BTreeSet::new(), |generation| match generation % 2 {
        // Generation `install + 1` runs install `install`: odd installs drop `debug.noise`, even ones `app.requests`.
        0 => routing(&[KEPT, ORDERS, APP], &[ORDERS]),
        _ => routing(&[KEPT, ORDERS, DEBUG], &[ORDERS]),
    });
}

// A live cut: an artifact that changes aggregation's window.

/// Waits until the DogStatsD source has sent `sent` batches: each was stamped by its admission point.
async fn wait_sent(topology: &TestTopology, sent: usize) {
    tokio::time::timeout(PATIENCE, async {
        while topology.sent("dsd_in.metrics") < sent {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("the source never sent {} batches", sent));
}

/// Returns the aggregated counts of the counter `name` that `output` received, as `(generation, interval in seconds,
/// bucket start, value)`, sorted.
fn aggregated_counts(deliveries: &[Delivery], output: &str, name: &str) -> Vec<(u64, u64, u64, f64)> {
    let mut counts = Vec::new();
    for delivery in deliveries.iter().filter(|delivery| delivery.output == output) {
        for metric in delivery.metrics.iter().filter(|metric| metric.context().name() == name) {
            let saluki_core::data_model::event::metric::MetricValues::Rate(points, interval) = metric.values() else {
                panic!("`{}` leaves aggregation as a rate: {:?}", name, metric.values());
            };
            for (timestamp, value) in points {
                let generation = delivery.generation.expect("aggregated output carries a generation");
                let bucket = timestamp.expect("aggregated points are timestamped").get();
                counts.push((generation, interval.as_secs(), bucket, value));
            }
        }
    }
    counts.sort_by(|a, b| a.partial_cmp(b).unwrap());
    counts
}

/// Starts the topology with clocked aggregation, as `options` says otherwise: the DogStatsD source's admission point
/// reads a manual wall clock, which the test moves, the slot flushes only when its input ends, and the topology runs
/// the whole cut protocol in between. Returns the topology and its clock.
async fn start_clocked(options: Options) -> (Lifecycle, ManualWallClock) {
    start_clocked_on(options, ROUTING).await
}

/// Starts the topology with clocked aggregation, as [`start_clocked`] does, with the local configuration `config` in
/// place of [`ROUTING`].
async fn start_clocked_on(mut options: Options, config: &'static str) -> (Lifecycle, ManualWallClock) {
    assert_eq!(options.aggregation, Aggregation::Clocked);
    let clock = Arc::new(StdMutex::new(None));
    options.before_topology = Some(Box::new({
        let clock = Arc::clone(&clock);
        move |boot: &mut BootTopology| *clock.lock().unwrap() = boot.state_admission_clock.clone()
    }));
    let mut start = Start::new(options);
    start.config = config;
    let lifecycle = Lifecycle::start_with(start).await;
    let clock = clock
        .lock()
        .unwrap()
        .clone()
        .expect("clocked aggregation's admission point runs on a manual clock");
    (lifecycle, clock)
}

/// Returns the export of the pre-aggregation scope with aggregation over `window`, and the timestamp split before it
/// with the same window, as validation requires; it flushes only at the end of input, as clocked aggregation does.
fn pre_aggregation_over(status: &PlanStatus, window: &str) -> Value {
    use crate::programmable_metrics::catalog::DSD_PRE_AGGREGATION_SCOPE;

    let scope = ScopeName::new(DSD_PRE_AGGREGATION_SCOPE).unwrap();
    let bundle = status.exports[&scope]
        .clone()
        .expect("the pre-aggregation scope exports");
    let mut artifact = serde_json::to_value(&bundle.artifact).unwrap();
    for node in nodes(&mut artifact) {
        match node["id"].as_str() {
            Some("dsd_agg") => {
                node["params"]["window"] = json!(window);
                node["params"]["flush_interval"] = json!("1h");
            }
            Some("dsd_no_agg_split") => node["params"]["window"] = json!(window),
            _ => {}
        }
    }
    artifact
}

/// Returns the cut, once the installer has armed it.
async fn armed_cut(lifecycle: &Lifecycle) -> saluki_metric_plan::install::CutStatus {
    let armed = lifecycle
        .status_where(|status| {
            status
                .installer
                .cut
                .as_ref()
                .is_some_and(|cut| matches!(cut.phase, CutPhase::Armed { .. }))
        })
        .await;
    armed.installer.cut.unwrap()
}

/// Feeds `count` batches of one count of the counter `agg.c` each, and waits until the source has sent them: once it
/// has, the admission point stamped each at the time its clock read.
async fn feed_counts(lifecycle: &mut Lifecycle, count: usize, sent: &mut usize) {
    feed_counts_on(lifecycle, |_| "dsd_in.metrics".to_string(), count, sent).await;
}

/// Feeds `count` batches of one count of the counter `agg.c` each, batch `i` on the feed `feed` gives for it, and
/// waits until the source has sent them.
async fn feed_counts_on(lifecycle: &mut Lifecycle, feed: fn(usize) -> String, count: usize, sent: &mut usize) {
    for batch in 0..count {
        lifecycle.topology.feed(
            &feed(*sent + batch),
            vec![Metric::counter(Context::from_static_name("agg.c"), 1.0)],
        );
    }
    *sent += count;
    wait_sent(&lifecycle.topology, *sent).await;
}

/// Three producer lanes, each its own admission point, all feeding aggregation on one state lane, and two
/// post-aggregation lanes.
const THREE_PRODUCER_LANES: &str = r#"
dogstatsd_workers_count: 3
experimental:
  programmable_metrics:
    lanes:
      mode: auto
      state: 1
      post: 2
"#;

#[tokio::test]
async fn on_producer_lanes_an_artifact_that_changes_aggregations_window_installs_at_a_cut_and_every_count_arrives_once()
{
    use crate::programmable_metrics::test_topology::ADMITTED_AT;

    // The cut is armed over the three producer lanes' points, and aggregation crosses once every point's `new` stamp
    // reached it.
    let options = Options::new(MetricsPath::Plan, Aggregation::Clocked);
    let (mut lifecycle, clock) = start_clocked_on(options, THREE_PRODUCER_LANES).await;
    let at = |relative: u64| AdmissionTime::from_unix_secs(ADMITTED_AT + relative);
    let artifact = pre_aggregation_over(&lifecycle.status.load(), "30s");
    let lane = |batch: usize| format!("dsd_in.metrics_{}", batch % 3);
    let mut sent = 0;

    // Three counts at 5 seconds past `B`, one per lane, in the old lineage's bucket [0, 10); the cut takes effect at
    // 30, the first multiple of lcm(10, 30) a second away from the points' latest stamp.
    feed_counts_on(&mut lifecycle, lane, 3, &mut sent).await;
    lifecycle.deliver(vec![config(1, &artifact)]);
    let cut = armed_cut(&lifecycle).await;
    assert_eq!(cut.generation, GenerationId::new(2));
    assert_eq!(cut.at, Some(at(25)));

    // Two counts at 15, before the cut, on two lanes; then four at 40, after it, on every lane, whose stamps make the
    // cut irrevocable and pass every point.
    clock.set(at(10));
    feed_counts_on(&mut lifecycle, lane, 2, &mut sent).await;
    clock.set(at(35));
    feed_counts_on(&mut lifecycle, lane, 4, &mut sent).await;
    lifecycle
        .status_where(|status| active(status) == Some(2) && status.installer.cut.is_none())
        .await;

    // Every count arrives once, in the buckets of the lineage that admitted it.
    let deliveries = lifecycle.topology.stop().await;
    let bucket = |relative: i64| (ADMITTED_AT as i64 - 5 + relative) as u64;
    assert_eq!(
        aggregated_counts(&deliveries, MAIN, "agg.c"),
        vec![
            (1, 10, bucket(0), 3.0),
            (1, 10, bucket(10), 2.0),
            (2, 30, bucket(30), 4.0),
        ]
    );
}

#[tokio::test]
async fn an_artifact_that_changes_aggregations_window_installs_at_a_cut_and_every_count_arrives_once() {
    use crate::programmable_metrics::{catalog::DSD_PRE_AGGREGATION_SCOPE, test_topology::ADMITTED_AT};

    let (mut lifecycle, clock) = start_clocked(Options::new(MetricsPath::Plan, Aggregation::Clocked)).await;
    let at = |relative: u64| AdmissionTime::from_unix_secs(ADMITTED_AT + relative);
    let artifact = pre_aggregation_over(&lifecycle.status.load(), "30s");
    let mut sent = 0;

    // Three counts at 5 seconds past `B`, in the old lineage's bucket [0, 10).
    feed_counts(&mut lifecycle, 3, &mut sent).await;

    // Arming: the latest stamp is at 5, so the cut takes effect at 30, the first multiple of lcm(10, 30) a second away.
    lifecycle.deliver(vec![config(1, &artifact)]);
    let cut = armed_cut(&lifecycle).await;
    assert_eq!(cut.generation, GenerationId::new(2));
    assert_eq!(
        cut.at,
        Some(at(25)),
        "the cut is at the next 30-second boundary at least a second away"
    );

    // Two counts at 15, before the cut: the old generation's, in [10, 20).
    clock.set(at(10));
    feed_counts(&mut lifecycle, 2, &mut sent).await;

    // Four counts at 40, after the cut: the new generation's, in its bucket [30, 60). The first of their stamps makes
    // the cut irrevocable, and passes the only affected point: the cut completes, and generation 2 runs alone.
    clock.set(at(35));
    feed_counts(&mut lifecycle, 4, &mut sent).await;
    let installed = lifecycle
        .status_where(|status| active(status) == Some(2) && status.installer.cut.is_none())
        .await;

    // The status names each scope's base program hash, that of its export, and the artifact active in the
    // pre-aggregation scope; the policy scope runs the default program's region.
    let scopes = &installed.apply_states.status.scopes;
    for (scope, bundle) in &installed.exports {
        let bundle = bundle.as_ref().expect("every scope exports");
        assert_eq!(
            scopes.get(scope).and_then(|status| status.default_program_hash),
            Some(bundle.artifact.base.program_hash),
            "{}",
            scope
        );
    }
    let active_in = |scope: &str| {
        let scope = ScopeName::new(scope).unwrap();
        scopes[&scope]
            .active_artifact
            .as_ref()
            .map(|artifact| artifact.revision)
    };
    assert_eq!(active_in(DSD_PRE_AGGREGATION_SCOPE), Some(1));
    assert_eq!(active_in(POST_ENRICH_SCOPE), None);
    assert_eq!(
        installed.apply_states.status.default_program_hash,
        scopes[&ScopeName::new(POST_ENRICH_SCOPE).unwrap()].default_program_hash
    );

    // Every count arrives once: the old lineage's buckets through [20, 30), under generation 1, at its final flush at
    // the crossing; the new lineage's from [30, 60), under generation 2, at the end of input.
    let deliveries = lifecycle.topology.stop().await;
    let bucket = |relative: i64| (ADMITTED_AT as i64 - 5 + relative) as u64;
    assert_eq!(
        aggregated_counts(&deliveries, MAIN, "agg.c"),
        vec![
            (1, 10, bucket(0), 3.0),
            (1, 10, bucket(10), 2.0),
            (2, 30, bucket(30), 4.0),
        ]
    );
}

#[tokio::test]
async fn revoking_an_artifact_while_its_cut_is_armed_cancels_the_cut_and_the_old_lineage_keeps_every_count() {
    use crate::programmable_metrics::{telemetry::PLAN_CUTS_TOTAL, test_topology::ADMITTED_AT};

    let recorder = TestRecorder::default();
    let _recorder = metrics::set_default_local_recorder(&recorder);
    let (mut lifecycle, clock) = start_clocked(Options::new(MetricsPath::Plan, Aggregation::Clocked)).await;
    let at = |relative: u64| AdmissionTime::from_unix_secs(ADMITTED_AT + relative);
    let artifact = pre_aggregation_over(&lifecycle.status.load(), "30s");
    let mut sent = 0;

    // Three counts at 5, and the cut armed at 30, as in the width change above.
    feed_counts(&mut lifecycle, 3, &mut sent).await;
    lifecycle.deliver(vec![config(1, &artifact)]);
    let cut = armed_cut(&lifecycle).await;
    assert_eq!((cut.generation, cut.at), (GenerationId::new(2), Some(at(25))));

    // Two counts at 15, then the revocation, before any point stamped generation 2: the cut is cancelled, the cut
    // generation rejected as revoked, and the default program asked for again.
    clock.set(at(10));
    feed_counts(&mut lifecycle, 2, &mut sent).await;
    lifecycle.deliver(Vec::new());
    let cancelled = lifecycle
        .status_where(|status| {
            status.installer.cut.is_none()
                && record(status, 2).is_some_and(|(state, _)| state == GenerationState::Rejected)
        })
        .await;
    assert_eq!(
        active(&cancelled),
        Some(1),
        "the old generation stays active through the cancellation"
    );
    assert_eq!(
        record(&cancelled, 2),
        Some((
            GenerationState::Rejected,
            Some("the channel revoked the policy before the artifact was installed".to_string())
        ))
    );
    assert_eq!(
        recorder.counter(metrics::Key::from_parts(PLAN_CUTS_TOTAL, &[("outcome", "cancelled")])),
        Some(1)
    );

    // Four counts at 40, past the cut that never was: the old lineage's, in its 10-second bucket [40, 50).
    clock.set(at(35));
    feed_counts(&mut lifecycle, 4, &mut sent).await;
    let settled = lifecycle
        .status_where(|status| {
            let installer = &status.installer;
            installer.cut.is_none() && installer.preparing.is_none() && installer.pending.is_none()
        })
        .await;
    // The default program asked for is the one generation 1 runs: nothing to install.
    assert_eq!((active(&settled), live(&settled)), (Some(1), vec![1]));

    // Every count arrives once, in 10-second buckets, at the end of input: none in a 30-second bucket, and none twice.
    let deliveries = lifecycle.topology.stop().await;
    let bucket = |relative: i64| (ADMITTED_AT as i64 - 5 + relative) as u64;
    assert_eq!(
        aggregated_counts(&deliveries, MAIN, "agg.c"),
        vec![
            (1, 10, bucket(0), 3.0),
            (1, 10, bucket(10), 2.0),
            (1, 10, bucket(40), 4.0)
        ]
    );
}

#[tokio::test]
async fn twenty_cut_installs_leave_health_registrations_resource_groups_bounds_and_metric_series_as_they_were() {
    use super::fault_tests::{bounds_tree, health_components, ready, resource_groups, series, wait_until_healthy};
    use crate::programmable_metrics::{telemetry::PLAN_CUTS_TOTAL, test_topology::ADMITTED_AT};

    let recorder = TestRecorder::default();
    let _recorder = metrics::set_default_local_recorder(&recorder);
    let options = Options::new(MetricsPath::Plan, Aggregation::Clocked);
    let (health, registry) = (options.health.clone(), options.registry.clone());
    let (mut lifecycle, clock) = start_clocked(options).await;
    // Installs alternate between aggregation over 30 seconds and over 10 (flushing only at the end of input, so unlike
    // the default program's): each changes the window, so each installs at a cut.
    let artifacts = [
        pre_aggregation_over(&lifecycle.status.load(), "30s"),
        pre_aggregation_over(&lifecycle.status.load(), "10s"),
    ];
    let registrations = |health: &HealthRegistry| {
        health_components(health)
            .into_iter()
            .map(|(component, _, ready)| (component, ready))
            .collect::<BTreeSet<_>>()
    };
    let mut sent = 0;

    // One count at 5 seconds past `B`; then each install `k` is armed at `T_k`, the first multiple of 30 seconds at
    // least a second after the latest stamp, and completes with one count 5 seconds past it, which the new lineage
    // holds in its bucket starting at `T_k`. `T_1` is `B + 25`, and each cut is 30 seconds after the one before.
    feed_counts(&mut lifecycle, 1, &mut sent).await;
    let cut_at = |install: u64| ADMITTED_AT + 25 + 30 * (install - 1);
    let cut_install = async |lifecycle: &mut Lifecycle, sent: &mut usize, install: u64| {
        let generation = install + 1;
        lifecycle.deliver(vec![config(install, &artifacts[(install as usize - 1) % 2])]);
        let cut = armed_cut(lifecycle).await;
        assert_eq!(cut.generation, GenerationId::new(generation), "install {}", install);
        assert_eq!(
            cut.at,
            Some(AdmissionTime::from_unix_secs(cut_at(install))),
            "install {}",
            install
        );
        clock.set(AdmissionTime::from_unix_secs(cut_at(install) + 5));
        feed_counts(lifecycle, 1, sent).await;
        lifecycle
            .status_where(|status| {
                active(status) == Some(generation) && status.installer.cut.is_none() && live(status) == [generation]
            })
            .await;
    };

    // Once each artifact has been installed, everything a cut install registers exists.
    for install in 1..=2 {
        cut_install(&mut lifecycle, &mut sent, install).await;
    }
    wait_until_healthy(&health).await;
    let before = (
        registrations(&health),
        resource_groups(),
        bounds_tree(&registry.as_bounds()),
        series(&recorder),
    );

    // Twenty cut installs more, each committed at its cut, its cut finalized, and its predecessor retired before the
    // next.
    const INSTALLS: u64 = 20;
    for install in 3..=INSTALLS + 2 {
        cut_install(&mut lifecycle, &mut sent, install).await;
    }
    wait_until_healthy(&health).await;
    let after = (
        registrations(&health),
        resource_groups(),
        bounds_tree(&registry.as_bounds()),
        series(&recorder),
    );
    assert_eq!(after.0, before.0, "health registrations");
    assert_eq!(after.1, before.1, "resource groups");
    assert_eq!(after.2, before.2, "memory bounds");
    assert_eq!(
        (
            after.3.difference(&before.3).collect::<Vec<_>>(),
            before.3.difference(&after.3).collect::<Vec<_>>()
        ),
        (Vec::<&String>::new(), Vec::<&String>::new()),
        "metric series added and removed"
    );
    assert_eq!(ready(&health).await, StatusCode::OK);
    assert_eq!(
        recorder.counter(metrics::Key::from_parts(PLAN_CUTS_TOTAL, &[("outcome", "completed")])),
        Some(INSTALLS + 2)
    );

    // Every count arrives once, under the generation whose lineage held it: the first in the default program's bucket,
    // each later one in its install's bucket starting at its cut, flushed at the next cut's crossing (or, for the last,
    // at the end of input).
    let deliveries = lifecycle.topology.stop().await;
    let mut expected = vec![(1, 10, ADMITTED_AT - 5, 1.0)];
    expected.extend((1..=INSTALLS + 2).map(|install| {
        let width = if install % 2 == 1 { 30 } else { 10 };
        (install + 1, width, cut_at(install), 1.0)
    }));
    assert_eq!(aggregated_counts(&deliveries, MAIN, "agg.c"), expected);
}

#[tokio::test]
async fn on_producer_lanes_a_cut_arms_at_once_while_every_producer_lane_waits_for_a_full_downstream_channel() {
    // The main output's encode host reads nothing until its gate opens: timestamped counters, which take the
    // passthrough to it, fill the post-aggregation lanes' inputs, and then every producer lane waits to send. Each
    // producer lane's point is locked only while the lane stamps, never while it sends, so the cut over the three
    // points arms at its first attempt, where a lock held across a blocked fan-out would put it off.
    let mut options = Options::new(MetricsPath::Plan, Aggregation::Clocked);
    options.gated.insert(MAIN.to_string());
    let (lifecycle, _clock) = start_clocked_on(options, THREE_PRODUCER_LANES).await;
    let artifact = pre_aggregation_over(&lifecycle.status.load(), "30s");
    let timestamped = |batch: u64| {
        Metric::counter(
            Context::from_parts(
                "lanes.blocked",
                TagSet::from_iter([Tag::from(format!("batch:{}", batch))]),
            ),
            (1_700_000_000 + batch, 1.0),
        )
    };
    const FED: usize = 3_000;
    for batch in 0..FED {
        lifecycle.topology.feed(
            &format!("dsd_in.metrics_{}", batch % 3),
            vec![timestamped(batch as u64)],
        );
    }
    // Once the sends stop completing, every producer lane is blocked.
    let mut sent = lifecycle.topology.sent("dsd_in.metrics");
    let blocked = tokio::time::timeout(PATIENCE, async {
        loop {
            tokio::time::sleep(Duration::from_millis(200)).await;
            let now = lifecycle.topology.sent("dsd_in.metrics");
            if now == sent && now < FED {
                return now;
            }
            sent = now;
        }
    })
    .await
    .expect("the producer lanes block on the full channels downstream");
    assert!(blocked < FED, "{} of {} batches were sent", blocked, FED);

    lifecycle.deliver(vec![config(1, &artifact)]);
    let cut = armed_cut(&lifecycle).await;
    assert_eq!(cut.generation, GenerationId::new(2));
    assert_eq!(cut.deferred, None, "arming was never put off");
    assert_eq!(
        lifecycle.topology.sent("dsd_in.metrics"),
        blocked,
        "the lanes stayed blocked while the cut armed"
    );

    lifecycle.topology.open_gate(MAIN);
    let deliveries = lifecycle.topology.stop().await;
    let delivered = deliveries
        .iter()
        .filter(|delivery| delivery.output == MAIN)
        .flat_map(|delivery| &delivery.metrics)
        .filter(|metric| metric.context().name() == "lanes.blocked")
        .count();
    assert_eq!(delivered, FED, "every counter arrives once the gate opens");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn on_post_aggregation_lanes_an_artifacts_panics_count_against_one_budget_and_its_generation_stops_in_every_lane()
{
    // Two producer lanes, each feeding its own post-aggregation lane, where the policy region runs the artifact: batch
    // `seq` goes to lane `seq mod 2` of both.
    const POST_LANES: &str = r#"
dd_url: https://primary.example.com
additional_endpoints: {https://b.example.com: [key-b]}
dogstatsd_workers_count: 2
experimental:
  metrics_endpoint_routing: {metric_allowlist: {https://b.example.com: [orders.total]}}
  programmable_metrics: {lanes: {mode: auto, state: 1, post: 2}}
"#;
    let mut options = Options::new(MetricsPath::Plan, Aggregation::Native);
    options.operators = operators();
    let mut start = Start::new(options);
    start.config = POST_LANES;
    start.feeds = vec!["dsd_in.metrics_0".to_string(), "dsd_in.metrics_1".to_string()];
    let mut harness = Lifecycle::start_with(start).await;
    let faulty = faulty(&harness.export, "faulty", PANIC_A);
    harness.deliver(vec![config(1, &faulty)]);
    harness.status_where(|status| active(status) == Some(2)).await;

    // Three panics, one in lane 0 and two in lane 1: within the artifact's budget of three a minute, which belongs to
    // the generation, not to a lane.
    harness.feed(&[KEPT, PANIC_A]);
    harness.feed(&[KEPT, PANIC_A]);
    assert_eq!(harness.send().await, Some(2));
    harness.feed(&[KEPT, PANIC_A]);
    assert_eq!(harness.send().await, Some(2));

    // The fourth, in lane 1, exceeds it, though neither lane panicked four times: the generation fails in every lane,
    // and the installer rolls back once, to the generation kept as last-known-good, the boot generation's default
    // program.
    harness.feed(&[KEPT, PANIC_A]);
    let rolled_back = harness.status_where(|status| active(status) == Some(1)).await;
    assert_eq!(
        record(&rolled_back, 2).map(|(state, _)| state),
        Some(GenerationState::Failed)
    );
    // Both lanes run the default program, which passes what the artifact panicked on, and nothing else installs.
    assert_eq!(harness.send_names(&[KEPT, PANIC_A]).await, Some(1));
    assert_eq!(harness.send_names(&[KEPT, PANIC_A]).await, Some(1));
    let settled = harness.status_where(|status| live(status) == [1]).await;
    assert_eq!(
        settled.installer.generations.iter().map(|record| record.id.get()).max(),
        Some(2),
        "{}",
        settled
    );
    drop(harness.finish().await);
}

// Faults on lanes: a producer lane's panic, and a state owner's, under the default program and under an artifact.

/// Three producer lanes, one lane of each other kind, with the endpoint routing group of [`ROUTING`].
const PRODUCER_LANES: &str = r#"
dd_url: https://primary.example.com
additional_endpoints: {https://b.example.com: [key-b]}
dogstatsd_workers_count: 3
experimental:
  metrics_endpoint_routing: {metric_allowlist: {https://b.example.com: [orders.total]}}
  programmable_metrics: {lanes: {mode: auto, state: 1, post: 1}}
"#;

/// Returns the catalog the configuration `config` lowers with.
async fn catalog_of(config: &str) -> AdpCatalog {
    let config = crate::programmable_metrics::test_support::configuration(config).await;
    Lowering::new(Arc::new(config.clone()), facts(&config))
        .expect("the configuration lowers")
        .expect("a metrics pipeline is required")
        .lower(&config)
        .expect("the configuration lowers")
        .catalog
}

/// Returns how a harness on the lanes of `config` starts, with the test operators, its batches fed to the three
/// producer lanes in turn.
fn on_producer_lanes(lanes: &'static str, aggregation: Aggregation) -> Start {
    let mut options = Options::new(MetricsPath::Plan, aggregation);
    options.operators = operators();
    let mut start = Start::new(options);
    start.config = lanes;
    start.feeds = (0..3).map(|lane| format!("dsd_in.metrics_{}", lane)).collect();
    start
}

/// Puts a native `test_panic` node, which panics on metrics named [`PANIC_NATIVE`], in front of the no-aggregation
/// split of the plan `plan`, where pre-aggregation's producer lanes run it.
fn native_fault_before_aggregation(plan: &mut Value) {
    let node = json!({
        "id": "native_fault", "op": "test_panic", "params": {"on": PANIC_NATIVE},
        "provenance": {"native": "dsd_no_agg_split"}
    });
    in_front_of(plan, "dsd_no_agg_split.in", node, "out");
}

/// Returns the export of the pre-aggregation scope, as the status serves it.
fn pre_aggregation_export(status: &PlanStatus) -> Value {
    use crate::programmable_metrics::catalog::DSD_PRE_AGGREGATION_SCOPE;

    let scope = ScopeName::new(DSD_PRE_AGGREGATION_SCOPE).unwrap();
    let bundle = status.exports[&scope]
        .clone()
        .expect("the pre-aggregation scope exports");
    serde_json::to_value(&bundle.artifact).unwrap()
}

/// Sends batches of every family until the next batch goes to producer lane `lane` of three.
async fn send_until_lane(harness: &mut Lifecycle, lane: usize) {
    while harness.sent.len() % 3 != lane {
        assert!(harness.send().await.is_some(), "the batch is processed");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn on_producer_lanes_a_native_nodes_panic_in_one_lane_ends_it_and_fails_its_component_and_the_topology() {
    let mut start = on_producer_lanes(PRODUCER_LANES, Aggregation::Native);
    start.paths = Some(Box::new(NativeFault {
        catalog: catalog_of(PRODUCER_LANES).await,
        edit: native_fault_before_aggregation,
    }));
    let mut harness = Lifecycle::start_with(start).await;
    // A configuration change lowers a default program with a native fault that pre-aggregation's producer lanes run.
    harness
        ._stream
        .send(crate::programmable_metrics::test_support::update(
            "metric_filterlist",
            json!(["blocked.metric"]),
        ))
        .await
        .expect("the configuration stream is open");
    harness.status_where(|status| active(status) == Some(2)).await;
    let first = harness.sent.len();
    for _ in 0..3 {
        assert_eq!(harness.send().await, Some(2));
    }

    // The default program's panic budget is zero: the panic in producer lane 1 ends that lane's task with the region's
    // failure, which fails the region and the topology, as a native component's failure does, while the other lanes
    // were running. The generation is neither failed nor rolled back.
    send_until_lane(&mut harness, 1).await;
    let status = Arc::clone(&harness.status);
    let fatal = harness.feed(&[KEPT, PANIC_NATIVE]);
    let (error, mut finished) = harness.failure().await;
    assert!(
        matches!(&error, SupervisorError::FailedToInitialize { child_name, .. } if child_name == "primary"),
        "{:?}",
        error
    );
    let failed = status.load_full();
    assert_eq!(
        (active(&failed), record(&failed, 2).map(|(state, _)| state)),
        (Some(2), Some(GenerationState::Active)),
        "{}",
        failed
    );
    // The fatal batch reached nothing: the producer lanes run ahead of every output, the tap's included.
    let reached = finished
        .arrivals
        .keys()
        .filter(|(_, _, seq)| *seq == fatal)
        .collect::<Vec<_>>();
    assert!(reached.is_empty(), "the fatal batch reached {:?}", reached);
    // Every batch before it arrived exactly once, as the default program delivers it, from every lane.
    finished.sent.truncate(fatal);
    finished.check(&BTreeSet::new(), |_| default_routing());
    let generations = finished.generations();
    assert!(generations[first..].iter().all(|generation| *generation == 2));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn on_producer_lanes_an_artifacts_node_that_panics_in_one_lane_costs_its_batch_and_every_lane_runs_on() {
    let start = on_producer_lanes(PRODUCER_LANES, Aggregation::Native);
    let mut harness = Lifecycle::start_with(start).await;
    // An artifact of the pre-aggregation scope with a `test_panic` node in front of the no-aggregation split, which
    // the producer lanes run, under the artifact's panic budget.
    let mut artifact = pre_aggregation_export(&harness.status.load());
    let node = json!({"id": "faulty", "op": "test_panic", "params": {"on": PANIC_A}});
    in_front_of(&mut artifact, "dsd_no_agg_split.in", node, "out");
    harness.deliver(vec![config(1, &artifact)]);
    harness.status_where(|status| active(status) == Some(2)).await;
    let first = harness.sent.len();

    // Two panics, in producer lanes 1 and 2, within the artifact's budget of three a minute: each lane loses the batch
    // that panicked, rebuilds its kernels, and runs on, as the other lanes do.
    send_until_lane(&mut harness, 1).await;
    let lost = BTreeSet::from([harness.feed(&[KEPT, PANIC_A]), harness.feed(&[KEPT, PANIC_A])]);
    for _ in 0..6 {
        assert_eq!(harness.send().await, Some(2));
    }
    let running = harness.status.load_full();
    assert_eq!(
        record(&running, 2).map(|(state, _)| state),
        Some(GenerationState::Active),
        "{}",
        running
    );

    let finished = harness.finish().await;
    for seq in &lost {
        let reached = finished
            .arrivals
            .keys()
            .filter(|(_, _, arrived)| arrived == seq)
            .collect::<Vec<_>>();
        assert!(reached.is_empty(), "batch {} panicked, but reached {:?}", seq, reached);
    }
    // Every other batch arrived exactly once, with the artifact's generation once it was installed.
    let generations = finished.generations_but(&lost);
    let after = generations.len() - (finished.sent.len() - first - lost.len());
    assert!(
        generations[after..].iter().all(|generation| *generation == 2),
        "{:?}",
        generations
    );
}

/// Feeds one batch of untimestamped counters, which aggregation aggregates, of the contexts `name` tagged `c:<i>` for
/// each `i` of `contexts`, or of `name` alone without contexts, to producer lane 0, and waits until it was sent.
async fn feed_aggregated(harness: &mut Lifecycle, name: &'static str, contexts: std::ops::Range<usize>) {
    let metrics = if contexts.is_empty() {
        vec![Metric::counter(Context::from_static_name(name), 1.0)]
    } else {
        contexts
            .map(|context| {
                Metric::counter(
                    Context::from_parts(name, TagSet::from_iter([Tag::from(format!("c:{}", context))])),
                    1.0,
                )
            })
            .collect()
    };
    let sent = harness.topology.sent("dsd_in.metrics_0") + 1;
    harness.topology.feed("dsd_in.metrics_0", metrics);
    tokio::time::timeout(PATIENCE, async {
        while harness.topology.sent("dsd_in.metrics_0") < sent {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("the source sends what it is fed");
}

/// Three producer lanes, one state lane, and one post-aggregation lane, with the endpoint routing group of
/// [`ROUTING`].
const ONE_STATE_LANE: &str = PRODUCER_LANES;

/// Three producer lanes, two state lanes, and one post-aggregation lane, with the endpoint routing group of
/// [`ROUTING`].
const TWO_STATE_LANES: &str = r#"
dd_url: https://primary.example.com
additional_endpoints: {https://b.example.com: [key-b]}
dogstatsd_workers_count: 3
experimental:
  metrics_endpoint_routing: {metric_allowlist: {https://b.example.com: [orders.total]}}
  programmable_metrics: {lanes: {mode: auto, state: 2, post: 1}}
"#;

/// Returns how a harness on the lanes of `lanes` starts whose aggregation panics on demand, from boot on
/// ([`panicking_state_operators`]).
fn with_panicking_state(lanes: &'static str) -> Start {
    let mut start = on_producer_lanes(lanes, Aggregation::Native);
    start.options.operators = panicking_state_operators();
    start
}

/// Checks that a state owner's panic under the default program fails its component and the topology, on the lanes
/// of `lanes`.
async fn a_state_owner_panic_under_the_default_program_fails_its_component(lanes: &'static str) {
    let mut harness = Lifecycle::start_with(with_panicking_state(lanes)).await;
    assert_eq!(harness.send().await, Some(1));

    // The default program's panic budget is zero: the panic in the state lane that owns the context fails the region
    // and the topology, and the generation is neither failed nor rolled back.
    let status = Arc::clone(&harness.status);
    feed_aggregated(&mut harness, PANIC_STATE, 0..0).await;
    let (error, _) = harness.failure().await;
    assert!(
        matches!(&error, SupervisorError::FailedToInitialize { child_name, .. } if child_name == "primary"),
        "{:?}",
        error
    );
    let failed = status.load_full();
    assert_eq!(
        (active(&failed), record(&failed, 1).map(|(state, _)| state)),
        (Some(1), Some(GenerationState::Active)),
        "{}",
        failed
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn on_one_state_lane_a_state_owner_panic_under_the_default_program_fails_its_component() {
    a_state_owner_panic_under_the_default_program_fails_its_component(ONE_STATE_LANE).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn on_two_state_lanes_a_state_owner_panic_under_the_default_program_fails_its_component() {
    a_state_owner_panic_under_the_default_program_fails_its_component(TWO_STATE_LANES).await;
}

/// Checks that a state owner's panic under an artifact, on the lanes of `lanes`, rebuilds only the instance of the
/// state lane that panicked, which loses its contexts, while the slot runs on and the artifact's generation asks to be
/// rolled back, and returns how many of `CONTEXTS` contexts held before the panic arrived.
async fn a_state_owner_panic_under_an_artifact_loses_its_lanes_contexts_only(lanes: &'static str) -> usize {
    const CONTEXTS: usize = 20;
    let mut harness = Lifecycle::start_with(with_panicking_state(lanes)).await;
    // An artifact of the pre-aggregation scope, whose aggregation, which panics on demand, runs over one-second
    // buckets that it flushes only when its lineage retires or the topology stops: it installs at a cut.
    let mut artifact = pre_aggregation_export(&harness.status.load());
    for node in nodes(&mut artifact) {
        match node["id"].as_str() {
            Some("dsd_agg") => {
                node["params"]["window"] = json!("1s");
                node["params"]["flush_interval"] = json!("1h");
            }
            Some("dsd_no_agg_split") => node["params"]["window"] = json!("1s"),
            _ => {}
        }
    }
    harness.deliver(vec![config(1, &artifact)]);
    harness
        .feed_until(|status| active(status) == Some(2) && status.installer.cut.is_none())
        .await;

    // Contexts before the panic and after it, all through one producer lane, so in this order at every state lane:
    // the lane that owns the panicking context loses what it held, and its instance is rebuilt empty, which every
    // generation binding it continues on. A panic under an artifact never fails the component: the artifact's
    // generation asks to be rolled back, which installs the default program again at a cut.
    //
    // The producer lane's keyed exit coalesces what one second's batches route to a state lane into one buffer, and
    // the batch that panics is that buffer: the contexts after the panic follow once the exit sent it, after its hold.
    feed_aggregated(&mut harness, "state.before", 0..CONTEXTS).await;
    feed_aggregated(&mut harness, PANIC_STATE, 0..0).await;
    tokio::time::sleep(limits::values::COALESCE_HOLD * 20).await;
    feed_aggregated(&mut harness, "state.after", 0..CONTEXTS).await;
    let rolled_back = harness
        .status_where(|status| record(status, 2).is_some_and(|(state, _)| state == GenerationState::Failed))
        .await;
    let reason = record(&rolled_back, 2)
        .and_then(|(_, reason)| reason)
        .unwrap_or_default();
    assert!(
        reason.starts_with("a state owner's kernel, of node `dsd_agg`, panicked"),
        "{}",
        reason
    );
    // The buckets close; the lineage's closing flush, at its crossing or at the topology's stop, delivers them.
    tokio::time::sleep(Duration::from_millis(2_500)).await;
    let deliveries = harness.topology.stop().await;
    let arrived = |name: &str| {
        deliveries
            .iter()
            .filter(|delivery| delivery.output == MAIN)
            .flat_map(|delivery| &delivery.metrics)
            .filter(|metric| metric.context().name() == name)
            .map(|metric| {
                metric
                    .context()
                    .tags()
                    .into_iter()
                    .next()
                    .map(|tag| tag.as_str().to_string())
            })
            .collect::<BTreeSet<_>>()
            .len()
    };
    assert_eq!(
        arrived("state.after"),
        CONTEXTS,
        "every context after the panic arrives"
    );
    assert_eq!(arrived(PANIC_STATE), 0, "the batch that panicked is lost");
    arrived("state.before")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn on_one_state_lane_a_state_owner_panic_under_an_artifact_loses_its_contexts_and_the_slot_runs_on() {
    let before = a_state_owner_panic_under_an_artifact_loses_its_lanes_contexts_only(ONE_STATE_LANE).await;
    assert_eq!(before, 0, "the one state lane lost every context it held");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn on_two_state_lanes_a_state_owner_panic_under_an_artifact_loses_only_its_lanes_contexts() {
    let before = a_state_owner_panic_under_an_artifact_loses_its_lanes_contexts_only(TWO_STATE_LANES).await;
    assert!(
        before > 0 && before < 20,
        "the lane that did not panic keeps its contexts, the one that did loses its own: {} of 20 arrived",
        before
    );
}

/// Returns the export of the pre-aggregation scope with aggregation over `window`, and the timestamp split before it
/// with the same window, flushing on its interval, for a run that keeps flushing.
fn pre_aggregation_flushing_over(status: &PlanStatus, window: &str) -> Value {
    use crate::programmable_metrics::catalog::DSD_PRE_AGGREGATION_SCOPE;

    let scope = ScopeName::new(DSD_PRE_AGGREGATION_SCOPE).unwrap();
    let bundle = status.exports[&scope]
        .clone()
        .expect("the pre-aggregation scope exports");
    let mut artifact = serde_json::to_value(&bundle.artifact).unwrap();
    for node in nodes(&mut artifact) {
        if matches!(node["id"].as_str(), Some("dsd_agg" | "dsd_no_agg_split")) {
            node["params"]["window"] = json!(window);
        }
    }
    artifact
}

/// One run of the lane stress test: its configuration, how long it feeds, and how often it changes what runs.
struct StressRun {
    /// The local configuration, whose lanes the run exercises.
    config: &'static str,
    /// How long the run feeds traffic.
    feed_for: Duration,
    /// How often a configuration change installs a new generation.
    install_every: Duration,
    /// Every how many installs one is a width change, which installs at a cut.
    cut_every: u64,
}

/// The name of the contexts a stress run keeps near the context limit.
const STRESS_CONTEXTS: &str = "stress.contexts";

/// The name of a stress run's canaries: one context per batch, which aggregation must deliver once.
const STRESS_CANARY: &str = "stress.canary";

/// Adds to `sums` the points of the counters named `name` in `deliveries` that the main output received: the sum of
/// every point, by the context's tag (`seq:<n>` for canaries, `c:<n>` for the contexts).
fn add_stress_sums(sums: &mut BTreeMap<String, f64>, deliveries: &[Delivery], name: &str) {
    for metric in deliveries
        .iter()
        .filter(|delivery| delivery.output == MAIN)
        .flat_map(|delivery| &delivery.metrics)
        .filter(|metric| metric.context().name() == name)
    {
        let saluki_core::data_model::event::metric::MetricValues::Rate(points, _) = metric.values() else {
            panic!("`{}` leaves aggregation as a rate: {:?}", name, metric.values());
        };
        let tag = metric
            .context()
            .tags()
            .into_iter()
            .next()
            .map(|tag| tag.as_str().to_string())
            .unwrap_or_default();
        *sums.entry(tag).or_default() += points.into_iter().map(|(_, value)| value).sum::<f64>();
    }
}

/// Returns the sum of the counters named `name` whose labels include every one of `labels`.
fn counter_sum(recorder: &TestRecorder, name: &str, labels: &[(&str, &str)]) -> u64 {
    recorder
        .counters()
        .into_iter()
        .filter(|(key, _)| key.name() == name)
        .filter(|(key, _)| {
            labels
                .iter()
                .all(|(label, value)| key.labels().any(|own| own.key() == *label && own.value() == *value))
        })
        .map(|(_, count)| count)
        .sum()
}

/// Returns the metrics aggregation refused at its context limit, as it counted them.
fn stress_refused(recorder: &TestRecorder) -> u64 {
    counter_sum(
        recorder,
        "component_events_dropped_total",
        &[("component_id", "dsd_agg"), ("intentional", "true")],
    )
}

/// Runs a stress run: batches of counters over contexts near the context limit, each with a canary of its own, on
/// four producer lanes in turn, while configuration changes install new generations and width changes cut over; then
/// waits until every bucket fed closed and flushed, stops, and checks the balance of every point fed.
///
/// Every canary is an untimestamped counter of one context of its own, so that it is aggregated, in whichever state
/// lane owns it: its points, summed over every flush, are exactly one, or none when the context limit refused it,
/// which aggregation counts as capacity loss. Over everything fed, what the main output received and what aggregation
/// refused add up to what was fed: nothing is lost or doubled by the lanes, the crossings, the cuts, or the installs.
async fn lane_stress(run: StressRun) {
    const CONTEXTS: usize = 12_000;
    const PER_BATCH: usize = 512;
    // The series aggregation counts its refusals in are registered on the topology's threads, so the recorder is the
    // process's: nextest runs each test in a process of its own.
    let recorder: &'static TestRecorder = Box::leak(Box::default());
    metrics::set_global_recorder(recorder).expect("no other recorder is installed: run the stress test with nextest");
    // Every operation on the state budgets is recorded, for the capacity oracle.
    super::record_state_budgets();
    let mut start = Start::new(Options::new(MetricsPath::Plan, Aggregation::Native));
    start.config = run.config;
    start.feeds = (0..4).map(|lane| format!("dsd_in.metrics_{}", lane)).collect();
    let mut harness = Lifecycle::start_with(start).await;
    let windows = ["1s", "2s"];
    let deadline = tokio::time::Instant::now() + run.feed_for;
    let (mut batches, mut installs, mut cuts) = (0usize, 0u64, 0u64);
    let mut next_install = tokio::time::Instant::now() + run.install_every;
    while tokio::time::Instant::now() < deadline {
        // A batch of contexts near the limit, on the next producer lane, with the canary of its sequence number.
        let lane = batches % 4;
        let mut metrics = (0..PER_BATCH)
            .map(|index| {
                let context = (batches * PER_BATCH + index) % CONTEXTS;
                Metric::counter(
                    Context::from_parts(
                        STRESS_CONTEXTS,
                        TagSet::from_iter([Tag::from(format!("c:{}", context))]),
                    ),
                    1.0,
                )
            })
            .collect::<Vec<_>>();
        metrics.push(Metric::counter(
            Context::from_parts(
                STRESS_CANARY,
                TagSet::from_iter([Tag::from(format!("seq:{}", batches))]),
            ),
            1.0,
        ));
        harness.topology.feed(&format!("dsd_in.metrics_{}", lane), metrics);
        batches += 1;

        if tokio::time::Instant::now() >= next_install {
            next_install += run.install_every;
            installs += 1;
            if installs % run.cut_every == 0 {
                // A width change installs at a cut.
                cuts += 1;
                let artifact = pre_aggregation_flushing_over(&harness.status.load(), windows[(cuts % 2) as usize]);
                harness.deliver(vec![config(cuts, &artifact)]);
            } else {
                harness
                    ._stream
                    .send(crate::programmable_metrics::test_support::update(
                        "metric_filterlist",
                        json!([format!("stress.blocked.{}", installs)]),
                    ))
                    .await
                    .expect("the configuration stream is open");
            }
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(cuts >= 1, "the run made a width cut");

    // Every point fed reaches a bucket that closes within one width and is flushed at the next tick: wait until what
    // arrived and what was refused account for everything fed, or for as long as a test waits, then stop. Stopping
    // discards open buckets, as native aggregation's last flush does, and they hold nothing by then.
    let fed = batches * (PER_BATCH + 1);
    let seen = std::cell::Cell::new(0usize);
    let arrived = std::cell::RefCell::new(BTreeMap::<String, f64>::new());
    let balanced = |deliveries: &[Delivery]| {
        let mut arrived = arrived.borrow_mut();
        add_stress_sums(&mut arrived, &deliveries[seen.get()..], STRESS_CANARY);
        add_stress_sums(&mut arrived, &deliveries[seen.get()..], STRESS_CONTEXTS);
        seen.set(deliveries.len());
        arrived.values().sum::<f64>() as u64 + stress_refused(recorder) >= fed as u64
    };
    let drained = tokio::time::Instant::now() + PATIENCE;
    while tokio::time::Instant::now() < drained {
        if harness
            .topology
            .received_within(Duration::from_millis(500), &balanced)
            .await
        {
            break;
        }
    }
    let deliveries = harness.topology.stop().await;
    let refused = stress_refused(recorder);

    // Every canary arrived exactly once, or not at all, refused at the context limit: none twice, none in part.
    let mut canaries = BTreeMap::new();
    add_stress_sums(&mut canaries, &deliveries, STRESS_CANARY);
    for (canary, sum) in &canaries {
        assert!(*sum == 0.0 || *sum == 1.0, "canary `{}` arrived as {}", canary, sum);
    }
    let arrived = canaries.values().filter(|sum| **sum == 1.0).count();
    assert!(arrived > 0, "canaries arrive");
    // What arrived and what was refused is what was fed, canaries and contexts alike.
    let mut contexts = BTreeMap::new();
    add_stress_sums(&mut contexts, &deliveries, STRESS_CONTEXTS);
    let contexts = contexts.values().sum::<f64>();
    assert_eq!(
        contexts as u64 + arrived as u64 + refused,
        fed as u64,
        "{} contexts' points and {} canaries arrived, {} were refused, of {} fed in {} batches",
        contexts,
        arrived,
        refused,
        fed,
        batches
    );
    assert!(
        batches - arrived <= refused as usize,
        "{} of {} canaries are missing, and only {} points were refused",
        batches - arrived,
        batches,
        refused
    );
    println!(
        "{} batches over {} installs and {} cuts: {} canaries arrived, {} points refused at the context limit",
        batches, installs, cuts, arrived, refused
    );
    // No state lane admitted a context another lane owns.
    assert_eq!(counter_sum(recorder, "plan_lane_misrouted_total", &[]), 0);
    // The budgets were never exceeded and every refusal was justified, by what each operation observed itself, on
    // every budget the lanes drew on: the global one, at the context limit, and each node's.
    let recordings = super::state_budget_recordings();
    assert_eq!(
        recordings.len(),
        1,
        "one metrics path booted, whose budgets were recorded"
    );
    let mut operations = 0;
    for (budget, trace) in recordings[0].traces() {
        operations += trace.operations.len();
        let verdict = check_capacity(&trace);
        assert_eq!(verdict, CapacityVerdict::Holds, "the budget `{}`", budget);
    }
    assert!(operations > 0, "the budgets recorded their operations");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn on_one_state_lane_a_short_stress_run_of_live_installs_and_width_cuts_delivers_or_refuses_every_point_once() {
    // Four producer lanes into one state lane: the balance a stress run checks, in a few seconds, at the lane counts
    // every part runs today. Aggregation runs 2-second buckets, flushed every second, with counters expiring two
    // seconds after they were last seen, so that the canaries keep the contexts near the limit, which refuses some.
    const STRESS: &str = r#"
dd_url: https://primary.example.com
dogstatsd_workers_count: 4
aggregate_window_duration_seconds: 2
aggregate_flush_interval: 1s
dogstatsd_expiry_seconds: 2
aggregate_context_limit: 12300
experimental: {programmable_metrics: {lanes: {mode: auto, state: 1, post: 2}}}
"#;
    lane_stress(StressRun {
        config: STRESS,
        feed_for: Duration::from_secs(4),
        install_every: Duration::from_millis(500),
        cut_every: 4,
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "about 30 seconds of traffic: `make test-stress` runs it"]
async fn lane_stress_live_installs_and_width_cuts_near_the_context_limit_deliver_every_canary_once() {
    // Four producer lanes, four state lanes, and two post-aggregation lanes on four workers, as the stress run of the
    // stage 9 design says: `SALUKI_STRESS_SECS` (30 by default) of traffic over 12,000 contexts and a canary context
    // per batch against a context limit of 12,300, a configuration change every second, and a width cut every ten
    // seconds.
    const STRESS: &str = r#"
dd_url: https://primary.example.com
dogstatsd_workers_count: 4
aggregate_window_duration_seconds: 2
aggregate_flush_interval: 1s
dogstatsd_expiry_seconds: 2
aggregate_context_limit: 12300
experimental: {programmable_metrics: {lanes: {mode: auto, state: 4, post: 2}}}
"#;
    let seconds = std::env::var("SALUKI_STRESS_SECS")
        .ok()
        .and_then(|secs| secs.parse::<u64>().ok())
        .unwrap_or(30)
        .max(10);
    lane_stress(StressRun {
        config: STRESS,
        feed_for: Duration::from_secs(seconds),
        install_every: Duration::from_secs(1),
        cut_every: 10,
    })
    .await;
}
