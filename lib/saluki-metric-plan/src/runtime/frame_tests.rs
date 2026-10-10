//! Frames of region drivers, wired together in process, deliver what the reference interpreter delivers.
//!
//! The [`FrameHarness`] runs every slot of a frame through a [`MetricRegionDriver`] and every fixed component through
//! the interpreter's opaque-host adapters, synchronously and without channels: a batch admitted at a source is stamped
//! as its admission point stamps it, runs through every region and host it reaches, and what reaches an output is
//! recorded. A host's output is re-admitted at its own admission point, with the generation the schedule holds then.
//! Generations are installed as the installer installs them: every region prepares the candidate's program, with the
//! kernels the installer would build, and then the commit replaces the generation that admission points stamp.

use std::collections::BTreeMap;

use proptest::{collection::vec, prelude::*, sample::select};
use saluki_core::{
    data_model::event::metric::{context::Context, Metric, ScalarPoints},
    execution::{AdmissionTime, LossClass, PortIndex},
};
use serde_json::{json, Value};

use super::harness::{FrameHarness, HarnessOptions};
use crate::{
    canonical::PlanHash,
    compile::{compile, Frame, FusionVerdict, Site},
    equivalence::{compare, compare_under_capacity, OutputPoints},
    interpret::{AdapterError, HostBatch, HostOutput, Interpreter, OpaqueHostAdapter, Trace},
    ir::{NativeComponentId, NodeId, OutputId, Plan, RecipientId},
    model::{Field, Kind, PortType},
    operators::{names, ConfiguredOperator, OperatorRegistry},
    predicate::{Predicate, StringMatch},
    testing::{
        arb_metric, arb_metrics_with_kind_siblings, arb_predicate, default_program_catalog, predicate_json,
        DEFAULT_PROGRAM_JSON, NAME_VOCABULARY, NO_AGGREGATION_PIPELINE_OFF_JSON, PREFIX_VOCABULARY, TAG_KEY_VOCABULARY,
        TAG_VALUE_VOCABULARY,
    },
    validate::{CatalogOutput, MemoryCatalog, ValidationReport, Validator},
};

fn id(value: &str) -> NodeId {
    value.parse().unwrap()
}

fn out(value: &str) -> OutputId {
    OutputId::new(value).unwrap()
}

/// An opaque host that forwards everything it admits unchanged, as an opaque native filter that keeps everything
/// would.
struct EchoHost;

impl OpaqueHostAdapter for EchoHost {
    fn admit(&mut self, batch: HostBatch, output: &mut HostOutput) {
        for metric in batch.events {
            output.push(PortIndex::PRIMARY, metric);
        }
    }
}

fn echo(_operator: &ConfiguredOperator) -> Result<Box<dyn OpaqueHostAdapter>, AdapterError> {
    Ok(Box::new(EchoHost))
}

/// Names a slot after the first of its boot nodes.
fn namer(nodes: &[NodeId]) -> Result<NativeComponentId, String> {
    NativeComponentId::new(format!("region_{}", nodes[0].local())).map_err(|error| error.to_string())
}

/// Checks that the harness's outputs and the policy loss its regions counted equal the interpreter's.
fn assert_matches_interpreter(harness: &mut FrameHarness, plan: &Plan, run: &crate::interpret::RunOutput) {
    harness.publish();
    if let Err(mismatch) = compare(&OutputPoints::from_run(run), harness.outputs()) {
        panic!("region drivers differ from the interpreter:\n{}", mismatch);
    }
    for node in harness.drops_in_slots(plan) {
        assert_eq!(
            harness.telemetry().loss(&node.to_string(), LossClass::Policy),
            run.loss(&node).loss(LossClass::Policy).points,
            "policy loss of `{}`",
            node
        );
    }
}

// The default program.

/// The base time, a multiple of the 10-second window.
const B: u64 = 1_700_000_000;

fn t(relative: u64) -> AdmissionTime {
    AdmissionTime::from_unix_secs(B + relative)
}

fn gauge_at(name: &'static str, relative: u64, value: f64) -> Metric {
    Metric::gauge(Context::from_static_name(name), (B + relative, value))
}

fn counter_at(name: &'static str, relative: u64, value: f64) -> Metric {
    Metric::counter(Context::from_static_name(name), (B + relative, value))
}

#[test]
fn the_default_program_runs_through_a_frame_of_region_drivers_as_the_interpreter_runs_it() {
    for (json, timestamped) in [(DEFAULT_PROGRAM_JSON, true), (NO_AGGREGATION_PIPELINE_OFF_JSON, false)] {
        let plan = Plan::from_json(json).unwrap();
        let registry = OperatorRegistry::native();
        let catalog = default_program_catalog();
        let report = Validator::new(&registry, &catalog).validate(&plan);

        // The trace: DogStatsD and OTLP batches, a flush tick, and batches after the host tags' deadline. Without the
        // no-aggregation pipeline, DogStatsD parses no timestamps, so its timestamped points are left out.
        let mut dogstatsd = vec![
            Metric::counter(Context::from_static_parts("raw.requests.login", &["env:prod"]), 1.0),
            Metric::counter(Context::from_static_name("raw.blocked"), 1.0),
            Metric::distribution(
                Context::from_static_parts("checkout.latency", &["user_id:42", "env:dev"]),
                10.0,
            ),
            Metric::gauge(Context::from_static_name("internal.queue"), 7.0),
            Metric::histogram(
                Context::from_static_parts("checkout.latency", &["env:prod"]),
                [1.0, 2.0, 3.0, 4.0, 5.0],
            ),
        ];
        let late_dogstatsd = if timestamped {
            dogstatsd.push(counter_at("checkout.requests", 1, 2.0));
            dogstatsd.push(Metric::gauge(
                Context::from_static_name("system.load"),
                ScalarPoints::from_iter([(None, 1.0), (std::num::NonZeroU64::new(B + 2), 2.0)]),
            ));
            gauge_at("system.disk", 302, 9.0)
        } else {
            Metric::gauge(Context::from_static_name("system.disk"), 9.0)
        };
        let otlp = vec![
            gauge_at("catalog.items", 4, 3.0),
            counter_at("requests.count", 4, 4.0),
            counter_at("checkout.requests", 4, 1.0),
        ];
        let trace = Trace::new()
            .install(plan.clone())
            .admit(id("dsd_in"), t(3), dogstatsd.clone())
            .admit(id("otlp_in"), t(4), otlp.clone())
            .advance(t(15))
            .admit(id("otlp_in"), t(301), vec![gauge_at("checkout.cpu", 301, 0.5)])
            .admit(id("dsd_in"), t(302), vec![late_dogstatsd.clone()]);
        let run = Interpreter::new(&registry, &catalog).run(&trace).unwrap();

        // Kernels with caches need a Tokio runtime to be built in, as the interpreter's are.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        let _guard = runtime.enter();
        let mut harness = FrameHarness::new(&plan, &report, registry);
        // R1's cost model cuts the default program into five fused slots: three before aggregation, the
        // post-aggregation filter's, and enrichment with the whole policy region, one decision; aggregation runs in
        // an isolation slot of its own between them.
        assert_eq!(harness.drivers().len(), 6, "the default program's frame has six slots");
        harness.admit("dsd_in", t(3), dogstatsd);
        harness.admit("otlp_in", t(4), otlp);
        harness.advance(t(15));
        harness.admit("otlp_in", t(301), vec![gauge_at("checkout.cpu", 301, 0.5)]);
        harness.admit("dsd_in", t(302), vec![late_dogstatsd]);

        assert!(
            OutputPoints::from_run(&run).count(&out("dd_metrics_encode")) > 10,
            "the trace delivers"
        );
        assert_matches_interpreter(&mut harness, &plan, &run);
        // The blocklist drops exactly `tenant.raw.blocked`, so its split counts one event on `true` and the rest of
        // the first batch, after the mapper and prefix, on `false`.
        let telemetry = harness.telemetry();
        assert_eq!(telemetry.port_events("dsd_prefix_filter_blocklist", "true"), 1);
        assert_eq!(
            telemetry.port_events("dsd_prefix_filter_blocklist", "false"),
            if timestamped { 6 + 1 } else { 5 },
            "every other DogStatsD event passes the blocklist"
        );
        assert_eq!(telemetry.loss("dsd_prefix_filter_blocklist_true", LossClass::Policy), 1);
    }
}

// Random plans.

/// The stateless kernels random plans use.
#[derive(Clone, Copy, Debug)]
enum KernelKind {
    NamespacePrefix,
    TagProjection,
    HostEnrichment,
    HostTags,
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
enum Shape {
    Emit,
    Drop,
    Split(Box<Shape>, Box<Shape>),
    Tee(Vec<Shape>),
    /// A split whose ports join again in a merge, followed by the rest.
    Rejoin(Box<Shape>),
    /// A split whose `true` port goes through an opaque host into a merge that its `false` port joins directly: the
    /// merge is one fence deeper than the split, so they run in different slots, joined by an exit.
    Detour(Box<Shape>),
    /// A split whose `true` port goes through an opaque host into a copy to two merges, `m`, which emits to an output
    /// of its own, and `k`, followed by the rest; its `false` port leads to a second split, whose ports join `m` and
    /// `k`. Both merges are one fence deeper than the splits, so the second split's two edges reach two nodes of the
    /// slot below over one exit, as two entries.
    DoubleDetour(Box<Shape>),
    /// An opaque host, followed by the rest.
    Fence(Box<Shape>),
    /// A stateless kernel, followed by the rest.
    Kernel(KernelKind, Box<Shape>),
    /// A timestamp split, with a shape on each of its ports.
    TimestampSplit(Box<Shape>, Box<Shape>),
}

fn arb_shape() -> impl Strategy<Value = Shape> {
    let leaf = prop_oneof![3 => Just(Shape::Emit), 1 => Just(Shape::Drop)];
    leaf.prop_recursive(8, 64, 2, |inner| {
        prop_oneof![
            (inner.clone(), inner.clone()).prop_map(|(yes, no)| Shape::Split(Box::new(yes), Box::new(no))),
            vec(inner.clone(), 1..4).prop_map(Shape::Tee),
            inner.clone().prop_map(|rest| Shape::Rejoin(Box::new(rest))),
            inner.clone().prop_map(|rest| Shape::Detour(Box::new(rest))),
            inner.clone().prop_map(|rest| Shape::DoubleDetour(Box::new(rest))),
            inner.clone().prop_map(|rest| Shape::Fence(Box::new(rest))),
            (select(KERNEL_KINDS.to_vec()), inner.clone()).prop_map(|(kind, rest)| Shape::Kernel(kind, Box::new(rest))),
            (inner.clone(), inner)
                .prop_map(|(default, passthrough)| Shape::TimestampSplit(Box::new(default), Box::new(passthrough))),
        ]
    })
}

/// The parameters of one node of a random plan; each kind of node reads the ones it needs.
#[derive(Clone, Debug)]
struct NodeParams {
    predicate: Predicate,
    namespace: &'static str,
    exclusions: Vec<&'static str>,
    tag_rules: Vec<(&'static str, bool, Vec<&'static str>)>,
    value_rule: Option<(&'static str, &'static str, Vec<&'static str>, bool)>,
    cache: bool,
    deadline: u64,
}

fn arb_node_params() -> impl Strategy<Value = NodeParams> {
    let names = [
        "app.requests",
        "app.errors",
        "my_metric_name",
        "demo.debug.trace",
        "ns.app.requests",
    ];
    (
        arb_predicate(),
        select(vec!["", "ns", "tenant."]),
        vec(select(PREFIX_VOCABULARY), 0..3),
        vec(
            (
                select(names.to_vec()),
                any::<bool>(),
                vec(select(TAG_KEY_VOCABULARY), 0..3),
            ),
            0..3,
        ),
        // A value rule needs a non-empty prefix and tag name.
        proptest::option::of((
            select(vec!["app.", "my"]),
            select(vec!["env", "team", "region"]),
            vec(select(TAG_VALUE_VOCABULARY), 0..3),
            any::<bool>(),
        )),
        any::<bool>(),
        1_000u64..1_010,
    )
        .prop_map(
            |(predicate, namespace, exclusions, tag_rules, value_rule, cache, deadline)| NodeParams {
                predicate,
                namespace,
                exclusions,
                tag_rules,
                value_rule,
                cache,
                deadline,
            },
        )
}

/// The parameters of every node of two plans of one shape: the second plan keeps each node's parameters from the
/// first unless `changed` says otherwise for its position, so that most of its kernels are reused, and swaps the
/// targets of each split whose position `swapped` says, so that its programs route, and enter regions, differently.
#[derive(Clone, Debug)]
struct PlanParams {
    first: Vec<NodeParams>,
    second: Vec<NodeParams>,
    changed: Vec<bool>,
    swapped: Vec<bool>,
}

impl PlanParams {
    fn get(&self, plan: usize, node: usize) -> &NodeParams {
        let position = node % self.first.len();
        if plan == 1 && self.changed[position] {
            &self.second[position]
        } else {
            &self.first[position]
        }
    }

    /// Returns the ports of a split at `node` in plan `plan`, as `(the port leading to the first branch, the port
    /// leading to the second)`.
    fn split_ports(&self, plan: usize, node: usize) -> (&'static str, &'static str) {
        if plan == 1 && self.swapped[node % self.swapped.len()] {
            ("false", "true")
        } else {
            ("true", "false")
        }
    }
}

fn arb_plan_params() -> impl Strategy<Value = PlanParams> {
    arb_plan_params_of(8)
}

/// Returns plan parameters with `distinct` parameter sets, from 1 to 8, that nodes cycle through: with few, many
/// `split`s of one decision share a predicate.
fn arb_plan_params_of(distinct: usize) -> impl Strategy<Value = PlanParams> {
    (
        vec(arb_node_params(), distinct),
        vec(arb_node_params(), distinct),
        vec(any::<bool>(), 8),
        vec(proptest::bool::weighted(0.25), 8),
    )
        .prop_map(|(first, second, changed, swapped)| PlanParams {
            first,
            second,
            changed,
            swapped,
        })
}

/// A node with native provenance of its own, so that lossy and host nodes have authority, and so that R1's cost model
/// may cut between any two nodes, as it cuts between native components.
fn node(id: &str, op: &str, params: Value) -> Value {
    json!({"id": id, "op": op, "params": params, "provenance": {"native": id}})
}

fn kernel_node(id: &str, kind: KernelKind, params: &NodeParams) -> Value {
    match kind {
        KernelKind::NamespacePrefix => node(
            id,
            names::NAMESPACE_PREFIX,
            json!({"namespace": params.namespace, "exclusions": params.exclusions}),
        ),
        KernelKind::TagProjection => {
            let rules = params
                .tag_rules
                .iter()
                .map(|(name, include, tags)| {
                    json!({"metric_name": name, "action": if *include { "include" } else { "exclude" }, "tags": tags})
                })
                .collect::<Vec<_>>();
            let value_rules = params
                .value_rule
                .iter()
                .map(|(prefix, tag, values, replace)| {
                    if *replace {
                        json!({"metric_prefix": prefix, "tag_name": tag, "values": values, "on_miss": "replace",
                               "replacement": "other"})
                    } else {
                        json!({"metric_prefix": prefix, "tag_name": tag, "values": values, "on_miss": "remove"})
                    }
                })
                .collect::<Vec<_>>();
            node(
                id,
                names::TAG_PROJECTION,
                json!({
                    "rules": rules,
                    "value_rules": value_rules,
                    "cache_capacity": if params.cache { 16 } else { 0 },
                }),
            )
        }
        KernelKind::HostEnrichment => node(id, names::HOST_ENRICHMENT, json!({})),
        KernelKind::HostTags => node(
            id,
            names::HOST_TAGS,
            json!({"tags": ["host_tag:a", "zone:b"], "deadline": format!("{}s", params.deadline)}),
        ),
        KernelKind::Mapper => node(
            id,
            names::DOGSTATSD_MAPPER,
            json!({
                "profiles": [{"name": "test", "prefix": "app.", "mappings": [{
                    "match": "app.*", "match_type": "wildcard", "name": "mapped",
                    "tags": [{"name": "tail", "value": "$1"}]
                }]}],
                "cache_size": if params.cache { 100 } else { 0 },
                "string_interner_bytes": 65_536
            }),
        ),
    }
}

fn opaque(id: &str) -> Value {
    let any = json!({"kinds": "all", "timestamps": "mixed"});
    node(
        id,
        names::OPAQUE_HOST,
        json!({"component": id, "input": any, "outputs": {"out": any}}),
    )
}

/// Lowers a shape below the port `from` into nodes and edges, numbering nodes and outputs as it goes.
struct Builder<'p> {
    params: &'p PlanParams,
    plan: usize,
    nodes: Vec<Value>,
    edges: Vec<Value>,
    outputs: Vec<String>,
    opaque: Vec<String>,
}

impl Builder<'_> {
    fn connect(&mut self, from: &str, to: String) {
        self.edges.push(json!({"from": from, "to": to}));
    }

    fn build(&mut self, shape: &Shape, from: &str) {
        let position = self.nodes.len();
        let id = format!("n{}", position);
        let params = self.params.get(self.plan, position);
        let split = |id: &str| {
            node(
                id,
                names::SPLIT,
                json!({"predicate": predicate_json(&params.predicate)}),
            )
        };
        match shape {
            Shape::Emit => {
                let output = format!("o{}", self.outputs.len());
                self.nodes.push(node(&id, names::EMIT, json!({"output": output})));
                self.outputs.push(output);
                self.connect(from, format!("{}.in", id));
            }
            Shape::Drop => {
                self.nodes.push(node(&id, names::DROP, json!({})));
                self.connect(from, format!("{}.in", id));
            }
            Shape::Split(yes, no) => {
                let (first, second) = self.params.split_ports(self.plan, position);
                self.nodes.push(split(&id));
                self.connect(from, format!("{}.in", id));
                self.build(yes, &format!("{}.{}", id, first));
                self.build(no, &format!("{}.{}", id, second));
            }
            Shape::Tee(branches) => {
                self.nodes.push(node(&id, names::TEE, json!({"ports": branches.len()})));
                self.connect(from, format!("{}.in", id));
                for (port, branch) in branches.iter().enumerate() {
                    self.build(branch, &format!("{}.{}", id, port));
                }
            }
            Shape::Rejoin(rest) => {
                self.nodes.push(split(&id));
                self.connect(from, format!("{}.in", id));
                let merge = format!("{}_m", id);
                self.nodes.push(node(&merge, names::MERGE, json!({"inputs": 2})));
                self.connect(&format!("{}.true", id), format!("{}.0", merge));
                self.connect(&format!("{}.false", id), format!("{}.1", merge));
                self.build(rest, &format!("{}.out", merge));
            }
            Shape::Detour(rest) => {
                self.nodes.push(split(&id));
                self.connect(from, format!("{}.in", id));
                let host = format!("{}_h", id);
                let merge = format!("{}_m", id);
                self.nodes.push(opaque(&host));
                self.opaque.push(host.clone());
                self.nodes.push(node(&merge, names::MERGE, json!({"inputs": 2})));
                self.connect(&format!("{}.true", id), format!("{}.in", host));
                self.connect(&format!("{}.out", host), format!("{}.0", merge));
                self.connect(&format!("{}.false", id), format!("{}.1", merge));
                self.build(rest, &format!("{}.out", merge));
            }
            Shape::DoubleDetour(rest) => {
                let host = format!("{}_h", id);
                let copies = format!("{}_t", id);
                let merge = format!("{}_m", id);
                let merged = format!("{}_m_out", id);
                let second = format!("{}_s", id);
                let last = format!("{}_k", id);
                let output = format!("o{}", self.outputs.len());
                self.outputs.push(output.clone());
                self.nodes.push(split(&id));
                self.nodes.push(opaque(&host));
                self.opaque.push(host.clone());
                self.nodes.push(node(&copies, names::TEE, json!({"ports": 2})));
                self.nodes.push(node(&merge, names::MERGE, json!({"inputs": 2})));
                self.nodes.push(node(&merged, names::EMIT, json!({"output": output})));
                self.nodes.push(split(&second));
                self.nodes.push(node(&last, names::MERGE, json!({"inputs": 2})));
                self.connect(from, format!("{}.in", id));
                self.connect(&format!("{}.true", id), format!("{}.in", host));
                self.connect(&format!("{}.out", host), format!("{}.in", copies));
                self.connect(&format!("{}.0", copies), format!("{}.0", merge));
                self.connect(&format!("{}.1", copies), format!("{}.0", last));
                self.connect(&format!("{}.out", merge), format!("{}.in", merged));
                self.connect(&format!("{}.false", id), format!("{}.in", second));
                // The second plan may swap which merge each port of the second split reaches, listing the edges in
                // that order, so that the same entry of the slot below, numbered in plan edge order, leads to the
                // other merge, and to the other output, in each generation.
                let (to_merge, to_last) = self.params.split_ports(self.plan, position);
                let mut edges = [
                    (format!("{}.{}", second, to_merge), format!("{}.1", merge)),
                    (format!("{}.{}", second, to_last), format!("{}.1", last)),
                ];
                if to_merge == "false" {
                    edges.swap(0, 1);
                }
                for (from, to) in edges {
                    self.connect(&from, to);
                }
                self.build(rest, &format!("{}.out", last));
            }
            Shape::Fence(rest) => {
                self.nodes.push(opaque(&id));
                self.opaque.push(id.clone());
                self.connect(from, format!("{}.in", id));
                self.build(rest, &format!("{}.out", id));
            }
            Shape::Kernel(kind, rest) => {
                self.nodes.push(kernel_node(&id, *kind, params));
                self.connect(from, format!("{}.in", id));
                self.build(rest, &format!("{}.out", id));
            }
            Shape::TimestampSplit(default, passthrough) => {
                self.nodes
                    .push(node(&id, names::TIMESTAMP_SPLIT, json!({"window": "10s"})));
                self.connect(from, format!("{}.in", id));
                self.build(default, &format!("{}.default", id));
                self.build(passthrough, &format!("{}.passthrough", id));
            }
        }
    }
}

fn fingerprint(output: &str) -> PlanHash {
    PlanHash::of_bytes(output.as_bytes())
}

/// The catalog random plans validate against: outputs `o0` to `o127`, each with its own recipient.
fn random_plan_catalog() -> MemoryCatalog {
    (0..128).fold(MemoryCatalog::new(), |catalog, index| {
        let output = format!("o{}", index);
        catalog.with_output(
            out(&output),
            CatalogOutput {
                accepts: PortType::ANY,
                fingerprint: fingerprint(&output),
                recipients: vec![RecipientId::new(format!("out/{}", output)).unwrap()],
            },
        )
    })
}

/// Builds plan `plan` (0 or 1) of a shape, fed by two sources that together admit every kind: DogStatsD, with client
/// timestamps, admits every kind but rates, and OTLP admits rates. Returns the plan and its opaque hosts.
fn random_plan(shape: &Shape, params: &PlanParams, plan: usize) -> (Plan, Vec<String>) {
    let mut builder = Builder {
        params,
        plan,
        nodes: vec![
            node(
                "dsd",
                names::SOURCE,
                json!({"binding": "dogstatsd", "timestamps": "mixed"}),
            ),
            node("otlp", names::SOURCE, json!({"binding": "otlp"})),
            node("in", names::MERGE, json!({"inputs": 2})),
        ],
        edges: vec![
            json!({"from": "dsd.metrics", "to": "in.0"}),
            json!({"from": "otlp.metrics", "to": "in.1"}),
        ],
        outputs: Vec::new(),
        opaque: Vec::new(),
    };
    builder.build(shape, "in.out");
    let outputs = builder
        .outputs
        .iter()
        .map(|output| {
            json!({"id": output, "accepts": {"kinds": "all", "timestamps": "mixed"},
                   "fingerprint": fingerprint(output).to_string(), "recipients": [format!("out/{}", output)]})
        })
        .collect::<Vec<_>>();
    let plan =
        Plan::from_value(json!({"ir_version": 1, "nodes": builder.nodes, "edges": builder.edges, "outputs": outputs}))
            .unwrap();
    (plan, builder.opaque)
}

/// Splits `metrics` into batches at the given cut points, each batch's rates for the OTLP source and everything else
/// for the DogStatsD source, so that every record is one its source can produce.
fn batches(metrics: &[Metric], cuts: &[usize]) -> Vec<(&'static str, Vec<Metric>)> {
    let mut batches = Vec::new();
    let mut start = 0;
    for end in cuts.iter().copied().chain([metrics.len()]) {
        let (rates, others): (Vec<_>, Vec<_>) = metrics[start..end]
            .iter()
            .cloned()
            .partition(|metric| Kind::of(metric.values()) == Kind::Rate);
        batches.push(("dsd", others));
        batches.push(("otlp", rates));
        start = end;
    }
    batches
}

#[test]
fn random_plans_of_every_shape_are_valid_and_span_several_slots() {
    // The property test generates plans from these shapes; each must validate, or the property would hold vacuously,
    // and together they must exercise slot-to-slot exits.
    let shapes = [
        Shape::Emit,
        Shape::Drop,
        Shape::Split(Box::new(Shape::Emit), Box::new(Shape::Drop)),
        Shape::Tee(vec![Shape::Emit, Shape::Fence(Box::new(Shape::Emit)), Shape::Drop]),
        Shape::Rejoin(Box::new(Shape::Kernel(KernelKind::Mapper, Box::new(Shape::Emit)))),
        Shape::Detour(Box::new(Shape::Kernel(
            KernelKind::TagProjection,
            Box::new(Shape::Emit),
        ))),
        Shape::Fence(Box::new(Shape::Fence(Box::new(Shape::Drop)))),
        Shape::TimestampSplit(
            Box::new(Shape::Kernel(KernelKind::HostTags, Box::new(Shape::Emit))),
            Box::new(Shape::Kernel(KernelKind::NamespacePrefix, Box::new(Shape::Drop))),
        ),
        Shape::Kernel(KernelKind::HostEnrichment, Box::new(Shape::Emit)),
        Shape::DoubleDetour(Box::new(Shape::Emit)),
        Shape::Kernel(
            KernelKind::Mapper,
            Box::new(Shape::Kernel(KernelKind::Mapper, Box::new(Shape::Emit))),
        ),
    ];
    let mut runner = proptest::test_runner::TestRunner::deterministic();
    let params = proptest::strategy::ValueTree::current(&arb_plan_params().new_tree(&mut runner).unwrap());
    let registry = OperatorRegistry::native();
    let catalog = random_plan_catalog();
    let mut slot_counts = Vec::new();
    for shape in shapes {
        for plan_index in 0..2 {
            let (plan, _) = random_plan(&shape, &params, plan_index);
            let report = Validator::new(&registry, &catalog).validate(&plan);
            assert!(report.is_valid(), "{:?}:\n{}", shape, report);
            let frame = Frame::compute(&plan, &registry, &namer).unwrap();
            compile(&plan, &report, &registry, &frame).unwrap();
            slot_counts.push(frame.slots().len());
        }
    }
    // The detour separates its split and its merge into two slots, and so does the double detour, whose second split
    // reaches the slot below by two edges over one exit. Two mappers, of 65 ns per event each, pipeline.
    assert_eq!(slot_counts[10], 2);
    assert_eq!(slot_counts[18], 2);
    assert_eq!(slot_counts[20], 2);
    assert!(slot_counts.iter().all(|count| *count >= 1));
}

#[test]
fn a_slot_reached_by_two_edges_over_one_exit_has_an_entry_for_each() {
    let mut runner = proptest::test_runner::TestRunner::deterministic();
    let mut params = proptest::strategy::ValueTree::current(&arb_plan_params().new_tree(&mut runner).unwrap());
    params.swapped = vec![true; 8];
    // Splits that route by name, so that R3 folds none of them and both of the second split's edges stay.
    for node in params.first.iter_mut().chain(params.second.iter_mut()) {
        node.predicate = Predicate::Name(crate::predicate::StringMatch::Eq("app.requests".to_string()));
    }
    let registry = OperatorRegistry::native();
    let catalog = random_plan_catalog();
    let shape = Shape::DoubleDetour(Box::new(Shape::Emit));
    let entries = |plan_index: usize| {
        let (plan, _) = random_plan(&shape, &params, plan_index);
        let report = Validator::new(&registry, &catalog).validate(&plan);
        let frame = Frame::compute(&random_plan(&shape, &params, 0).0, &registry, &namer).unwrap();
        let compiled = compile(&plan, &report, &registry, &frame).unwrap();
        compiled.placement.slots()[1]
            .entries
            .iter()
            .map(|entry| (entry.key, entry.source, entry.input.to_string()))
            .collect::<Vec<_>>()
    };
    let region = |entry: u16| crate::compile::EntryKey::Region(saluki_core::execution::EntryId::new(entry));
    let exit = crate::compile::FrameSource::Exit {
        slot: crate::compile::SlotId::new(0),
        exit: crate::compile::ExitIndex::new(1),
    };
    // The second split's two edges share the first slot's exit to the second, as entries 0 and 1, in plan edge order;
    // the second plan swaps the merges they reach, so the same entries lead to the other merge.
    let host_entry = |entries: &[(crate::compile::EntryKey, crate::compile::FrameSource, String)]| entries[0].clone();
    let first = entries(0);
    let second = entries(1);
    assert_eq!(first.len(), 3);
    assert_eq!(host_entry(&first), host_entry(&second));
    assert_eq!(first[1], (region(0), exit, "n3_m.1".to_string()));
    assert_eq!(first[2], (region(1), exit, "n3_k.1".to_string()));
    assert_eq!(second[1], (region(0), exit, "n3_k.1".to_string()));
    assert_eq!(second[2], (region(1), exit, "n3_m.1".to_string()));
}

#[test]
fn a_batch_held_between_regions_across_a_commit_runs_its_own_generations_program_below() {
    // The double detour's second split sends to two merges of the slot below, over one exit, as entries 0 and 1; the
    // second plan swaps which merge each entry leads to, and the merges lead to different outputs. A batch of the first
    // generation held between the slots while the second commits, and while a batch of the second runs, must still
    // reach the merges its own program says.
    let mut runner = proptest::test_runner::TestRunner::deterministic();
    let mut params = proptest::strategy::ValueTree::current(&arb_plan_params().new_tree(&mut runner).unwrap());
    params.swapped = vec![true; 8];
    params.changed = vec![false; 8];
    // The splits test the kind, so that the first one sends the batch's other kinds on, below the host, whatever the
    // strategy draws.
    for node in &mut params.first {
        node.predicate = Predicate::KindIn([Kind::Counter].into_iter().collect());
    }
    let shape = Shape::DoubleDetour(Box::new(Shape::Emit));
    let registry = OperatorRegistry::native();
    let catalog = random_plan_catalog();
    let (first, opaque) = random_plan(&shape, &params, 0);
    let (second, _) = random_plan(&shape, &params, 1);
    let validator = Validator::new(&registry, &catalog);
    let (first_report, second_report) = (validator.validate(&first), validator.validate(&second));
    // Counters and other kinds, so that the splits send events each way.
    let metrics = (0..16)
        .map(|index| match index % 4 {
            0 => Metric::counter(Context::from_static_name("app.requests"), 1.0),
            1 => Metric::gauge(Context::from_static_name("app.errors"), 2.0),
            2 => Metric::distribution(Context::from_static_name("my_metric_name"), 3.0),
            _ => Metric::counter(Context::from_static_name("demo.debug.trace"), 4.0),
        })
        .collect::<Vec<_>>();
    let trace = Trace::new()
        .install(first.clone())
        .admit(id("dsd"), AdmissionTime::from_unix_secs(1_000), metrics.clone())
        .install(second.clone())
        .admit(id("dsd"), AdmissionTime::from_unix_secs(1_001), metrics.clone());
    let interpreter = opaque
        .iter()
        .fold(Interpreter::new(&registry, &catalog), |interpreter, host| {
            interpreter.with_opaque_host(id(host), echo)
        });
    let run = interpreter.run(&trace).unwrap();

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    let _guard = runtime.enter();
    let mut harness = FrameHarness::new(&first, &first_report, registry);
    harness.prepare(&second, &second_report);
    harness.hold_between_slots();
    harness.admit("dsd", AdmissionTime::from_unix_secs(1_000), metrics.clone());
    assert!(harness.held() > 0, "the first batch is held between the slots");
    harness.commit();
    harness.stop_holding();
    // The second batch runs through every region before the first one's held buffers reach the slot below.
    harness.admit("dsd", AdmissionTime::from_unix_secs(1_001), metrics);
    harness.release();
    assert_eq!(harness.held(), 0);
    assert_matches_interpreter(&mut harness, &second, &run);
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn property_test_frames_of_region_drivers_deliver_what_the_interpreter_delivers(
        shape in arb_shape(),
        params in arb_plan_params(),
        metrics in vec(arb_metric(), 0..40),
        cuts in vec(any::<proptest::sample::Index>(), 0..4),
        install in proptest::option::of(any::<proptest::sample::Index>()),
        hold in any::<bool>(),
    ) {
        let registry = OperatorRegistry::native();
        let catalog = random_plan_catalog();
        let (first, opaque) = random_plan(&shape, &params, 0);
        let (second, _) = random_plan(&shape, &params, 1);
        let validator = Validator::new(&registry, &catalog);
        let (first_report, second_report) = (validator.validate(&first), validator.validate(&second));
        prop_assert!(first_report.is_valid(), "the first plan is invalid:\n{}", first_report);
        prop_assert!(second_report.is_valid(), "the second plan is invalid:\n{}", second_report);

        let mut cuts = cuts.iter().map(|cut| cut.index(metrics.len() + 1)).collect::<Vec<_>>();
        cuts.sort_unstable();
        let batches = batches(&metrics, &cuts);
        let install = install.map(|index| index.index(batches.len()));

        // The interpreter: the second plan installed after batch `install`, if any.
        let mut trace = Trace::new().install(first.clone());
        for (position, (source, batch)) in batches.iter().enumerate() {
            trace = trace.admit(id(source), AdmissionTime::from_unix_secs(1_000 + position as u64), batch.clone());
            if install == Some(position) {
                trace = trace.install(second.clone());
            }
        }
        let interpreter = opaque.iter().fold(Interpreter::new(&registry, &catalog), |interpreter, host| {
            interpreter.with_opaque_host(id(host), echo)
        });
        let run = interpreter.run(&trace).unwrap();

        // The same trace through a frame of region drivers. The second plan is prepared before batch `install`, which
        // is still stamped with the first generation and must run its programs although every region holds the second
        // one's, and committed after it. With `hold`, what batch `install` sends from one region to another is held
        // between them until the next batch has run, so that a region below runs a batch of the first generation after
        // the second committed and ran, and must find its entries in the first generation's program.
        let runtime = tokio::runtime::Builder::new_current_thread().enable_time().build().unwrap();
        let _guard = runtime.enter();
        let mut harness = FrameHarness::new(&first, &first_report, registry);
        let mut plan = &first;
        for (position, (source, batch)) in batches.into_iter().enumerate() {
            if install == Some(position) {
                harness.prepare(&second, &second_report);
            }
            if install == Some(position) && hold {
                harness.hold_between_slots();
            }
            harness.admit(source, AdmissionTime::from_unix_secs(1_000 + position as u64), batch);
            if install.is_some_and(|install| install + 1 == position) {
                harness.release();
            }
            if install == Some(position) {
                prop_assert_eq!(harness.drivers()[0].generations().len(), 2, "both generations are live");
                harness.commit();
                harness.stop_holding();
                plan = &second;
            }
        }
        harness.release();
        harness.publish();

        if let Err(mismatch) = compare(&OutputPoints::from_run(&run), harness.outputs()) {
            prop_assert!(false, "region drivers differ from the interpreter:\n{}", mismatch);
        }
        // Without an install, every drop's policy loss matches; with one, the counters of a drop add up over both
        // generations, as the interpreter's do.
        for node in harness.drops_in_slots(plan) {
            prop_assert_eq!(
                harness.telemetry().loss(&node.to_string(), LossClass::Policy),
                run.loss(&node).loss(LossClass::Policy).points,
                "policy loss of `{}`", node
            );
        }
        // Retired generations are released: only the active one is left in every region.
        for driver in harness.drivers() {
            prop_assert_eq!(driver.generations().len(), 1);
        }
    }
}

// Decisions: grouping, the decision-time rewrites (R2, R3's folding of bound constants, R4), and derived counts.

/// The configurations decisions run in: production's, memos that evict on every other context and none, and programs
/// compiled without the decision-time rewrites.
fn decision_configurations() -> [(&'static str, HarnessOptions); 4] {
    let production = HarnessOptions::default();
    [
        ("decision rewrites and the default memo", production),
        (
            "decision rewrites and one-entry memos",
            HarnessOptions {
                memo_entries: 1,
                ..production
            },
        ),
        (
            "decision rewrites without memos",
            HarnessOptions {
                memo_entries: 0,
                ..production
            },
        ),
        (
            "no decision-time rewrites",
            HarnessOptions {
                decision_rewrites: false,
                memo_entries: 0,
                ..production
            },
        ),
    ]
}

/// Runs batches through a frame of region drivers compiled and run as `options` say, and publishes their telemetry.
fn run_harness(
    plan: &Plan, report: &ValidationReport, batches: &[(&'static str, Vec<Metric>)], options: HarnessOptions,
) -> FrameHarness {
    let mut harness = FrameHarness::with_options(plan, report, OperatorRegistry::native(), options);
    for (position, (source, batch)) in batches.iter().enumerate() {
        harness.admit(
            source,
            AdmissionTime::from_unix_secs(1_000 + position as u64),
            batch.clone(),
        );
    }
    harness.publish();
    harness
}

/// Returns the events each port of every `split` and `tee` that runs in a slot sent, by node and port, as the
/// regions' published telemetry counts them.
fn decision_port_events(harness: &FrameHarness, plan: &Plan) -> BTreeMap<(String, String), u64> {
    let mut events = BTreeMap::new();
    for node in &plan.nodes {
        if !matches!(node.op.as_str(), names::SPLIT | names::TEE)
            || !matches!(harness.placement().site(&node.id), Some(Site::Slot(_)))
        {
            continue;
        }
        let operator = harness.registry().configure(node.op.as_str(), &node.params).unwrap();
        for port in &operator.ports().outputs {
            let (node, port) = (node.id.to_string(), port.name.to_string());
            let count = harness.telemetry().port_events(&node, &port);
            events.insert((node, port), count);
        }
    }
    events
}

/// Returns the interpreter's run of a random plan's batches.
fn interpreted(plan: &Plan, opaque: &[String], batches: &[(&'static str, Vec<Metric>)]) -> crate::interpret::RunOutput {
    let registry = OperatorRegistry::native();
    let catalog = random_plan_catalog();
    let mut trace = Trace::new().install(plan.clone());
    for (position, (source, batch)) in batches.iter().enumerate() {
        trace = trace.admit(
            id(source),
            AdmissionTime::from_unix_secs(1_000 + position as u64),
            batch.clone(),
        );
    }
    let interpreter = opaque
        .iter()
        .fold(Interpreter::new(&registry, &catalog), |interpreter, host| {
            interpreter.with_opaque_host(id(host), echo)
        });
    interpreter.run(&trace).unwrap()
}

/// Returns plan parameters for the decision property tests: those of [`arb_plan_params_of`], with from 1 to 8 distinct
/// sets, in which each predicate is, half of the time, an endpoint allowlist restricted to some kinds, `all(kind_in,
/// any(intake_name in, intake_name prefix_in))`: costly enough to memoize, and reading the kind, so that memos meet
/// the metrics' kind siblings.
fn arb_decision_plan_params() -> impl Strategy<Value = PlanParams> {
    let allowlist = (
        vec(select(Kind::ALL.to_vec()), 1..4),
        vec(select(NAME_VOCABULARY), 0..3),
        vec(select(PREFIX_VOCABULARY), 0..3),
    )
        .prop_map(|(kinds, names, prefixes)| {
            let strings = |values: Vec<&str>| values.into_iter().map(str::to_string).collect();
            Predicate::All(vec![
                Predicate::KindIn(kinds.into_iter().collect()),
                Predicate::Any(vec![
                    Predicate::IntakeName(StringMatch::In(strings(names))),
                    Predicate::IntakeName(StringMatch::PrefixIn(strings(prefixes))),
                ]),
            ])
        });
    (
        (1usize..=8).prop_flat_map(arb_plan_params_of),
        vec(proptest::option::of(allowlist), 16),
    )
        .prop_map(|(mut params, allowlists)| {
            let nodes = params.first.iter_mut().chain(params.second.iter_mut());
            for (node, allowlist) in nodes.zip(allowlists) {
                if let Some(allowlist) = allowlist {
                    node.predicate = allowlist;
                }
            }
            params
        })
}

#[test]
fn random_plans_exercise_decision_groups_and_every_decision_time_rewrite() {
    // The property tests below draw plans from these strategies; they hold vacuously unless the plans have decisions of
    // several nodes, predicates R2 shares, predicates R3 folds, and decisions R4 memoizes, some of them on the kind,
    // which the metrics' kind siblings (a metric of the same context and another kind after each) then tell apart.
    let strategy = (arb_shape(), arb_decision_plan_params());
    let mut runner = proptest::test_runner::TestRunner::deterministic();
    let registry = OperatorRegistry::native();
    let catalog = random_plan_catalog();
    let (mut grouped, mut shared, mut folded, mut memoized, mut on_the_kind) = (0, 0, 0, 0, 0);
    for _ in 0..256 {
        let (shape, params) = proptest::strategy::ValueTree::current(&strategy.new_tree(&mut runner).unwrap());
        let (plan, _) = random_plan(&shape, &params, 0);
        let report = Validator::new(&registry, &catalog).validate(&plan);
        assert!(report.is_valid(), "{:?}:\n{}", shape, report);
        let frame = Frame::compute(&plan, &registry, &namer).unwrap();
        let compiled = compile(&plan, &report, &registry, &frame).unwrap();
        let decisions = compiled.programs.iter().flat_map(|program| program.decisions());
        for decision in decisions {
            grouped += usize::from(decision.nodes.len() > 1);
            on_the_kind += usize::from(decision.memoized && decision.predicates.reads().contains(Field::Kind));
        }
        for record in &compiled.rewrites {
            match record.id {
                crate::compile::RewriteId::R2 => shared += 1,
                crate::compile::RewriteId::R3 => folded += 1,
                crate::compile::RewriteId::R4 => memoized += 1,
                _ => {}
            }
        }
    }
    assert!(grouped >= 32, "{} decisions of several nodes", grouped);
    assert!(shared >= 8, "{} shared predicates", shared);
    assert!(folded >= 8, "{} folded predicates", folded);
    assert!(memoized >= 32, "{} memoized decisions", memoized);
    assert!(on_the_kind >= 32, "{} memoized decisions read the kind", on_the_kind);
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn property_test_decisions_with_and_without_rewrites_and_memos_deliver_what_the_interpreter_delivers(
        shape in arb_shape(),
        params in arb_decision_plan_params(),
        metrics in arb_metrics_with_kind_siblings(0..20),
        cuts in vec(any::<proptest::sample::Index>(), 0..4),
    ) {
        let registry = OperatorRegistry::native();
        let catalog = random_plan_catalog();
        let (plan, opaque) = random_plan(&shape, &params, 0);
        let report = Validator::new(&registry, &catalog).validate(&plan);
        prop_assert!(report.is_valid(), "the plan is invalid:\n{}", report);
        let mut cuts = cuts.iter().map(|cut| cut.index(metrics.len() + 1)).collect::<Vec<_>>();
        cuts.sort_unstable();
        let batches = batches(&metrics, &cuts);
        let run = interpreted(&plan, &opaque, &batches);

        // Every configuration delivers what the interpreter delivers, with its policy loss, and derives the same
        // per-port counts for every decision node: neither the rewrites nor a memo, however small, changes an exit set.
        let runtime = tokio::runtime::Builder::new_current_thread().enable_time().build().unwrap();
        let _guard = runtime.enter();
        let mut first_events = None;
        for (configuration, options) in decision_configurations() {
            let harness = run_harness(&plan, &report, &batches, options);
            if let Err(mismatch) = compare(&OutputPoints::from_run(&run), harness.outputs()) {
                prop_assert!(
                    false,
                    "with {}, region drivers differ from the interpreter:\n{}",
                    configuration,
                    mismatch
                );
            }
            for node in harness.drops_in_slots(&plan) {
                prop_assert_eq!(
                    harness.telemetry().loss(&node.to_string(), LossClass::Policy),
                    run.loss(&node).loss(LossClass::Policy).points,
                    "policy loss of `{}` with {}", node, configuration
                );
            }
            let events = decision_port_events(&harness, &plan);
            match &first_events {
                None => first_events = Some(events),
                Some(expected) => prop_assert_eq!(&events, expected, "per-port counts with {}", configuration),
            }
        }
    }

    #[test]
    fn property_test_decision_rewrites_only_reduce_capacity_loss_under_capacity_pressure(
        shape in arb_shape(),
        params in arb_decision_plan_params(),
        metrics in arb_metrics_with_kind_siblings(0..20),
        cuts in vec(any::<proptest::sample::Index>(), 0..4),
        capacity in 0usize..12,
    ) {
        let registry = OperatorRegistry::native();
        let catalog = random_plan_catalog();
        let (plan, opaque) = random_plan(&shape, &params, 0);
        let report = Validator::new(&registry, &catalog).validate(&plan);
        prop_assert!(report.is_valid(), "the plan is invalid:\n{}", report);
        let mut cuts = cuts.iter().map(|cut| cut.index(metrics.len() + 1)).collect::<Vec<_>>();
        cuts.sort_unstable();
        let batches = batches(&metrics, &cuts);
        // What the policy delivers: the plan without capacity pressure.
        let policy = OutputPoints::from_run(&interpreted(&plan, &opaque, &batches));

        // Each output accepts at most `capacity` records. The original runs every `split`'s predicate as written,
        // unshared and unmemoized; the rewritten one with R2, R3's folding, and R4, as production does.
        let runtime = tokio::runtime::Builder::new_current_thread().enable_time().build().unwrap();
        let _guard = runtime.enter();
        let pressure = HarnessOptions { output_capacity: Some(capacity), ..HarnessOptions::default() };
        let original = run_harness(&plan, &report, &batches, HarnessOptions {
            decision_rewrites: false,
            memo_entries: 0,
            ..pressure
        });
        let rewritten = run_harness(&plan, &report, &batches, pressure);
        let (original, rewritten) = (original.capacity_outcome(), rewritten.capacity_outcome());
        if let Err(mismatch) = compare_under_capacity(&policy, &original, &rewritten) {
            prop_assert!(false, "the decision-time rewrites break the capacity rule:\n{}", mismatch);
        }
        // The rewrites change no exit set and no order within an exit, so under pressure they deliver and refuse
        // exactly what the original does.
        prop_assert_eq!(rewritten, original);
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    /// R1's cost model over random plans, in which every node is a native component of its own: a decision (`split`s
    /// and `tee`s joined by edges) is never cut, no region runs only terminals, and a region costs more than the budget
    /// only when one decision alone does, which takes a `tee` (no single node costs more than the budget, which is the
    /// costliest node's estimate, and a decision without copies costs no more than its costliest `split`).
    #[test]
    fn property_test_the_cost_model_keeps_decisions_whole_and_regions_within_the_budget(
        shape in arb_shape(),
        params in arb_plan_params(),
    ) {
        let registry = OperatorRegistry::native();
        let (plan, _) = random_plan(&shape, &params, 0);
        let frame = Frame::compute(&plan, &registry, &namer).unwrap();
        let operator = |node: &NodeId| plan.node(node).map_or("", |node| node.op.as_str());
        let decides = |node: &NodeId| matches!(operator(node), names::SPLIT | names::TEE);
        for decision in frame.fusion() {
            if decides(&decision.from.node) && decides(&decision.to.node) {
                prop_assert!(
                    !matches!(decision.verdict, FusionVerdict::Pipelined { .. }),
                    "{} to {} is cut: {:?}", decision.from, decision.to, decision.verdict
                );
            }
        }
        for slot in frame.slots() {
            prop_assert!(
                slot.nodes.iter().any(|node| !matches!(operator(node), names::EMIT | names::DROP)),
                "`{}` runs only terminals: {:?}", slot.name, slot.nodes
            );
            if slot.cost > frame.cost_budget() {
                prop_assert!(
                    slot.nodes.iter().any(|node| operator(node) == names::TEE),
                    "`{}` costs {} of {} without a decision that copies: {:?}",
                    slot.name, slot.cost, frame.cost_budget(), slot.nodes
                );
            }
        }
    }
}
