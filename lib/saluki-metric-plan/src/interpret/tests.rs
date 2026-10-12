//! Hand-written interpreter fixtures: small plans and traces, each with the outputs the specification requires,
//! written down independently of the interpreter.

use std::sync::Arc;

use proptest::{collection::vec, prelude::*, sample::Index};
use saluki_core::{
    data_model::event::metric::{context::Context, Metric, MetricValues, SetPoints},
    execution::{AdmissionTime, GenerationId, LossCount, PortIndex},
};
use serde_json::{json, Value};
use stringtheory::MetaString;

use super::{test_operators::test_registry, *};
use crate::{
    canonical::PlanHash,
    equivalence::{compare, OutputPoints},
    ir::{Plan, RecipientId},
    model::{Kind, PortType},
    predicate::Predicate,
    testing::{arb_metric, arb_predicate, predicate_json},
    validate::{CatalogOutput, MemoryCatalog, Validator},
};

// Helpers.

fn t(secs: u64) -> AdmissionTime {
    AdmissionTime::from_unix_secs(secs)
}

fn id(value: &str) -> NodeId {
    value.parse().unwrap()
}

fn out(value: &str) -> OutputId {
    OutputId::new(value).unwrap()
}

fn fingerprint(output: &str) -> PlanHash {
    PlanHash::of_bytes(output.as_bytes())
}

/// The outputs the test catalog declares, besides `o0` to `o127` for generated plans. Each has its own recipient.
const OUTPUTS: &[&str] = &["a", "b", "c", "prod_counts", "other_counts", "named", "hostless"];

/// The catalog the test plans validate against: the outputs above, the binding `test.names`, a live list of
/// strings, and the binding `test.profiles`, startup-only mapper profiles.
fn test_catalog() -> MemoryCatalog {
    let generated = (0..128).map(|i| format!("o{}", i)).collect::<Vec<_>>();
    let mut catalog = MemoryCatalog::new()
        .with_binding(
            "test.names".parse().unwrap(),
            crate::ir::ParamType::List(&crate::ir::ParamType::String),
            crate::validate::Liveness::Live,
        )
        .with_binding(
            "test.profiles".parse().unwrap(),
            crate::operators::MAPPER_PROFILES,
            crate::validate::Liveness::Startup,
        );
    for output in OUTPUTS.iter().copied().chain(generated.iter().map(String::as_str)) {
        catalog = catalog.with_output(
            out(output),
            CatalogOutput {
                accepts: PortType::ANY,
                fingerprint: fingerprint(output),
                recipients: vec![RecipientId::new(format!("out/{}", output)).unwrap()],
                legacy_fingerprint: None,
            },
        );
    }
    catalog
}

/// A node with native provenance, so that lossy and host nodes have authority.
fn node(id: &str, op: &str, params: Value) -> Value {
    json!({"id": id, "op": op, "params": params, "provenance": {"native": "test"}})
}

fn source(id: &str, binding: &str) -> Value {
    node(id, "source", json!({"binding": binding}))
}

fn emit(id: &str, output: &str) -> Value {
    node(id, "emit", json!({"output": output}))
}

fn split(id: &str, predicate: Value) -> Value {
    node(id, "split", json!({"predicate": predicate}))
}

fn opaque(id: &str) -> Value {
    let any = json!({"kinds": "all", "timestamps": "mixed"});
    node(
        id,
        "opaque_host",
        json!({"component": id, "input": any, "outputs": {"out": any}}),
    )
}

/// Builds a plan from nodes, edges written as `from.port -> to.port`, and the outputs it declares.
fn plan(nodes: Vec<Value>, edges: &[&str], outputs: &[&str]) -> Plan {
    let edges = edges
        .iter()
        .map(|edge| {
            let (from, to) = edge.split_once(" -> ").unwrap();
            json!({"from": from, "to": to})
        })
        .collect::<Vec<_>>();
    let outputs = outputs
        .iter()
        .map(|output| {
            json!({"id": output, "accepts": {"kinds": "all", "timestamps": "mixed"},
                   "fingerprint": fingerprint(output).to_string(), "recipients": [format!("out/{}", output)]})
        })
        .collect::<Vec<_>>();
    let plan = Plan::from_value(json!({"ir_version": 1, "nodes": nodes, "edges": edges, "outputs": outputs})).unwrap();
    let registry = test_registry();
    let report = Validator::new(&registry, &test_catalog()).validate(&plan);
    assert!(report.is_valid(), "test plan is invalid:\n{}", report);
    plan
}

fn gauge(name: &'static str) -> Metric {
    Metric::gauge(name, 1.0)
}

fn counter(name: &'static str) -> Metric {
    Metric::counter(name, 1.0)
}

fn with_tags(metric: Metric, tags: &[&'static str]) -> Metric {
    let (context, values, metadata) = metric.into_parts();
    let context = Context::from_static_parts("placeholder", tags).with_name(context.name().clone());
    Metric::from_parts(context, values, metadata)
}

fn with_host(metric: Metric, host: &'static str) -> Metric {
    let (context, values, metadata) = metric.into_parts();
    Metric::from_parts(context.with_host(MetaString::from_static(host)), values, metadata)
}

/// Runs a trace with the test registry and catalog, and no opaque hosts.
fn execute(trace: Trace) -> RunOutput {
    let registry = test_registry();
    let catalog = test_catalog();
    let output = Interpreter::new(&registry, &catalog).run(&trace).unwrap();
    output
}

/// Runs a trace that must fail, and returns the event it failed at and the error's message.
fn run_error(interpreter: &Interpreter<'_>, trace: Trace) -> (Option<usize>, String) {
    let error = interpreter.run(&trace).expect_err("the trace must fail");
    (error.event, error.kind.to_string())
}

/// Returns the names an output received, in delivery order.
fn names(run: &RunOutput, output: &str) -> Vec<String> {
    run.output(&out(output))
        .iter()
        .map(|record| record.metric.context().name().to_string())
        .collect()
}

fn strings(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| value.to_string()).collect()
}

/// A test adapter that emits every batch it admits at once, unchanged.
struct EchoHost;

impl OpaqueHostAdapter for EchoHost {
    fn admit(&mut self, batch: HostBatch, output: &mut HostOutput) {
        for metric in batch.events {
            output.push(PortIndex::PRIMARY, metric);
        }
    }
}

/// A test adapter that holds what it admits and emits it at the next tick, followed by a gauge `host.watermark` whose
/// value is the tick's watermark in seconds, or `-1` without one. It discards what it holds when it finishes, as native
/// aggregation discards its open buckets at shutdown, and reports the discarded records as shutdown loss.
#[derive(Default)]
struct HoldingHost {
    held: Vec<Metric>,
}

impl OpaqueHostAdapter for HoldingHost {
    fn admit(&mut self, batch: HostBatch, _output: &mut HostOutput) {
        self.held.extend(batch.events);
    }

    fn tick(&mut self, tick: HostTick, output: &mut HostOutput) {
        let watermark = tick.watermark.map_or(-1.0, |watermark| watermark.as_unix_secs() as f64);
        let emitted = self.held.drain(..).chain([Metric::gauge("host.watermark", watermark)]);
        for metric in emitted {
            output.push(PortIndex::PRIMARY, metric);
        }
    }

    fn finish(&mut self, _finish: HostFinish, output: &mut HostOutput) {
        let points = self.held.iter().map(|metric| metric.values().len() as u64).sum();
        output.record_loss(LossClass::Shutdown, self.held.len() as u64, points);
        self.held.clear();
    }
}

fn echo(_: &ConfiguredOperator) -> Result<Box<dyn OpaqueHostAdapter>, AdapterError> {
    Ok(Box::new(EchoHost))
}

fn holding(_: &ConfiguredOperator) -> Result<Box<dyn OpaqueHostAdapter>, AdapterError> {
    Ok(Box::<HoldingHost>::default())
}

/// Returns the watermark values an output received from a [`HoldingHost`], in order.
fn watermarks(run: &RunOutput, output: &str) -> Vec<f64> {
    run.output(&out(output))
        .iter()
        .filter(|record| record.metric.context().name() == "host.watermark")
        .map(|record| match record.metric.values() {
            MetricValues::Gauge(points) => points.into_iter().next().map_or(f64::NAN, |(_, value)| value),
            _ => f64::NAN,
        })
        .collect()
}

// Routing.

#[test]
fn split_sends_each_record_to_exactly_one_port() {
    let plan = plan(
        vec![
            source("src", "otlp"),
            split("s", json!({"name": {"prefix": "a."}})),
            emit("ea", "a"),
            emit("eb", "b"),
        ],
        &["src.metrics -> s.in", "s.true -> ea.in", "s.false -> eb.in"],
        &["a", "b"],
    );
    let run = execute(Trace::new().install(plan).admit(
        id("src"),
        t(100),
        vec![gauge("a.x"), gauge("b.y"), gauge("a.z"), gauge("ab")],
    ));

    assert_eq!(names(&run, "a"), strings(&["a.x", "a.z"]));
    assert_eq!(names(&run, "b"), strings(&["b.y", "ab"]));
}

#[test]
fn tee_sends_each_record_to_every_port() {
    let plan = plan(
        vec![
            source("src", "otlp"),
            node("t", "tee", json!({"ports": 3})),
            emit("e0", "a"),
            emit("e1", "b"),
            emit("e2", "c"),
        ],
        &["src.metrics -> t.in", "t.0 -> e0.in", "t.1 -> e1.in", "t.2 -> e2.in"],
        &["a", "b", "c"],
    );
    let run = execute(
        Trace::new()
            .install(plan)
            .admit(id("src"), t(100), vec![gauge("x"), counter("y")]),
    );

    for output in ["a", "b", "c"] {
        assert_eq!(names(&run, output), strings(&["x", "y"]), "{}", output);
    }
}

#[test]
fn merge_interleaves_its_inputs() {
    // Two sources into one merge: the output receives both, in the order the trace admits them.
    let two_sources = plan(
        vec![
            source("s1", "otlp"),
            source("s2", "checks"),
            node("m", "merge", json!({"inputs": 2})),
            emit("e", "a"),
        ],
        &["s1.metrics -> m.0", "s2.metrics -> m.1", "m.out -> e.in"],
        &["a"],
    );
    let run = execute(
        Trace::new()
            .install(two_sources)
            .admit(id("s1"), t(100), vec![gauge("x1")])
            .admit(id("s2"), t(100), vec![gauge("y1")])
            .admit(id("s1"), t(101), vec![gauge("x2")]),
    );
    assert_eq!(names(&run, "a"), strings(&["x1", "y1", "x2"]));

    // The two ports of one split rejoin: every record arrives once, the merge's first input's records first.
    let rejoin = plan(
        vec![
            source("src", "otlp"),
            split("s", json!({"name": {"prefix": "a"}})),
            node("m", "merge", json!({"inputs": 2})),
            emit("e", "a"),
        ],
        &[
            "src.metrics -> s.in",
            "s.true -> m.0",
            "s.false -> m.1",
            "m.out -> e.in",
        ],
        &["a"],
    );
    let run = execute(Trace::new().install(rejoin).admit(
        id("src"),
        t(100),
        vec![gauge("a1"), gauge("b1"), gauge("a2")],
    ));
    assert_eq!(names(&run, "a"), strings(&["a1", "a2", "b1"]));
}

#[test]
fn a_merge_passes_its_inputs_on_in_port_order_whatever_the_lengths_of_their_paths() {
    // The split's `true` port goes straight to the merge's second input, its `false` port through two kernels to the
    // first: the merge passes on its first input's records, then its second's, each in batch order. Neither the port
    // the split writes first nor the shorter path decides.
    let plan = plan(
        vec![
            source("src", "otlp"),
            split("s", json!({"name": {"prefix": "a"}})),
            node("k1", "test_prefix", json!({"prefix": "x."})),
            node("k2", "test_prefix", json!({"prefix": "y."})),
            node("m", "merge", json!({"inputs": 2})),
            emit("e", "a"),
        ],
        &[
            "src.metrics -> s.in",
            "s.true -> m.1",
            "s.false -> k1.in",
            "k1.out -> k2.in",
            "k2.out -> m.0",
            "m.out -> e.in",
        ],
        &["a"],
    );
    let run = execute(Trace::new().install(plan).admit(
        id("src"),
        t(100),
        vec![gauge("a1"), gauge("b1"), gauge("a2"), gauge("b2")],
    ));
    assert_eq!(names(&run, "a"), strings(&["y.x.b1", "y.x.b2", "a1", "a2"]));
}

#[test]
fn records_from_several_emits_reach_one_output_in_the_plan_order_of_the_emits() {
    // `first` is first in the plan, below the split's `false` port and a kernel; `second` takes the `true` port
    // straight away.
    let plan = plan(
        vec![
            source("src", "otlp"),
            split("s", json!({"name": {"prefix": "a"}})),
            emit("first", "a"),
            node("k", "test_prefix", json!({"prefix": "x."})),
            emit("second", "a"),
        ],
        &[
            "src.metrics -> s.in",
            "s.true -> second.in",
            "s.false -> k.in",
            "k.out -> first.in",
        ],
        &["a"],
    );
    let run = execute(
        Trace::new()
            .install(plan)
            .admit(id("src"), t(100), vec![gauge("a1"), gauge("b1"), gauge("a2")]),
    );
    assert_eq!(names(&run, "a"), strings(&["x.b1", "a1", "a2"]));
}

#[test]
fn time_driven_output_of_several_state_owners_reaches_a_merge_in_port_order() {
    // Both state owners tick in one pass: `early`, first in the plan, feeds the merge's second input, and `late`,
    // through a kernel, its first.
    let plan = plan(
        vec![
            source("src", "otlp"),
            split("s", json!({"name": {"prefix": "a"}})),
            node("early", "test_hold", json!({})),
            node("late", "test_hold", json!({})),
            node("k", "test_prefix", json!({"prefix": "x."})),
            node("m", "merge", json!({"inputs": 2})),
            emit("e", "a"),
        ],
        &[
            "src.metrics -> s.in",
            "s.true -> early.in",
            "s.false -> late.in",
            "late.out -> k.in",
            "k.out -> m.0",
            "early.out -> m.1",
            "m.out -> e.in",
        ],
        &["a"],
    );
    let run = execute(
        Trace::new()
            .install(plan)
            .admit(id("src"), t(100), vec![gauge("a1"), gauge("b1"), gauge("a2")])
            .advance(t(110)),
    );
    assert_eq!(names(&run, "a"), strings(&["x.b1", "a1", "a2"]));
}

#[test]
fn drop_discards_records_as_policy_loss() {
    let plan = plan(
        vec![
            source("src", "otlp"),
            split("s", json!({"kind_in": ["counter"]})),
            node("d", "drop", json!({})),
            emit("e", "a"),
        ],
        &["src.metrics -> s.in", "s.true -> d.in", "s.false -> e.in"],
        &["a"],
    );
    let two_points = Metric::counter("c", [(10, 1.0), (20, 2.0)]);
    let run = execute(
        Trace::new()
            .install(plan)
            .admit(id("src"), t(100), vec![two_points, gauge("g")]),
    );

    assert_eq!(names(&run, "a"), strings(&["g"]));
    let loss = run.loss(&id("d"));
    assert_eq!(loss.loss(LossClass::Policy), LossCount { events: 1, points: 2 });
    for class in [LossClass::Capacity, LossClass::Invalid, LossClass::Shutdown] {
        assert_eq!(loss.loss(class), LossCount::default(), "{:?}", class);
    }
    assert!(run.loss(&id("e")).is_empty());
}

#[test]
fn capacity_bounds_what_an_output_accepts_until_the_next_bound_and_across_installations() {
    let plan = plan(
        vec![
            source("src", "otlp"),
            split("s", json!({"kind_in": ["gauge"]})),
            emit("ga", "a"),
            emit("gb", "b"),
        ],
        &["src.metrics -> s.in", "s.true -> ga.in", "s.false -> gb.in"],
        &["a", "b"],
    );
    let two_points = Metric::counter("c", [(10, 1.0), (20, 2.0)]);
    let run = execute(
        Trace::new()
            .capacity(out("a"), 3)
            .install(plan.clone())
            .admit(id("src"), t(100), vec![gauge("g1"), gauge("g2")])
            // The bound lasts across installations: one record of the next batch is accepted, the rest refused.
            .install(plan.clone())
            .admit(id("src"), t(101), vec![gauge("g3"), gauge("g4"), two_points.clone()])
            .admit(id("src"), t(102), vec![gauge("g5")])
            // A new bound replaces what the last one left.
            .capacity(out("a"), 1)
            .capacity(out("b"), 0)
            .admit(id("src"), t(103), vec![gauge("g6"), gauge("g7"), two_points]),
    );

    assert_eq!(names(&run, "a"), strings(&["g1", "g2", "g3", "g6"]));
    let refused = |output: &str| {
        run.refused(&out(output))
            .iter()
            .map(|record| record.metric.context().name().to_string())
            .collect::<Vec<_>>()
    };
    assert_eq!(refused("a"), strings(&["g4", "g5", "g7"]));
    // An output never bounded accepts everything, until a bound of zero refuses everything.
    assert_eq!(names(&run, "b"), strings(&["c"]));
    assert_eq!(refused("b"), strings(&["c"]));
    assert_eq!(
        run.refused_outputs()
            .map(|(output, _)| output.to_string())
            .collect::<Vec<_>>(),
        strings(&["a", "b"])
    );

    // Refused records are capacity loss of the `emit` that delivered them, in events and points.
    let capacity = |node: &str| run.loss(&id(node)).loss(LossClass::Capacity);
    assert_eq!(capacity("ga"), LossCount { events: 3, points: 3 });
    assert_eq!(capacity("gb"), LossCount { events: 1, points: 2 });
}

#[test]
fn capacity_bounds_an_output_across_installations_and_counts_what_it_refuses() {
    let two_emits = plan(
        vec![
            source("src", "otlp"),
            split("s", json!({"name": {"prefix": "a."}})),
            emit("ea", "a"),
            emit("eb", "a"),
        ],
        &["src.metrics -> s.in", "s.true -> ea.in", "s.false -> eb.in"],
        &["a"],
    );
    let one_emit = plan(
        vec![source("src", "otlp"), emit("e", "a")],
        &["src.metrics -> e.in"],
        &["a"],
    );
    let points = |name: &'static str, count: usize| Metric::counter(name, &[1.0, 2.0, 3.0][..count]);

    // (what the step does, the step, the names `a` has accepted and refused after it, in delivery order)
    type Step = Box<dyn Fn(Trace) -> Trace>;
    let steps: Vec<(&str, Step, &[&str], &[&str])> = vec![
        (
            "bound `a` to two records, before any plan declares it",
            Box::new(|trace| trace.capacity(out("a"), 2)),
            &[],
            &[],
        ),
        (
            "install a plan with two emits to `a`, which share its bound",
            Box::new(move |trace| trace.install(two_emits.clone())),
            &[],
            &[],
        ),
        (
            "admit three records: the split's `true` port delivers first, and `b.1` finds the bound used up",
            Box::new(move |trace| {
                trace.admit(
                    id("src"),
                    t(100),
                    vec![points("a.1", 1), points("b.1", 2), points("a.2", 1)],
                )
            }),
            &["a.1", "a.2"],
            &["b.1"],
        ),
        (
            "install a plan with one emit to `a`: the bound belongs to the output, so it holds",
            Box::new(move |trace| trace.install(one_emit.clone())),
            &["a.1", "a.2"],
            &["b.1"],
        ),
        (
            "admit one record",
            Box::new(move |trace| trace.admit(id("src"), t(101), vec![points("c.1", 1)])),
            &["a.1", "a.2"],
            &["b.1", "c.1"],
        ),
        (
            "bound `a` again, to one record: the new bound replaces what was left",
            Box::new(|trace| trace.capacity(out("a"), 1)),
            &["a.1", "a.2"],
            &["b.1", "c.1"],
        ),
        (
            "admit two records",
            Box::new(move |trace| trace.admit(id("src"), t(102), vec![points("d.1", 1), points("d.2", 3)])),
            &["a.1", "a.2", "d.1"],
            &["b.1", "c.1", "d.2"],
        ),
    ];

    let refused = |run: &RunOutput| {
        run.refused(&out("a"))
            .iter()
            .map(|record| record.metric.context().name().to_string())
            .collect::<Vec<_>>()
    };
    let mut trace = Trace::new();
    for (what, step, accepted, refusals) in &steps {
        trace = step(trace);
        let run = execute(trace.clone());
        assert_eq!(names(&run, "a"), strings(accepted), "{}", what);
        assert_eq!(refused(&run), strings(refusals), "{}", what);
    }
    let run = execute(trace);

    // Each refused record is the capacity loss of the `emit` that delivered it, with its points; nothing else is lost.
    for (node, events, points) in [("ea", 0, 0), ("eb", 1, 2), ("e", 2, 4)] {
        let loss = run.loss(&id(node));
        assert_eq!(loss.loss(LossClass::Capacity), LossCount { events, points }, "{}", node);
        assert_eq!(loss.loss(LossClass::Policy), LossCount::default(), "{}", node);
    }
    // A refused record carries the installation that delivered it, as an accepted one does.
    let installs = run
        .refused(&out("a"))
        .iter()
        .map(|record| record.install)
        .collect::<Vec<_>>();
    assert_eq!(installs, vec![1, 3, 3]);
}

#[test]
fn emit_records_carry_their_generation_installation_and_admission_time() {
    let plan = plan(
        vec![source("src", "otlp"), emit("e", "a")],
        &["src.metrics -> e.in"],
        &["a"],
    );
    let run = execute(
        Trace::new()
            .advance(t(50))
            .install(plan)
            .admit(id("src"), t(100), vec![gauge("x")]),
    );

    let records = run.output(&out("a"));
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].generation, GenerationId::new(1));
    assert_eq!(records[0].install, 1);
    assert_eq!(records[0].admitted_at, Some(t(100)));
    assert_eq!(records[0].metric, gauge("x"));
    // Declared outputs are listed even when nothing reached them.
    assert_eq!(run.outputs().count(), 1);
    assert!(run.output(&out("b")).is_empty());
}

#[test]
fn predicates_route_by_name_tags_host_and_kind() {
    // by_kind: counters and rates -> by_tag (env is prod -> prod_counts, otherwise other_counts); other kinds ->
    // by_intake (intake name `my_metric` or `app.requests` -> named) -> by_host (unset host -> hostless, else drop).
    // The source is DogStatsD, which admits every kind but rates.
    let plan = plan(
        vec![
            node(
                "src",
                "source",
                json!({"binding": "dogstatsd", "timestamps": "untimestamped"}),
            ),
            split("by_kind", json!({"kind_in": ["counter", "rate"]})),
            split("by_tag", json!({"tag": {"key": "env", "value": {"eq": "prod"}}})),
            split(
                "by_intake",
                json!({"intake_name": {"in": ["my_metric", "app.requests"]}}),
            ),
            split("by_host", json!({"host_unset": {}})),
            emit("prod", "prod_counts"),
            emit("other", "other_counts"),
            emit("named", "named"),
            emit("hostless", "hostless"),
            node("dropped", "drop", json!({})),
        ],
        &[
            "src.metrics -> by_kind.in",
            "by_kind.true -> by_tag.in",
            "by_kind.false -> by_intake.in",
            "by_tag.true -> prod.in",
            "by_tag.false -> other.in",
            "by_intake.true -> named.in",
            "by_intake.false -> by_host.in",
            "by_host.true -> hostless.in",
            "by_host.false -> dropped.in",
        ],
        &["prod_counts", "other_counts", "named", "hostless"],
    );
    let batch = vec![
        with_tags(counter("c.prod"), &["env:prod"]),
        with_tags(counter("c.staging"), &["env:staging"]),
        // A bare `env` tag has no value, so `tag(env)` never matches it.
        with_tags(counter("c.bare"), &["env"]),
        with_host(gauge("my metric"), "web"),
        gauge("app.requests"),
        // Normalizing drops the leading digit, so the intake name is `app.requests`.
        gauge("1app.requests"),
        Metric::set("unnamed.set", SetPoints::from("member")),
        // An empty host is a host.
        with_host(gauge("unnamed.empty_host"), ""),
        // A name without an ASCII letter has no intake name, so no `intake_name` leaf matches it.
        Metric::distribution("🍣", 1.0),
        // `tag` matches if any tag with the key matches.
        with_tags(counter("c.both"), &["env:dev", "env:prod"]),
    ];
    let run = execute(Trace::new().install(plan).admit(id("src"), t(100), batch));

    assert_eq!(names(&run, "prod_counts"), strings(&["c.prod", "c.both"]));
    assert_eq!(names(&run, "other_counts"), strings(&["c.staging", "c.bare"]));
    assert_eq!(
        names(&run, "named"),
        strings(&["my metric", "app.requests", "1app.requests"])
    );
    assert_eq!(names(&run, "hostless"), strings(&["unnamed.set", "🍣"]));
    assert_eq!(run.loss(&id("dropped")).loss(LossClass::Policy).events, 1);
}

#[test]
fn split_predicates_use_the_plans_resolved_values() {
    let plan = plan(
        vec![
            source("src", "otlp"),
            split(
                "s",
                json!({"name": {"matches": {"values": {"config": "test.names", "resolved": ["x"]}, "prefix": false}}}),
            ),
            emit("ea", "a"),
            emit("eb", "b"),
        ],
        &["src.metrics -> s.in", "s.true -> ea.in", "s.false -> eb.in"],
        &["a", "b"],
    );
    let registry = test_registry();
    let catalog = test_catalog();
    let interpreter = Interpreter::new(&registry, &catalog);
    let run = interpreter
        .run(
            &Trace::new()
                .install(plan.clone())
                .admit(id("src"), t(1), vec![gauge("x"), gauge("y")]),
        )
        .unwrap();
    assert_eq!(names(&run, "a"), strings(&["x"]));
    assert_eq!(names(&run, "b"), strings(&["y"]));

    // A binding without a resolved value, which the catalog has no value for either, cannot be evaluated, so the
    // installation is rejected.
    let mut unresolved = plan;
    unresolved.nodes[1].params.insert(
        "predicate".parse().unwrap(),
        json!({"name": {"matches": {"values": {"config": "test.names"}, "prefix": false}}}),
    );
    let run = interpreter.run(&Trace::new().install(unresolved.clone())).unwrap();
    match &run.installs()[0].outcome {
        InstallOutcome::Rejected(rejection @ InstallRejection::UnresolvedBinding { node, .. }) => {
            assert_eq!(node, &id("s"));
            assert!(
                rejection.to_string().contains("`test.names` has no value"),
                "{}",
                rejection
            );
        }
        other => panic!("expected an unresolved binding, got {:?}", other),
    }

    // A catalog that carries the binding's value, as a host's exported catalog does, previews the unresolved plan.
    let with_values = test_catalog().with_catalog_binding(
        "test.names".parse().unwrap(),
        crate::validate::CatalogBinding {
            ty: crate::ir::ValueType::from(crate::ir::ParamType::List(&crate::ir::ParamType::String)),
            liveness: crate::validate::Liveness::Live,
            value: Some(json!(["y"])),
        },
    );
    let run = Interpreter::new(&registry, &with_values)
        .run(
            &Trace::new()
                .install(unresolved)
                .admit(id("src"), t(1), vec![gauge("x"), gauge("y")]),
        )
        .unwrap();
    assert_eq!(names(&run, "a"), strings(&["y"]));
    assert_eq!(names(&run, "b"), strings(&["x"]));
}

// Opaque hosts.

#[test]
fn opaque_host_output_is_readmitted_at_the_hosts_admission_clock() {
    let plan = plan(
        vec![
            source("s1", "otlp"),
            source("s2", "checks"),
            node("m", "merge", json!({"inputs": 2})),
            opaque("h"),
            emit("e", "a"),
        ],
        &[
            "s1.metrics -> m.0",
            "s2.metrics -> m.1",
            "m.out -> h.in",
            "h.out -> e.in",
        ],
        &["a"],
    );
    let registry = test_registry();
    let catalog = test_catalog();
    let interpreter = Interpreter::new(&registry, &catalog).with_opaque_host(id("h"), echo);
    let run = interpreter
        .run(
            &Trace::new()
                .install(plan)
                .advance(t(110))
                // Stamped before the tick at 110, so it arrives at 110.
                .admit(id("s1"), t(105), vec![gauge("queued")])
                .admit(id("s1"), t(200), vec![gauge("later")])
                // Its own source stamped 150, but the host's clock has already read 200 and never moves back.
                .admit(id("s2"), t(150), vec![gauge("other_source")]),
        )
        .unwrap();

    let stamps = run
        .output(&out("a"))
        .iter()
        .map(|record| (record.metric.context().name().to_string(), record.admitted_at))
        .collect::<Vec<_>>();
    assert_eq!(
        stamps,
        vec![
            ("queued".to_string(), Some(t(110))),
            ("later".to_string(), Some(t(200))),
            ("other_source".to_string(), Some(t(200))),
        ]
    );
}

#[test]
fn opaque_host_tick_output_is_readmitted_at_the_tick() {
    let plan = plan(
        vec![source("src", "otlp"), opaque("h"), emit("e", "a")],
        &["src.metrics -> h.in", "h.out -> e.in"],
        &["a"],
    );
    let registry = test_registry();
    let catalog = test_catalog();
    let interpreter = Interpreter::new(&registry, &catalog).with_opaque_host(id("h"), holding);
    let run = interpreter
        .run(
            &Trace::new()
                .install(plan)
                .admit(id("src"), t(100), vec![gauge("m1")])
                .advance(t(110))
                .admit(id("src"), t(105), vec![gauge("m2")])
                .admit(id("src"), t(120), vec![gauge("m3")])
                .advance(t(130))
                .admit(id("src"), t(131), vec![gauge("never_flushed")])
                .close(),
        )
        .unwrap();

    let stamps = run
        .output(&out("a"))
        .iter()
        .map(|record| (record.metric.context().name().to_string(), record.admitted_at))
        .collect::<Vec<_>>();
    let at = |name: &str, secs| (name.to_string(), Some(t(secs)));
    assert_eq!(
        stamps,
        vec![
            at("m1", 110),
            at("host.watermark", 110),
            at("m2", 130),
            at("m3", 130),
            at("host.watermark", 130),
        ]
    );
    // The host's watermark is its source's latest admission time at each tick.
    assert_eq!(watermarks(&run, "a"), vec![100.0, 120.0]);
    // At shutdown, the host discards what it holds, as native aggregation discards open buckets, and the loss it
    // reports is the node's.
    assert!(!names(&run, "a").contains(&"never_flushed".to_string()));
    assert_eq!(
        run.loss(&id("h")).loss(LossClass::Shutdown),
        LossCount { events: 1, points: 1 }
    );
    assert_eq!(run.loss(&id("h")).loss(LossClass::Capacity), LossCount::default());
}

#[test]
fn watermarks_are_the_earliest_latest_admission_time_upstream() {
    let plan = plan(
        vec![
            source("s1", "otlp"),
            source("s2", "checks"),
            node("m", "merge", json!({"inputs": 2})),
            opaque("h"),
            emit("e", "a"),
        ],
        &[
            "s1.metrics -> m.0",
            "s2.metrics -> m.1",
            "m.out -> h.in",
            "h.out -> e.in",
        ],
        &["a"],
    );
    let registry = test_registry();
    let catalog = test_catalog();
    let interpreter = Interpreter::new(&registry, &catalog).with_opaque_host(id("h"), holding);
    let run = interpreter
        .run(
            &Trace::new()
                .install(plan)
                // Neither source has admitted anything.
                .advance(t(10))
                // `s2` has still admitted nothing, which holds the watermark back.
                .admit(id("s1"), t(100), vec![gauge("x")])
                .advance(t(101))
                // An idle source's heartbeat advances it: min(100, 90).
                .heartbeat(id("s2"), t(90))
                .advance(t(102))
                // min(100, 150).
                .heartbeat(id("s2"), t(150))
                .advance(t(103))
                // min(200, 150).
                .admit(id("s1"), t(200), vec![gauge("y")])
                .advance(t(104)),
        )
        .unwrap();

    assert_eq!(watermarks(&run, "a"), vec![-1.0, -1.0, 90.0, 100.0, 150.0]);
    assert_eq!(run.watermark(&id("h")), Some(t(150)));
    // Stateless nodes have no watermark.
    assert_eq!(run.watermark(&id("e")), None);
}

#[test]
fn an_opaque_host_starts_a_new_segment_for_the_watermark() {
    // `h` is fed by `e`'s output, so its watermark follows `e`'s admission clock, not the source's: at the tick at 130,
    // `e` ticks first and heartbeats at 130.
    let plan = plan(
        vec![source("src", "otlp"), opaque("e"), opaque("h"), emit("out", "a")],
        &["src.metrics -> e.in", "e.out -> h.in", "h.out -> out.in"],
        &["a"],
    );
    let registry = test_registry();
    let catalog = test_catalog();
    let interpreter = Interpreter::new(&registry, &catalog)
        .with_opaque_host(id("e"), echo)
        .with_opaque_host(id("h"), holding);
    let run = interpreter
        .run(
            &Trace::new()
                .install(plan)
                .admit(id("src"), t(100), vec![gauge("m")])
                .advance(t(130)),
        )
        .unwrap();

    assert_eq!(watermarks(&run, "a"), vec![130.0]);
    assert_eq!(names(&run, "a"), strings(&["m", "host.watermark"]));
}

// Installation.

#[test]
fn install_switches_plans_at_its_point_in_the_trace() {
    let to = |output: &str| {
        plan(
            vec![source("src", "otlp"), emit("e", output)],
            &["src.metrics -> e.in"],
            &[output],
        )
    };
    let run = execute(
        Trace::new()
            .install(to("a"))
            .admit(id("src"), t(100), vec![gauge("m1")])
            .install(to("b"))
            .admit(id("src"), t(101), vec![gauge("m2")]),
    );

    let a = run.output(&out("a"));
    assert_eq!(a.len(), 1);
    assert_eq!(
        (a[0].metric.clone(), a[0].generation, a[0].install),
        (gauge("m1"), GenerationId::new(1), 0)
    );
    let b = run.output(&out("b"));
    assert_eq!(b.len(), 1);
    assert_eq!(
        (b[0].metric.clone(), b[0].generation, b[0].install),
        (gauge("m2"), GenerationId::new(2), 2)
    );

    let outcomes = run
        .installs()
        .iter()
        .map(|record| match record.outcome {
            InstallOutcome::Active { generation } => (record.event, generation),
            InstallOutcome::Rejected(ref rejection) => panic!("rejected: {}", rejection),
            ref other => panic!("not installed at once: {:?}", other),
        })
        .collect::<Vec<_>>();
    assert_eq!(outcomes, vec![(0, GenerationId::new(1)), (2, GenerationId::new(2))]);
}

#[test]
fn an_invalid_plan_is_rejected_and_the_active_plan_stays() {
    let valid = plan(
        vec![source("src", "otlp"), emit("e", "a")],
        &["src.metrics -> e.in"],
        &["a"],
    );
    let mut invalid = valid.clone();
    invalid.edges.clear();

    let run = execute(
        Trace::new()
            .install(valid)
            .install(invalid.clone())
            .admit(id("src"), t(100), vec![gauge("m")]),
    );
    assert_eq!(names(&run, "a"), strings(&["m"]));
    assert_eq!(run.output(&out("a"))[0].generation, GenerationId::new(1));
    match &run.installs()[1].outcome {
        InstallOutcome::Rejected(InstallRejection::Invalid { report }) => assert!(!report.is_valid()),
        other => panic!("expected a rejection, got {:?}", other),
    }

    // With nothing active, an admission fails and names the rejection.
    let registry = test_registry();
    let catalog = test_catalog();
    let interpreter = Interpreter::new(&registry, &catalog);
    let (event, message) = run_error(
        &interpreter,
        Trace::new().install(invalid).admit(id("src"), t(100), vec![gauge("m")]),
    );
    assert_eq!(event, Some(1));
    assert!(
        message.contains("the last installation was rejected: the plan is invalid"),
        "{}",
        message
    );
}

#[test]
fn a_retained_host_flushes_through_the_newest_plan() {
    // The specification's "changed destination" trace: a state owner is unchanged, so its state is retained, and its
    // flush after the commit goes to the new destination, including what it held before.
    let to = |output: &str| {
        plan(
            vec![source("src", "otlp"), opaque("h"), emit("e", output)],
            &["src.metrics -> h.in", "h.out -> e.in"],
            &[output],
        )
    };
    let registry = test_registry();
    let catalog = test_catalog();
    let interpreter = Interpreter::new(&registry, &catalog).with_opaque_host(id("h"), holding);
    let run = interpreter
        .run(
            &Trace::new()
                .install(to("a"))
                .admit(id("src"), t(100), vec![gauge("held")])
                .install(to("b"))
                .advance(t(110)),
        )
        .unwrap();

    assert!(run.output(&out("a")).is_empty());
    let b = run.output(&out("b"));
    assert_eq!(names(&run, "b"), strings(&["held", "host.watermark"]));
    assert!(b.iter().all(|record| record.generation == GenerationId::new(2)));

    // An opaque host whose parameters change cannot keep its state: its transition is `reject`.
    let mut changed = to("b");
    changed.nodes[1]
        .params
        .insert("component".parse().unwrap(), json!("other_component"));
    let run = interpreter
        .run(
            &Trace::new()
                .install(to("a"))
                .install(changed)
                .admit(id("src"), t(100), vec![gauge("m")])
                .advance(t(110)),
        )
        .unwrap();
    match &run.installs()[1].outcome {
        InstallOutcome::Rejected(rejection @ InstallRejection::Transition { node, class }) => {
            assert_eq!((node, *class), (&id("h"), TransitionClass::Reject));
            assert!(rejection.to_string().contains("transition to the new plan is `reject`"));
        }
        other => panic!("expected a rejected transition, got {:?}", other),
    }
    assert_eq!(names(&run, "a"), strings(&["m", "host.watermark"]));
}

#[test]
fn installations_that_need_later_stages_are_explicit_errors() {
    let plain = plan(
        vec![source("src", "otlp"), emit("e", "a")],
        &["src.metrics -> e.in"],
        &["a"],
    );
    let hosted = plan(
        vec![source("src", "otlp"), opaque("h"), emit("e", "a")],
        &["src.metrics -> h.in", "h.out -> e.in"],
        &["a"],
    );
    let held = plan(
        vec![source("src", "otlp"), node("k", "test_hold", json!({})), emit("e", "a")],
        &["src.metrics -> k.in", "k.out -> e.in"],
        &["a"],
    );
    let mut relabeled = held.clone();
    relabeled.nodes[1]
        .params
        .insert("label".parse().unwrap(), json!("other"));

    let registry = test_registry();
    let catalog = test_catalog();
    let interpreter = Interpreter::new(&registry, &catalog).with_opaque_host(id("h"), holding);
    // (what, trace, stage, message fragment)
    let cases: Vec<(&str, Trace, &str, &str)> = vec![
        (
            "adding a host",
            Trace::new().install(plain.clone()).install(hosted.clone()),
            "stage 10",
            "adding the host component `h`",
        ),
        (
            "removing a host",
            Trace::new().install(hosted).install(plain.clone()),
            "stage 10",
            "removing the host component `h`",
        ),
        // A state owner that keeps its state in its own program, outside an isolation fence, has no lineage that an
        // installation could start, end, or keep through a change.
        (
            "adding a state owner outside an isolation fence",
            Trace::new().install(plain.clone()).install(held.clone()),
            "stage 10",
            "adding the state owner `k` outside an isolation fence",
        ),
        (
            "removing a state owner outside an isolation fence",
            Trace::new().install(held.clone()).install(plain),
            "stage 10",
            "removing the state owner `k` outside an isolation fence",
        ),
        (
            "changing a state owner's parameters outside an isolation fence",
            Trace::new().install(held).install(relabeled),
            "stage 10",
            "changing the state owner `k` outside an isolation fence while it keeps its state",
        ),
    ];
    for (what, trace, stage, fragment) in cases {
        let (event, message) = run_error(&interpreter, trace);
        assert_eq!(event, Some(1), "{}", what);
        assert!(message.contains(stage), "{}: {}", what, message);
        assert!(message.contains(fragment), "{}: {}", what, message);
    }
}

// Trace events.

#[test]
fn malformed_and_unsupported_trace_events_are_explicit_errors() {
    let plan = plan(
        vec![source("src", "otlp"), emit("e", "a")],
        &["src.metrics -> e.in"],
        &["a"],
    );
    let registry = test_registry();
    let catalog = test_catalog();
    let interpreter = Interpreter::new(&registry, &catalog);
    let installed = || Trace::new().install(plan.clone());
    let too_many = (0..1025).map(|_| gauge("x")).collect::<Vec<_>>();

    // (what, trace, failing event, message fragment)
    let cases: Vec<(&str, Trace, usize, &str)> = vec![
        (
            "admission before installation",
            Trace::new().admit(id("src"), t(1), vec![gauge("x")]),
            0,
            "No plan is active",
        ),
        (
            "an unknown source",
            installed().admit(id("nope"), t(1), vec![]),
            1,
            "`nope` is not a `source` node",
        ),
        (
            "admission at a node that is not a source",
            installed().heartbeat(id("e"), t(1)),
            1,
            "`e` is not a `source` node",
        ),
        (
            "a batch larger than the batch size",
            installed().admit(id("src"), t(1), too_many),
            1,
            "1025 events, more than the batch size of 1024",
        ),
        (
            "a flush tick that moves back",
            installed().advance(t(10)).advance(t(9)),
            2,
            "moves back from 10.000000000 to 9.000000000",
        ),
        (
            "an admission time that moves back within the clock-reset threshold",
            installed()
                .admit(id("src"), t(400), vec![])
                .admit(id("src"), t(100), vec![]),
            2,
            "Admission point `src.metrics` stamps 100.000000000 after 400.000000000, a backward jump of at most the \
             clock-reset threshold of 300 s",
        ),
        (
            "an event after close",
            installed().close().advance(t(1)),
            2,
            "continues after `close`",
        ),
        (
            "a kind the source does not produce",
            installed().admit(
                id("src"),
                t(1),
                vec![gauge("fine"), Metric::set("members", SetPoints::from("member"))],
            ),
            1,
            "Port `src.metrics` carries counter, rate, gauge, distribution (mixed timestamps), but the batch has the \
             untimestamped set `members`",
        ),
    ];
    for (what, trace, event, fragment) in cases {
        let (at, message) = run_error(&interpreter, trace);
        assert_eq!(at, Some(event), "{}: {}", what, message);
        assert!(message.contains(fragment), "{}: {}", what, message);
    }

    // A DogStatsD source that discards client timestamps never admits a timestamped point.
    let untimestamped = self::plan(
        vec![
            node(
                "src",
                "source",
                json!({"binding": "dogstatsd", "timestamps": "untimestamped"}),
            ),
            emit("e", "a"),
        ],
        &["src.metrics -> e.in"],
        &["a"],
    );
    let (at, message) = run_error(
        &interpreter,
        Trace::new()
            .install(untimestamped)
            .admit(id("src"), t(1), vec![Metric::counter("c", [(10, 1.0), (0, 1.0)])]),
    );
    assert_eq!(at, Some(1));
    assert!(
        message.contains(
            "carries counter, gauge, set, histogram, distribution (untimestamped), but the batch has the \
                          partly timestamped counter `c`"
        ),
        "{}",
        message
    );

    // A clock reset reaches the state owners an admission point feeds, which a host component handles itself: the
    // interpreter supports resets of isolated state owners only.
    let hosted = self::plan(
        vec![source("src", "otlp"), opaque("h"), emit("e", "a")],
        &["src.metrics -> h.in", "h.out -> e.in"],
        &["a"],
    );
    let with_host = Interpreter::new(&registry, &catalog).with_opaque_host(id("h"), holding);
    let (at, message) = run_error(
        &with_host,
        Trace::new()
            .install(hosted)
            .admit(id("src"), t(7_200), vec![])
            .heartbeat(id("src"), t(3_600)),
    );
    assert_eq!(at, Some(2));
    assert!(
        message.contains("a clock reset at admission point `src.metrics` upstream of `h`"),
        "{}",
        message
    );
    assert!(message.contains("stage 10"), "{}", message);

    // A reset upstream of nothing that keeps state is a new epoch of the point's clock, and so is a `cancel` with no
    // cut in flight nothing to cancel.
    let run = interpreter
        .run(
            &installed()
                .admit(id("src"), t(7_200), vec![gauge("before")])
                .admit(id("src"), t(3_600), vec![gauge("after")])
                .cancel(),
        )
        .unwrap();
    assert_eq!(names(&run, "a"), strings(&["before", "after"]));
    assert!(matches!(run.installs()[1].outcome, InstallOutcome::CancelRefused));
    assert_eq!(run.installs()[1].event, 3);
}

#[test]
fn plans_the_interpreter_cannot_run_yet_are_explicit_errors() {
    let registry = test_registry();
    let catalog = test_catalog();
    let interpreter = Interpreter::new(&registry, &catalog);

    // A synchronous operator without a kernel, which only a test registry can have: every native one has a kernel.
    let kernelless = plan(
        vec![
            source("src", "otlp"),
            node("x", "test_kernelless", json!({})),
            emit("e", "a"),
        ],
        &["src.metrics -> x.in", "x.out -> e.in"],
        &["a"],
    );
    let (event, message) = run_error(&interpreter, Trace::new().install(kernelless));
    assert_eq!(event, Some(0));
    assert!(
        message.contains("validated node `x` is a `test_kernelless` node, a synchronous operator without a kernel"),
        "{}",
        message
    );

    // An operator that reads a signal.
    let reader = plan(
        vec![
            source("src", "otlp"),
            node("x", "test_signal_reader", json!({})),
            emit("e", "a"),
        ],
        &["src.metrics -> x.in", "x.out -> e.in"],
        &["a"],
    );
    let (_, message) = run_error(&interpreter, Trace::new().install(reader));
    assert!(message.contains("node `x` reads `queue_full`"), "{}", message);
    assert!(message.contains("stage 10"), "{}", message);

    // An opaque host without an adapter.
    let hosted = plan(
        vec![source("src", "otlp"), opaque("h"), emit("e", "a")],
        &["src.metrics -> h.in", "h.out -> e.in"],
        &["a"],
    );
    let (_, message) = run_error(&interpreter, Trace::new().install(hosted.clone()));
    assert!(message.contains("Opaque host `h` has no adapter"), "{}", message);

    // An adapter that fails to build rejects the installation.
    let failing = Interpreter::new(&registry, &catalog).with_opaque_host(id("h"), |_| {
        Err(AdapterError {
            message: "no such component".to_string(),
        })
    });
    let run = failing.run(&Trace::new().install(hosted)).unwrap();
    match &run.installs()[0].outcome {
        InstallOutcome::Rejected(rejection @ InstallRejection::HostBuild { .. }) => {
            assert!(rejection.to_string().contains("no such component"), "{}", rejection)
        }
        other => panic!("expected a rejection, got {:?}", other),
    }
}

#[test]
fn signals_are_recorded_and_change_nothing() {
    let plan = plan(
        vec![source("src", "otlp"), emit("e", "a")],
        &["src.metrics -> e.in"],
        &["a"],
    );
    let name: SignalName = "queue_full".parse().unwrap();
    let run = execute(
        Trace::new()
            .install(plan)
            .signal(name.clone(), SignalValue::Bool(true))
            .admit(id("src"), t(1), vec![gauge("x")])
            .signal(name.clone(), SignalValue::Ratio(0.5)),
    );
    assert_eq!(run.signal(&name), Some(SignalValue::Ratio(0.5)));
    assert_eq!(names(&run, "a"), strings(&["x"]));
}

// Kernels.

#[test]
fn kernels_transform_their_input_in_place() {
    let plan = plan(
        vec![
            source("src", "otlp"),
            node("p", "test_prefix", json!({"prefix": "pre."})),
            emit("e", "a"),
        ],
        &["src.metrics -> p.in", "p.out -> e.in"],
        &["a"],
    );
    let run = execute(
        Trace::new()
            .install(plan.clone())
            .admit(id("src"), t(1), vec![gauge("x"), counter("y")]),
    );
    assert_eq!(names(&run, "a"), strings(&["pre.x", "pre.y"]));

    // A kernel that fails to build rejects the installation.
    let mut empty = plan;
    empty.nodes[1].params.insert("prefix".parse().unwrap(), json!(""));
    let run = execute(Trace::new().install(empty));
    match &run.installs()[0].outcome {
        InstallOutcome::Rejected(rejection @ InstallRejection::KernelBuild { node, .. }) => {
            assert_eq!(node, &id("p"));
            assert!(
                rejection.to_string().contains("the prefix must not be empty"),
                "{}",
                rejection
            );
        }
        other => panic!("expected a rejection, got {:?}", other),
    }
}

#[test]
fn the_mapper_kernel_maps_records_in_place() {
    // The `dogstatsd_mapper` operator runs the native mapper: the profiles whose prefixes match are tried in order,
    // each trying its mappings in order, and the first matching mapping renames the metric and adds its tags with
    // captures expanded; the host and existing tags stay, and a metric no mapping matches passes unchanged.
    let mapper = |profiles: Value| {
        node(
            "dsd_enrich",
            "dogstatsd_mapper",
            json!({"profiles": profiles, "cache_size": 16, "string_interner_bytes": 65536}),
        )
    };
    let profiles = json!([
        {"name": "jobs", "prefix": "job.", "mappings": [
            {"match": "job.*.duration", "match_type": "wildcard", "name": "job.duration",
             "tags": [{"name": "job", "value": "$1"}]},
            {"match": "job\\.(\\w+)\\.(\\w+)", "match_type": "regex", "name": "job.$2",
             "tags": [{"name": "kind", "value": "${1}_x"}]}
        ]},
        {"name": "late", "prefix": "*", "mappings": [
            {"match": "job.*.duration", "match_type": "wildcard", "name": "never"},
            {"match": "job.*.*.*", "match_type": "wildcard", "name": "job.deep"}
        ]}
    ]);
    let plan = plan(
        vec![
            node(
                "src",
                "source",
                json!({"binding": "dogstatsd", "timestamps": "untimestamped"}),
            ),
            mapper(profiles),
            emit("e", "a"),
        ],
        &["src.metrics -> dsd_enrich.in", "dsd_enrich.out -> e.in"],
        &["a"],
    );
    let batch = || {
        vec![
            with_host(with_tags(counter("job.build.duration"), &["env:prod"]), "h"),
            gauge("job.deploy.count"),
            counter("other.metric"),
            counter("job.a.b.c"),
        ]
    };
    let run = execute(
        Trace::new()
            .install(plan)
            .admit(id("src"), t(1), batch())
            // The second batch is mapped from the result cache.
            .admit(id("src"), t(2), batch()),
    );

    let records = run.output(&out("a"));
    assert_eq!(records.len(), 8);
    for batch in records.chunks(4) {
        let contexts = batch.iter().map(|record| record.metric.context()).collect::<Vec<_>>();
        assert_eq!(contexts[0].name(), "job.duration");
        assert_eq!(contexts[0].host(), Some("h"));
        assert!(contexts[0].tags().has_tag("env:prod") && contexts[0].tags().has_tag("job:build"));
        assert_eq!(contexts[0].tags().len(), 2);
        assert_eq!(contexts[1].name(), "job.count");
        assert!(contexts[1].tags().has_tag("kind:deploy_x"));
        assert_eq!(contexts[1].tags().len(), 1);
        assert_eq!(contexts[2].name(), "other.metric");
        assert!(contexts[2].tags().is_empty());
        // No mapping of the first profile matches, so the second profile's applies.
        assert_eq!(contexts[3].name(), "job.deep");
        // Mapping changes only the context: values pass unchanged.
        assert_eq!(batch[1].metric.values(), &MetricValues::gauge(1.0));
    }
}

#[test]
fn the_mapper_kernel_needs_resolved_parameters() {
    let plan = plan(
        vec![
            node(
                "src",
                "source",
                json!({"binding": "dogstatsd", "timestamps": "untimestamped"}),
            ),
            node(
                "dsd_enrich",
                "dogstatsd_mapper",
                json!({"profiles": {"config": "test.profiles"}, "cache_size": 16, "string_interner_bytes": 65536}),
            ),
            emit("e", "a"),
        ],
        &["src.metrics -> dsd_enrich.in", "dsd_enrich.out -> e.in"],
        &["a"],
    );
    let run = execute(Trace::new().install(plan));
    match &run.installs()[0].outcome {
        InstallOutcome::Rejected(rejection @ InstallRejection::KernelBuild { node, .. }) => {
            assert_eq!(node, &id("dsd_enrich"));
            assert!(
                rejection
                    .to_string()
                    .contains("the `profiles` parameter is bound to configuration and has no resolved value"),
                "{}",
                rejection
            );
        }
        other => panic!("expected a rejection, got {:?}", other),
    }
}

#[test]
fn kernels_write_to_several_ports() {
    let routing = |route_all: bool| {
        plan(
            vec![
                source("src", "otlp"),
                node("r", "test_route", json!({"route_all": route_all})),
                emit("kept", "a"),
                emit("gauges", "b"),
            ],
            &["src.metrics -> r.in", "r.kept -> kept.in", "r.gauges -> gauges.in"],
            &["a", "b"],
        )
    };
    let batch = || vec![gauge("g1"), counter("c1"), gauge("g2"), counter("c2")];

    // Gauges are pushed one by one; the rest stays in the input, which goes to the primary port.
    let run = execute(Trace::new().install(routing(false)).admit(id("src"), t(1), batch()));
    assert_eq!(names(&run, "a"), strings(&["c1", "c2"]));
    assert_eq!(names(&run, "b"), strings(&["g1", "g2"]));

    // Routing everything moves the whole input to one port.
    let run = execute(Trace::new().install(routing(true)).admit(id("src"), t(1), batch()));
    assert!(names(&run, "a").is_empty());
    assert_eq!(names(&run, "b"), strings(&["g1", "c1", "g2", "c2"]));
}

#[test]
fn stateful_kernels_tick_finish_and_carry_their_state_across_installations() {
    let to = |output: &str| {
        plan(
            vec![
                source("src", "otlp"),
                node("k", "test_hold", json!({})),
                emit("e", output),
            ],
            &["src.metrics -> k.in", "k.out -> e.in"],
            &[output],
        )
    };
    let run = execute(
        Trace::new()
            .install(to("a"))
            .admit(id("src"), t(100), vec![gauge("x"), gauge("y")])
            // The kernel yields after each event it emits; the interpreter resumes it until it is done.
            .advance(t(110))
            .admit(id("src"), t(120), vec![gauge("carried")])
            .install(to("b"))
            .advance(t(130))
            .admit(
                id("src"),
                t(140),
                vec![Metric::counter("discarded", [(1, 1.0), (2, 1.0)])],
            )
            .close(),
    );

    let a = run.output(&out("a"));
    assert_eq!(names(&run, "a"), strings(&["x", "y"]));
    // Time-driven output is stamped with the state owner's watermark.
    assert!(a.iter().all(|record| record.admitted_at == Some(t(100))));
    // What the kernel held before the installation is emitted by the new plan.
    let b = run.output(&out("b"));
    assert_eq!(names(&run, "b"), strings(&["carried"]));
    assert_eq!(
        (b[0].generation, b[0].admitted_at),
        (GenerationId::new(2), Some(t(120)))
    );
    // At shutdown, what it still holds is lost, and counted.
    assert_eq!(
        run.loss(&id("k")).loss(LossClass::Shutdown),
        LossCount { events: 1, points: 2 }
    );
}

#[test]
fn kernels_finish_when_their_generation_retires_and_at_shutdown() {
    // The old plan's kernel finishes under `retire` when the new plan commits, and what it emits flows through the old
    // plan, in the old generation; at `close`, the new plan's kernel finishes under `shutdown`.
    let to = |output: &str| {
        plan(
            vec![
                source("src", "otlp"),
                node("k", "test_finish_marker", json!({})),
                emit("e", output),
            ],
            &["src.metrics -> k.in", "k.out -> e.in"],
            &[output],
        )
    };
    let run = execute(
        Trace::new()
            .install(to("a"))
            .admit(id("src"), t(1), vec![gauge("x")])
            .install(to("b"))
            .admit(id("src"), t(2), vec![gauge("y")])
            .close(),
    );

    let generations = |output: &str| {
        run.output(&out(output))
            .iter()
            .map(|record| (record.metric.context().name().to_string(), record.generation.get()))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        generations("a"),
        vec![("x".to_string(), 1), ("finished.retire".to_string(), 1)]
    );
    assert_eq!(
        generations("b"),
        vec![("y".to_string(), 2), ("finished.shutdown".to_string(), 2)]
    );
}

#[test]
fn a_kernel_that_never_finishes_is_an_error() {
    let plan = plan(
        vec![
            source("src", "otlp"),
            node("x", "test_stall", json!({})),
            emit("e", "a"),
        ],
        &["src.metrics -> x.in", "x.out -> e.in"],
        &["a"],
    );
    let registry = test_registry();
    let catalog = test_catalog();
    let (event, message) = run_error(
        &Interpreter::new(&registry, &catalog),
        Trace::new().install(plan).admit(id("src"), t(1), vec![gauge("x")]),
    );
    assert_eq!(event, Some(1));
    assert!(
        message.contains("The kernel of node `x` yielded 100000 times"),
        "{}",
        message
    );
}

#[test]
fn kernels_are_built_inside_a_tokio_context() {
    // `test_needs_runtime` fails to build outside a Tokio context, as a kernel that spawns its cache maintenance would.
    let plan = plan(
        vec![
            source("src", "otlp"),
            node("x", "test_needs_runtime", json!({})),
            emit("e", "a"),
        ],
        &["src.metrics -> x.in", "x.out -> e.in"],
        &["a"],
    );
    let run = execute(Trace::new().install(plan).admit(id("src"), t(1), vec![gauge("x")]));
    assert!(
        matches!(run.installs()[0].outcome, InstallOutcome::Active { .. }),
        "{:?}",
        run.installs()[0].outcome
    );
    assert_eq!(names(&run, "a"), strings(&["x"]));
}

// The oracle against hand-written expected outputs.

#[test]
fn routed_outputs_equal_hand_written_expectations() {
    let plan = plan(
        vec![
            source("src", "otlp"),
            node("t", "tee", json!({"ports": 2})),
            split("s", json!({"not": {"has_tag": "debug"}})),
            emit("all", "a"),
            emit("kept", "b"),
            node("d", "drop", json!({})),
        ],
        &[
            "src.metrics -> t.in",
            "t.0 -> all.in",
            "t.1 -> s.in",
            "s.true -> kept.in",
            "s.false -> d.in",
        ],
        &["a", "b"],
    );
    let debug = with_tags(Metric::counter("requests", [(10, 1.0), (20, 2.0)]), &["debug"]);
    let normal = Metric::gauge("load", (10, 0.5));
    let run = execute(
        Trace::new()
            .install(plan)
            .admit(id("src"), t(1), vec![debug.clone()])
            .admit(id("src"), t(2), vec![normal.clone()]),
    );

    // Written as single-point records, in another order: grouping and order are not observable.
    let expected = OutputPoints::new()
        .with(
            &out("a"),
            &[
                Metric::gauge("load", (10, 0.5)),
                with_tags(Metric::counter("requests", (20, 2.0)), &["debug"]),
                with_tags(Metric::counter("requests", (10, 1.0)), &["debug"]),
            ],
        )
        .with(&out("b"), &[normal]);
    assert_eq!(compare(&expected, &OutputPoints::from_run(&run)), Ok(()));

    // A wrong expectation is reported as a difference.
    let wrong = OutputPoints::new().with(&out("b"), &[debug]);
    let mismatch = compare(&wrong, &OutputPoints::from_run(&run)).unwrap_err();
    assert_eq!(mismatch.outputs.len(), 2);
}

// Property tests.

/// The shape of a generated routing-only plan below one input.
#[derive(Clone, Debug)]
enum Shape {
    Emit,
    Drop,
    Split(Predicate, Box<Shape>, Box<Shape>),
    Tee(Vec<Shape>),
    /// A split whose two ports rejoin in a merge, followed by the shape.
    Rejoin(Predicate, Box<Shape>),
}

fn arb_shape() -> impl Strategy<Value = Shape> {
    let leaf = prop_oneof![3 => Just(Shape::Emit), 1 => Just(Shape::Drop)];
    leaf.prop_recursive(3, 24, 3, |inner| {
        prop_oneof![
            (arb_predicate(), inner.clone(), inner.clone()).prop_map(|(predicate, yes, no)| Shape::Split(
                predicate,
                Box::new(yes),
                Box::new(no)
            )),
            vec(inner.clone(), 1..4).prop_map(Shape::Tee),
            (arb_predicate(), inner).prop_map(|(predicate, rest)| Shape::Rejoin(predicate, Box::new(rest))),
        ]
    })
}

/// Lowers a shape below the port `from` into nodes and edges, numbering nodes and outputs as it goes.
struct Builder {
    nodes: Vec<Value>,
    edges: Vec<Value>,
    outputs: Vec<String>,
}

impl Builder {
    fn next_id(&self) -> String {
        format!("n{}", self.nodes.len())
    }

    fn connect(&mut self, from: &str, to: String) {
        self.edges.push(json!({"from": from, "to": to}));
    }

    fn build(&mut self, shape: &Shape, from: &str) {
        let id = self.next_id();
        match shape {
            Shape::Emit => {
                let output = format!("o{}", self.outputs.len());
                self.nodes.push(emit(&id, &output));
                self.outputs.push(output);
                self.connect(from, format!("{}.in", id));
            }
            Shape::Drop => {
                self.nodes.push(node(&id, "drop", json!({})));
                self.connect(from, format!("{}.in", id));
            }
            Shape::Split(predicate, yes, no) => {
                self.nodes.push(split(&id, predicate_json(predicate)));
                self.connect(from, format!("{}.in", id));
                self.build(yes, &format!("{}.true", id));
                self.build(no, &format!("{}.false", id));
            }
            Shape::Tee(branches) => {
                self.nodes.push(node(&id, "tee", json!({"ports": branches.len()})));
                self.connect(from, format!("{}.in", id));
                for (port, branch) in branches.iter().enumerate() {
                    self.build(branch, &format!("{}.{}", id, port));
                }
            }
            Shape::Rejoin(predicate, rest) => {
                self.nodes.push(split(&id, predicate_json(predicate)));
                self.connect(from, format!("{}.in", id));
                let merge = format!("{}_m", id);
                self.nodes.push(node(&merge, "merge", json!({"inputs": 2})));
                self.connect(&format!("{}.true", id), format!("{}.0", merge));
                self.connect(&format!("{}.false", id), format!("{}.1", merge));
                self.build(rest, &format!("{}.out", merge));
            }
        }
    }
}

/// Builds a routing plan of the given shape, fed by two sources that together admit every kind: DogStatsD, with client
/// timestamps, admits every kind but rates, and OTLP admits rates.
fn routing_plan(shape: &Shape) -> Plan {
    let mut builder = Builder {
        nodes: vec![
            node("dsd", "source", json!({"binding": "dogstatsd", "timestamps": "mixed"})),
            source("otlp", "otlp"),
            node("in", "merge", json!({"inputs": 2})),
        ],
        edges: vec![
            json!({"from": "dsd.metrics", "to": "in.0"}),
            json!({"from": "otlp.metrics", "to": "in.1"}),
        ],
        outputs: Vec::new(),
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
    Plan::from_value(json!({"ir_version": 1, "nodes": builder.nodes, "edges": builder.edges, "outputs": outputs}))
        .unwrap()
}

/// Admits `metrics`, split into batches at the given cut points, with non-decreasing admission times. Each batch's
/// rates are admitted at the OTLP source, and everything else at the DogStatsD source, so that every record is one its
/// source can produce.
fn batched_trace(plan: &Arc<Plan>, metrics: &[Metric], cuts: &[usize]) -> Trace {
    let mut trace = Trace::new();
    trace.push(TraceEvent::Install { plan: Arc::clone(plan) });
    let mut start = 0;
    for (batch, end) in cuts.iter().copied().chain([metrics.len()]).enumerate() {
        let (rates, others): (Vec<_>, Vec<_>) = metrics[start..end]
            .iter()
            .cloned()
            .partition(|metric| Kind::of(metric.values()) == Kind::Rate);
        for (source, batch_metrics) in [("dsd", others), ("otlp", rates)] {
            trace.push(TraceEvent::Admit {
                source: id(source),
                admitted_at: t(1_000 + batch as u64),
                batch: batch_metrics,
            });
        }
        start = end;
    }
    trace
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn property_test_routing_outputs_are_invariant_under_regrouping_batches(
        shape in arb_shape(),
        metrics in vec(arb_metric(), 0..32),
        cuts in vec(any::<Index>(), 0..6),
    ) {
        let plan = routing_plan(&shape);
        let registry = OperatorRegistry::native();
        let catalog = test_catalog();
        let interpreter = Interpreter::new(&registry, &catalog);

        let mut cuts = cuts.iter().map(|cut| cut.index(metrics.len() + 1)).collect::<Vec<_>>();
        cuts.sort_unstable();
        let plan = Arc::new(plan);
        let whole = interpreter.run(&batched_trace(&plan, &metrics, &[])).unwrap();
        prop_assert!(
            matches!(whole.installs()[0].outcome, InstallOutcome::Active { .. }),
            "the generated plan is invalid: {:?}",
            whole.installs()[0].outcome
        );
        let regrouped = interpreter.run(&batched_trace(&plan, &metrics, &cuts)).unwrap();

        let whole_points = OutputPoints::from_run(&whole);
        if let Err(mismatch) = compare(&whole_points, &OutputPoints::from_run(&regrouped)) {
            prop_assert!(false, "regrouping {:?} changed the outputs:\n{}", cuts, mismatch);
        }

        // Routing only selects records: every output receives each admitted point at most as often as it was admitted.
        let input = out("o127");
        let admitted = OutputPoints::new().with(&input, &metrics);
        let available = admitted.points(&input).collect::<std::collections::BTreeMap<_, _>>();
        for (output, _) in whole.outputs() {
            for (point, count) in whole_points.points(output) {
                let limit = available.get(point).copied().unwrap_or(0);
                prop_assert!(count <= limit, "`{}` received {} copies of {}, admitted {}", output, count, point, limit);
            }
        }
    }
}

#[test]
fn routing_plans_with_every_shape_are_valid() {
    // The property test generates plans from these shapes; each must validate, or the property would hold vacuously.
    let any = Predicate::Const(true);
    let shapes = [
        Shape::Emit,
        Shape::Drop,
        Shape::Split(any.clone(), Box::new(Shape::Emit), Box::new(Shape::Drop)),
        Shape::Tee(vec![Shape::Emit, Shape::Emit, Shape::Drop]),
        Shape::Rejoin(any.clone(), Box::new(Shape::Tee(vec![Shape::Emit, Shape::Emit]))),
        Shape::Tee(vec![Shape::Rejoin(any, Box::new(Shape::Emit)), Shape::Emit]),
    ];
    let registry = OperatorRegistry::native();
    let catalog = test_catalog();
    for shape in shapes {
        let plan = routing_plan(&shape);
        let report = Validator::new(&registry, &catalog).validate(&plan);
        assert!(report.is_valid(), "{:?}:\n{}", shape, report);
    }
}
