use proptest::prelude::*;
use serde_json::{json, Value};

use super::*;
use crate::{
    compile::rewrite::{
        harness::{
            arb_plan_params, arb_shape, arb_trace, drop, edges, emit, ids, node, plan, sample, sorted, source, split,
            with_scope, Case, TEST_KEEP,
        },
        r5a::R5a,
        Optimized, RewriteOutcome, RewriteRecord,
    },
    ir::NodeId,
};

fn id(value: &str) -> NodeId {
    value.parse().unwrap()
}

/// Runs R6 alone to a fixed point over a valid plan, framed by itself.
fn r6(plan: Plan) -> (Case, Optimized) {
    let case = Case::of(plan, Vec::new()).unwrap();
    let optimized = case.optimize(&[&R6]);
    assert!(optimized.converged);
    (case, optimized)
}

fn applied(optimized: &Optimized) -> Vec<&RewriteRecord> {
    optimized.records.iter().filter(|record| record.is_applied()).collect()
}

/// A tee whose two ports lead to the filters `a` and `b`, each followed by an emit.
fn guarded(a: (Value, Value, &str), b: (Value, Value, &str)) -> Plan {
    let (a_filter, a_drop, a_dropped) = a;
    let (b_filter, b_drop, b_dropped) = b;
    let kept = |dropped: &str| if dropped == "true" { "false" } else { "true" };
    let a_kept = format!("policy/a.{} -> policy/ea.in", kept(a_dropped));
    let b_kept = format!("policy/b.{} -> policy/eb.in", kept(b_dropped));
    let a_dropped = format!("policy/a.{} -> policy/a_drop.in", a_dropped);
    let b_dropped = format!("policy/b.{} -> policy/b_drop.in", b_dropped);
    plan(
        vec![
            source("src", "otlp"),
            node("t", names::TEE, json!({"ports": 2}), false),
            a_filter,
            a_drop,
            emit("ea", "o0"),
            b_filter,
            b_drop,
            emit("eb", "o1"),
        ],
        &[
            "src.metrics -> policy/t.in",
            "policy/t.0 -> policy/a.in",
            &a_dropped,
            &a_kept,
            "policy/t.1 -> policy/b.in",
            &b_dropped,
            &b_kept,
        ],
        &["o0", "o1"],
    )
}

#[test]
fn a_drop_every_port_of_a_tee_applies_is_hoisted_above_it() {
    // `a` keeps what is tagged `env`; `b` drops what is not: the same drop condition, written two ways.
    let (case, optimized) = r6(guarded(
        (
            split("a", json!({"has_tag": "env"}), false),
            drop("a_drop", false),
            "false",
        ),
        (
            split("b", json!({"not": {"has_tag": "env"}}), false),
            drop("b_drop", false),
            "true",
        ),
    ));
    assert_eq!(
        edges(&optimized.plan),
        sorted(&[
            "src.metrics -> policy/a_r6h0.in",
            "policy/a_r6h0.true -> policy/t.in",
            "policy/a_r6h0.false -> policy/a_drop_r6h0.in",
            "policy/t.0 -> policy/ea.in",
            "policy/t.1 -> policy/eb.in",
        ])
    );
    assert_eq!(
        ids(&optimized.plan),
        vec![
            "src",
            "policy/a_r6h0",
            "policy/a_drop_r6h0",
            "policy/t",
            "policy/ea",
            "policy/eb"
        ]
    );

    let records = applied(&optimized);
    assert_eq!(records.len(), 1);
    let record = records[0];
    assert_eq!(record.id, RewriteId::R6);
    assert_eq!(
        record.nodes,
        vec![
            id("policy/t"),
            id("policy/a"),
            id("policy/a_drop"),
            id("policy/b"),
            id("policy/b_drop"),
            id("policy/a_r6h0"),
            id("policy/a_drop_r6h0"),
        ]
    );
    assert_eq!(
        record
            .telemetry
            .iter()
            .map(|effect| (effect.node.to_string(), effect.change))
            .collect::<Vec<_>>(),
        vec![
            ("policy/t".to_string(), TelemetryChange::Changed),
            ("policy/a".to_string(), TelemetryChange::Removed),
            ("policy/a_drop".to_string(), TelemetryChange::Removed),
            ("policy/b".to_string(), TelemetryChange::Removed),
            ("policy/b_drop".to_string(), TelemetryChange::Removed),
            ("policy/a_r6h0".to_string(), TelemetryChange::Added),
            ("policy/a_drop_r6h0".to_string(), TelemetryChange::Added),
        ]
    );
    assert!(!record.capacity_rule);
    assert!(
        record
            .justification
            .contains(r#"drop condition {"not":{"has_tag":"env"}} in canonical form"#),
        "{}",
        record.justification
    );
    case.check_structure(&optimized).unwrap();
}

#[test]
fn a_drop_some_port_does_not_apply_stays_below_the_tee() {
    let tag = |id: &str, key: &str, native: bool| split(id, json!({"has_tag": key}), native);
    // (case, port 0's filter, port 1's filter)
    let cases = [
        (
            "different predicates",
            (tag("a", "env", false), drop("a_drop", false), "false"),
            (tag("b", "team", false), drop("b_drop", false), "false"),
        ),
        (
            "one predicate, dropped by opposite ports",
            (tag("a", "env", false), drop("a_drop", false), "false"),
            (tag("b", "env", false), drop("b_drop", false), "true"),
        ),
        (
            "a native filter, whose series a copy would not carry",
            (tag("a", "env", false), drop("a_drop", false), "false"),
            (tag("b", "env", true), drop("b_drop", true), "false"),
        ),
    ];
    for (what, a, b) in cases {
        let mut plan = guarded(a, b);
        // The native filter's nodes are named without the policy prefix.
        for edge in &mut plan.edges {
            for port in [&mut edge.from, &mut edge.to] {
                if plan.nodes.iter().all(|node| node.id != port.node) {
                    port.node = port.node.local().to_string().parse().unwrap();
                }
            }
        }
        let (case, optimized) = r6(plan);
        assert!(optimized.records.is_empty(), "{}: {:?}", what, optimized.records);
        assert_eq!(optimized.plan, case.plan, "{}", what);
    }
}

#[test]
fn a_drop_is_hoisted_within_its_scope_only() {
    let scoped = |input: &str, exits: &[(&str, &str)]| {
        let plan = guarded(
            (
                split("a", json!({"has_tag": "env"}), false),
                drop("a_drop", false),
                "false",
            ),
            (
                split("b", json!({"has_tag": "env"}), false),
                drop("b_drop", false),
                "false",
            ),
        );
        with_scope(plan, input, exits)
    };
    // The tee is the scope's first node: the hoisted filter takes its place. A scope that ends at what a filter's kept
    // records enter still ends there.
    for exits in [&[][..], &[("enriched", "policy/ea.in")][..]] {
        let (case, optimized) = r6(scoped("src.metrics", exits));
        assert_eq!(applied(&optimized).len(), 1, "{:?}: {:?}", exits, optimized.records);
        case.check_structure(&optimized).unwrap();
    }
    // (case, the scope's input, its exits)
    type Exits<'a> = &'a [(&'a str, &'a str)];
    let cases: [(&str, &str, Exits<'_>); 3] = [
        (
            "the scope begins below the tee, at one of its ports: hoisting would move the filter out of it",
            "policy/t.0",
            &[],
        ),
        (
            "the scope begins at a filter's kept port, which hoisting removes",
            "policy/a.true",
            &[],
        ),
        (
            "the scope ends at a filter's drop, which hoisting removes",
            "src.metrics",
            &[("dropped", "policy/a_drop.in")],
        ),
    ];
    for (what, input, exits) in cases {
        let (_, optimized) = r6(scoped(input, exits));
        assert!(optimized.records.is_empty(), "{}: {:?}", what, optimized.records);
    }
}

#[test]
fn a_rewritten_plan_whose_scope_names_a_removed_port_is_skipped() {
    // Revalidation, which every application passes, checks the ports a scope names: without it, a hoist that removed
    // the node a scope begins at would be accepted.
    let scoped = with_scope(
        guarded(
            (
                split("a", json!({"has_tag": "env"}), false),
                drop("a_drop", false),
                "false",
            ),
            (
                split("b", json!({"has_tag": "env"}), false),
                drop("b_drop", false),
                "false",
            ),
        ),
        "policy/a.true",
        &[],
    );
    let mut removed = scoped.clone();
    let a = id("policy/a");
    removed.nodes.retain(|node| node.id != a);
    removed.edges.retain(|edge| edge.from.node != a && edge.to.node != a);
    let report = crate::validate::revalidate(crate::compile::rewrite::harness::registry(), &removed);
    assert!(
        report.errors().any(|finding| finding
            .to_string()
            .contains("refers to `policy/a.true`, which is not an output port")),
        "{}",
        report
    );
}

#[test]
fn a_port_without_a_filter_keeps_the_drop_below() {
    let (_, optimized) = r6(plan(
        vec![
            source("src", "otlp"),
            node("t", names::TEE, json!({"ports": 2}), false),
            split("a", json!({"has_tag": "env"}), false),
            drop("a_drop", false),
            emit("ea", "o0"),
            emit("eb", "o1"),
        ],
        &[
            "src.metrics -> policy/t.in",
            "policy/t.0 -> policy/a.in",
            "policy/a.false -> policy/a_drop.in",
            "policy/a.true -> policy/ea.in",
            "policy/t.1 -> policy/eb.in",
        ],
        &["o0", "o1"],
    ));
    assert!(optimized.records.is_empty(), "{:?}", optimized.records);
}

#[test]
fn equal_filter_operators_are_hoisted_and_a_binding_differs_from_its_value() {
    let keep = |id: &str, predicate: Value| node(id, TEST_KEEP, json!({"predicate": predicate}), false);
    let shaped = |a: Value, b: Value| {
        plan(
            vec![
                source("src", "otlp"),
                node("t", names::TEE, json!({"ports": 2}), false),
                a,
                emit("ea", "o0"),
                b,
                emit("eb", "o1"),
            ],
            &[
                "src.metrics -> policy/t.in",
                "policy/t.0 -> policy/a.in",
                "policy/a.out -> policy/ea.in",
                "policy/t.1 -> policy/b.in",
                "policy/b.out -> policy/eb.in",
            ],
            &["o0", "o1"],
        )
    };
    let names = json!({"name": {"in": ["app.requests"]}});
    let (case, optimized) = r6(shaped(keep("a", names.clone()), keep("b", names.clone())));
    assert_eq!(
        edges(&optimized.plan),
        sorted(&[
            "src.metrics -> policy/a_r6h0.in",
            "policy/a_r6h0.out -> policy/t.in",
            "policy/t.0 -> policy/ea.in",
            "policy/t.1 -> policy/eb.in",
        ])
    );
    case.check_structure(&optimized).unwrap();

    // The same values, once bound to configuration: equal resolved values do not make equal operands.
    let bound = json!({"name": {"matches": {"values": {"config": "test.names", "resolved": ["app.requests"]},
                                             "prefix": false}}});
    let split_of = |id: &str, predicate: Value| (split(id, predicate, false), drop(&format!("{}_drop", id), false));
    let (a, a_drop) = split_of("a", names);
    let (b, b_drop) = split_of("b", bound);
    let (_, optimized) = r6(guarded((a, a_drop, "false"), (b, b_drop, "false")));
    assert!(optimized.records.is_empty(), "{:?}", optimized.records);
}

#[test]
fn r5a_brings_filters_up_to_a_tee_and_r6_hoists_them() {
    // Each port of the tee has a prefix (which writes `name`) and then a filter on `host`: R5a moves each filter above
    // its prefix, to the tee, and R6 then hoists one copy above the tee.
    let prefix = |id: &str| {
        node(
            id,
            names::NAMESPACE_PREFIX,
            json!({"namespace": "ns", "exclusions": []}),
            true,
        )
    };
    let case = Case::of(
        plan(
            vec![
                source("src", "otlp"),
                node("t", names::TEE, json!({"ports": 2}), false),
                prefix("pa"),
                split("a", json!({"host": {"eq": "web-1"}}), false),
                drop("a_drop", false),
                emit("ea", "o0"),
                prefix("pb"),
                split("b", json!({"host": {"eq": "web-1"}}), false),
                drop("b_drop", false),
                emit("eb", "o1"),
            ],
            &[
                "src.metrics -> policy/t.in",
                "policy/t.0 -> pa.in",
                "pa.out -> policy/a.in",
                "policy/a.false -> policy/a_drop.in",
                "policy/a.true -> policy/ea.in",
                "policy/t.1 -> pb.in",
                "pb.out -> policy/b.in",
                "policy/b.false -> policy/b_drop.in",
                "policy/b.true -> policy/eb.in",
            ],
            &["o0", "o1"],
        ),
        Vec::new(),
    )
    .unwrap();
    let optimized = case.optimize(&[&R5a, &R6]);
    assert_eq!(
        edges(&optimized.plan),
        sorted(&[
            "src.metrics -> policy/a_r6h0.in",
            "policy/a_r6h0.true -> policy/t.in",
            "policy/a_r6h0.false -> policy/a_drop_r6h0.in",
            "policy/t.0 -> pa.in",
            "pa.out -> policy/ea.in",
            "policy/t.1 -> pb.in",
            "pb.out -> policy/eb.in",
        ])
    );
    assert_eq!(
        applied(&optimized).iter().map(|record| record.id).collect::<Vec<_>>(),
        vec![RewriteId::R5a, RewriteId::R5a, RewriteId::R6]
    );
    case.check_structure(&optimized).unwrap();
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn property_test_r6_rewritten_plans_deliver_what_the_original_delivers(
        shape in arb_shape(),
        params in arb_plan_params(),
        trace in arb_trace(),
    ) {
        let case = Case::new(&shape, &params).map_err(TestCaseError::fail)?;
        let optimized = case.optimize(&[&R6]);
        prop_assert!(optimized.converged, "R6 did not reach a fixed point: {:?}", optimized.records);
        let structure = case.check_structure(&optimized);
        prop_assert!(structure.is_ok(), "{:?}", structure.err());
        let equivalent = case.check_equivalent(&optimized.plan, &trace);
        prop_assert!(equivalent.is_ok(), "{:?}", equivalent.err());
    }

    #[test]
    fn property_test_r6_only_reduces_capacity_loss(
        shape in arb_shape(),
        params in arb_plan_params(),
        trace in arb_trace(),
    ) {
        let case = Case::new(&shape, &params).map_err(TestCaseError::fail)?;
        let optimized = case.optimize(&[&R6]);
        let capacity = case.check_capacity(&optimized.plan, &trace);
        prop_assert!(capacity.is_ok(), "{:?}", capacity.err());
    }

    /// R5a and R6 together, in pipeline order, as the pipeline runs them: R5a brings filters up to the `tee`s that
    /// R6 hoists them across.
    #[test]
    fn property_test_r5a_and_r6_together_deliver_what_the_original_delivers_under_capacity_too(
        shape in arb_shape(),
        params in arb_plan_params(),
        trace in arb_trace(),
    ) {
        let case = Case::new(&shape, &params).map_err(TestCaseError::fail)?;
        let optimized = case.optimize(&[&R5a, &R6]);
        prop_assert!(optimized.converged, "R5a and R6 did not reach a fixed point: {:?}", optimized.records);
        let structure = case.check_structure(&optimized);
        prop_assert!(structure.is_ok(), "{:?}", structure.err());
        let equivalent = case.check_equivalent(&optimized.plan, &trace);
        prop_assert!(equivalent.is_ok(), "{:?}", equivalent.err());
        let capacity = case.check_capacity(&optimized.plan, &trace);
        prop_assert!(capacity.is_ok(), "{:?}", capacity.err());
    }
}

#[test]
fn r6_applies_to_random_plans() {
    // The property tests would hold vacuously if R6 never applied: count applications over a sample, alone and after
    // R5a.
    let (mut alone, mut together, mut skipped, mut pressured) = (0, 0, 0, 0);
    for (case, trace) in sample(512) {
        let optimized = case.optimize(&[&R6]);
        alone += applied(&optimized).len();
        skipped += optimized
            .records
            .iter()
            .filter(|record| matches!(record.outcome, RewriteOutcome::Skipped { .. }))
            .count();
        if optimized.changed() && case.check_capacity(&optimized.plan, &trace).unwrap() {
            pressured += 1;
        }
        let both = case.optimize(&[&R5a, &R6]);
        together += applied(&both)
            .iter()
            .filter(|record| record.id == RewriteId::R6)
            .count();
    }
    eprintln!(
        "alone {} together {} skipped {} pressured {}",
        alone, together, skipped, pressured
    );
    assert!(alone > 10, "{} hoists", alone);
    // R5a moves the filter below a one-port tee above it whole, so R6 finds fewer sites after it.
    assert!(together > 5, "{} hoists after R5a", together);
    assert!(pressured > 5, "{} rewritten plans under capacity pressure", pressured);
    assert!(skipped < 10, "{} skipped applications", skipped);
}
