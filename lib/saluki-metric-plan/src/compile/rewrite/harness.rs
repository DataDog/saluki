//! The shared harness of the rewrites' property tests.
//!
//! Every rewrite's property test draws a random valid plan and a random trace from here, rewrites the plan, and checks
//! the rewritten plan against the original under the reference interpreter:
//!
//! - [`arb_shape`] and [`arb_plan_params`] describe a plan below two sources (DogStatsD with client timestamps, and
//!   OTLP, which together admit every kind) and their merge: [links](Link) (filters, that is `split`s with one port
//!   to a `drop` and the filter-cardinality test operator `test_keep`; the registered stateless kernels; tag
//!   projection followed by a filter on tags; the test map `test_shed`, which loses large metrics to capacity),
//!   `tee`s, `tee`s whose every port starts with an equivalent filter, `split`s, `split`s whose ports join again in a
//!   `merge` through chains of links (among them, paths of one length of which a partial push-down lengthens one),
//!   complementary `split`s below a `tee` joined by a `merge`, timestamp splits, opaque hosts (fences, below which
//!   every node is native), `emit`s, and `drop`s. [`Case::new`] builds and validates the plan and computes its
//!   frame, as boot does.
//! - [`arb_trace`] draws metrics in batches for both sources, and `capacity` events that bound the plan's outputs.
//! - [`Case::optimize`] runs some rewrites alone, with the pipeline's own checks.
//! - [`Case::check_equivalent`] runs both plans over the trace without its capacity events and requires equal
//!   outputs; [`Case::check_capacity`] runs both over the trace with them and requires the specification's capacity
//!   rule; both require that no node loses more to capacity in the rewritten plan. [`Case::check_structure`] requires
//!   what every rewrite keeps (outputs, `emit`s, native nodes), that the rewritten plan compiles into the frame, and
//!   that the records describe the plan they produced.
//!
//! Routing and filter nodes carry the provenance of one artifact revision, in the policy namespace, except filters a
//! plan's parameters mark native; kernels, timestamp splits, sources, and hosts are native nodes of their own, so
//! that R1 may cut between any two of them.

use std::{collections::BTreeSet, sync::OnceLock};

use proptest::{collection::vec, prelude::*, sample::select, strategy::ValueTree as _, test_runner::TestRunner};
use saluki_core::{
    data_model::event::{metric::Metric, Event},
    execution::{AdmissionTime, Kernel, LossClass, PortIndex, Progress, Turn},
    topology::EventsBuffer,
};
use serde_json::{json, Value};

use super::{run_pipeline, Optimized, Rewrite, RewriteContext, RewriteId, TelemetryChange};
use crate::{
    canonical::PlanHash,
    compile::{compile, compile_unvalidated, Frame},
    equivalence::{compare, compare_under_capacity, CapacityOutcome, OutputPoints},
    extension::{SignalDecl, SignalType},
    interpret::{
        AdapterError, HostBatch, HostOutput, InstallOutcome, Interpreter, OpaqueHostAdapter, RunOutput, Trace,
        TraceEvent,
    },
    ir::{
        DecodedParams, ExitName, InputName, NativeComponentId, NodeId, OutputId, ParamSpec, ParamType, Plan,
        Provenance, RecipientId, ScopeDecl, ScopeName,
    },
    model::{EventView, Field, FieldSet, Kind, PortType},
    operators::{
        names, Cardinality, ConfiguredOperator, EffectContract, KernelSpec, Loss, LossDecl, OperatorDef,
        OperatorRegistry, PerKind, PortMapping, Ports,
    },
    predicate::{CompiledPredicate, NoBindings, Predicate, StringMatch, TagMatch},
    testing::{
        arb_metric, arb_predicate, predicate_json, HOST_VOCABULARY, NAME_VOCABULARY, PREFIX_VOCABULARY,
        TAG_KEY_VOCABULARY, TAG_VALUE_VOCABULARY,
    },
    validate::{CatalogOutput, MemoryCatalog, ValidationReport, Validator},
};

// Test operators.

/// The name of the filter-cardinality test operator.
pub(crate) const TEST_KEEP: &str = "test_keep";

/// The name of the test operator that reads a host signal.
pub(crate) const TEST_SIGNAL_MAP: &str = "test_signal_map";

const KEEP_PARAMS: &[ParamSpec] = &[ParamSpec::required("predicate", ParamType::Predicate)];

/// `test_keep`: an operator of `filter` cardinality, which keeps the metrics its predicate matches and counts the
/// rest as policy loss. No native operator has `filter` cardinality; this one exercises that form of filter.
struct TestKeep;

impl OperatorDef for TestKeep {
    fn name(&self) -> &'static str {
        TEST_KEEP
    }

    fn summary(&self) -> &'static str {
        "Keeps the metrics its predicate matches."
    }

    fn params(&self) -> &'static [ParamSpec] {
        KEEP_PARAMS
    }

    fn ports(&self, _params: &DecodedParams) -> Ports {
        Ports::single(PortType::ANY, PortMapping::IDENTITY)
    }

    fn contract(&self, params: &DecodedParams) -> EffectContract {
        let reads = params.predicate("predicate").map_or(FieldSet::ALL, Predicate::reads);
        EffectContract {
            reads: PerKind::uniform(reads),
            cardinality: Cardinality::Filter,
            loss: Loss::Lossy(vec![LossDecl {
                class: LossClass::Policy,
                description: "Discards the metrics its predicate does not match.",
            }]),
            ..EffectContract::passthrough()
        }
    }

    fn kernel(&self, params: &DecodedParams) -> Option<KernelSpec> {
        let predicate = params.predicate("predicate")?.compile(&NoBindings).ok()?;
        Some(KernelSpec::new(move |_| {
            Ok(Box::new(KeepKernel {
                predicate: predicate.clone(),
            }))
        }))
    }
}

struct KeepKernel {
    predicate: CompiledPredicate,
}

impl Kernel for KeepKernel {
    fn consume(&mut self, turn: &mut Turn<'_>, input: &mut EventsBuffer) -> Progress {
        let (mut events, mut points) = (0, 0);
        input.retain(|event| {
            let kept = match event {
                Event::Metric(metric) => self.predicate.evaluate(&mut EventView::new(metric)),
                _ => false,
            };
            if !kept {
                events += 1;
                points += event.try_as_metric().map_or(0, |metric| metric.values().len() as u64);
            }
            kept
        });
        turn.counters().record_loss(LossClass::Policy, events, points);
        Progress::Done
    }
}

/// `test_signal_map`: a passthrough map that declares that it reads the signal `queue_full`, which makes it dynamic.
/// It has no kernel: plans with it are only rewritten, never run.
struct TestSignalMap;

impl OperatorDef for TestSignalMap {
    fn name(&self) -> &'static str {
        TEST_SIGNAL_MAP
    }

    fn summary(&self) -> &'static str {
        "Reads a host signal."
    }

    fn params(&self) -> &'static [ParamSpec] {
        &[]
    }

    fn ports(&self, _params: &DecodedParams) -> Ports {
        Ports::single(PortType::ANY, PortMapping::IDENTITY)
    }

    fn contract(&self, _params: &DecodedParams) -> EffectContract {
        EffectContract {
            signals: vec![SignalDecl {
                name: "queue_full".parse().expect("valid signal name"),
                ty: SignalType::Bool,
            }],
            ..EffectContract::passthrough()
        }
    }
}

/// The name of the test map that loses records to capacity.
pub(crate) const TEST_SHED: &str = "test_shed";

const SHED_PARAMS: &[ParamSpec] = &[ParamSpec::required(
    "max_points",
    ParamType::Integer { min: 0, max: 1_000 },
)];

/// `test_shed`: a map that sheds every metric with more than `max_points` points, which does not fit its buffer, as
/// capacity loss, and passes every other metric unchanged.
///
/// No native stateless operator declares capacity loss; this one exercises R5a's reliance on the capacity rule. It
/// sheds by each metric alone, so what it sheds does not depend on order or on batches, and a filter that runs first
/// only spares it the records the filter removes anyway.
struct TestShed;

impl OperatorDef for TestShed {
    fn name(&self) -> &'static str {
        TEST_SHED
    }

    fn summary(&self) -> &'static str {
        "Sheds metrics with more points than its buffer holds."
    }

    fn params(&self) -> &'static [ParamSpec] {
        SHED_PARAMS
    }

    fn ports(&self, _params: &DecodedParams) -> Ports {
        Ports::single(PortType::ANY, PortMapping::IDENTITY)
    }

    fn contract(&self, _params: &DecodedParams) -> EffectContract {
        EffectContract {
            reads: PerKind::uniform(FieldSet::of(&[Field::Points])),
            loss: Loss::Lossy(vec![LossDecl {
                class: LossClass::Capacity,
                description: "Sheds metrics with more than `max_points` points.",
            }]),
            ..EffectContract::passthrough()
        }
    }

    fn kernel(&self, params: &DecodedParams) -> Option<KernelSpec> {
        let max_points = usize::try_from(params.integer("max_points")?).ok()?;
        Some(KernelSpec::new(move |_| Ok(Box::new(ShedKernel { max_points }))))
    }
}

struct ShedKernel {
    max_points: usize,
}

impl Kernel for ShedKernel {
    fn consume(&mut self, turn: &mut Turn<'_>, input: &mut EventsBuffer) -> Progress {
        let (mut events, mut points) = (0, 0);
        input.retain(|event| {
            let size = event.try_as_metric().map_or(0, |metric| metric.values().len());
            let kept = size <= self.max_points;
            if !kept {
                events += 1;
                points += size as u64;
            }
            kept
        });
        turn.counters().record_loss(LossClass::Capacity, events, points);
        Progress::Done
    }
}

/// Returns `plan` with every `test_shed` node's bound lifted: the plan as its policy delivers without capacity limits.
pub(crate) fn unbounded(plan: &Plan) -> Plan {
    let mut plan = plan.clone();
    for node in &mut plan.nodes {
        if node.op.as_str() == TEST_SHED {
            node.params
                .insert("max_points".parse().expect("valid parameter name"), json!(1_000));
        }
    }
    plan
}

/// Returns the native operators plus `test_keep`, `test_shed`, and `test_signal_map`.
pub(crate) fn registry() -> &'static OperatorRegistry {
    static REGISTRY: OnceLock<OperatorRegistry> = OnceLock::new();
    REGISTRY.get_or_init(|| {
        let tests: [&'static dyn OperatorDef; 3] = [&TestKeep, &TestShed, &TestSignalMap];
        let operators = OperatorRegistry::native().iter().chain(tests).collect::<Vec<_>>();
        OperatorRegistry::with_operators(Box::leak(operators.into_boxed_slice()))
    })
}

// Plans.

/// The artifact revision that routing and filter nodes of random plans come from.
fn revision() -> Value {
    json!({"revision": 1, "hash": PlanHash::of_bytes(b"rewrite harness").to_string()})
}

/// Returns the identifier of a node: in the policy namespace for an artifact node, reserved for a native one.
pub(crate) fn node_id(local: &str, native: bool) -> String {
    if native {
        local.to_string()
    } else {
        format!("policy/{}", local)
    }
}

/// A node with native provenance of its own, or the harness artifact's provenance.
pub(crate) fn node(local: &str, op: &str, params: Value, native: bool) -> Value {
    if native {
        json!({"id": local, "op": op, "params": params, "provenance": {"native": local}})
    } else {
        json!({"id": node_id(local, false), "op": op, "params": params, "provenance": {"artifact": revision()}})
    }
}

/// Builds a plan from nodes, edges written as `from.port -> to.port`, and the outputs it declares, in the harness's
/// conventions: node IDs as [`node`] writes them, and every output accepting everything with a recipient of its own.
pub(crate) fn plan_of(nodes: Vec<Value>, edges: &[(String, String)], outputs: &[String]) -> Plan {
    let edges = edges
        .iter()
        .map(|(from, to)| json!({"from": from, "to": to}))
        .collect::<Vec<_>>();
    let outputs = outputs
        .iter()
        .map(|output| {
            json!({"id": output, "accepts": {"kinds": "all", "timestamps": "mixed"},
                   "fingerprint": fingerprint(output).to_string(), "recipients": [format!("out/{}", output)]})
        })
        .collect::<Vec<_>>();
    Plan::from_value(json!({
        "ir_version": 1,
        "provenance": {"artifacts": [revision()]},
        "nodes": nodes,
        "edges": edges,
        "outputs": outputs,
    }))
    .expect("harness plans are well formed")
}

fn fingerprint(output: &str) -> PlanHash {
    PlanHash::of_bytes(output.as_bytes())
}

/// The catalog random plans validate against: outputs `o0` to `o127`, each with its own recipient, the binding
/// `test.names`, a list of strings, and the scope `post_enrich`, whose input `metrics` carries anything.
pub(crate) fn catalog() -> MemoryCatalog {
    (0..128).fold(
        MemoryCatalog::new()
            .with_binding(
                "test.names".parse().expect("valid binding name"),
                ParamType::List(&ParamType::String),
                crate::validate::Liveness::Live,
            )
            .with_scope_input(
                ScopeName::new("post_enrich").expect("valid scope name"),
                InputName::new("metrics").expect("valid input name"),
                PortType::ANY,
            ),
        |catalog, index| {
            let output = format!("o{}", index);
            catalog.with_output(
                OutputId::new(output.as_str()).expect("valid output ID"),
                CatalogOutput {
                    accepts: PortType::ANY,
                    fingerprint: fingerprint(&output),
                    recipients: vec![RecipientId::new(format!("out/{}", output)).expect("valid recipient")],
                    legacy_fingerprint: None,
                },
            )
        },
    )
}

/// Builds a plan in the harness's conventions from nodes, edges written as `from -> to`, and its outputs.
pub(crate) fn plan(nodes: Vec<Value>, edges: &[&str], outputs: &[&str]) -> Plan {
    let edges = edges
        .iter()
        .map(|edge| {
            let (from, to) = edge.split_once(" -> ").expect("edges are written `from -> to`");
            (from.to_string(), to.to_string())
        })
        .collect::<Vec<_>>();
    let outputs = outputs.iter().map(|output| output.to_string()).collect::<Vec<_>>();
    plan_of(nodes, &edges, &outputs)
}

/// Returns `plan` with the scope `post_enrich`, whose input `metrics` is the output port `input`, and whose exits are
/// `exits`, each a name and the input port it enters.
pub(crate) fn with_scope(mut plan: Plan, input: &str, exits: &[(&str, &str)]) -> Plan {
    let exits = exits
        .iter()
        .map(|(name, port)| {
            (
                ExitName::new(*name).expect("valid exit name"),
                port.parse().expect("valid port reference"),
            )
        })
        .collect();
    plan.scopes.insert(
        ScopeName::new("post_enrich").expect("valid scope name"),
        ScopeDecl {
            inputs: [(
                InputName::new("metrics").expect("valid input name"),
                input.parse().expect("valid port reference"),
            )]
            .into(),
            exits,
        },
    );
    plan
}

/// Returns a plan's edges as `from -> to`, sorted.
pub(crate) fn edges(plan: &Plan) -> Vec<String> {
    let mut edges = plan
        .edges
        .iter()
        .map(|edge| format!("{} -> {}", edge.from, edge.to))
        .collect::<Vec<_>>();
    edges.sort();
    edges
}

/// Returns edges written as `from -> to`, sorted, to compare with [`edges`].
pub(crate) fn sorted(edges: &[&str]) -> Vec<String> {
    let mut edges = edges.iter().map(|edge| edge.to_string()).collect::<Vec<_>>();
    edges.sort();
    edges
}

/// Returns a plan's node IDs, in plan order.
pub(crate) fn ids(plan: &Plan) -> Vec<String> {
    plan.nodes.iter().map(|node| node.id.to_string()).collect()
}

/// A native `source` node.
pub(crate) fn source(id: &str, binding: &str) -> Value {
    node(id, names::SOURCE, json!({"binding": binding}), true)
}

/// A `split` node.
pub(crate) fn split(id: &str, predicate: Value, native: bool) -> Value {
    node(id, names::SPLIT, json!({"predicate": predicate}), native)
}

/// A `drop` node.
pub(crate) fn drop(id: &str, native: bool) -> Value {
    node(id, names::DROP, json!({}), native)
}

/// An `emit` node of the harness artifact.
pub(crate) fn emit(id: &str, output: &str) -> Value {
    node(id, names::EMIT, json!({"output": output}), false)
}

/// Names a slot after the first of its boot nodes.
pub(crate) fn namer(nodes: &[NodeId]) -> Result<NativeComponentId, String> {
    NativeComponentId::new(format!("region_{}", nodes[0].local())).map_err(|error| error.to_string())
}

/// The stateless kernels random plans use.
#[derive(Clone, Copy, Debug)]
pub(crate) enum KernelKind {
    /// `namespace_prefix`: writes `name`.
    NamespacePrefix,

    /// `tag_projection`: writes `tags` and `origin_tags` of counters and distributions only.
    TagProjection,

    /// `host_enrichment`: writes nothing.
    HostEnrichment,

    /// `host_tags`: writes `tags`, by admission time.
    HostTags,

    /// `dogstatsd_mapper`: writes `name` and `tags`.
    Mapper,
}

const KERNEL_KINDS: [KernelKind; 5] = [
    KernelKind::NamespacePrefix,
    KernelKind::TagProjection,
    KernelKind::HostEnrichment,
    KernelKind::HostTags,
    KernelKind::Mapper,
];

/// The shape of a random plan below its sources' merge.
#[derive(Clone, Debug)]
pub(crate) enum Shape {
    /// An `emit` to an output of its own.
    Emit,

    /// A `drop`.
    Drop,

    /// One link, followed by the rest.
    Link(Link, Box<Shape>),

    /// A `split` with a shape on each port.
    Split(Box<Shape>, Box<Shape>),

    /// A `tee` with a shape on each port.
    Tee(Vec<Shape>),

    /// A `tee` whose every port leads to a filter with the same drop condition, each followed by a shape.
    GuardedTee(Vec<Shape>),

    /// A `split` whose ports, each through a chain of links, join again in a `merge`, followed by the rest. The
    /// chains make the paths into the `merge` differ, as rewrites then change them differently.
    Rejoin(Vec<Link>, Vec<Link>, Box<Shape>),

    /// A `tee` into two `split`s of one predicate, the first keeping its `true` port and the second its `false` port,
    /// joined by a `merge`: complementary paths, which the duplicate analysis accepts. Followed by the rest.
    Diamond(Box<Shape>),

    /// An opaque host, followed by the rest.
    Fence(Box<Shape>),

    /// A timestamp split, with a shape on each of its ports.
    TimestampSplit(Box<Shape>, Box<Shape>),
}

/// A piece of a path with one input port and one output port, which shapes chain.
#[derive(Clone, Debug)]
pub(crate) enum Link {
    /// A filter: a `split` one of whose ports leads to a `drop`.
    Filter,

    /// The filter-cardinality operator `test_keep`.
    Keep,

    /// A stateless kernel.
    Kernel(KernelKind),

    /// Tag projection followed by a filter on tags, which commute only for the kinds tag projection leaves alone.
    Projected,

    /// The map `test_shed`, which loses large metrics to capacity.
    Shed,
}

fn arb_link() -> impl Strategy<Value = Link> {
    prop_oneof![
        4 => Just(Link::Filter),
        1 => Just(Link::Keep),
        4 => select(KERNEL_KINDS.to_vec()).prop_map(Link::Kernel),
        1 => Just(Link::Projected),
        1 => Just(Link::Shed),
    ]
}

/// Returns a strategy for plan shapes, weighted toward filters and kernels, which the rewrites move filters across.
///
/// Among the rejoining `split`s, some have tag projection and a filter on tags on one port and two kernels on the
/// other: paths of one length, of which a partial push-down makes the first one node longer. An output below their
/// `merge`, under a capacity bound, then accepts the same records only if delivery order does not depend on path
/// length, which these shapes check.
pub(crate) fn arb_shape() -> impl Strategy<Value = Shape> {
    let leaf = prop_oneof![3 => Just(Shape::Emit), 1 => Just(Shape::Drop)];
    leaf.prop_recursive(6, 48, 3, |inner| {
        let boxed = inner.clone().prop_map(Box::new);
        let kernel = || select(KERNEL_KINDS.to_vec()).prop_map(Link::Kernel);
        let rest = prop_oneof![Just(Box::new(Shape::Emit)), boxed.clone()];
        prop_oneof![
            10 => (arb_link(), boxed.clone()).prop_map(|(link, rest)| Shape::Link(link, rest)),
            1 => (boxed.clone(), boxed.clone()).prop_map(|(yes, no)| Shape::Split(yes, no)),
            1 => vec(inner.clone(), 1..4).prop_map(Shape::Tee),
            2 => vec(inner.clone(), 1..4).prop_map(Shape::GuardedTee),
            2 => (vec(arb_link(), 0..3), vec(arb_link(), 0..3), boxed.clone())
                .prop_map(|(yes, no, rest)| Shape::Rejoin(yes, no, rest)),
            2 => (kernel(), kernel(), rest)
                .prop_map(|(a, b, rest)| Shape::Rejoin(vec![Link::Projected], vec![a, b], rest)),
            1 => boxed.clone().prop_map(Shape::Diamond),
            1 => boxed.clone().prop_map(Shape::Fence),
            1 => (boxed.clone(), boxed).prop_map(|(default, passthrough)| Shape::TimestampSplit(default, passthrough)),
        ]
    })
}

/// The parameters of one node of a random plan; each kind of node reads the ones it needs.
#[derive(Clone, Debug)]
pub(crate) struct NodeParams {
    predicate: Predicate,
    drops_true: bool,
    native: bool,
    namespace: &'static str,
    tag_rule: (&'static str, bool, Vec<&'static str>),
    cache: bool,
    deadline: u64,
    max_points: u64,
}

/// Returns a strategy for a predicate that reads few fields, so that filters commute with the kernels that do not
/// write them, or any generated predicate.
fn arb_filter_predicate() -> impl Strategy<Value = Predicate> {
    let string = |values: &'static [&'static str]| select(values).prop_map(|value| value.to_string());
    let kinds = vec(select(Kind::ALL.to_vec()), 1..4).prop_map(|kinds| Predicate::KindIn(kinds.into_iter().collect()));
    let tag = (string(TAG_KEY_VOCABULARY), string(TAG_VALUE_VOCABULARY)).prop_map(|(key, value)| {
        Predicate::Tag(TagMatch {
            key,
            value: StringMatch::Eq(value),
        })
    });
    let leaf = prop_oneof![
        kinds,
        string(HOST_VOCABULARY).prop_map(|host| Predicate::Host(StringMatch::Eq(host))),
        Just(Predicate::HostUnset {}),
        string(TAG_KEY_VOCABULARY).prop_map(Predicate::HasTag),
        tag,
        string(TAG_KEY_VOCABULARY).prop_map(Predicate::HasOriginTag),
        string(NAME_VOCABULARY).prop_map(|name| Predicate::Name(StringMatch::Eq(name))),
        string(PREFIX_VOCABULARY).prop_map(|prefix| Predicate::IntakeName(StringMatch::Prefix(prefix))),
    ];
    let pair = (leaf.clone(), leaf.clone(), any::<bool>()).prop_map(|(a, b, all)| {
        if all {
            Predicate::All(vec![a, b])
        } else {
            Predicate::Any(vec![a, b])
        }
    });
    prop_oneof![4 => leaf, 2 => pair, 1 => arb_predicate()]
}

fn arb_node_params() -> impl Strategy<Value = NodeParams> {
    (
        arb_filter_predicate(),
        any::<bool>(),
        proptest::bool::weighted(0.2),
        select(vec!["", "ns", "tenant."]),
        (
            select(vec!["app.requests", "app.errors", "my_metric_name", "demo.debug.trace"]),
            any::<bool>(),
            vec(select(TAG_KEY_VOCABULARY), 0..3),
        ),
        any::<bool>(),
        1_000u64..1_010,
        1u64..4,
    )
        .prop_map(
            |(predicate, drops_true, native, namespace, tag_rule, cache, deadline, max_points)| NodeParams {
                predicate,
                drops_true,
                native,
                namespace,
                tag_rule,
                cache,
                deadline,
                max_points,
            },
        )
}

/// The parameters of every node of a random plan, by position, repeating.
#[derive(Clone, Debug)]
pub(crate) struct PlanParams(Vec<NodeParams>);

impl PlanParams {
    fn get(&self, position: usize) -> &NodeParams {
        &self.0[position % self.0.len()]
    }
}

/// Returns a strategy for the parameters of a random plan's nodes.
pub(crate) fn arb_plan_params() -> impl Strategy<Value = PlanParams> {
    vec(arb_node_params(), 12).prop_map(PlanParams)
}

/// Lowers a shape below the port `from` into nodes and edges, numbering nodes and outputs as it goes.
struct Builder<'p> {
    params: &'p PlanParams,
    nodes: Vec<Value>,
    edges: Vec<(String, String)>,
    outputs: Vec<String>,
    opaque: Vec<NodeId>,
    position: usize,
    /// Whether the nodes being built are below an opaque host, where every node is native.
    fenced: bool,
}

impl Builder<'_> {
    fn connect(&mut self, from: impl Into<String>, to: impl Into<String>) {
        self.edges.push((from.into(), to.into()));
    }

    /// Returns the local ID and the parameters of the next node.
    fn next(&mut self) -> (String, NodeParams) {
        let position = self.position;
        self.position += 1;
        (format!("n{}", position), self.params.get(position).clone())
    }

    /// Adds a filter below `from` with predicate `predicate`, dropping its `true` port if `drops_true`; returns the
    /// port its kept records leave by.
    fn filter(&mut self, from: &str, local: &str, predicate: &Predicate, drops_true: bool, native: bool) -> String {
        let id = node_id(local, native);
        let drop = format!("{}_drop", local);
        self.nodes.push(node(
            local,
            names::SPLIT,
            json!({"predicate": predicate_json(predicate)}),
            native,
        ));
        self.nodes.push(node(&drop, names::DROP, json!({}), native));
        let (dropped, kept) = if drops_true {
            ("true", "false")
        } else {
            ("false", "true")
        };
        self.connect(from, format!("{}.in", id));
        self.connect(format!("{}.{}", id, dropped), format!("{}.in", node_id(&drop, native)));
        format!("{}.{}", id, kept)
    }

    /// Adds a chain of links below `from`; returns the port the chain's records leave by.
    fn chain(&mut self, links: &[Link], from: String) -> String {
        links.iter().fold(from, |from, link| {
            let (local, params) = self.next();
            self.link(link, &from, &local, &params)
        })
    }

    /// Adds one link below `from`, as node `local` with `params`; returns the port its records leave by.
    fn link(&mut self, link: &Link, from: &str, local: &str, params: &NodeParams) -> String {
        match link {
            Link::Filter => self.filter(
                from,
                local,
                &params.predicate,
                params.drops_true,
                params.native || self.fenced,
            ),
            Link::Keep => {
                let id = node_id(local, self.fenced);
                self.nodes.push(node(
                    local,
                    TEST_KEEP,
                    json!({"predicate": predicate_json(&params.predicate)}),
                    self.fenced,
                ));
                self.connect(from, format!("{}.in", id));
                format!("{}.out", id)
            }
            Link::Kernel(kind) => {
                self.nodes.push(kernel_node(local, *kind, params));
                self.connect(from, format!("{}.in", local));
                format!("{}.out", local)
            }
            Link::Projected => {
                self.nodes.push(kernel_node(local, KernelKind::TagProjection, params));
                let (filter, filter_params) = self.next();
                let native = filter_params.native || self.fenced;
                let drops_true = filter_params.drops_true;
                let key = params.tag_rule.2.first().copied().unwrap_or("env");
                let predicate = if params.cache {
                    Predicate::HasTag(key.to_string())
                } else {
                    Predicate::Any(vec![Predicate::HasTag(key.to_string()), Predicate::HostUnset {}])
                };
                self.connect(from, format!("{}.in", local));
                self.filter(&format!("{}.out", local), &filter, &predicate, drops_true, native)
            }
            Link::Shed => {
                self.nodes
                    .push(node(local, TEST_SHED, json!({"max_points": params.max_points}), true));
                self.connect(from, format!("{}.in", local));
                format!("{}.out", local)
            }
        }
    }

    fn build(&mut self, shape: &Shape, from: &str) {
        let (local, params) = self.next();
        let id = node_id(&local, self.fenced);
        match shape {
            Shape::Emit => {
                let output = format!("o{}", self.outputs.len());
                self.nodes
                    .push(node(&local, names::EMIT, json!({"output": output}), self.fenced));
                self.outputs.push(output);
                self.connect(from, format!("{}.in", id));
            }
            Shape::Drop => {
                self.nodes.push(node(&local, names::DROP, json!({}), self.fenced));
                self.connect(from, format!("{}.in", id));
            }
            Shape::Link(link, rest) => {
                let kept = self.link(link, from, &local, &params);
                self.build(rest, &kept);
            }
            Shape::Split(yes, no) => {
                self.nodes.push(node(
                    &local,
                    names::SPLIT,
                    json!({"predicate": predicate_json(&params.predicate)}),
                    self.fenced,
                ));
                self.connect(from, format!("{}.in", id));
                self.build(yes, &format!("{}.true", id));
                self.build(no, &format!("{}.false", id));
            }
            Shape::Tee(branches) => {
                self.nodes
                    .push(node(&local, names::TEE, json!({"ports": branches.len()}), self.fenced));
                self.connect(from, format!("{}.in", id));
                for (port, branch) in branches.iter().enumerate() {
                    self.build(branch, &format!("{}.{}", id, port));
                }
            }
            Shape::GuardedTee(branches) => {
                self.nodes
                    .push(node(&local, names::TEE, json!({"ports": branches.len()}), self.fenced));
                self.connect(from, format!("{}.in", id));
                let negated = Predicate::Not(Box::new(params.predicate.clone()));
                for (port, branch) in branches.iter().enumerate() {
                    // Every filter drops what the predicate does not match: `split(p)` by its `false` port, or
                    // `split(not(p))` by its `true` port.
                    let (guard, guard_params) = self.next();
                    let (predicate, drops_true) = if guard_params.drops_true {
                        (&negated, true)
                    } else {
                        (&params.predicate, false)
                    };
                    let native = guard_params.native || self.fenced;
                    let kept = self.filter(&format!("{}.{}", id, port), &guard, predicate, drops_true, native);
                    self.build(branch, &kept);
                }
            }
            Shape::Rejoin(yes, no, rest) => {
                let merge = format!("{}_m", local);
                self.nodes.push(node(
                    &local,
                    names::SPLIT,
                    json!({"predicate": predicate_json(&params.predicate)}),
                    self.fenced,
                ));
                self.nodes
                    .push(node(&merge, names::MERGE, json!({"inputs": 2}), self.fenced));
                let merge = node_id(&merge, self.fenced);
                self.connect(from, format!("{}.in", id));
                let yes = self.chain(yes, format!("{}.true", id));
                self.connect(yes, format!("{}.0", merge));
                let no = self.chain(no, format!("{}.false", id));
                self.connect(no, format!("{}.1", merge));
                self.build(rest, &format!("{}.out", merge));
            }
            Shape::Diamond(rest) => {
                let merge = format!("{}_m", local);
                self.nodes
                    .push(node(&local, names::TEE, json!({"ports": 2}), self.fenced));
                self.connect(from, format!("{}.in", id));
                let (first, _) = self.next();
                let (second, _) = self.next();
                let yes = self.filter(&format!("{}.0", id), &first, &params.predicate, false, self.fenced);
                let no = self.filter(&format!("{}.1", id), &second, &params.predicate, true, self.fenced);
                self.nodes
                    .push(node(&merge, names::MERGE, json!({"inputs": 2}), self.fenced));
                let merge = node_id(&merge, self.fenced);
                self.connect(yes, format!("{}.0", merge));
                self.connect(no, format!("{}.1", merge));
                self.build(rest, &format!("{}.out", merge));
            }
            Shape::Fence(rest) => {
                let any = json!({"kinds": "all", "timestamps": "mixed"});
                self.nodes.push(node(
                    &local,
                    names::OPAQUE_HOST,
                    json!({"component": local, "input": any, "outputs": {"out": any}}),
                    true,
                ));
                self.opaque.push(local.parse().expect("valid node ID"));
                self.connect(from, format!("{}.in", local));
                // An opaque host declares no expansion maximum, so an artifact node below one would make the work of
                // a record entering the artifact above it unbounded: what follows a host is native.
                let fenced = std::mem::replace(&mut self.fenced, true);
                self.build(rest, &format!("{}.out", local));
                self.fenced = fenced;
            }
            Shape::TimestampSplit(default, passthrough) => {
                self.nodes
                    .push(node(&local, names::TIMESTAMP_SPLIT, json!({"window": "10s"}), true));
                self.connect(from, format!("{}.in", local));
                self.build(default, &format!("{}.default", local));
                self.build(passthrough, &format!("{}.passthrough", local));
            }
        }
    }
}

fn kernel_node(local: &str, kind: KernelKind, params: &NodeParams) -> Value {
    match kind {
        KernelKind::NamespacePrefix => node(
            local,
            names::NAMESPACE_PREFIX,
            json!({"namespace": params.namespace, "exclusions": ["app."]}),
            true,
        ),
        KernelKind::TagProjection => {
            let (name, include, tags) = &params.tag_rule;
            node(
                local,
                names::TAG_PROJECTION,
                json!({
                    "rules": [{"metric_name": name, "action": if *include { "include" } else { "exclude" },
                               "tags": tags}],
                    "value_rules": [],
                    "cache_capacity": if params.cache { 16 } else { 0 },
                }),
                true,
            )
        }
        KernelKind::HostEnrichment => node(local, names::HOST_ENRICHMENT, json!({}), true),
        KernelKind::HostTags => node(
            local,
            names::HOST_TAGS,
            json!({"tags": ["host_tag:a", "zone:b"], "deadline": format!("{}s", params.deadline)}),
            true,
        ),
        KernelKind::Mapper => node(
            local,
            names::DOGSTATSD_MAPPER,
            json!({
                "profiles": [{"name": "test", "prefix": "app.", "mappings": [{
                    "match": "app.*", "match_type": "wildcard", "name": "mapped",
                    "tags": [{"name": "tail", "value": "$1"}]
                }]}],
                "cache_size": if params.cache { 100 } else { 0 },
                "string_interner_bytes": 65_536
            }),
            true,
        ),
    }
}

/// Builds the plan of a shape, fed by two sources that together admit every kind, and returns it with its opaque
/// hosts.
pub(crate) fn random_plan(shape: &Shape, params: &PlanParams) -> (Plan, Vec<NodeId>) {
    let mut builder = Builder {
        params,
        nodes: vec![
            node(
                "dsd",
                names::SOURCE,
                json!({"binding": "dogstatsd", "timestamps": "mixed"}),
                true,
            ),
            node("otlp", names::SOURCE, json!({"binding": "otlp"}), true),
            node("in", names::MERGE, json!({"inputs": 2}), false),
        ],
        edges: vec![
            ("dsd.metrics".to_string(), "policy/in.0".to_string()),
            ("otlp.metrics".to_string(), "policy/in.1".to_string()),
        ],
        outputs: Vec::new(),
        opaque: Vec::new(),
        position: 0,
        fenced: false,
    };
    builder.build(shape, "policy/in.out");
    (plan_of(builder.nodes, &builder.edges, &builder.outputs), builder.opaque)
}

// Traces.

/// A random trace's content: metrics in batches, and capacity bounds between them.
#[derive(Clone, Debug)]
pub(crate) struct TraceParams {
    metrics: Vec<Metric>,
    cuts: Vec<proptest::sample::Index>,
    capacity: Vec<(proptest::sample::Index, proptest::sample::Index, u64)>,
}

/// Returns a strategy for traces: up to 40 metrics in up to five batches per source, and up to four capacity bounds
/// of up to six records on outputs of the plan.
pub(crate) fn arb_trace() -> impl Strategy<Value = TraceParams> {
    (
        vec(arb_metric(), 0..40),
        vec(any::<proptest::sample::Index>(), 0..4),
        vec(
            (
                any::<proptest::sample::Index>(),
                any::<proptest::sample::Index>(),
                0u64..6,
            ),
            1..5,
        ),
    )
        .prop_map(|(metrics, cuts, capacity)| TraceParams {
            metrics,
            cuts,
            capacity,
        })
}

impl TraceParams {
    /// Returns the trace's events after its installation, with its capacity bounds on `outputs` if `capacity`.
    ///
    /// Batches alternate between the sources: each batch's rates go to OTLP and everything else to DogStatsD, so
    /// that every record is one its source can produce.
    pub fn events(&self, outputs: &[OutputId], capacity: bool) -> Vec<TraceEvent> {
        let mut cuts = self
            .cuts
            .iter()
            .map(|cut| cut.index(self.metrics.len() + 1))
            .collect::<Vec<_>>();
        cuts.sort_unstable();
        let mut batches = Vec::new();
        let mut start = 0;
        for end in cuts.into_iter().chain([self.metrics.len()]) {
            let (rates, others): (Vec<_>, Vec<_>) = self.metrics[start..end]
                .iter()
                .cloned()
                .partition(|metric| Kind::of(metric.values()) == Kind::Rate);
            batches.push(("dsd", others));
            batches.push(("otlp", rates));
            start = end;
        }

        let mut bounds = Vec::new();
        if capacity && !outputs.is_empty() {
            bounds = self
                .capacity
                .iter()
                .map(|(at, output, limit)| {
                    (
                        at.index(batches.len()),
                        outputs[output.index(outputs.len())].clone(),
                        *limit,
                    )
                })
                .collect::<Vec<_>>();
            bounds.sort_by_key(|(at, ..)| *at);
        }

        let mut events = Vec::new();
        let mut bounds = bounds.into_iter().peekable();
        for (position, (source, batch)) in batches.into_iter().enumerate() {
            while let Some((_, output, limit)) = bounds.next_if(|(at, ..)| *at == position) {
                events.push(TraceEvent::Capacity { output, limit });
            }
            events.push(TraceEvent::Admit {
                source: source.parse().expect("valid node ID"),
                lane: 0,
                admitted_at: AdmissionTime::from_unix_secs(1_000 + position as u64),
                batch,
            });
        }
        events
    }
}

/// An opaque host that forwards everything it admits unchanged.
struct EchoHost;

impl OpaqueHostAdapter for EchoHost {
    fn admit(&mut self, batch: HostBatch, output: &mut HostOutput) {
        for metric in batch.events {
            output.push(PortIndex::PRIMARY, metric);
        }
    }
}

fn echo(_operator: &ConfiguredOperator) -> Result<Box<dyn OpaqueHostAdapter>, AdapterError> {
    Ok(echo_host())
}

/// Returns the adapter that a random plan's opaque hosts run as: it forwards everything it admits unchanged, on its
/// one output port.
pub(crate) fn echo_host() -> Box<dyn OpaqueHostAdapter> {
    Box::new(EchoHost)
}

// Cases.

/// One random plan, validated, with the frame boot would compute for it.
pub(crate) struct Case {
    /// The plan.
    pub plan: Plan,

    /// Its opaque hosts, which run as [`EchoHost`]s.
    pub opaque: Vec<NodeId>,

    /// Its validation report.
    pub report: ValidationReport,

    /// The frame computed from it.
    pub frame: Frame,
}

impl Case {
    /// Builds, validates, and frames the plan of a shape.
    ///
    /// # Errors
    ///
    /// Returns what is wrong if the plan does not validate or its frame cannot be computed: a harness bug.
    pub fn new(shape: &Shape, params: &PlanParams) -> Result<Self, String> {
        let (plan, opaque) = random_plan(shape, params);
        Self::of(plan, opaque)
    }

    /// Validates and frames `plan`, whose opaque hosts are `opaque`.
    ///
    /// # Errors
    ///
    /// Returns what is wrong if the plan does not validate or its frame cannot be computed.
    pub fn of(plan: Plan, opaque: Vec<NodeId>) -> Result<Self, String> {
        let report = Validator::new(registry(), &catalog()).validate(&plan);
        if !report.is_valid() {
            return Err(format!("the plan is invalid:\n{}", report));
        }
        let frame = Frame::compute(&plan, registry(), &namer).map_err(|error| error.to_string())?;
        Ok(Self {
            plan,
            opaque,
            report,
            frame,
        })
    }

    /// Returns the context the pipeline would rewrite this plan in.
    pub fn context(&self) -> RewriteContext<'_> {
        RewriteContext::new(registry(), &self.frame, &self.report)
    }

    /// Runs `rewrites` alone, in order, to a fixed point, with the pipeline's checks.
    pub fn optimize(&self, rewrites: &[&dyn Rewrite]) -> Optimized {
        run_pipeline(&self.plan, &self.context(), rewrites)
    }

    /// Returns the outputs the plan declares.
    pub fn outputs(&self) -> Vec<OutputId> {
        self.plan.outputs.iter().map(|output| output.id.clone()).collect()
    }

    /// Runs `plan` over `events`, after its installation.
    ///
    /// # Errors
    ///
    /// Returns what went wrong if the interpreter fails or rejects the plan.
    pub fn run(&self, plan: &Plan, events: &[TraceEvent]) -> Result<RunOutput, String> {
        let catalog = catalog();
        let interpreter = self
            .opaque
            .iter()
            .fold(Interpreter::new(registry(), &catalog), |interpreter, host| {
                interpreter.with_opaque_host(host.clone(), echo)
            });
        let mut trace = Trace::new().install(plan.clone());
        trace.extend(events.iter().cloned());
        let run = interpreter.run(&trace).map_err(|error| error.to_string())?;
        match run.installs().first().map(|install| &install.outcome) {
            Some(InstallOutcome::Active { .. }) => Ok(run),
            Some(InstallOutcome::Rejected(rejection)) => Err(format!("the plan was rejected: {}", rejection)),
            Some(other) => Err(format!("the plan was not installed at once: {:?}", other)),
            None => Err("the plan was not installed".to_string()),
        }
    }

    /// Requires `rewritten` to deliver exactly what the original plan delivers over the trace, without capacity
    /// bounds.
    ///
    /// # Errors
    ///
    /// Returns the difference, or what went wrong running either plan.
    pub fn check_equivalent(&self, rewritten: &Plan, trace: &TraceParams) -> Result<(), String> {
        let events = trace.events(&self.outputs(), false);
        let original = self.run(&self.plan, &events)?;
        let after = self.run(rewritten, &events)?;
        compare(&OutputPoints::from_run(&original), &OutputPoints::from_run(&after))
            .map_err(|mismatch| format!("the rewritten plan delivers something else:\n{}", mismatch))?;
        self.check_capacity_loss(rewritten, &original, &after)
    }

    /// Requires `rewritten` to keep the specification's capacity rule over the trace with its capacity bounds: it
    /// delivers nothing the policy excludes, and loses no policy-included contribution more often than the original.
    /// What the policy delivers is what the original plan delivers without capacity limits: without the trace's bounds,
    /// and with every `test_shed` node [unbounded].
    ///
    /// Returns whether the trace put the original plan under capacity pressure at an output.
    ///
    /// # Errors
    ///
    /// Returns every violation, or what went wrong running either plan.
    pub fn check_capacity(&self, rewritten: &Plan, trace: &TraceParams) -> Result<bool, String> {
        let outputs = self.outputs();
        let policy = OutputPoints::from_run(&self.run(&unbounded(&self.plan), &trace.events(&outputs, false))?);
        let events = trace.events(&outputs, true);
        let (original, after) = (self.run(&self.plan, &events)?, self.run(rewritten, &events)?);
        self.check_capacity_loss(rewritten, &original, &after)?;
        let (original, after) = (CapacityOutcome::from_run(&original), CapacityOutcome::from_run(&after));
        compare_under_capacity(&policy, &original, &after)
            .map_err(|mismatch| format!("the rewritten plan breaks the capacity rule:\n{}", mismatch))?;
        Ok(original.refused != OutputPoints::new())
    }

    /// Requires that no node loses more to capacity running `rewritten` (`after`) than running the original plan
    /// (`before`): a rewrite may only reduce capacity loss, at an operator as at an output. A node the rewrite
    /// inserted loses nothing to capacity.
    fn check_capacity_loss(&self, rewritten: &Plan, before: &RunOutput, after: &RunOutput) -> Result<(), String> {
        for node in &rewritten.nodes {
            let lost = after.loss(&node.id).loss(LossClass::Capacity);
            let was = match self.plan.node(&node.id) {
                Some(_) => before.loss(&node.id).loss(LossClass::Capacity),
                None => Default::default(),
            };
            if lost.events > was.events || lost.points > was.points {
                return Err(format!(
                    "`{}` loses more to capacity in the rewritten plan: {} records and {} points, against {} and {}",
                    node.id, lost.events, lost.points, was.events, was.points
                ));
            }
        }
        Ok(())
    }

    /// Requires what every logical rewrite keeps, and that the records describe the plan they produced:
    ///
    /// - the rewritten plan validates against the catalog, and its programs compile into the frame as they are (the
    ///   decision-time rewrites apply, the logical ones already have);
    /// - it declares the same outputs, with the same `emit` nodes delivering to each, but those an applied R3 record
    ///   names, which received nothing;
    /// - every native node is still there, with its operator and parameters, but those an applied R3 record names
    ///   (removed, or a `merge` with fewer inputs), and predicates, which R7 may write in another order of the same
    ///   canonical form;
    /// - every applied record names the nodes of its site and justifies itself, and the last one lists telemetry
    ///   effects that agree with the plan it produced: a removed series' node is gone, and an added or changed one's
    ///   is there. (A later application may remove what an earlier one added.)
    ///
    /// # Errors
    ///
    /// Returns the first requirement that does not hold.
    pub fn check_structure(&self, optimized: &Optimized) -> Result<(), String> {
        let rewritten = &optimized.plan;
        let report = Validator::new(registry(), &catalog()).validate(rewritten);
        if !report.is_valid() {
            return Err(format!("the rewritten plan is invalid:\n{}", report));
        }
        compile_unvalidated(rewritten, registry(), &self.frame)
            .map_err(|error| format!("the rewritten plan does not compile into the frame: {}", error))?;
        if rewritten.outputs != self.plan.outputs {
            return Err("the rewritten plan declares other outputs".to_string());
        }
        // What R3 removed: a folded `split` and the nodes it made unreachable, which received nothing.
        let folded = optimized
            .records
            .iter()
            .filter(|record| record.is_applied() && record.id == RewriteId::R3)
            .flat_map(|record| record.nodes.iter().cloned())
            .collect::<BTreeSet<_>>();
        let emits = |plan: &Plan| {
            let mut emits = plan
                .nodes
                .iter()
                .filter(|node| node.op.as_str() == names::EMIT && !folded.contains(&node.id))
                .map(|node| (node.id.clone(), node.params.clone()))
                .collect::<Vec<_>>();
            emits.sort_by(|a, b| a.0.cmp(&b.0));
            emits
        };
        if emits(rewritten) != emits(&self.plan) {
            return Err("the rewritten plan has other `emit` nodes".to_string());
        }
        // A node's parameters with its predicate in canonical form, which R7's reordering keeps.
        let canonical = |node: &crate::ir::Node| {
            let mut params = serde_json::to_value(&node.params).expect("parameters serialize");
            if let Some(predicate) = params.get_mut("predicate") {
                if let Ok(decoded) = serde_json::from_value::<Predicate>(predicate.clone()) {
                    *predicate = serde_json::to_value(decoded.canonical()).expect("predicates serialize");
                }
            }
            params
        };
        for node in &self.plan.nodes {
            if !matches!(node.provenance, Some(Provenance::Native(_))) {
                continue;
            }
            match rewritten.node(&node.id) {
                // R3 removes what a fold makes unreachable, and takes the inputs that carry nothing off a `merge`.
                None if folded.contains(&node.id) => {}
                Some(kept) if folded.contains(&node.id) && kept.op.as_str() == names::MERGE => {}
                Some(kept) if kept.op == node.op && canonical(kept) == canonical(node) => {}
                _ => return Err(format!("the native node `{}` was removed or changed", node.id)),
            }
        }
        let mut applied = optimized.records.iter().filter(|record| record.is_applied());
        for record in applied.clone() {
            if record.nodes.is_empty() || record.justification.is_empty() {
                return Err(format!("{} names no site or gives no justification", record.id));
            }
        }
        if let Some(record) = applied.next_back() {
            for effect in &record.telemetry {
                let present = rewritten.node(&effect.node).is_some();
                if present == (effect.change == TelemetryChange::Removed) {
                    return Err(format!(
                        "{} says the series of `{}` are {}, but the node is {}",
                        record.id,
                        effect.node,
                        effect.change.as_str(),
                        if present { "there" } else { "gone" }
                    ));
                }
            }
        }
        Ok(())
    }
}

/// Draws `count` cases of every shape and their traces with a deterministic runner, for tests that check how often
/// something happens.
pub(crate) fn sample(count: usize) -> Vec<(Case, TraceParams)> {
    let mut runner = TestRunner::deterministic();
    let strategy = (arb_shape(), arb_plan_params(), arb_trace());
    (0..count)
        .map(|_| {
            let (shape, params, trace) = strategy
                .new_tree(&mut runner)
                .expect("strategies produce values")
                .current();
            (Case::new(&shape, &params).expect("harness plans are valid"), trace)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]

        #[test]
        fn property_test_harness_plans_are_valid_compile_into_their_frame_and_run(
            shape in arb_shape(),
            params in arb_plan_params(),
            trace in arb_trace(),
        ) {
            let case = Case::new(&shape, &params).map_err(TestCaseError::fail)?;
            let compiled = compile(&case.plan, &case.report, registry(), &case.frame);
            prop_assert!(compiled.is_ok(), "{:?}", compiled.err());
            let run = case.run(&case.plan, &trace.events(&case.outputs(), true));
            prop_assert!(run.is_ok(), "{:?}", run.err());
        }

        /// Under capacity bounds, each output's delivery splits into what it accepts and what it refuses: together,
        /// exactly what the output receives without bounds, and refused records are counted as the capacity loss of
        /// the `emit` nodes that delivered them.
        #[test]
        fn property_test_capacity_bounds_split_each_outputs_delivery_into_accepted_and_refused(
            shape in arb_shape(),
            params in arb_plan_params(),
            trace in arb_trace(),
        ) {
            let case = Case::new(&shape, &params).map_err(TestCaseError::fail)?;
            let outputs = case.outputs();
            let unbounded = case.run(&case.plan, &trace.events(&outputs, false)).map_err(TestCaseError::fail)?;
            let bounded = case.run(&case.plan, &trace.events(&outputs, true)).map_err(TestCaseError::fail)?;
            for output in &outputs {
                let mut together = OutputPoints::new();
                for record in bounded.output(output).iter().chain(bounded.refused(output)) {
                    together.insert(output, &record.metric);
                }
                let mut expected = OutputPoints::new();
                for record in unbounded.output(output) {
                    expected.insert(output, &record.metric);
                }
                prop_assert!(compare(&expected, &together).is_ok(), "output `{}`", output);
            }
            let refused_points = bounded
                .refused_outputs()
                .flat_map(|(_, records)| records)
                .map(|record| record.metric.values().len() as u64)
                .sum::<u64>();
            // `test_shed` nodes lose to capacity too, on runs with or without bounds: only `emit`s count refusals.
            let capacity_loss = case
                .plan
                .nodes
                .iter()
                .filter(|node| node.op.as_str() == names::EMIT)
                .map(|node| bounded.loss(&node.id).loss(LossClass::Capacity).points)
                .sum::<u64>();
            prop_assert_eq!(refused_points, capacity_loss);
            // An output that no bound limits refuses nothing.
            let bounded_outputs = trace
                .events(&outputs, true)
                .into_iter()
                .filter_map(|event| match event {
                    TraceEvent::Capacity { output, .. } => Some(output),
                    _ => None,
                })
                .collect::<std::collections::BTreeSet<_>>();
            for (output, records) in bounded.refused_outputs() {
                prop_assert!(records.is_empty() || bounded_outputs.contains(output));
            }
        }
    }

    #[test]
    fn random_plans_exercise_every_shape_and_traces_put_outputs_under_pressure() {
        let cases = sample(64);
        let pressured = cases
            .iter()
            .filter(|(case, trace)| {
                let events = trace.events(&case.outputs(), true);
                case.run(&case.plan, &events)
                    .is_ok_and(|run| run.refused_outputs().any(|(_, records)| !records.is_empty()))
            })
            .count();
        assert!(pressured > 8, "only {} of 64 traces refused anything", pressured);
        let filters = cases
            .iter()
            .filter(|(case, _)| case.plan.nodes.iter().any(|node| node.op.as_str() == TEST_KEEP))
            .count();
        assert!(filters > 2, "only {} of 64 plans have a filter operator", filters);
    }

    #[test]
    fn the_filter_operator_keeps_what_its_predicate_matches_as_a_split_to_a_drop_does() {
        let keep = |predicate: Value| {
            plan_of(
                vec![
                    node("src", names::SOURCE, json!({"binding": "otlp"}), true),
                    node("k", TEST_KEEP, json!({"predicate": predicate}), false),
                    node("e", names::EMIT, json!({"output": "o0"}), false),
                ],
                &[
                    ("src.metrics".into(), "policy/k.in".into()),
                    ("policy/k.out".into(), "policy/e.in".into()),
                ],
                &["o0".to_string()],
            )
        };
        let case = Case::of(keep(json!({"kind_in": ["gauge"]})), Vec::new()).unwrap();
        let events = vec![TraceEvent::Admit {
            source: "src".parse().unwrap(),
            lane: 0,
            admitted_at: AdmissionTime::from_unix_secs(1_000),
            batch: vec![Metric::gauge("kept", 1.0), Metric::counter("dropped", [1.0, 2.0])],
        }];
        let run = case.run(&case.plan, &events).unwrap();
        let names = run
            .output(&OutputId::new("o0").unwrap())
            .iter()
            .map(|record| record.metric.context().name().to_string())
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["kept"]);
        let loss = run.loss(&"policy/k".parse().unwrap()).loss(LossClass::Policy);
        assert_eq!((loss.events, loss.points), (1, 2));
    }
}
