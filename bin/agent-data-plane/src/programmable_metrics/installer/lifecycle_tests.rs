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
    ir::{DecodedParams, ParamSpec, ParamType},
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
    let id = node["id"].as_str().expect("nodes have IDs").to_string();
    let main = edges(document)
        .iter_mut()
        .find(|edge| edge["to"] == "metrics_policy_0.in")
        .expect("the main output's emit has an input");
    main["to"] = json!(format!("{}.in", id));
    edges(document).push(json!({"from": format!("{}.{}", id, out), "to": "metrics_policy_0.in"}));
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
            transport: Transport::Publisher,
            paths: None,
        }
    }
}

/// A running plan-driven topology of the [`ROUTING`] configuration, connected to an artifact channel and to the
/// core's own artifact paths.
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
            transport,
            paths,
        } = start;
        let (system, updates, stream) = connected_configuration_system(ROUTING).await;
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
        }
    }

    /// Delivers a snapshot of `configs`.
    fn deliver(&self, configs: Vec<ArtifactConfig>) {
        let transport = self.transport.as_ref().expect("the test is the transport");
        assert!(transport
            .publisher
            .publish(ArtifactSnapshot::new(ArtifactProduct::Policy, configs)));
    }

    /// Sends the next batch of `names` without waiting for it, and returns its sequence number.
    fn feed(&mut self, names: &[&'static str]) -> usize {
        let seq = self.sent.len();
        self.topology.feed("dsd_in.metrics", batch(seq, names));
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
                "The artifact emits to output `dd_metrics_encode`, which the host now declares with another binding. \
                 Rebase the artifact on the current export."
                    .to_string()
            },
            read: true,
            authentic: true,
        },
        Refused {
            what: "an output the host does not declare",
            configs: |export, revision| {
                let mut artifact = with_fingerprint(export, "gone_encode", Some(PlanHash::of_bytes(b"gone")));
                let emit = nodes(&mut artifact)
                    .iter_mut()
                    .find(|node| node["id"] == "metrics_routing_filter_0_true")
                    .expect("the routing group's emit");
                emit["params"]["output"] = json!("gone_encode");
                vec![config(revision, &artifact)]
            },
            error: |_, _| {
                "The artifact emits to output `gone_encode`, which the host no longer declares. Rebase the artifact \
                 on the current export."
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
                "The candidate plan cannot run in the physical structure built at boot, and needs a restart: node \
                 `policy/agg`: it is a state owner, which runs only in an isolation slot that the region its input \
                 comes from has an exit to, and the topology built at boot has no such isolation slot for \
                 `metrics_enrich`: a new exit from a region to an isolation slot requires stage 7 resource \
                 generations, which change physical structure without a restart."
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

/// The core's own artifact paths, except that every default program lowered after boot has a native `test_panic` node
/// in front of the main output, which panics on metrics named [`PANIC_NATIVE`]: a native kernel with a bug, which no
/// native kernel has on demand.
struct NativeFault {
    catalog: AdpCatalog,
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
        let node = json!({
            "id": "native_fault", "op": "test_panic", "params": {"on": PANIC_NATIVE},
            "provenance": {"native": "metrics_enrich"}
        });
        in_front_of_the_main_output(&mut plan, node, "out");
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
    start.paths = Some(Box::new(NativeFault {
        catalog: routing_catalog().await,
    }));
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
    let frame = Frame::compute(&harness.default_program.0, &operators(), &slot_name).expect("the boot plan lays out");
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
async fn start_clocked(mut options: Options) -> (Lifecycle, ManualWallClock) {
    assert_eq!(options.aggregation, Aggregation::Clocked);
    let clock = Arc::new(StdMutex::new(None));
    options.before_topology = Some(Box::new({
        let clock = Arc::clone(&clock);
        move |boot: &mut BootTopology| *clock.lock().unwrap() = boot.state_admission_clock.clone()
    }));
    let lifecycle = Lifecycle::start_with(Start::new(options)).await;
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
    for _ in 0..count {
        lifecycle.topology.feed(
            "dsd_in.metrics",
            vec![Metric::counter(Context::from_static_name("agg.c"), 1.0)],
        );
    }
    *sent += count;
    wait_sent(&lifecycle.topology, *sent).await;
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
