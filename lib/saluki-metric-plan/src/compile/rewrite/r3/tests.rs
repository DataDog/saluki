use proptest::{prelude::*, strategy::ValueTree as _, test_runner::TestRunner};
use serde_json::{json, Value};

use super::*;
use crate::{
    canonical::PlanHash,
    compile::{
        compile, place_unvalidated,
        rewrite::{
            check,
            random_plans::{
                arb_batches, arb_bounds, arb_shape, arb_split_predicate, arbitrary_bounds, batch_aligned_bounds,
                catalog, check_equivalent, check_same_order, check_under_capacity, trace, Case,
            },
            RewriteOutcome, RewriteRecord,
        },
        Frame,
    },
    explain::Explain,
    ir::NativeComponentId,
    operators::OperatorRegistry,
    testing::{default_program_catalog, default_program_slot_name, with_opaque_gateways, DEFAULT_PROGRAM_JSON},
    validate::{revalidate, MemoryCatalog, Validator},
};

fn id(value: &str) -> NodeId {
    value.parse().unwrap()
}

/// A node with native provenance of its own.
fn node(id: &str, op: &str, params: Value) -> Value {
    json!({"id": id, "op": op, "params": params, "provenance": {"native": id}})
}

fn split(id: &str, predicate: Value) -> Value {
    node(id, "split", json!({"predicate": predicate}))
}

fn emit(id: &str, output: &str) -> Value {
    node(id, "emit", json!({"output": output}))
}

fn drop_node(id: &str) -> Value {
    node(id, "drop", json!({}))
}

fn source(id: &str, binding: &str) -> Value {
    node(id, "source", json!({"binding": binding}))
}

/// A kernel that leaves records unchanged, so that a folded `split`'s input and target share a slot.
fn kernel(id: &str) -> Value {
    node(id, "host_enrichment", json!({}))
}

fn opaque(id: &str) -> Value {
    let any = json!({"kinds": "all", "timestamps": "mixed"});
    node(
        id,
        "opaque_host",
        json!({"component": id, "input": any, "outputs": {"out": any}}),
    )
}

/// Builds a plan from nodes, edges written as `from.port -> to.port`, and the outputs it declares, from `o0` up.
fn plan_with(nodes: Vec<Value>, edges: &[&str], outputs: usize, extra: Value) -> Plan {
    let edges = edges
        .iter()
        .map(|edge| {
            let (from, to) = edge.split_once(" -> ").unwrap();
            json!({"from": from, "to": to})
        })
        .collect::<Vec<_>>();
    let outputs = (0..outputs)
        .map(|index| {
            let output = format!("o{}", index);
            json!({"id": output, "accepts": {"kinds": "all", "timestamps": "mixed"},
                   "fingerprint": PlanHash::of_bytes(output.as_bytes()).to_string(),
                   "recipients": [format!("out/{}", output)]})
        })
        .collect::<Vec<_>>();
    let mut plan = json!({"ir_version": 1, "nodes": nodes, "edges": edges, "outputs": outputs});
    if let (Value::Object(plan), Value::Object(extra)) = (&mut plan, extra) {
        plan.extend(extra);
    }
    Plan::from_value(plan).unwrap()
}

fn plan(nodes: Vec<Value>, edges: &[&str], outputs: usize) -> Plan {
    plan_with(nodes, edges, outputs, json!({}))
}

fn namer(nodes: &[NodeId]) -> Result<NativeComponentId, String> {
    NativeComponentId::new(format!("region_{}", nodes[0].local())).map_err(|error| error.to_string())
}

/// A valid plan with its frame, against `catalog`.
struct Fixture {
    plan: Plan,
    report: ValidationReport,
    frame: Frame,
    registry: OperatorRegistry,
}

impl Fixture {
    fn new(plan: Plan) -> Self {
        Self::with_catalog(plan, &catalog())
    }

    fn with_catalog(plan: Plan, catalog: &MemoryCatalog) -> Self {
        let registry = OperatorRegistry::native();
        let report = Validator::new(&registry, catalog).validate(&plan);
        assert!(report.is_valid(), "test plan is invalid:\n{}", report);
        let frame = Frame::compute(&plan, &registry, &namer).unwrap();
        Self {
            plan,
            report,
            frame,
            registry,
        }
    }

    fn apply(&self) -> Option<Rewritten> {
        R3.apply(
            &RewriteContext::new(&self.registry, &self.frame, &self.report),
            &self.plan,
        )
    }
}

fn ids(list: &[&str]) -> Vec<NodeId> {
    list.iter().map(|value| id(value)).collect()
}

fn effects(rewritten: &Rewritten) -> Vec<(String, TelemetryChange)> {
    rewritten
        .telemetry
        .iter()
        .map(|effect| (effect.node.to_string(), effect.change))
        .collect()
}

// Hand-written applications.

#[test]
fn a_constant_split_becomes_an_edge_and_the_branch_it_never_takes_is_removed() {
    // (predicate, the port every record takes)
    let cases = [
        (json!({"const": true}), "true"),
        (json!({"const": false}), "false"),
        (json!({"not": {"const": true}}), "false"),
        (json!({"all": [{"name": {"eq": "a"}}, {"const": false}]}), "false"),
        (json!({"any": [{"has_tag": "env"}, {"not": {"const": false}}]}), "true"),
        // `kind_in` decides the predicate for every kind the input carries.
        (json!({"kind_in": "all"}), "true"),
        (json!({"kind_in": []}), "false"),
    ];
    for (predicate, taken) in cases {
        let original = plan(
            vec![
                source("src", "otlp"),
                kernel("k"),
                split("s", predicate.clone()),
                emit("yes", "o0"),
                drop_node("no"),
            ],
            &[
                "src.metrics -> k.in",
                "k.out -> s.in",
                "s.true -> yes.in",
                "s.false -> no.in",
            ],
            1,
        );
        let fixture = Fixture::new(original);
        let rewritten = fixture.apply().expect("the split is constant");
        let (kept, removed) = if taken == "true" { ("yes", "no") } else { ("no", "yes") };
        let mut nodes = vec![source("src", "otlp"), kernel("k")];
        nodes.push(if taken == "true" {
            emit("yes", "o0")
        } else {
            drop_node("no")
        });
        let expected = plan(nodes, &["src.metrics -> k.in", &format!("k.out -> {}.in", kept)], 1);
        assert_eq!(rewritten.plan, expected, "{}", predicate);
        // Records list the folded split and the removed nodes in plan order.
        assert_eq!(rewritten.nodes, ids(&["s", removed]), "{}", predicate);
        assert!(!rewritten.capacity_rule);
        assert!(
            rewritten
                .justification
                .contains(&format!("every record leaves by `{}`", taken)),
            "{}",
            rewritten.justification
        );
        // The folded split's per-port counts go; a removed `drop` loses its policy-loss series, a removed `emit` has
        // none.
        let mut expected_effects = vec![("s".to_string(), TelemetryChange::Removed)];
        if removed == "no" {
            expected_effects.push(("no".to_string(), TelemetryChange::Removed));
        }
        assert_eq!(effects(&rewritten), expected_effects, "{}", predicate);
    }
}

#[test]
fn justifications_name_why_the_split_is_constant() {
    let cases = [
        (json!({"const": true}), "the predicate of `s` is the constant `true`"),
        (
            json!({"all": [{"name": {"eq": "a"}}, {"const": false}]}),
            "the literal constants of the predicate of `s` decide it `false`",
        ),
        (
            json!({"kind_in": "all"}),
            "the predicate of `s` is `true` for every kind its input can carry (counter, rate, gauge, distribution)",
        ),
        (
            json!({"any": [{"kind_in": ["counter", "rate"]}, {"kind_in": ["gauge", "distribution"]}]}),
            "the predicate of `s` is `true` for every kind its input can carry (counter, rate, gauge, distribution)",
        ),
    ];
    for (predicate, expected) in cases {
        let fixture = Fixture::new(plan(
            vec![
                source("src", "otlp"),
                kernel("k"),
                split("s", predicate.clone()),
                emit("yes", "o0"),
                drop_node("no"),
            ],
            &[
                "src.metrics -> k.in",
                "k.out -> s.in",
                "s.true -> yes.in",
                "s.false -> no.in",
            ],
            1,
        ));
        let rewritten = fixture.apply().unwrap();
        assert!(
            rewritten.justification.contains(expected),
            "{}: {}",
            predicate,
            rewritten.justification
        );
    }
}

#[test]
fn a_nested_constant_split_goes_with_the_branch_it_is_on_and_outputs_stay_declared() {
    let fixture = Fixture::new(plan(
        vec![
            source("src", "otlp"),
            kernel("k"),
            split("outer", json!({"const": true})),
            emit("kept", "o0"),
            split("inner", json!({"const": false})),
            emit("gone", "o1"),
            drop_node("discarded"),
        ],
        &[
            "src.metrics -> k.in",
            "k.out -> outer.in",
            "outer.true -> kept.in",
            "outer.false -> inner.in",
            "inner.true -> gone.in",
            "inner.false -> discarded.in",
        ],
        2,
    ));
    let rewritten = fixture.apply().unwrap();
    let expected = plan(
        vec![source("src", "otlp"), kernel("k"), emit("kept", "o0")],
        &["src.metrics -> k.in", "k.out -> kept.in"],
        2,
    );
    assert_eq!(rewritten.plan, expected);
    assert_eq!(rewritten.nodes, ids(&["outer", "inner", "gone", "discarded"]));
    assert_eq!(
        effects(&rewritten),
        vec![
            ("outer".to_string(), TelemetryChange::Removed),
            ("inner".to_string(), TelemetryChange::Removed),
            ("discarded".to_string(), TelemetryChange::Removed),
        ]
    );
    assert!(rewritten
        .justification
        .contains("unreachable: `inner`, `gone`, `discarded` receive records only through `outer.false`"));
    assert!(rewritten
        .justification
        .contains("the outputs of removed `emit`s stay declared, and their exits connected and idle"));
}

#[test]
fn a_merge_the_dead_branch_feeds_keeps_its_other_inputs_renumbered_in_order() {
    // The dead branch feeds the merge's first input, a second source its second, and the live port its third.
    let fixture = Fixture::new(plan(
        vec![
            node("dsd", "source", json!({"binding": "dogstatsd", "timestamps": "mixed"})),
            source("otlp", "otlp"),
            kernel("k"),
            split("s", json!({"const": false})),
            node("x", "namespace_prefix", json!({"namespace": "ns", "exclusions": []})),
            node("m", "merge", json!({"inputs": 3})),
            emit("e", "o0"),
        ],
        &[
            "dsd.metrics -> k.in",
            "k.out -> s.in",
            "s.true -> x.in",
            "x.out -> m.0",
            "otlp.metrics -> m.1",
            "s.false -> m.2",
            "m.out -> e.in",
        ],
        1,
    ));
    let rewritten = fixture.apply().unwrap();
    let expected = plan(
        vec![
            node("dsd", "source", json!({"binding": "dogstatsd", "timestamps": "mixed"})),
            source("otlp", "otlp"),
            kernel("k"),
            node("m", "merge", json!({"inputs": 2})),
            emit("e", "o0"),
        ],
        &[
            "dsd.metrics -> k.in",
            "k.out -> m.1",
            "otlp.metrics -> m.0",
            "m.out -> e.in",
        ],
        1,
    );
    assert_eq!(rewritten.plan, expected);
    assert_eq!(rewritten.nodes, ids(&["s", "x", "m"]));
    assert_eq!(
        effects(&rewritten),
        vec![
            ("s".to_string(), TelemetryChange::Removed),
            ("x".to_string(), TelemetryChange::Removed),
        ]
    );
    assert!(rewritten
        .justification
        .contains("merges: `m` keeps its other inputs, renumbered in order"));
}

#[test]
fn a_split_whose_ports_rejoin_folds_into_a_merge_of_one_input() {
    let fixture = Fixture::new(plan(
        vec![
            source("src", "otlp"),
            kernel("k"),
            split("s", json!({"const": true})),
            node("m", "merge", json!({"inputs": 2})),
            emit("e", "o0"),
        ],
        &[
            "src.metrics -> k.in",
            "k.out -> s.in",
            "s.true -> m.0",
            "s.false -> m.1",
            "m.out -> e.in",
        ],
        1,
    ));
    let rewritten = fixture.apply().unwrap();
    let expected = plan(
        vec![
            source("src", "otlp"),
            kernel("k"),
            node("m", "merge", json!({"inputs": 1})),
            emit("e", "o0"),
        ],
        &["src.metrics -> k.in", "k.out -> m.0", "m.out -> e.in"],
        1,
    );
    assert_eq!(rewritten.plan, expected);
    assert_eq!(rewritten.nodes, ids(&["s", "m"]));
}

// What R3 never folds or removes.

#[test]
fn configuration_constants_are_opaque_in_structure() {
    let cases = [
        json!({"flag": false}),
        json!({"flag": true}),
        json!({"flag": {"config": "test.off", "resolved": false}}),
        json!({"is_empty": []}),
        json!({"is_empty": {"config": "test.empty", "resolved": []}}),
        json!({"all": [{"flag": {"config": "test.off", "resolved": false}}, {"name": {"eq": "a"}}]}),
        json!({"any": [{"is_empty": {"config": "test.empty", "resolved": []}}, {"const": false}]}),
        json!({"name": {"prefix": ""}}),
    ];
    for predicate in cases {
        let fixture = Fixture::new(plan(
            vec![
                source("src", "otlp"),
                kernel("k"),
                split("s", predicate.clone()),
                emit("yes", "o0"),
                drop_node("no"),
            ],
            &[
                "src.metrics -> k.in",
                "k.out -> s.in",
                "s.true -> yes.in",
                "s.false -> no.in",
            ],
            1,
        ));
        assert!(fixture.apply().is_none(), "{} must not be folded", predicate);
    }
}

#[test]
fn a_dead_branch_that_reaches_a_fence_is_not_removed() {
    // An opaque host may produce records without input, and is part of the frame.
    let fixture = Fixture::new(plan(
        vec![
            source("src", "otlp"),
            kernel("k"),
            split("s", json!({"const": true})),
            emit("yes", "o0"),
            opaque("h"),
            emit("after", "o1"),
        ],
        &[
            "src.metrics -> k.in",
            "k.out -> s.in",
            "s.true -> yes.in",
            "s.false -> h.in",
            "h.out -> after.in",
        ],
        2,
    ));
    assert!(fixture.apply().is_none());
}

#[test]
fn a_split_that_a_scope_names_is_not_folded() {
    let nodes = vec![
        source("src", "otlp"),
        kernel("k"),
        split("s", json!({"const": true})),
        emit("yes", "o0"),
        drop_node("no"),
    ];
    let edges = [
        "src.metrics -> k.in",
        "k.out -> s.in",
        "s.true -> yes.in",
        "s.false -> no.in",
    ];
    let unscoped = plan(nodes.clone(), &edges, 1);
    let registry = OperatorRegistry::native();
    let port_type = Validator::new(&registry, &catalog())
        .validate(&unscoped)
        .port_type(&"s.true".parse().unwrap())
        .unwrap();
    let scoped = plan_with(
        nodes,
        &edges,
        1,
        json!({"scopes": {"after_split": {"inputs": {"matched": "s.true"}}}}),
    );
    let catalog = catalog().with_scope_input("after_split".parse().unwrap(), "matched".parse().unwrap(), port_type);
    let fixture = Fixture::with_catalog(scoped, &catalog);
    assert!(fixture.apply().is_none());
}

/// Returns the type validation gives `port` in `plan`, which is valid without scopes.
fn port_type_in(plan: &Plan, port: &str) -> crate::model::PortType {
    let registry = OperatorRegistry::native();
    Validator::new(&registry, &catalog())
        .validate(plan)
        .port_type(&port.parse().unwrap())
        .unwrap()
}

#[test]
fn nodes_and_ports_that_a_scope_names_keep_r3_from_folding() {
    // (nodes, edges, the scope, its input, the port feeding the input): each plan folds without its scope.
    let cases = [
        // A node in the dead subgraph: `x`, which the scope's input names, would be removed.
        (
            vec![
                source("src", "otlp"),
                kernel("k"),
                split("s", json!({"const": true})),
                emit("yes", "o0"),
                kernel("x"),
                drop_node("no"),
            ],
            vec![
                "src.metrics -> k.in",
                "k.out -> s.in",
                "s.true -> yes.in",
                "s.false -> x.in",
                "x.out -> no.in",
            ],
            json!({"inputs": {"after": "x.out"}}),
            "x.out",
        ),
        // A merge that would keep its other input, renumbered: the scope's input names it.
        (
            vec![
                source("src", "otlp"),
                kernel("k"),
                split("s", json!({"const": true})),
                kernel("x"),
                node("m", "merge", json!({"inputs": 2})),
                emit("e", "o0"),
            ],
            vec![
                "src.metrics -> k.in",
                "k.out -> s.in",
                "s.true -> x.in",
                "x.out -> m.0",
                "s.false -> m.1",
                "m.out -> e.in",
            ],
            json!({"inputs": {"after": "m.out"}}),
            "m.out",
        ),
        // The port the live port leads to: a scope exit enters it, and the fold would feed it from outside the scope.
        (
            vec![
                source("src", "otlp"),
                kernel("k"),
                split("s", json!({"const": true})),
                emit("yes", "o0"),
                drop_node("no"),
            ],
            vec![
                "src.metrics -> k.in",
                "k.out -> s.in",
                "s.true -> yes.in",
                "s.false -> no.in",
            ],
            json!({"inputs": {"after": "k.out"}, "exits": {"done": "yes.in"}}),
            "k.out",
        ),
    ];
    for (nodes, edges, scope, input) in cases {
        let unscoped = plan(nodes.clone(), &edges, 1);
        assert!(
            Fixture::new(unscoped.clone()).apply().is_some(),
            "{} folds without its scope",
            scope
        );
        let scoped = plan_with(nodes, &edges, 1, json!({"scopes": {"policy": scope}}));
        let catalog = catalog().with_scope_input(
            "policy".parse().unwrap(),
            "after".parse().unwrap(),
            port_type_in(&unscoped, input),
        );
        let fixture = Fixture::with_catalog(scoped, &catalog);
        assert!(fixture.apply().is_none(), "{} must keep R3 from folding", scope);
    }
}

#[test]
fn a_dead_branch_that_reaches_a_state_owner_or_a_signal_reader_is_not_removed() {
    // A state owner may produce records without input, and a signal reader may change what it does at any time. Neither
    // runs in a region before stages 6 and 10, so these plans have no frame of their own: R3 proposes without looking
    // at the frame (the pipeline checks placement), so any frame serves.
    let registry = crate::interpret::test_operators::test_registry();
    let frame_plan = plan(
        vec![source("src", "otlp"), emit("e", "o0")],
        &["src.metrics -> e.in"],
        1,
    );
    let frame = Frame::compute(&frame_plan, &registry, &namer).unwrap();
    // (the operator on the dead branch, whether R3 folds)
    let cases = [
        ("test_hold", false),
        ("test_signal_reader", false),
        ("host_enrichment", true),
    ];
    for (op, folds) in cases {
        let plan = plan(
            vec![
                source("src", "otlp"),
                kernel("k"),
                split("s", json!({"const": true})),
                emit("yes", "o0"),
                node("x", op, json!({})),
                drop_node("no"),
            ],
            &[
                "src.metrics -> k.in",
                "k.out -> s.in",
                "s.true -> yes.in",
                "s.false -> x.in",
                "x.out -> no.in",
            ],
            1,
        );
        let report = Validator::new(&registry, &catalog()).validate(&plan);
        assert!(report.is_valid(), "{}: {}", op, report);
        let applied = R3.apply(&RewriteContext::new(&registry, &frame, &report), &plan);
        assert_eq!(applied.is_some(), folds, "{}", op);
    }
}

#[test]
fn a_constant_split_that_nothing_reaches_is_left_alone() {
    // Untimestamped DogStatsD input leaves the timestamp split's passthrough port carrying nothing; the split below it
    // is part of a dead subgraph R3 cannot remove, since the timestamp split's port needs its edge.
    let fixture = Fixture::new(plan(
        vec![
            node(
                "dsd",
                "source",
                json!({"binding": "dogstatsd", "timestamps": "untimestamped"}),
            ),
            node("ts", "timestamp_split", json!({"window": "10s"})),
            emit("default", "o0"),
            split("s", json!({"const": true})),
            emit("yes", "o1"),
            drop_node("no"),
        ],
        &[
            "dsd.metrics -> ts.in",
            "ts.default -> default.in",
            "ts.passthrough -> s.in",
            "s.true -> yes.in",
            "s.false -> no.in",
        ],
        2,
    ));
    let passthrough = fixture.report.port_type(&"ts.passthrough".parse().unwrap()).unwrap();
    assert!(passthrough.kinds.is_empty(), "{:?}", passthrough);
    assert!(fixture.apply().is_none());
}

#[test]
fn regions_receive_only_metrics_which_folding_a_split_to_its_true_port_relies_on() {
    use std::num::NonZeroUsize;

    use saluki_core::{
        components::transforms::TransformBuilder,
        data_model::event::{eventd::EventD, metric::Metric, Event, EventType},
        execution::{control_mailbox, RegionHostBuilder},
    };

    use crate::{
        compile::DecisionScratch,
        runtime::{MetricRegionDriver, RegionControl},
    };

    // A compiled decision sends an event other than a metric down a split's `false` port, whatever the predicate: a
    // split folded to its `true` port would deliver it instead.
    let fixture = Fixture::new(plan(
        vec![
            source("src", "otlp"),
            kernel("k"),
            split("s", json!({"const": true})),
            emit("yes", "o0"),
            drop_node("no"),
        ],
        &[
            "src.metrics -> k.in",
            "k.out -> s.in",
            "s.true -> yes.in",
            "s.false -> no.in",
        ],
        1,
    ));
    assert!(fixture.apply().is_some());
    let compiled = compile(&fixture.plan, &fixture.report, &fixture.registry, &fixture.frame).unwrap();
    let unfolded = crate::compile::compile_unvalidated(&fixture.plan, &fixture.registry, &fixture.frame).unwrap();
    let decision = unfolded
        .programs
        .iter()
        .flat_map(|program| program.decisions())
        .find(|decision| decision.root().node == id("s"))
        .unwrap();
    let exit = |port: &str| {
        let index = decision
            .exits
            .iter()
            .position(|exit| exit.port.as_str() == port)
            .unwrap();
        crate::compile::DecisionExit::new(u16::try_from(index).unwrap())
    };
    let mut scratch = DecisionScratch::default();
    let exits =
        |event: &Event, scratch: &mut DecisionScratch| decision.evaluate(event, scratch).iter().collect::<Vec<_>>();
    let metric = Event::Metric(Metric::gauge("a", 1.0));
    let other = Event::EventD(EventD::new("deploy", "an event other than a metric"));
    assert_eq!(exits(&metric, &mut scratch), vec![exit("true")]);
    assert_eq!(exits(&other, &mut scratch), vec![exit("false")]);
    assert!(compiled.plan.node(&id("s")).is_none());

    // So R3 relies on regions receiving metrics alone: their host's input type says so, and a topology connects
    // nothing else to them. Changing it needs R3 to fold to `true` only where the input provably carries metrics.
    let (_control, mailbox) = control_mailbox::<RegionControl>();
    let builder = RegionHostBuilder::<MetricRegionDriver>::new(["out"], NonZeroUsize::MIN, mailbox, |_| {
        Err(saluki_error::generic_error!("the test never builds the region"))
    })
    .unwrap();
    assert_eq!(builder.input_event_type(), EventType::Metric);
}

#[test]
fn the_default_program_has_no_constant_split() {
    let registry = OperatorRegistry::native();
    for plan in [
        Plan::from_json(DEFAULT_PROGRAM_JSON).unwrap(),
        Plan::from_value(with_opaque_gateways(DEFAULT_PROGRAM_JSON)).unwrap(),
    ] {
        let report = Validator::new(&registry, &default_program_catalog()).validate(&plan);
        assert!(report.is_valid(), "{}", report);
        let frame = Frame::compute(&plan, &registry, &default_program_slot_name).unwrap();
        assert!(R3
            .apply(&RewriteContext::new(&registry, &frame, &report), &plan)
            .is_none());
    }
}

// R3 in the pipeline.

#[test]
fn a_fold_the_frame_cannot_run_is_skipped_and_recorded() {
    // Folding the split would connect the opaque host straight to the output, an edge the frame built at boot does
    // not have: the host's admission point leads to the slot that runs the split.
    let fixture = Fixture::new(plan(
        vec![
            source("src", "otlp"),
            opaque("h"),
            split("s", json!({"const": true})),
            emit("yes", "o0"),
            drop_node("no"),
        ],
        &[
            "src.metrics -> h.in",
            "h.out -> s.in",
            "s.true -> yes.in",
            "s.false -> no.in",
        ],
        1,
    ));
    let compiled = compile(&fixture.plan, &fixture.report, &fixture.registry, &fixture.frame).unwrap();
    assert_eq!(*compiled.plan, fixture.plan);
    // The logical fold is skipped; the decision that runs the split still folds its constant predicate, inside the
    // compiled program, which needs no edge the frame lacks.
    assert_eq!(compiled.rewrites.len(), 2, "{:#?}", compiled.rewrites);
    let record = &compiled.rewrites[0];
    assert_eq!(record.id, RewriteId::R3);
    assert_eq!(record.nodes, ids(&["s", "no"]));
    let RewriteOutcome::Skipped { reason } = &record.outcome else {
        panic!("the fold is skipped: {:?}", record);
    };
    assert!(
        reason.starts_with("the rewritten plan would not fit the frame built at boot: "),
        "{}",
        reason
    );
    let folded = &compiled.rewrites[1];
    assert_eq!(folded.id, RewriteId::R3);
    assert_eq!(folded.outcome, RewriteOutcome::Applied);
    assert_eq!(folded.nodes, ids(&["s"]));
    assert!(compiled.converged);
}

#[test]
fn explain_lists_an_applied_fold_with_its_justification_and_telemetry() {
    let fixture = Fixture::new(plan(
        vec![
            source("src", "otlp"),
            kernel("k"),
            split("s", json!({"const": true})),
            emit("yes", "o0"),
            drop_node("no"),
        ],
        &[
            "src.metrics -> k.in",
            "k.out -> s.in",
            "s.true -> yes.in",
            "s.false -> no.in",
        ],
        1,
    ));
    let compiled = compile(&fixture.plan, &fixture.report, &fixture.registry, &fixture.frame).unwrap();
    let r3 = compiled
        .rewrites
        .iter()
        .filter(|record| record.id == RewriteId::R3)
        .collect::<Vec<_>>();
    assert_eq!(r3.len(), 1);
    assert!(r3[0].is_applied());
    assert!(compiled.plan.node(&id("s")).is_none());

    let text = Explain::of(&fixture.plan, &fixture.registry, &fixture.report)
        .with_rewrites(&compiled.rewrites)
        .to_text();
    let expected = "  R3 applied: s, no\n\
         \x20   justification constant: the predicate of `s` is the constant `true`, so every record leaves by `true`, \
         and `false` carries nothing; the split becomes an edge from `k.out` to `yes.in`; unreachable: `no` receives \
         records only through `s.false`, so nothing reaches it; exits stay: no source, fence, stateful or dynamic \
         operator, scope port, or declared output is removed; capacity: not relied on; every port left carries what it \
         carried, in the same order, so the plan delivers, and under any capacity bound refuses, exactly what it did\n\
         \x20   telemetry removed s: `plan_node_events_total{component_id:s}` for ports `true` and `false`: the split \
         is folded, and what its live port counted is counted at no node\n\
         \x20   telemetry removed no: `plan_loss_total{component_id:no, class:policy}`, zero before: nothing reaches \
         it\n";
    assert!(text.contains(expected), "{}", text);
}

// Random plans.

/// Checks what every application of R3 to a random plan must keep, whatever the trace.
fn check_obligations(case: &Case, optimized: &crate::compile::Optimized) -> Result<(), TestCaseError> {
    prop_assert!(optimized.converged, "R3 did not converge: {:?}", optimized.records);
    for record in &optimized.records {
        prop_assert_eq!(record.id, RewriteId::R3);
        prop_assert!(!record.capacity_rule);
        prop_assert!(!record.justification.is_empty());
        if let RewriteOutcome::Skipped { reason } = &record.outcome {
            prop_assert!(!reason.is_empty());
        }
    }
    let rewritten = &optimized.plan;
    let registry = &case.registry;

    // The rewritten plan is valid and runs in the boot frame.
    let ctx = RewriteContext::new(registry, &case.frame, &case.report);
    let placement =
        place_unvalidated(&case.plan, registry, &case.frame).map_err(|error| TestCaseError::fail(error.to_string()))?;
    let (report, _) = check(&ctx, &placement, rewritten).map_err(TestCaseError::fail)?;

    // Outputs and scopes never change; sources and fences stay as they were.
    prop_assert_eq!(&rewritten.outputs, &case.plan.outputs);
    prop_assert_eq!(&rewritten.scopes, &case.plan.scopes);
    for node in &case.plan.nodes {
        let op = registry.configure(node.op.as_str(), &node.params).unwrap();
        if op.contract().boundary.is_fence() {
            prop_assert_eq!(rewritten.node(&node.id), Some(node), "fixed node `{}`", node.id);
        }
    }
    // R3 only removes nodes, and renumbers merges: every node left is one of the original's, unchanged but for a
    // merge's number of inputs.
    for node in &rewritten.nodes {
        let original = case.plan.node(&node.id);
        prop_assert!(original.is_some(), "`{}` is new", node.id);
        let original = original.unwrap();
        if node != original {
            prop_assert_eq!(node.op.as_str(), names::MERGE, "`{}` changed", node.id);
            let inputs = |node: &Node| node.params.get("inputs").and_then(Value::as_u64);
            prop_assert!(inputs(node) < inputs(original), "`{}` gained inputs", node.id);
        }
    }

    // A fixed point: R3 proposes nothing more, after what the pipeline skipped.
    let ctx = ctx.with(&report, &optimized.records);
    prop_assert!(R3.apply(&ctx, rewritten).is_none(), "R3 still applies");
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn property_test_r3_changes_no_output_of_random_plans(
        shape in arb_shape(arb_split_predicate()),
        batches in arb_batches(),
    ) {
        let case = Case::new(&shape)?;
        let optimized = case.optimize(&R3, None);
        check_obligations(&case, &optimized)?;
        let (before, after) = check_equivalent(&case, &case.plan, &optimized.plan, &batches)?;
        // Every kept port carries what it carried, in order, so every output receives its records in the same order.
        check_same_order(&case.plan, &before, &after)?;

        // Only nodes that received nothing are removed, and every node left receives what it received: so removed
        // nodes lost nothing, and the others lose what they lost.
        for node in &case.plan.nodes {
            if optimized.plan.node(&node.id).is_some() {
                prop_assert_eq!(before.loss(&node.id), after.loss(&node.id), "loss of `{}`", node.id);
            } else {
                let lost = before.loss(&node.id);
                prop_assert!(lost.is_empty(), "removed `{}` lost {:?}", node.id, lost);
            }
        }
    }

    #[test]
    fn property_test_r3_under_capacity_pressure_delivers_and_refuses_what_the_original_does(
        shape in arb_shape(arb_split_predicate()),
        batches in arb_batches(),
        choices in arb_bounds(),
    ) {
        let case = Case::new(&shape)?;
        let optimized = case.optimize(&R3, None);
        check_obligations(&case, &optimized)?;

        // Every kept port carries what it carried, in the same order, so whatever a bound cuts, even inside a batch,
        // exactly the same records are delivered and refused.
        let arbitrary = arbitrary_bounds(&case.plan, &batches, &choices);
        check_under_capacity(&case, &case.plan, &optimized.plan, &batches, &arbitrary)?;
        let unbounded = case.run(&trace(&case.plan, &batches, &[]))?;
        let aligned = batch_aligned_bounds(&case.plan, &unbounded, &batches, &choices);
        check_under_capacity(&case, &case.plan, &optimized.plan, &batches, &aligned)?;
    }
}

#[test]
fn random_plans_exercise_every_part_of_r3() {
    // The property tests hold vacuously unless R3 applies to many generated plans, in every form, and their bounds
    // put the outputs under capacity pressure, including bounds that cut inside a batch of a plan R3 rewrites.
    let mut runner = TestRunner::deterministic();
    let strategy = (arb_shape(arb_split_predicate()), arb_batches(), arb_bounds());
    let (mut plans, mut applied, mut skipped, mut merges, mut kinds, mut literals) = (0, 0, 0, 0, 0, 0);
    let (mut pressured, mut cut_inside) = (0, 0);
    for _ in 0..300 {
        let (shape, batches, choices) = strategy.new_tree(&mut runner).unwrap().current();
        let case = Case::new(&shape).unwrap();
        let optimized = case.optimize(&R3, None);
        plans += 1;
        let records = optimized.records.iter().collect::<Vec<&RewriteRecord>>();
        if records.iter().any(|record| record.is_applied()) {
            applied += 1;
        }
        skipped += records.iter().filter(|record| !record.is_applied()).count();
        let justified = |text: &str| {
            records
                .iter()
                .filter(|record| record.justification.contains(text))
                .count()
        };
        merges += justified("merges: ");
        kinds += justified("for every kind its input can carry");
        literals += justified("literal constants");

        let unbounded = case.run(&trace(&case.plan, &batches, &[])).unwrap();
        let aligned = batch_aligned_bounds(&case.plan, &unbounded, &batches, &choices);
        let bounded = case.run(&trace(&case.plan, &batches, &aligned)).unwrap();
        if bounded.refused_outputs().any(|(_, refused)| !refused.is_empty()) {
            pressured += 1;
        }
        // An arbitrary bound cuts inside a batch when an output both accepts and refuses records of it.
        let arbitrary = arbitrary_bounds(&case.plan, &batches, &choices);
        let bounded = case.run(&trace(&case.plan, &batches, &arbitrary)).unwrap();
        let cut = bounded.refused_outputs().any(|(output, refused)| {
            refused.iter().any(|record| {
                bounded
                    .output(output)
                    .iter()
                    .any(|accepted| accepted.admitted_at == record.admitted_at)
            })
        });
        if cut && optimized.changed() {
            cut_inside += 1;
        }
    }
    assert!(applied * 3 > plans, "R3 applied to {} of {} plans", applied, plans);
    assert!(skipped > 0, "no application was skipped");
    assert!(merges > 0, "no application renumbered a merge");
    assert!(kinds > 0, "no application folded a split decided by kinds");
    assert!(
        literals > 0,
        "no application folded a junction decided by literal constants"
    );
    assert!(
        pressured * 5 > plans,
        "only {} of {} traces refused anything",
        pressured,
        plans
    );
    assert!(
        cut_inside * 20 > plans,
        "only {} of {} traces cut inside a batch of a plan R3 rewrites",
        cut_inside,
        plans
    );
}

#[test]
fn a_bound_inside_a_batch_cuts_the_same_records_after_a_fold() {
    // Records matching `s` pass the constant filter `f` on their way to the merge, the others go straight to it. The
    // fold shortens their path, and may renumber the merge's inputs, but the merge passes its inputs on in port order
    // whatever the paths' lengths: a bound of one record keeps the same record before and after.
    // (`f`'s predicate, its edges and the other port's, the merge's inputs, the record the bound keeps)
    let cases: [(bool, [&str; 3], usize, &str); 3] = [
        (true, ["f.true -> m.0", "f.false -> f_d.in", "s.false -> m.1"], 2, "a"),
        (true, ["f.true -> m.1", "f.false -> f_d.in", "s.false -> m.0"], 2, "b"),
        // `f.true` carries nothing, so the merge loses its first input, and the others are renumbered.
        (false, ["f.true -> m.0", "f.false -> m.2", "s.false -> m.1"], 3, "b"),
    ];
    for (constant, edges, inputs, kept) in cases {
        let mut nodes = vec![
            source("src", "otlp"),
            kernel("k"),
            split("s", json!({"name": {"eq": "a"}})),
            split("f", json!({"const": constant})),
            node("m", "merge", json!({"inputs": inputs})),
            emit("e", "o0"),
        ];
        if constant {
            nodes.push(drop_node("f_d"));
        }
        let all_edges = [
            "src.metrics -> k.in",
            "k.out -> s.in",
            "s.true -> f.in",
            "m.out -> e.in",
        ]
        .into_iter()
        .chain(edges)
        .collect::<Vec<_>>();
        let case = Case::from_plan(plan(nodes, &all_edges, 1), Vec::new()).unwrap();
        let optimized = case.optimize(&R3, None);
        assert!(optimized.plan.node(&id("f")).is_none(), "{:?}", optimized.records);
        let batches = vec![(
            "src",
            vec![
                saluki_core::data_model::event::metric::Metric::gauge("a", 1.0),
                saluki_core::data_model::event::metric::Metric::gauge("b", 2.0),
            ],
        )];
        let bound = [crate::compile::rewrite::random_plans::Bound {
            position: 0,
            output: "o0".parse().unwrap(),
            limit: 1,
        }];
        let delivered = |plan: &Plan| {
            let run = case.run(&trace(plan, &batches, &bound)).unwrap();
            run.output(&"o0".parse().unwrap())
                .iter()
                .map(|record| record.metric.context().name().to_string())
                .collect::<Vec<_>>()
        };
        assert_eq!(delivered(&case.plan), vec![kept], "{:?}", edges);
        assert_eq!(delivered(&optimized.plan), vec![kept], "{:?}", edges);
        check_under_capacity(&case, &case.plan, &optimized.plan, &batches, &bound).unwrap();
    }
}

#[test]
fn literal_values_follow_three_valued_logic() {
    let name = Predicate::Name(crate::predicate::StringMatch::Eq("a".into()));
    let flag = Predicate::Flag(crate::ir::Operand::literal(false));
    // (predicate, expected)
    let cases = [
        (Predicate::Const(true), Some(true)),
        (Predicate::All(vec![]), Some(true)),
        (Predicate::Any(vec![]), Some(false)),
        (Predicate::All(vec![name.clone(), Predicate::Const(false)]), Some(false)),
        (Predicate::All(vec![name.clone(), Predicate::Const(true)]), None),
        (Predicate::Any(vec![name.clone(), Predicate::Const(true)]), Some(true)),
        (
            Predicate::Any(vec![Predicate::Const(false), Predicate::Const(false)]),
            Some(false),
        ),
        (
            Predicate::Not(Box::new(Predicate::Any(vec![Predicate::Const(true)]))),
            Some(false),
        ),
        // Configuration leaves and kinds are never literal constants.
        (flag.clone(), None),
        (Predicate::All(vec![flag, Predicate::Const(true)]), None),
        (Predicate::KindIn(crate::model::KindSet::ALL), None),
    ];
    for (predicate, expected) in cases {
        assert_eq!(literal_value(&predicate), expected, "{:?}", predicate);
    }
}

#[test]
fn port_types_of_constant_splits_carry_nothing_on_the_port_never_taken() {
    // R3 relies on validation's port types: a literal constant leaves its other port without kinds.
    let fixture = Fixture::new(plan(
        vec![
            source("src", "otlp"),
            kernel("k"),
            split("s", json!({"any": [{"const": true}, {"has_tag": "a"}]})),
            emit("yes", "o0"),
            drop_node("no"),
        ],
        &[
            "src.metrics -> k.in",
            "k.out -> s.in",
            "s.true -> yes.in",
            "s.false -> no.in",
        ],
        1,
    ));
    let port = |name: &str| fixture.report.port_type(&name.parse().unwrap()).unwrap();
    assert_eq!(port("s.true"), port("k.out"));
    assert_eq!(port("s.false").kinds, crate::model::KindSet::EMPTY);
    let revalidated = revalidate(&fixture.registry, &fixture.apply().unwrap().plan);
    assert!(revalidated.is_valid(), "{}", revalidated);
}
