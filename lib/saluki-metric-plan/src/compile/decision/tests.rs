use std::sync::Arc;

use proptest::{collection::vec, prelude::*, sample::select};
use saluki_core::data_model::event::{
    metric::{context::Context, Metric},
    service_check::{CheckStatus, ServiceCheck},
    Event,
};
use serde_json::{json, Value};

use super::*;
use crate::{
    compile::{compile, CompileError, DecisionIndex, EntrySource, ExitIndex, Frame, RegisterId, Route, Step},
    ir::{NativeComponentId, NodeId, Operand, OutputId, Plan},
    model::{Kind, PortType},
    operators::OperatorRegistry,
    predicate::NoBindings,
    testing::{
        arb_metrics_with_kind_siblings, arb_predicate, default_program_catalog, default_program_slot_name,
        DEFAULT_PROGRAM_JSON,
    },
    validate::{CatalogOutput, MemoryCatalog, Validator},
};

fn id(name: &str) -> NodeId {
    name.parse().unwrap()
}

fn port(name: &str) -> PortName {
    name.parse().unwrap()
}

fn exits(exits: &[u16]) -> ExitSet {
    let mut set = ExitSet::EMPTY;
    for &exit in exits {
        set.insert(DecisionExit::new(exit));
    }
    set
}

fn event(name: &'static str) -> Event {
    Event::Metric(Metric::gauge(Context::from_static_name(name), 1.0))
}

fn counter(name: &'static str) -> Event {
    Event::Metric(Metric::counter(Context::from_static_name(name), 1.0))
}

fn check() -> Event {
    Event::ServiceCheck(ServiceCheck::new("check", CheckStatus::Ok))
}

fn predicate(value: Value) -> Predicate {
    serde_json::from_value(value).unwrap()
}

/// Interns the predicate `value` of `node` into `table`.
fn intern(table: &mut PredicateTable, node: &str, value: Value) -> PredicateIndex {
    let predicate = predicate(value);
    let compiled = Arc::new(predicate.compile(&NoBindings).unwrap());
    table.intern(&id(node), &predicate, compiled).unwrap()
}

fn split(node: &str, predicate: PredicateIndex, branches: [Branch; 2]) -> DecisionNode {
    DecisionNode {
        node: id(node),
        ports: vec![port("true"), port("false")],
        kind: DecisionKind::Split { predicate },
        branches: branches.to_vec(),
    }
}

fn tee(node: &str, branches: Vec<Branch>) -> DecisionNode {
    DecisionNode {
        node: id(node),
        ports: (0..branches.len()).map(|index| port(&index.to_string())).collect(),
        kind: DecisionKind::Tee,
        branches,
    }
}

fn to_exit(exit: u16) -> Branch {
    Branch::Exit(DecisionExit::new(exit))
}

/// Builds a decision of `nodes`, whose exits are numbered as the nodes' branches number them.
fn decision(nodes: Vec<DecisionNode>, predicates: PredicateTable) -> Decision {
    let mut targets = Vec::new();
    for node in &nodes {
        for (branch, port) in node.branches.iter().zip(&node.ports) {
            if let Branch::Exit(exit) = branch {
                targets.push((
                    exit.get(),
                    ExitTarget {
                        node: node.node.clone(),
                        port: port.clone(),
                    },
                ));
            }
        }
    }
    targets.sort_by_key(|(exit, _)| *exit);
    Decision {
        nodes,
        exits: targets.into_iter().map(|(_, target)| target).collect(),
        predicates,
        memoized: false,
        rewrites: Vec::new(),
    }
}

/// `t` (a tee) sends port `0` to `s` (a split on `name eq a`, `true` to exit 0, `false` to exit 1) and port `1` to
/// exit 2.
fn tee_over_split() -> Decision {
    let mut predicates = PredicateTable::new();
    let index = intern(&mut predicates, "s", json!({"name": {"eq": "a"}}));
    decision(
        vec![
            tee("t", vec![Branch::Node(1), to_exit(2)]),
            split("s", index, [to_exit(0), to_exit(1)]),
        ],
        predicates,
    )
}

// Exit sets, counts, and memos.

#[test]
fn exit_sets_hold_every_exit_below_the_capacity() {
    let last = (EXIT_SET_CAPACITY - 1) as u16;
    let set = exits(&[0, 63, 64, last]);
    assert_eq!(set.len(), 4);
    for exit in [0, 63, 64, last] {
        assert!(set.contains(DecisionExit::new(exit)));
    }
    assert!(!set.contains(DecisionExit::new(1)));
    assert_eq!(
        set.iter().map(DecisionExit::get).collect::<Vec<_>>(),
        vec![0, 63, 64, EXIT_SET_CAPACITY - 1]
    );
    assert_eq!(set.last(), Some(DecisionExit::new(last)));
    assert_eq!(exits(&[3, 64]).last(), Some(DecisionExit::new(64)));
    assert_eq!(ExitSet::EMPTY.last(), None);
    assert_eq!(ExitSet::EMPTY.iter().count(), 0);
    assert_eq!(ExitSet::first(3), exits(&[0, 1, 2]));
    assert_eq!(ExitSet::single(DecisionExit::new(70)), exits(&[70]));
    assert!(ExitSet::EMPTY.is_empty());
    assert!(exits(&[1, 2]).intersects(&exits(&[2, 3])));
    assert!(!exits(&[1]).intersects(&exits(&[2])));
    assert_eq!(exits(&[1]).union(exits(&[2])), exits(&[1, 2]));
    assert_eq!(exits(&[1, 2, 70]).intersection(exits(&[2, 3, 70])), exits(&[2, 70]));
}

#[test]
fn exit_set_counts_hold_a_fixed_number_of_distinct_sets() {
    let mut counts = ExitSetCounts::with_capacity(2);
    assert!(counts.is_empty());
    assert!(counts.add(exits(&[0, 2]), 5, 7));
    assert!(counts.add(exits(&[1, 2]), 3, 3));
    assert!(counts.add(exits(&[0, 2]), 1, 2));
    // Full: a third distinct set is not counted, and the others are kept.
    assert!(!counts.add(exits(&[2]), 1, 1));
    let mut counted = counts.iter().collect::<Vec<_>>();
    counted.sort();
    assert_eq!(counted, vec![(exits(&[0, 2]), 6, 9), (exits(&[1, 2]), 3, 3)]);

    counts.clear();
    assert!(counts.is_empty());
    assert_eq!(counts.iter().count(), 0);
    assert!(counts.add(exits(&[2]), 1, 4));
    assert_eq!(counts.iter().collect::<Vec<_>>(), vec![(exits(&[2]), 1, 4)]);
}

#[test]
fn per_port_counts_are_derived_from_counts_per_exit_set() {
    let decision = tee_over_split();
    let mut counts = ExitSetCounts::with_capacity(COUNTED_EXIT_SETS);
    assert!(counts.add(exits(&[0, 2]), 5, 0));
    assert!(counts.add(exits(&[1, 2]), 3, 0));
    assert!(counts.add(exits(&[0, 2]), 1, 0));

    // Ports in node order: `t.0`, `t.1`, `s.true`, `s.false`.
    let mut ports = vec![0; decision.port_count()];
    decision.port_counts(&counts, &mut ports);
    assert_eq!(ports, vec![9, 9, 6, 3]);

    // The compiled function derives the same counts, adding to what the ports hold.
    let function = DecisionFunction::new(&decision).unwrap();
    assert_eq!(function.port_count(), 4);
    function.port_counts(&counts, &mut ports);
    assert_eq!(ports, vec![18, 18, 12, 6]);
}

#[test]
fn the_memo_is_direct_mapped_with_a_power_of_two_capacity() {
    assert_eq!(DecisionMemo::new(0).capacity(), 1);
    assert_eq!(DecisionMemo::new(4096).capacity(), 4096);
    assert_eq!(DecisionMemo::new(5000).capacity(), 4096);

    let mut memo = DecisionMemo::new(1);
    let counter = MemoKey {
        context: 7,
        kind: Kind::Counter,
    };
    let gauge = MemoKey {
        context: 7,
        kind: Kind::Gauge,
    };
    assert_eq!(memo.get(counter), None);
    memo.insert(counter, exits(&[1, 200]));
    assert_eq!(memo.get(counter), Some(exits(&[1, 200])));
    assert_eq!(memo.get(gauge), None);
    // One entry: the gauge's result replaces the counter's.
    memo.insert(gauge, exits(&[2]));
    assert_eq!(memo.get(gauge), Some(exits(&[2])));
    assert_eq!(memo.get(counter), None);
    memo.clear();
    assert_eq!(memo.get(gauge), None);

    // With room, equal contexts of different kinds keep entries of their own.
    let mut memo = DecisionMemo::new(4096);
    memo.insert(counter, exits(&[1]));
    memo.insert(gauge, exits(&[2]));
    assert_eq!(memo.get(counter), Some(exits(&[1])));
    assert_eq!(memo.get(gauge), Some(exits(&[2])));
}

#[test]
fn a_memo_stores_only_the_words_its_decisions_exits_need() {
    // Three words per entry for up to 64 exits (the context, the kind, one word of exits), six for every width.
    assert_eq!(DecisionMemo::footprint(4096, 3), 4096 * 3 * 8);
    assert_eq!(DecisionMemo::footprint(4096, 65), 4096 * 4 * 8);
    assert_eq!(DecisionMemo::footprint(4096, EXIT_SET_CAPACITY), 4096 * 6 * 8);
    assert_eq!(DecisionMemo::footprint(5000, 1), 4096 * 3 * 8);

    let key = MemoKey {
        context: 42,
        kind: Kind::Set,
    };
    let mut memo = DecisionMemo::for_exits(16, 3);
    memo.insert(key, exits(&[2]));
    assert_eq!(memo.get(key), Some(exits(&[2])));
    // A set beyond the memo's width is not memoized, so the lookup misses rather than returning a truncated set.
    memo.insert(key, exits(&[2, 70]));
    assert_eq!(memo.get(key), None);
}

#[test]
fn a_memo_evaluates_only_what_it_does_not_hold() {
    let key = |context| MemoKey {
        context,
        kind: Kind::Counter,
    };
    let mut memo = DecisionMemo::for_exits(1, 3);
    let mut evaluated = 0;
    let mut lookup = |memo: &mut DecisionMemo, context: u64, set: ExitSet| {
        memo.get_or_insert_with(key(context), || {
            evaluated += 1;
            set
        })
    };
    assert_eq!(lookup(&mut memo, 1, exits(&[0])), exits(&[0]));
    // A hit returns what the entry holds, without evaluating.
    assert_eq!(lookup(&mut memo, 1, exits(&[1])), exits(&[0]));
    // A miss evaluates and replaces the entry; a set wider than the memo is returned, and not memoized.
    assert_eq!(lookup(&mut memo, 2, exits(&[2])), exits(&[2]));
    assert_eq!(lookup(&mut memo, 3, exits(&[1, 70])), exits(&[1, 70]));
    assert_eq!(lookup(&mut memo, 3, exits(&[1])), exits(&[1]));
    assert_eq!(evaluated, 4);
    assert_eq!(memo.get(key(3)), Some(exits(&[1])));
    assert_eq!(memo.get(key(2)), None);
}

// R3: bound constants.

#[test]
fn bound_constants_fold_by_three_valued_logic() {
    let binding = || crate::ir::BindingName::new("test.flag").unwrap();
    let flag = |resolved: Option<bool>| match resolved {
        Some(value) => Predicate::Flag(Operand::config(binding()).with_resolved(value)),
        None => Predicate::Flag(Operand::config(binding())),
    };
    let name = predicate(json!({"name": {"eq": "a"}}));
    let cases: Vec<(Predicate, Option<bool>)> = vec![
        (Predicate::Const(true), Some(true)),
        (Predicate::Flag(Operand::literal(false)), Some(false)),
        (flag(Some(true)), Some(true)),
        // A binding without a value decides nothing.
        (flag(None), None),
        (predicate(json!({"is_empty": []})), Some(true)),
        (predicate(json!({"is_empty": ["a"]})), Some(false)),
        (Predicate::All(vec![flag(Some(false)), name.clone()]), Some(false)),
        (Predicate::All(vec![flag(Some(true)), name.clone()]), None),
        (
            Predicate::Any(vec![predicate(json!({"is_empty": []})), name.clone()]),
            Some(true),
        ),
        (Predicate::Any(vec![flag(Some(false)), name.clone()]), None),
        (Predicate::Not(Box::new(flag(Some(true)))), Some(false)),
        (Predicate::All(vec![]), Some(true)),
        (Predicate::Any(vec![]), Some(false)),
        (name, None),
        (predicate(json!({"kind_in": ["counter"]})), None),
    ];
    for (predicate, expected) in cases {
        assert_eq!(fold_bound_constants(&predicate), expected, "{:?}", predicate);
    }
}

// R2: shared predicates.

#[test]
fn equal_canonical_predicates_share_one_entry_that_lists_every_split() {
    let mut table = PredicateTable::new();
    let first = intern(
        &mut table,
        "s1",
        json!({"any": [{"name": {"eq": "a"}}, {"has_tag": "x"}]}),
    );
    // Equal in canonical form: the operands of `any` are sorted.
    let second = intern(
        &mut table,
        "s2",
        json!({"any": [{"has_tag": "x"}, {"name": {"eq": "a"}}]}),
    );
    let other = intern(&mut table, "s3", json!({"has_tag": "x"}));
    let folded = intern(&mut table, "s4", json!({"all": [{"flag": false}, {"has_tag": "x"}]}));
    // Equality is structural: two bindings with equal values are two predicates, one binding with its value one.
    let enabled = intern(
        &mut table,
        "s5",
        json!({"flag": {"config": "test.enabled", "resolved": true}}),
    );
    let mirrored = intern(
        &mut table,
        "s6",
        json!({"flag": {"config": "test.mirrored", "resolved": true}}),
    );
    let enabled_again = intern(
        &mut table,
        "s7",
        json!({"flag": {"config": "test.enabled", "resolved": true}}),
    );
    assert_eq!(first, second);
    assert_ne!(first, other);
    assert_ne!(enabled, mirrored);
    assert_eq!(enabled, enabled_again);
    assert_eq!(table.get(enabled).unwrap().folded, Some(true));
    assert_eq!(table.get(mirrored).unwrap().folded, Some(true));
    assert_eq!(table.get(enabled).unwrap().nodes, vec![id("s5"), id("s7")]);
    assert_eq!(table.len(), 5);
    assert_eq!(table.get(first).unwrap().nodes, vec![id("s1"), id("s2")]);
    assert_eq!(table.get(first).unwrap().folded, None);
    assert_eq!(table.get(other).unwrap().nodes, vec![id("s3")]);
    assert_eq!(table.get(folded).unwrap().folded, Some(false));
    assert_eq!(
        table.reads(),
        FieldSet::of(&[Field::Name, Field::Tags]),
        "the predicates read the name and the tags"
    );
}

// Decision functions.

#[test]
fn a_decision_function_computes_what_the_reference_walk_computes() {
    let decision = tee_over_split();
    let function = DecisionFunction::new(&decision).unwrap();
    let mut scratch = DecisionScratch::default();
    for (event, expected) in [
        (event("a"), exits(&[0, 2])),
        (event("b"), exits(&[1, 2])),
        // Events other than metrics take every split's `false` port.
        (check(), exits(&[1, 2])),
    ] {
        assert_eq!(decision.evaluate(&event, &mut scratch), expected);
        assert_eq!(function.evaluate(&event), expected);
    }
    assert_eq!(function.exit_count(), 3);
    assert_eq!(function.constant(), None);
    assert!(decision.memoizable());
    assert_eq!(decision.evaluation_cost(|_| 10), 10, "one split to evaluate");
}

#[test]
fn decisions_without_a_live_predicate_are_constant_functions() {
    // A tee alone: every event, metric or not, takes every exit.
    let tees = decision(vec![tee("t", vec![to_exit(0), to_exit(1)])], PredicateTable::new());
    let function = DecisionFunction::new(&tees).unwrap();
    assert_eq!(function.constant(), Some(exits(&[0, 1])));
    assert_eq!(function.evaluate(&check()), exits(&[0, 1]));
    assert_eq!(tees.evaluation_cost(|_| 10), 0);

    // A split R3 folded to `true`: every metric takes `true`, but events other than metrics still take `false`, so the
    // function depends on the event's type, and is not constant.
    let mut predicates = PredicateTable::new();
    let index = intern(&mut predicates, "s", json!({"flag": true}));
    let folded = decision(vec![split("s", index, [to_exit(0), to_exit(1)])], predicates);
    let function = DecisionFunction::new(&folded).unwrap();
    assert_eq!(function.constant(), None);
    assert_eq!(function.evaluate(&event("a")), exits(&[0]));
    assert_eq!(function.evaluate(&check()), exits(&[1]));
    assert!(folded.memoizable());
    assert_eq!(folded.evaluation_cost(|_| 10), 0, "nothing is left to evaluate");

    let mut predicates = PredicateTable::new();
    let index = intern(&mut predicates, "k", json!({"kind_in": ["counter"]}));
    let kinds = decision(vec![split("k", index, [to_exit(0), to_exit(1)])], predicates);
    let function = DecisionFunction::new(&kinds).unwrap();
    assert_eq!(function.evaluate(&counter("a")), exits(&[0]));
    assert_eq!(function.evaluate(&event("a")), exits(&[1]));
}

#[test]
fn a_predicate_two_splits_share_is_evaluated_once_per_event() {
    // `t` copies to `s1` and `s2`, which share `name prefix a.`, and to `s3`, whose predicate is folded.
    let mut predicates = PredicateTable::new();
    let shared = intern(&mut predicates, "s1", json!({"name": {"prefix": "a."}}));
    assert_eq!(intern(&mut predicates, "s2", json!({"name": {"prefix": "a."}})), shared);
    let folded = intern(
        &mut predicates,
        "s3",
        json!({"all": [{"flag": false}, {"has_tag": "x"}]}),
    );
    let decision = decision(
        vec![
            tee("t", vec![Branch::Node(1), Branch::Node(2), Branch::Node(3)]),
            split("s1", shared, [to_exit(0), to_exit(1)]),
            split("s2", shared, [to_exit(2), to_exit(3)]),
            split("s3", folded, [to_exit(4), to_exit(5)]),
        ],
        predicates,
    );
    let function = DecisionFunction::new(&decision).unwrap();
    // The shared predicate is tested by two instructions, which share one cache slot; the folded one by none. Its cost
    // counts once, from the first `split` using it, and the folded one's not at all.
    assert_eq!(function.cache_slots(), 1);
    assert_eq!(
        decision.evaluation_cost(|node| if node == &id("s1") { 7 } else { 100 }),
        7
    );
    let mut scratch = DecisionScratch::default();
    for (event, expected) in [
        (event("a.x"), exits(&[0, 2, 5])),
        (event("b.x"), exits(&[1, 3, 5])),
        (check(), exits(&[1, 3, 5])),
    ] {
        assert_eq!(function.evaluate(&event), expected);
        assert_eq!(decision.evaluate(&event, &mut scratch), expected);
    }
}

#[test]
fn a_memoized_function_returns_what_its_memo_holds_for_the_context_and_kind() {
    let decision = tee_over_split();
    let function = DecisionFunction::new(&decision).unwrap();
    let mut memo = DecisionMemo::for_exits(64, decision.exits.len());
    let a = event("a");
    assert_eq!(function.evaluate_memoized(&a, &mut memo), exits(&[0, 2]));
    let key = MemoKey::of(a.try_as_metric().unwrap());
    assert_eq!(memo.get(key), Some(exits(&[0, 2])), "a miss memoizes its result");

    // A hit is one lookup: the function returns the memoized set without evaluating the predicate. (A memo serves one
    // decision, so its sets are always the function's; this one is planted to observe the lookup.)
    memo.insert(key, exits(&[1]));
    assert_eq!(function.evaluate_memoized(&a, &mut memo), exits(&[1]));
    // Another kind of the same context is another key.
    assert_eq!(function.evaluate_memoized(&counter("a"), &mut memo), exits(&[0, 2]));
    // Events other than metrics have no context, and bypass the memo.
    assert_eq!(function.evaluate_memoized(&check(), &mut memo), exits(&[1, 2]));
}

#[test]
fn malformed_decisions_do_not_compile_to_functions() {
    let mut predicates = PredicateTable::new();
    let index = intern(&mut predicates, "s", json!({"name": {"eq": "a"}}));
    let with = |nodes: Vec<DecisionNode>, exits: usize| Decision {
        exits: (0..exits)
            .map(|exit| ExitTarget {
                node: id("t"),
                port: port(&exit.to_string()),
            })
            .collect(),
        ..decision(nodes, predicates.clone())
    };
    let mut three_ports = split("s", index, [to_exit(0), to_exit(1)]);
    three_ports.ports.push(port("other"));
    three_ports.branches.push(to_exit(2));
    let cases = vec![
        (with(vec![], 0), "it has no nodes"),
        (
            with(vec![tee("t", vec![to_exit(0), to_exit(0)])], 1),
            "exit 0 is reached by more than one branch",
        ),
        (
            with(vec![tee("t", vec![to_exit(0)])], 2),
            "exit 1 is reached by no branch",
        ),
        (
            with(vec![tee("t", vec![to_exit(3)])], 1),
            "node `t` leads to exit 3, which it lacks",
        ),
        (
            with(
                vec![
                    tee("t", vec![Branch::Node(1), Branch::Node(1)]),
                    tee("u", vec![to_exit(0)]),
                ],
                1,
            ),
            "node 1 is entered by more than one branch",
        ),
        (
            with(vec![tee("t", vec![Branch::Node(0)])], 0),
            "node `t` leads to node 0, which is not a later node of the decision",
        ),
        (
            with(vec![tee("t", vec![to_exit(0)]), tee("u", vec![to_exit(1)])], 2),
            "node 1 is entered by no branch",
        ),
        (with(vec![three_ports], 3), "split `s` must have two ports"),
        (
            with(vec![split("s", PredicateIndex::new(7), [to_exit(0), to_exit(1)])], 2),
            "split `s` has no predicate 7",
        ),
    ];
    for (decision, problem) in cases {
        let error = DecisionFunction::new(&decision).unwrap_err();
        assert_eq!(error.problem, problem);
    }
    let error = DecisionFunction::new(&with(vec![], 0)).unwrap_err();
    assert_eq!(error.to_string(), "decision `<none>` is malformed: it has no nodes");
}

// Grouping, in compiled programs.

/// A plan whose nodes share one native provenance, so that R1 keeps them in one slot, fed by an OTLP source, with one
/// output `out`.
fn grouped_plan(nodes: &[(&str, &str, Value)], edges: &[(&str, &str)]) -> Plan {
    let mut all = vec![json!({"id": "src", "op": "source", "params": {"binding": "otlp"}})];
    all.extend(
        nodes
            .iter()
            .map(|(id, op, params)| json!({"id": id, "op": op, "params": params, "provenance": {"native": "policy"}})),
    );
    let edges = edges
        .iter()
        .map(|(from, to)| json!({"from": from, "to": to}))
        .collect::<Vec<_>>();
    Plan::from_value(json!({
        "ir_version": 1,
        "nodes": all,
        "edges": edges,
        "outputs": [{"id": "out", "accepts": {"kinds": "all", "timestamps": "mixed"},
                     "fingerprint": format!("sha256:{}", "7".repeat(64))}],
    }))
    .unwrap()
}

fn compiled(plan: &Plan) -> Result<crate::compile::CompiledPlan, CompileError> {
    let registry = OperatorRegistry::native();
    let catalog = MemoryCatalog::new().with_output(
        OutputId::new("out").unwrap(),
        CatalogOutput {
            accepts: PortType::ANY,
            fingerprint: format!("sha256:{}", "7".repeat(64)).parse().unwrap(),
            recipients: Vec::new(),
            legacy_fingerprint: None,
        },
    );
    let report = Validator::new(&registry, &catalog).validate(plan);
    assert!(report.is_valid(), "{}", report);
    let namer =
        |nodes: &[NodeId]| NativeComponentId::new(format!("slot_{}", nodes[0].local())).map_err(|e| e.to_string());
    let frame = Frame::compute(plan, &registry, &namer).unwrap();
    compile(plan, &report, &registry, &frame)
}

fn nodes_of(decision: &Decision) -> Vec<String> {
    decision.nodes.iter().map(|node| node.node.to_string()).collect()
}

fn exits_of(decision: &Decision) -> Vec<String> {
    decision
        .exits
        .iter()
        .map(|exit| format!("{}.{}", exit.node, exit.port))
        .collect()
}

fn drop_route(node: &str) -> Route {
    Route::Drop(id(node))
}

fn register(register: u16) -> Route {
    Route::Register(RegisterId::new(register))
}

#[test]
fn splits_and_tees_joined_by_edges_are_one_decision_and_a_map_ends_it() {
    // `t` copies to `s1`, `s2`, and host enrichment `h`; `s2`'s `true` port leads to `s3`; below `h`, `s4`.
    let plan = grouped_plan(
        &[
            ("t", "tee", json!({"ports": 3})),
            ("s1", "split", json!({"predicate": {"intake_name": {"eq": "a"}}})),
            ("e", "emit", json!({"output": "out"})),
            ("d1", "drop", json!({})),
            ("s2", "split", json!({"predicate": {"kind_in": ["gauge"]}})),
            ("d2", "drop", json!({})),
            ("s3", "split", json!({"predicate": {"name": {"prefix": "b"}}})),
            ("d3", "drop", json!({})),
            ("d4", "drop", json!({})),
            ("h", "host_enrichment", json!({})),
            ("s4", "split", json!({"predicate": {"kind_in": ["counter"]}})),
            ("d6", "drop", json!({})),
            ("d7", "drop", json!({})),
        ],
        &[
            ("src.metrics", "t.in"),
            ("t.0", "s1.in"),
            ("s1.true", "e.in"),
            ("s1.false", "d1.in"),
            ("t.1", "s2.in"),
            ("s2.true", "s3.in"),
            ("s2.false", "d2.in"),
            ("s3.true", "d3.in"),
            ("s3.false", "d4.in"),
            ("t.2", "h.in"),
            ("h.out", "s4.in"),
            ("s4.true", "d6.in"),
            ("s4.false", "d7.in"),
        ],
    );
    let compiled = compiled(&plan).unwrap();
    let [program] = compiled.programs.as_slice() else {
        panic!("one slot")
    };
    let decisions = program.decisions();
    assert_eq!(decisions.len(), 2);

    // The group, depth first; its exits, node by node and port by port, are every port leading out of it.
    assert_eq!(nodes_of(&decisions[0]), vec!["t", "s1", "s2", "s3"]);
    assert_eq!(
        exits_of(&decisions[0]),
        vec!["t.2", "s1.true", "s1.false", "s2.false", "s3.true", "s3.false"]
    );
    assert_eq!(
        decisions[0]
            .nodes
            .iter()
            .map(|node| node.branches.clone())
            .collect::<Vec<_>>(),
        vec![
            vec![Branch::Node(1), Branch::Node(2), to_exit(0)],
            vec![to_exit(1), to_exit(2)],
            vec![Branch::Node(3), to_exit(3)],
            vec![to_exit(4), to_exit(5)],
        ]
    );
    // The map ends the group: `s4` is a decision of its own. The group's three splits cost 36 ns by R1's cost model,
    // at least `MEMO_COST_FLOOR`, and are memoized; `s4`, on the kind alone, costs 6, and is not.
    assert_eq!(nodes_of(&decisions[1]), vec!["s4"]);
    assert!(decisions[0].memoized);
    assert!(!decisions[1].memoized);

    // One step runs the whole group; its other nodes are no steps.
    let decide = |decision: u16, input: u16, targets: Vec<Route>| Step::Decide {
        decision: DecisionIndex::new(decision),
        input: RegisterId::new(input),
        targets,
    };
    assert_eq!(
        program.entries()[0].steps,
        vec![
            decide(
                0,
                0,
                vec![
                    register(1),
                    register(2),
                    drop_route("d1"),
                    drop_route("d2"),
                    drop_route("d3"),
                    drop_route("d4"),
                ],
            ),
            Step::Exit {
                register: RegisterId::new(2),
                exit: ExitIndex::new(0),
                entry: EntrySource::None,
            },
            Step::Map {
                kernel: crate::compile::KernelIndex::new(0),
                register: RegisterId::new(1),
            },
            decide(1, 1, vec![drop_route("d6"), drop_route("d7")]),
        ]
    );
    assert_eq!(
        compiled
            .rewrites
            .iter()
            .map(|record| (record.id, record.nodes.clone()))
            .collect::<Vec<_>>(),
        vec![(RewriteId::R4, vec![id("t"), id("s1"), id("s2"), id("s3")])]
    );
}

#[test]
fn a_merge_ends_a_decision() {
    // `s`'s ports join again in `m`, and `k` follows: a decision is a tree, so `k` is a decision of its own.
    let plan = grouped_plan(
        &[
            ("s", "split", json!({"predicate": {"name": {"eq": "a"}}})),
            ("m", "merge", json!({"inputs": 2})),
            ("k", "split", json!({"predicate": {"name": {"eq": "b"}}})),
            ("e", "emit", json!({"output": "out"})),
            ("d", "drop", json!({})),
        ],
        &[
            ("src.metrics", "s.in"),
            ("s.true", "m.0"),
            ("s.false", "m.1"),
            ("m.out", "k.in"),
            ("k.true", "e.in"),
            ("k.false", "d.in"),
        ],
    );
    let compiled = compiled(&plan).unwrap();
    let decisions = compiled.programs[0].decisions();
    assert_eq!(
        decisions.iter().map(nodes_of).collect::<Vec<_>>(),
        vec![vec!["s".to_string()], vec!["k".to_string()]]
    );
}

#[test]
fn decisions_record_shared_folded_and_memoized_predicates() {
    let plan = grouped_plan(
        &[
            ("t", "tee", json!({"ports": 3})),
            (
                "s1",
                "split",
                json!({"predicate": {"any": [
                    {"intake_name": {"matches": {"values": ["a."], "prefix": true}}},
                    {"intake_name": {"eq": "b"}},
                ]}}),
            ),
            ("e", "emit", json!({"output": "out"})),
            ("d1", "drop", json!({})),
            (
                "s2",
                "split",
                json!({"predicate": {"any": [{"intake_name": {"eq": "b"}}, {"intake_name": {"prefix_in": ["a."]}}]}}),
            ),
            ("d2", "drop", json!({})),
            ("d3", "drop", json!({})),
            (
                "s3",
                "split",
                json!({"predicate": {"all": [{"flag": false}, {"has_tag": "x"}]}}),
            ),
            ("d4", "drop", json!({})),
            ("d5", "drop", json!({})),
        ],
        &[
            ("src.metrics", "t.in"),
            ("t.0", "s1.in"),
            ("s1.true", "e.in"),
            ("s1.false", "d1.in"),
            ("t.1", "s2.in"),
            ("s2.true", "d2.in"),
            ("s2.false", "d3.in"),
            ("t.2", "s3.in"),
            ("s3.true", "d4.in"),
            ("s3.false", "d5.in"),
        ],
    );
    let compiled = compiled(&plan).unwrap();
    let decision = &compiled.programs[0].decisions()[0];
    // `prefix_in` is shorthand for `matches` by prefix, and `any` is unordered, so the two are equal in canonical form:
    // one entry, evaluated once. It costs 25 ns by R1's cost model (the intake name normalized once), and `s3` folds,
    // so the decision is memoized. Before decision compilation, R7 writes `s1`'s exact match before its prefix match,
    // as `s2` already has it: the logical record comes first.
    assert_eq!(decision.predicates.len(), 2);
    assert!(decision.memoized);
    let records = compiled
        .rewrites
        .iter()
        .map(|record| (record.id, record.nodes.clone(), record.outcome.clone()))
        .collect::<Vec<_>>();
    assert_eq!(
        records,
        vec![
            (RewriteId::R7, vec![id("s1")], RewriteOutcome::Applied),
            (RewriteId::R2, vec![id("s1"), id("s2")], RewriteOutcome::Applied),
            (RewriteId::R3, vec![id("s3")], RewriteOutcome::Applied),
            (
                RewriteId::R4,
                vec![id("t"), id("s1"), id("s2"), id("s3")],
                RewriteOutcome::Applied
            ),
        ]
    );
    assert_eq!(
        compiled.rewrites[2].justification,
        "The predicate is constant given its literals: it is false for every event, so the compiled decision sends \
         every metric to the `false` port without evaluating it. The plan keeps the node, and its `true` port's exit \
         stays, idle; a new value of a binding changes only the next generation's program."
    );
    assert!(compiled
        .rewrites
        .iter()
        .all(|record| record.telemetry.is_empty() && !record.capacity_rule));
}

#[test]
fn a_group_beyond_the_exit_capacity_is_cut_at_node_boundaries() {
    // A tee of 200 ports, each to a split with two drops: one group would have 400 exits.
    let mut nodes = vec![("t".to_string(), "tee", json!({"ports": 200}))];
    let mut edges = vec![("src.metrics".to_string(), "t.in".to_string())];
    for index in 0..200 {
        let split = format!("s{}", index);
        nodes.push((split.clone(), "split", json!({"predicate": {"name": {"eq": "a"}}})));
        edges.push((format!("t.{}", index), format!("{}.in", split)));
        for port in ["true", "false"] {
            let drop = format!("{}_{}", split, port);
            nodes.push((drop.clone(), "drop", json!({})));
            edges.push((format!("{}.{}", split, port), format!("{}.in", drop)));
        }
    }
    let nodes = nodes
        .iter()
        .map(|(id, op, params)| (id.as_str(), *op, params.clone()))
        .collect::<Vec<_>>();
    let edges = edges
        .iter()
        .map(|(from, to)| (from.as_str(), to.as_str()))
        .collect::<Vec<_>>();
    let compiled = compiled(&grouped_plan(&nodes, &edges)).unwrap();
    let [program] = compiled.programs.as_slice() else {
        panic!("one slot")
    };
    let decisions = program.decisions();
    // The tee and the first 56 splits fill one decision's 256 exits; each later split is a decision of its own.
    assert_eq!(decisions.len(), 1 + 144);
    assert_eq!(decisions[0].nodes.len(), 1 + 56);
    assert_eq!(decisions[0].exits.len(), EXIT_SET_CAPACITY);
    assert_eq!(nodes_of(&decisions[1]), vec!["s56"]);
    assert_eq!(nodes_of(&decisions[144]), vec!["s199"]);
    assert!(decisions
        .iter()
        .all(|decision| decision.exits.len() <= EXIT_SET_CAPACITY));
}

#[test]
fn a_tee_with_more_ports_than_a_decision_has_exits_does_not_compile() {
    let mut nodes = vec![("t".to_string(), json!({"ports": 300}))];
    let mut edges = vec![("src.metrics".to_string(), "t.in".to_string())];
    for index in 0..300 {
        nodes.push((format!("d{}", index), json!({})));
        edges.push((format!("t.{}", index), format!("d{}.in", index)));
    }
    let nodes = nodes
        .iter()
        .map(|(id, params)| (id.as_str(), if id == "t" { "tee" } else { "drop" }, params.clone()))
        .collect::<Vec<_>>();
    let edges = edges
        .iter()
        .map(|(from, to)| (from.as_str(), to.as_str()))
        .collect::<Vec<_>>();
    let error = compiled(&grouped_plan(&nodes, &edges)).unwrap_err();
    assert_eq!(
        error.to_string(),
        "The program of slot 0 has more output ports on one `tee` than a program supports (256)."
    );
}

#[test]
fn the_default_programs_decisions_are_its_filters_its_fan_outs_and_its_gateways() {
    let plan = Plan::from_json(DEFAULT_PROGRAM_JSON).unwrap();
    let registry = OperatorRegistry::native();
    let report = Validator::new(&registry, &default_program_catalog()).validate(&plan);
    let frame = Frame::compute(&plan, &registry, &default_program_slot_name).unwrap();
    let compiled = compile(&plan, &report, &registry, &frame).unwrap();
    let decisions = compiled
        .programs
        .iter()
        .flat_map(|program| {
            let slot = frame.slot(program.slot()).unwrap().name.to_string();
            program
                .decisions()
                .iter()
                .map(move |decision| (slot.clone(), nodes_of(decision), decision.memoized))
        })
        .collect::<Vec<_>>();
    let strings = |list: &[&str]| list.iter().map(|item| item.to_string()).collect::<Vec<_>>();
    // R1 never cuts a decision, and prices one by its function (a memo lookup when regions memoize it), so the policy
    // fan-out and every gateway below it are one decision of `metrics_enrich`. Multi-Region Failover's predicate is
    // constant for this configuration (enabled, mirroring, an empty allowlist: everything is forwarded), so R3 folds
    // it. What is left to evaluate, the routing filters on two intake-name leaves each (25 ns each by R1's cost
    // model) and the autoscaling gateway's on the kind and the raw name (11 ns), costs at least `MEMO_COST_FLOOR`, so
    // the decision is memoized. The filters on one intake-name leaf (20 ns, and 21 with the kind) cost less, and are
    // not.
    assert_eq!(
        decisions,
        vec![
            (
                "dsd_prefix_filter_region".to_string(),
                strings(&["dsd_prefix_filter_blocklist"]),
                false
            ),
            (
                "dsd_post_agg_filter_region".to_string(),
                strings(&["dsd_post_agg_filter", "dsd_post_agg_filter_fanout"]),
                false
            ),
            (
                "metrics_enrich".to_string(),
                strings(&[
                    "metrics_policy",
                    "metrics_routing_filter_0",
                    "metrics_routing_filter_1",
                    "mrf_metrics_gateway",
                    "af_metrics_gateway"
                ]),
                true
            ),
        ]
    );
    let records = compiled
        .rewrites
        .iter()
        .map(|record| {
            (
                record.id,
                record.nodes.iter().map(ToString::to_string).collect::<Vec<_>>(),
            )
        })
        .collect::<Vec<_>>();
    // R5b's record comes first: it is found on the logical plan, before programs are compiled.
    assert_eq!(
        records,
        vec![
            (RewriteId::R5b, strings(&["dsd_agg", "dsd_post_agg_filter"])),
            (RewriteId::R3, strings(&["mrf_metrics_gateway"])),
            (
                RewriteId::R4,
                strings(&[
                    "metrics_policy",
                    "metrics_routing_filter_0",
                    "metrics_routing_filter_1",
                    "mrf_metrics_gateway",
                    "af_metrics_gateway"
                ])
            ),
        ]
    );
    let folded = &compiled.rewrites[1].justification;
    assert!(
        folded.starts_with(
            "The predicate is constant given the resolved values of \
             `multi_region_failover.metric_mirroring.allowlist`, `multi_region_failover.enabled`, and \
             `multi_region_failover.metric_mirroring.enabled`: it is true for every event"
        ),
        "{}",
        folded
    );
}

// The compiled function against the reference walk, on random decisions.

/// The shape of a random decision.
#[derive(Clone, Debug)]
enum Tree {
    Exit,
    /// A split on one of the pool's predicates.
    Split(usize, Box<Tree>, Box<Tree>),
    Tee(Vec<Tree>),
}

fn arb_tree() -> impl Strategy<Value = Tree> {
    Just(Tree::Exit)
        .prop_recursive(6, 48, 4, |inner| {
            prop_oneof![
                (0usize..4, inner.clone(), inner.clone()).prop_map(|(predicate, yes, no)| Tree::Split(
                    predicate,
                    Box::new(yes),
                    Box::new(no)
                )),
                vec(inner, 1..4).prop_map(Tree::Tee),
            ]
        })
        .prop_map(|tree| match tree {
            Tree::Exit => Tree::Tee(vec![Tree::Exit]),
            tree => tree,
        })
}

/// Builds the decision of a tree, depth first, with its exits numbered in depth-first order, and its predicates
/// interned from `pool`, as written, shared and folded as decision compilation does. Returns it with the canonical
/// form of each `split`'s predicate, by the `split`'s position.
fn decision_of(tree: &Tree, pool: &[Predicate]) -> (Decision, Vec<(usize, Predicate)>) {
    fn visit(
        tree: &Tree, pool: &[Predicate], nodes: &mut Vec<DecisionNode>, exits: &mut Vec<ExitTarget>,
        table: &mut PredicateTable, canonical: &mut Vec<(usize, Predicate)>,
    ) -> u16 {
        let position = nodes.len();
        let name = format!("n{}", position);
        let node = id(&name);
        nodes.push(tee(&name, Vec::new()));
        let (kind, children, ports) = match tree {
            Tree::Split(index, yes, no) => {
                let predicate = &pool[*index % pool.len()];
                let compiled = Arc::new(predicate.compile(&NoBindings).unwrap());
                canonical.push((position, predicate.canonical()));
                let index = table.intern(&node, predicate, compiled).unwrap();
                (
                    DecisionKind::Split { predicate: index },
                    vec![&**yes, &**no],
                    vec![port("true"), port("false")],
                )
            }
            Tree::Tee(children) => (
                DecisionKind::Tee,
                children.iter().collect(),
                (0..children.len()).map(|index| port(&index.to_string())).collect(),
            ),
            Tree::Exit => unreachable!("exits are branches"),
        };
        let mut branches = Vec::new();
        for (child, name) in children.into_iter().zip(&ports) {
            match child {
                Tree::Exit => {
                    branches.push(to_exit(exits.len() as u16));
                    exits.push(ExitTarget {
                        node: node.clone(),
                        port: name.clone(),
                    });
                }
                child => branches.push(Branch::Node(visit(child, pool, nodes, exits, table, canonical))),
            }
        }
        nodes[position] = DecisionNode {
            node,
            ports,
            kind,
            branches,
        };
        position as u16
    }

    let (mut nodes, mut exits, mut table, mut canonical) = (Vec::new(), Vec::new(), PredicateTable::new(), Vec::new());
    visit(tree, pool, &mut nodes, &mut exits, &mut table, &mut canonical);
    let decision = Decision {
        nodes,
        exits,
        predicates: table,
        memoized: false,
        rewrites: Vec::new(),
    };
    (decision, canonical)
}

/// Returns a strategy for the predicates a random decision's `split`s draw from: a few random predicates, and one on
/// the kind, so that a memo that ignored the kind would return the wrong exit set for an event's kind sibling.
fn arb_pool() -> impl Strategy<Value = Vec<Predicate>> {
    (vec(arb_predicate(), 0..3), vec(select(Kind::ALL.to_vec()), 1..4)).prop_map(|(mut pool, kinds)| {
        pool.push(Predicate::KindIn(kinds.into_iter().collect()));
        pool
    })
}

#[test]
fn random_decisions_fold_bound_constants_and_meet_contexts_with_two_kinds() {
    // The property test below holds vacuously for R3's folding of bound constants unless its pools have predicates
    // folded given a binding's value, and for the kind in R4's memo key unless its metrics repeat a context with
    // another kind.
    use proptest::strategy::ValueTree as _;

    let mut runner = proptest::test_runner::TestRunner::deterministic();
    let mut bound = 0;
    for _ in 0..256 {
        let pool = arb_pool().new_tree(&mut runner).unwrap().current();
        for predicate in &pool {
            let mut bindings = Vec::new();
            constant_bindings(predicate, &mut bindings);
            if !bindings.is_empty() && fold_bound_constants(predicate).is_some() {
                bound += 1;
            }
        }
    }
    assert!(bound >= 32, "{} predicates fold given bindings", bound);

    let metrics = arb_metrics_with_kind_siblings(1..4)
        .new_tree(&mut runner)
        .unwrap()
        .current();
    for pair in metrics.chunks(2) {
        assert_eq!(pair[0].context(), pair[1].context());
        assert_ne!(Kind::of(pair[0].values()), Kind::of(pair[1].values()));
    }
}

/// Walks a decision literally, evaluating each `split`'s compiled predicate afresh, unshared and unfolded: returns the
/// exit set and, laid out as [`Decision::port_counts`] lays them out, whether the event left each port.
fn walk(decision: &Decision, event: &Event) -> (ExitSet, Vec<bool>) {
    let mut offsets = Vec::new();
    let mut offset = 0;
    for node in &decision.nodes {
        offsets.push(offset);
        offset += node.ports.len();
    }
    let mut ports = vec![false; offset];
    let mut set = ExitSet::EMPTY;
    let mut pending = vec![0usize];
    while let Some(position) = pending.pop() {
        let node = &decision.nodes[position];
        let taken: Vec<usize> = match node.kind {
            DecisionKind::Tee => (0..node.branches.len()).collect(),
            DecisionKind::Split { predicate } => {
                let entry = decision.predicates.get(predicate).unwrap();
                let matched = event
                    .try_as_metric()
                    .is_some_and(|metric| entry.compiled.evaluate(&mut EventView::new(metric)));
                vec![if matched { 0 } else { 1 }]
            }
        };
        for port in taken {
            ports[offsets[position] + port] = true;
            match node.branches[port] {
                Branch::Node(next) => pending.push(usize::from(next)),
                Branch::Exit(exit) => set.insert(exit),
            }
        }
    }
    (set, ports)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn property_test_decision_functions_and_memos_compute_what_the_reference_walk_computes(
        tree in arb_tree(),
        pool in arb_pool(),
        metrics in arb_metrics_with_kind_siblings(0..16),
    ) {
        let (decision, canonical) = decision_of(&tree, &pool);
        // R2 shares only predicates equal in canonical form, with their bindings: each `split` keeps its own predicate.
        for (position, predicate) in &canonical {
            let DecisionKind::Split { predicate: index } = decision.nodes[*position].kind else {
                unreachable!("splits are recorded at their positions");
            };
            prop_assert_eq!(&decision.predicates.get(index).unwrap().predicate, predicate);
        }
        let function = DecisionFunction::new(&decision).unwrap();
        prop_assert_eq!(function.port_count(), decision.port_count());
        let mut scratch = DecisionScratch::default();
        let mut tiny = DecisionMemo::for_exits(1, decision.exits.len());
        let mut memo = DecisionMemo::for_exits(4096, decision.exits.len());
        let mut counts = ExitSetCounts::with_capacity(COUNTED_EXIT_SETS);
        let mut expected_ports = vec![0u64; decision.port_count()];

        // Every event twice, so that the memos hit, each metric followed by a sibling of the same context and another
        // kind; and an event other than a metric.
        let events = metrics.into_iter().map(Event::Metric).chain([check()]).collect::<Vec<_>>();
        for event in events.iter().chain(&events) {
            let (expected, ports) = walk(&decision, event);
            prop_assert_eq!(decision.evaluate(event, &mut scratch), expected, "the reference walk, with R2 and R3");
            prop_assert_eq!(function.evaluate(event), expected, "the compiled function");
            prop_assert_eq!(function.evaluate_memoized(event, &mut tiny), expected, "a one-entry memo");
            prop_assert_eq!(function.evaluate_memoized(event, &mut memo), expected, "a full memo");
            if let Some(set) = function.constant() {
                prop_assert_eq!(set, expected, "a constant function");
            }
            prop_assert!(counts.add(expected, 1, 0));
            for (count, taken) in expected_ports.iter_mut().zip(ports) {
                *count += u64::from(taken);
            }
        }

        // Per-port counts derived from counts per exit set are exactly the ports events took.
        let mut derived = vec![0u64; decision.port_count()];
        function.port_counts(&counts, &mut derived);
        prop_assert_eq!(&derived, &expected_ports);
        let mut reference = vec![0u64; decision.port_count()];
        decision.port_counts(&counts, &mut reference);
        prop_assert_eq!(&reference, &expected_ports);
    }
}
