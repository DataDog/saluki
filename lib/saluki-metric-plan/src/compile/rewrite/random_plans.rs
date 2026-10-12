//! Random valid plans and traces for the rewrites' property tests.
//!
//! [`arb_shape`] generates the shape of a plan below its sources, with the predicates of its `split`s drawn from a
//! strategy each test chooses, so that a rewrite's tests can favor the predicates it acts on. [`Case`] builds the plan,
//! validates it, and computes its frame as boot would; it runs one rewrite to a fixed point through the pipeline's own
//! loop and checks, and runs plans over traces in the reference interpreter.
//!
//! The checks every rewrite's property tests share are here too: [`check_equivalent`], that the rewritten plan
//! delivers what the original delivers, [`check_same_order`], that it delivers every output's records in the same
//! order, and [`check_under_capacity`], that it delivers and refuses the same under capacity pressure.

use std::collections::BTreeMap;

use proptest::{
    collection::vec,
    prelude::*,
    sample::{select, Index},
    test_runner::TestCaseError,
};
use saluki_core::{data_model::event::metric::Metric, execution::AdmissionTime};
use serde_json::{json, Value};

use super::{r7::OperandProfile, run_pipeline, Optimized, Rewrite, RewriteContext};
use crate::{
    canonical::PlanHash,
    compile::Frame,
    equivalence::{compare, compare_under_capacity, CapacityOutcome, OutputPoints},
    interpret::{AdapterError, HostBatch, HostOutput, Interpreter, OpaqueHostAdapter, RunOutput, Trace},
    ir::{BindingName, NativeComponentId, NodeId, Operand, OutputId, ParamType, Plan, RecipientId},
    model::{Kind, KindSet, PortType},
    operators::{names, ConfiguredOperator, OperatorRegistry},
    predicate::Predicate,
    testing::{arb_metric, arb_predicate},
    validate::{CatalogOutput, Liveness, MemoryCatalog, ValidationReport, Validator},
};

/// A stateless kernel of a random plan, with fixed parameters.
#[derive(Clone, Copy, Debug)]
pub(super) enum KernelKind {
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

/// One step of a chain inside a [`Shape::Rejoin`].
#[derive(Clone, Debug)]
pub(super) enum Step {
    /// A `split` whose `true` port (with `keep` true) or `false` port continues the chain, and whose other port leads
    /// to a `drop`.
    Filter { predicate: Predicate, keep: bool },

    /// A stateless kernel.
    Kernel(KernelKind),
}

/// The shape of a random plan below its sources' merge.
#[derive(Clone, Debug)]
pub(super) enum Shape {
    /// An `emit` to an output of its own.
    Emit,
    Drop,
    Split(Predicate, Box<Shape>, Box<Shape>),
    Tee(Vec<Shape>),
    /// A `split` whose ports each pass through a chain of steps before they join again in a `merge`, followed by the
    /// rest: paths of different lengths into one `merge`.
    Rejoin(Predicate, Vec<Step>, Vec<Step>, Box<Shape>),
    /// An opaque host that forwards everything, followed by the rest.
    Fence(Box<Shape>),
    /// A stateless kernel, followed by the rest.
    Kernel(KernelKind, Box<Shape>),
    /// A timestamp split, with a shape on each of its ports.
    TimestampSplit(Box<Shape>, Box<Shape>),
}

fn arb_step<P>(predicate: P) -> impl Strategy<Value = Step>
where
    P: Strategy<Value = Predicate>,
{
    prop_oneof![
        2 => (predicate, any::<bool>()).prop_map(|(predicate, keep)| Step::Filter { predicate, keep }),
        1 => select(KERNEL_KINDS.to_vec()).prop_map(Step::Kernel),
    ]
}

/// Returns a strategy for plan shapes whose `split`s draw their predicates from `predicate`.
pub(super) fn arb_shape<P>(predicate: P) -> impl Strategy<Value = Shape>
where
    P: Strategy<Value = Predicate> + Clone + 'static,
{
    let leaf = prop_oneof![3 => Just(Shape::Emit), 1 => Just(Shape::Drop)];
    leaf.prop_recursive(6, 48, 3, move |inner| {
        prop_oneof![
            3 => (predicate.clone(), inner.clone(), inner.clone())
                .prop_map(|(predicate, yes, no)| Shape::Split(predicate, Box::new(yes), Box::new(no))),
            1 => vec(inner.clone(), 1..4).prop_map(Shape::Tee),
            2 => (
                predicate.clone(),
                vec(arb_step(predicate.clone()), 0..3),
                vec(arb_step(predicate.clone()), 0..3),
                inner.clone(),
            )
                .prop_map(|(predicate, yes, no, rest)| Shape::Rejoin(predicate, yes, no, Box::new(rest))),
            1 => inner.clone().prop_map(|rest| Shape::Fence(Box::new(rest))),
            1 => (select(KERNEL_KINDS.to_vec()), inner.clone())
                .prop_map(|(kind, rest)| Shape::Kernel(kind, Box::new(rest))),
            1 => (inner.clone(), inner)
                .prop_map(|(default, passthrough)| Shape::TimestampSplit(Box::new(default), Box::new(passthrough))),
        ]
    })
}

/// The live bindings random plans may use, with the values their operands resolve to.
const BOUND_FLAGS: [(&str, bool); 2] = [("test.on", true), ("test.off", false)];
const BOUND_LISTS: [(&str, &[&str]); 2] = [("test.empty", &[]), ("test.names", &["app.requests"])];

fn binding(name: &str) -> BindingName {
    BindingName::new(name).expect("valid binding name")
}

/// Returns a strategy for `flag` and `is_empty` leaves, literal or bound to configuration with resolved values:
/// predicates that are constant given their operands, which structure must treat as opaque.
pub(super) fn arb_configuration_leaf() -> impl Strategy<Value = Predicate> {
    prop_oneof![
        any::<bool>().prop_map(|value| Predicate::Flag(Operand::literal(value))),
        select(BOUND_FLAGS.to_vec())
            .prop_map(|(name, value)| Predicate::Flag(Operand::config(binding(name)).with_resolved(value))),
        any::<bool>().prop_map(|empty| {
            let values = if empty { Vec::new() } else { vec!["a".to_string()] };
            Predicate::IsEmpty(Operand::literal(values))
        }),
        select(BOUND_LISTS.to_vec()).prop_map(|(name, values)| {
            let values = values.iter().map(|value| value.to_string()).collect::<Vec<_>>();
            Predicate::IsEmpty(Operand::config(binding(name)).with_resolved(values))
        }),
    ]
}

/// Returns a strategy for predicates that favors the ones constant in structure: literal constants, junctions that
/// literal constants decide, and `kind_in` leaves that decide some inputs, besides general predicates and the
/// configuration leaves that must stay opaque.
pub(super) fn arb_split_predicate() -> BoxedStrategy<Predicate> {
    let kinds = vec(select(Kind::ALL.to_vec()), 0..4).prop_map(|kinds| Predicate::KindIn(kinds.into_iter().collect()));
    prop_oneof![
        3 => arb_predicate(),
        2 => any::<bool>().prop_map(Predicate::Const),
        2 => (arb_predicate(), arb_predicate(), any::<bool>(), any::<bool>()).prop_map(|(a, b, decisive, first)| {
            // An `all` with a `const(false)` operand, or an `any` with a `const(true)` one, wherever it is.
            let constant = Predicate::Const(decisive);
            let operands = if first { vec![constant, a, b] } else { vec![a, b, constant] };
            if decisive {
                Predicate::Any(operands)
            } else {
                Predicate::All(operands)
            }
        }),
        1 => any::<bool>().prop_map(|value| Predicate::Not(Box::new(Predicate::Const(value)))),
        1 => prop_oneof![Just(Predicate::KindIn(KindSet::ALL)), Just(Predicate::KindIn(KindSet::EMPTY)), kinds],
        2 => (arb_configuration_leaf(), arb_predicate(), any::<bool>()).prop_map(|(leaf, other, all)| {
            if all {
                Predicate::All(vec![leaf, other])
            } else {
                Predicate::Any(vec![other, leaf])
            }
        }),
        1 => arb_configuration_leaf(),
    ]
    .boxed()
}

/// Returns a predicate as a plan's `split` holds it: the test bindings of [`arb_predicate`] (`test.values`,
/// `test.enabled`, and `test.mirrored`), which generated predicates bind to different values, replaced by their
/// resolved values, and the bindings of [`arb_configuration_leaf`], which [`catalog`] declares, kept.
fn predicate_json(predicate: &Predicate) -> Value {
    fn declared(name: &str) -> bool {
        BOUND_FLAGS.iter().any(|(flag, _)| *flag == name) || BOUND_LISTS.iter().any(|(list, _)| *list == name)
    }
    fn literal(value: &mut Value) {
        match value {
            Value::Object(members) => {
                if members
                    .get("config")
                    .and_then(Value::as_str)
                    .is_some_and(|name| !declared(name))
                {
                    if let Some(resolved) = members.get("resolved").cloned() {
                        *value = resolved;
                        return;
                    }
                }
                members.values_mut().for_each(literal);
            }
            Value::Array(values) => values.iter_mut().for_each(literal),
            _ => {}
        }
    }
    let mut value = serde_json::to_value(predicate).expect("predicates serialize");
    literal(&mut value);
    value
}

/// A node with native provenance of its own, so that lossy and host nodes have authority.
fn node(id: &str, op: &str, params: Value) -> Value {
    json!({"id": id, "op": op, "params": params, "provenance": {"native": id}})
}

fn kernel_node(id: &str, kind: KernelKind) -> Value {
    match kind {
        KernelKind::NamespacePrefix => node(
            id,
            names::NAMESPACE_PREFIX,
            json!({"namespace": "ns", "exclusions": ["app."]}),
        ),
        KernelKind::TagProjection => node(
            id,
            names::TAG_PROJECTION,
            json!({
                "rules": [{"metric_name": "app.requests", "action": "exclude", "tags": ["env"]}],
                "value_rules": [{"metric_prefix": "my", "tag_name": "team", "values": ["platform"],
                                 "on_miss": "replace", "replacement": "other"}],
                "cache_capacity": 16,
            }),
        ),
        KernelKind::HostEnrichment => node(id, names::HOST_ENRICHMENT, json!({})),
        KernelKind::HostTags => node(
            id,
            names::HOST_TAGS,
            json!({"tags": ["host_tag:a", "zone:b"], "deadline": "1003s"}),
        ),
        KernelKind::Mapper => node(
            id,
            names::DOGSTATSD_MAPPER,
            json!({
                "profiles": [{"name": "test", "prefix": "app.", "mappings": [{
                    "match": "app.*", "match_type": "wildcard", "name": "mapped",
                    "tags": [{"name": "tail", "value": "$1"}]
                }]}],
                "cache_size": 100,
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
#[derive(Default)]
struct Builder {
    nodes: Vec<Value>,
    edges: Vec<Value>,
    outputs: Vec<String>,
    opaque: Vec<String>,
}

impl Builder {
    fn next_id(&self) -> String {
        format!("n{}", self.nodes.len())
    }

    fn connect(&mut self, from: &str, to: String) {
        self.edges.push(json!({"from": from, "to": to}));
    }

    fn split(&mut self, id: &str, predicate: &Predicate, from: &str) {
        self.nodes
            .push(node(id, names::SPLIT, json!({"predicate": predicate_json(predicate)})));
        self.connect(from, format!("{}.in", id));
    }

    fn build(&mut self, shape: &Shape, from: &str) {
        let id = self.next_id();
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
            Shape::Split(predicate, yes, no) => {
                self.split(&id, predicate, from);
                self.build(yes, &format!("{}.true", id));
                self.build(no, &format!("{}.false", id));
            }
            Shape::Tee(branches) => {
                self.nodes.push(node(&id, names::TEE, json!({"ports": branches.len()})));
                self.connect(from, format!("{}.in", id));
                for (port, branch) in branches.iter().enumerate() {
                    self.build(branch, &format!("{}.{}", id, port));
                }
            }
            Shape::Rejoin(predicate, yes, no, rest) => {
                self.split(&id, predicate, from);
                let merge = format!("{}_m", id);
                self.nodes.push(node(&merge, names::MERGE, json!({"inputs": 2})));
                let yes = self.chain(yes, format!("{}.true", id));
                self.connect(&yes, format!("{}.0", merge));
                let no = self.chain(no, format!("{}.false", id));
                self.connect(&no, format!("{}.1", merge));
                self.build(rest, &format!("{}.out", merge));
            }
            Shape::Fence(rest) => {
                self.nodes.push(opaque(&id));
                self.opaque.push(id.clone());
                self.connect(from, format!("{}.in", id));
                self.build(rest, &format!("{}.out", id));
            }
            Shape::Kernel(kind, rest) => {
                self.nodes.push(kernel_node(&id, *kind));
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

    /// Builds a chain of steps below `from`, returning the port its end leaves by.
    fn chain(&mut self, steps: &[Step], mut from: String) -> String {
        for step in steps {
            let id = self.next_id();
            match step {
                Step::Filter { predicate, keep } => {
                    self.split(&id, predicate, &from);
                    let (kept, dropped) = if *keep { ("true", "false") } else { ("false", "true") };
                    let drop = format!("{}_d", id);
                    self.nodes.push(node(&drop, names::DROP, json!({})));
                    self.connect(&format!("{}.{}", id, dropped), format!("{}.in", drop));
                    from = format!("{}.{}", id, kept);
                }
                Step::Kernel(kind) => {
                    self.nodes.push(kernel_node(&id, *kind));
                    self.connect(&from, format!("{}.in", id));
                    from = format!("{}.out", id);
                }
            }
        }
        from
    }
}

fn fingerprint(output: &str) -> PlanHash {
    PlanHash::of_bytes(output.as_bytes())
}

/// The catalog random plans validate against: outputs `o0` to `o127`, each with its own recipient, and the live
/// bindings of [`arb_configuration_leaf`].
pub(super) fn catalog() -> MemoryCatalog {
    let catalog = BOUND_FLAGS.iter().fold(MemoryCatalog::new(), |catalog, (name, _)| {
        catalog.with_binding(binding(name), ParamType::Bool, Liveness::Live)
    });
    let catalog = BOUND_LISTS.iter().fold(catalog, |catalog, (name, _)| {
        catalog.with_binding(binding(name), ParamType::List(&ParamType::String), Liveness::Live)
    });
    (0..128).fold(catalog, |catalog, index| {
        let output = format!("o{}", index);
        catalog.with_output(
            OutputId::new(output.as_str()).expect("valid output ID"),
            CatalogOutput {
                accepts: PortType::ANY,
                fingerprint: fingerprint(&output),
                recipients: vec![RecipientId::new(format!("out/{}", output)).expect("valid recipient ID")],
                legacy_fingerprint: None,
            },
        )
    })
}

/// Builds the plan of a shape, fed by two sources that together admit every kind: DogStatsD, with client timestamps,
/// admits every kind but rates, and OTLP admits rates. Returns the plan and its opaque hosts.
pub(super) fn plan_of(shape: &Shape) -> (Plan, Vec<NodeId>) {
    let mut builder = Builder {
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
        ..Builder::default()
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
            .expect("random plans are well formed");
    let opaque = builder
        .opaque
        .iter()
        .map(|id| id.parse().expect("valid node ID"))
        .collect();
    (plan, opaque)
}

/// Names a slot after the first of its boot nodes.
fn namer(nodes: &[NodeId]) -> Result<NativeComponentId, String> {
    NativeComponentId::new(format!("region_{}", nodes[0].local())).map_err(|error| error.to_string())
}

/// An opaque host that forwards everything it admits unchanged.
struct EchoHost;

impl OpaqueHostAdapter for EchoHost {
    fn admit(&mut self, batch: HostBatch, output: &mut HostOutput) {
        for metric in batch.events {
            output.push(saluki_core::execution::PortIndex::PRIMARY, metric);
        }
    }
}

fn echo(_operator: &ConfiguredOperator) -> Result<Box<dyn OpaqueHostAdapter>, AdapterError> {
    Ok(Box::new(EchoHost))
}

/// A random plan as boot sees it: validated, with its frame.
pub(super) struct Case {
    pub plan: Plan,
    pub report: ValidationReport,
    pub frame: Frame,
    pub registry: OperatorRegistry,
    catalog: MemoryCatalog,
    opaque: Vec<NodeId>,
}

impl Case {
    /// Builds the plan of a shape, validates it, and computes its frame.
    ///
    /// Fails if the plan is invalid or has no frame, so that a property never holds vacuously.
    pub fn new(shape: &Shape) -> Result<Self, TestCaseError> {
        let (plan, opaque) = plan_of(shape);
        Self::from_plan(plan, opaque)
    }

    /// Validates a plan against the random plans' [catalog], with `opaque` its opaque hosts, and computes its frame.
    pub fn from_plan(plan: Plan, opaque: Vec<NodeId>) -> Result<Self, TestCaseError> {
        let registry = OperatorRegistry::native();
        let catalog = catalog();
        let report = Validator::new(&registry, &catalog).validate(&plan);
        prop_assert!(report.is_valid(), "the generated plan is invalid:\n{}", report);
        let frame = Frame::compute(&plan, &registry, &namer)
            .map_err(|error| TestCaseError::fail(format!("the generated plan has no frame: {}", error)))?;
        Ok(Self {
            plan,
            report,
            frame,
            registry,
            catalog,
            opaque,
        })
    }

    /// Runs `rewrite` alone to a fixed point over the plan, with the pipeline's loop and checks, against the plan's
    /// own frame.
    pub fn optimize(&self, rewrite: &dyn Rewrite, profile: Option<&OperandProfile>) -> Optimized {
        let ctx = RewriteContext::new(&self.registry, &self.frame, &self.report);
        let ctx = match profile {
            Some(profile) => ctx.with_profile(profile),
            None => ctx,
        };
        run_pipeline(&self.plan, &ctx, &[rewrite])
    }

    /// Runs a trace in the reference interpreter, with every opaque host forwarding what it admits.
    pub fn run(&self, trace: &Trace) -> Result<RunOutput, TestCaseError> {
        let interpreter = self
            .opaque
            .iter()
            .fold(Interpreter::new(&self.registry, &self.catalog), |interpreter, host| {
                interpreter.with_opaque_host(host.clone(), echo)
            });
        interpreter
            .run(trace)
            .map_err(|error| TestCaseError::fail(format!("the interpreter failed: {}", error)))
    }
}

/// One batch of a trace: its source, and its metrics.
pub(super) type Batch = (&'static str, Vec<Metric>);

/// Returns a strategy for the batches of a trace: random metrics, cut into batches at random points, each batch's rates
/// for the OTLP source and everything else for the DogStatsD source, so that every record is one its source can
/// produce.
pub(super) fn arb_batches() -> impl Strategy<Value = Vec<Batch>> {
    (vec(arb_metric(), 0..40), vec(any::<Index>(), 0..4)).prop_map(|(metrics, cuts)| {
        let mut cuts = cuts.iter().map(|cut| cut.index(metrics.len() + 1)).collect::<Vec<_>>();
        cuts.sort_unstable();
        let mut batches = Vec::new();
        let mut start = 0;
        for end in cuts.into_iter().chain([metrics.len()]) {
            let (rates, others): (Vec<_>, Vec<_>) = metrics[start..end]
                .iter()
                .cloned()
                .partition(|metric| Kind::of(metric.values()) == Kind::Rate);
            batches.push(("dsd", others));
            batches.push(("otlp", rates));
            start = end;
        }
        batches
    })
}

/// The admission time of the batch at `position` in a trace: each batch has its own, so that the records an output
/// receives can be told apart by the batch they came from.
fn admitted_at(position: usize) -> AdmissionTime {
    AdmissionTime::from_unix_secs(1_000 + position as u64)
}

/// A `capacity` event: before the batch at `position`, `output` accepts at most `limit` more records.
#[derive(Clone, Debug)]
pub(super) struct Bound {
    pub position: usize,
    pub output: OutputId,
    pub limit: u64,
}

/// Returns a trace that installs `plan`, then admits every batch, with each bound before its batch.
pub(super) fn trace(plan: &Plan, batches: &[Batch], bounds: &[Bound]) -> Trace {
    let mut trace = Trace::new().install(plan.clone());
    for (position, (source, metrics)) in batches.iter().enumerate() {
        for bound in bounds.iter().filter(|bound| bound.position == position) {
            trace = trace.capacity(bound.output.clone(), bound.limit);
        }
        let source = source.parse().expect("valid node ID");
        trace = trace.admit(source, admitted_at(position), metrics.clone());
    }
    trace
}

/// What a bound strategy picks: an output, the batch the bound comes before, and how many batches it lasts (or, for
/// arbitrary bounds, its limit in records).
pub(super) type BoundChoice = (Index, Index, u8);

/// Returns a strategy for up to six bound choices.
pub(super) fn arb_bounds() -> impl Strategy<Value = Vec<BoundChoice>> {
    vec((any::<Index>(), any::<Index>(), 0u8..4), 0..6)
}

/// Returns bounds that each last a whole number of batches, as the plan's run without bounds delivered them: before
/// batch `i`, a bound of `span` batches on output `o` is the number of records `o` received from batches `i` to `i +
/// span - 1`. Every batch's records at an output are then either all accepted or all refused, whatever the order in
/// which they reach it, so two plans that deliver the same records from every batch deliver and refuse the same.
pub(super) fn batch_aligned_bounds(
    plan: &Plan, unbounded: &RunOutput, batches: &[Batch], choices: &[BoundChoice],
) -> Vec<Bound> {
    if plan.outputs.is_empty() || batches.is_empty() {
        return Vec::new();
    }
    let mut received = BTreeMap::<(&OutputId, AdmissionTime), u64>::new();
    for (output, records) in unbounded.outputs() {
        for record in records {
            if let Some(at) = record.admitted_at {
                *received.entry((output, at)).or_default() += 1;
            }
        }
    }
    choices
        .iter()
        .map(|(output, position, span)| {
            let output = &plan.outputs[output.index(plan.outputs.len())].id;
            let position = position.index(batches.len());
            let limit = (position..(position + usize::from(*span)).min(batches.len()))
                .map(|batch| received.get(&(output, admitted_at(batch))).copied().unwrap_or(0))
                .sum();
            Bound {
                position,
                output: output.clone(),
                limit,
            }
        })
        .collect()
}

/// Returns bounds with arbitrary limits, of 0 to 3 records, which can cut a batch's records at an output anywhere.
pub(super) fn arbitrary_bounds(plan: &Plan, batches: &[Batch], choices: &[BoundChoice]) -> Vec<Bound> {
    if plan.outputs.is_empty() || batches.is_empty() {
        return Vec::new();
    }
    choices
        .iter()
        .map(|(output, position, limit)| Bound {
            position: position.index(batches.len()),
            output: plan.outputs[output.index(plan.outputs.len())].id.clone(),
            limit: u64::from(*limit),
        })
        .collect()
}

/// Checks that `rewritten` delivers what `original` delivers, over `batches`, in the reference interpreter.
pub(super) fn check_equivalent(
    case: &Case, original: &Plan, rewritten: &Plan, batches: &[Batch],
) -> Result<(RunOutput, RunOutput), TestCaseError> {
    let before = case.run(&trace(original, batches, &[]))?;
    let after = case.run(&trace(rewritten, batches, &[]))?;
    if let Err(mismatch) = compare(&OutputPoints::from_run(&before), &OutputPoints::from_run(&after)) {
        prop_assert!(false, "the rewritten plan delivers differently:\n{}", mismatch);
    }
    Ok((before, after))
}

/// Checks that every output of `plan` received the same records in both runs, in the same order.
///
/// Equivalence compares multisets; this is stronger, and what makes a capacity bound, which counts records in delivery
/// order, cut the same records from both plans whatever its limit.
pub(super) fn check_same_order(plan: &Plan, before: &RunOutput, after: &RunOutput) -> Result<(), TestCaseError> {
    for output in &plan.outputs {
        prop_assert_eq!(
            before.output(&output.id),
            after.output(&output.id),
            "output `{}` receives its records in another order",
            output.id
        );
    }
    Ok(())
}

/// Checks the rewritten plan against the original under the capacity `bounds`: both deliver and refuse exactly the
/// same points, and the specification's capacity rule holds.
///
/// Bounds count records in the interpreter's delivery order, which follows from the plan's dataflow alone, so this
/// holds for every rewrite that keeps each remaining port's records in order, whatever the bounds cut.
pub(super) fn check_under_capacity(
    case: &Case, original: &Plan, rewritten: &Plan, batches: &[Batch], bounds: &[Bound],
) -> Result<(), TestCaseError> {
    let policy = case.run(&trace(original, batches, &[]))?;
    let before = case.run(&trace(original, batches, bounds))?;
    let after = case.run(&trace(rewritten, batches, bounds))?;
    let (policy, before, after) = (
        OutputPoints::from_run(&policy),
        CapacityOutcome::from_run(&before),
        CapacityOutcome::from_run(&after),
    );
    if let Err(mismatch) = compare(&before.delivered, &after.delivered) {
        prop_assert!(
            false,
            "under capacity, the rewritten plan delivers differently:\n{}",
            mismatch
        );
    }
    if let Err(mismatch) = compare(&before.refused, &after.refused) {
        prop_assert!(
            false,
            "under capacity, the rewritten plan refuses differently:\n{}",
            mismatch
        );
    }
    if let Err(mismatch) = compare_under_capacity(&policy, &before, &after) {
        prop_assert!(false, "the rewritten plan breaks the capacity rule:\n{}", mismatch);
    }
    Ok(())
}
