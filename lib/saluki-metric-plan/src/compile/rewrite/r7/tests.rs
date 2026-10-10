use proptest::{collection::vec, prelude::*, strategy::ValueTree as _, test_runner::TestRunner};
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
            Optimized, RewriteOutcome,
        },
        Frame,
    },
    explain::Explain,
    ir::{NativeComponentId, NodeId},
    operators::{names, OperatorRegistry},
    testing::{default_program_catalog, default_program_slot_name, with_opaque_gateways, DEFAULT_PROGRAM_JSON},
    validate::{ValidationReport, Validator},
};

fn predicate(value: Value) -> Predicate {
    serde_json::from_value(value).unwrap()
}

/// Orders a predicate as R7 does, returning it and the number of junctions whose order changed.
fn ordered(value: Value, profile: Option<&OperandProfile>) -> (Value, usize) {
    let mut changed = 0;
    let ordered = Order { profile }.apply(&predicate(value), &mut changed);
    (serde_json::to_value(ordered).unwrap(), changed)
}

// The order.

#[test]
fn the_static_order_puts_the_cheapest_operands_first() {
    let (result, changed) = ordered(
        json!({"all": [
            {"tag": {"key": "team", "value": {"prefix": "p"}}},
            {"has_tag": "env"},
            {"intake_name": {"prefix": "x"}},
            {"name": {"prefix_in": ["y"]}},
            {"intake_name": {"eq": "z"}},
            {"host": {"in": ["h"]}},
            {"host_unset": {}},
            {"is_empty": []},
            {"flag": true},
            {"kind_in": ["gauge"]},
            {"const": true}
        ]}),
        None,
    );
    assert_eq!(
        result,
        json!({"all": [
            {"const": true},
            {"kind_in": ["gauge"]},
            {"flag": true},
            {"is_empty": []},
            {"host_unset": {}},
            {"host": {"in": ["h"]}},
            {"intake_name": {"eq": "z"}},
            {"name": {"prefix_in": ["y"]}},
            {"intake_name": {"prefix": "x"}},
            {"has_tag": "env"},
            {"tag": {"key": "team", "value": {"prefix": "p"}}}
        ]})
    );
    assert_eq!(changed, 1);
}

#[test]
fn nested_junctions_are_ordered_inside_and_cost_the_sum_of_their_operands() {
    // The `any` costs 8 + 40, more than the prefix match's 16, and its own operands are ordered too, as are those under
    // a `not`.
    let (result, changed) = ordered(
        json!({"all": [
            {"any": [{"tag": {"key": "env", "value": {"eq": "prod"}}}, {"name": {"eq": "a"}}]},
            {"name": {"prefix": "b"}},
            {"not": {"any": [{"has_origin_tag": "pod"}, {"host_unset": {}}]}}
        ]}),
        None,
    );
    assert_eq!(
        result,
        json!({"all": [
            {"name": {"prefix": "b"}},
            {"not": {"any": [{"host_unset": {}}, {"has_origin_tag": "pod"}]}},
            {"any": [{"name": {"eq": "a"}}, {"tag": {"key": "env", "value": {"eq": "prod"}}}]}
        ]})
    );
    assert_eq!(changed, 3);
}

#[test]
fn an_ordered_predicate_is_left_as_it_is() {
    for value in [
        json!({"all": [{"kind_in": ["gauge"]}, {"name": {"eq": "a"}}]}),
        json!({"any": [{"name": {"eq": "a"}}, {"intake_name": {"prefix": "b"}}]}),
        json!({"name": {"eq": "a"}}),
        json!({"all": []}),
    ] {
        let (result, changed) = ordered(value.clone(), None);
        assert_eq!(result, value);
        assert_eq!(changed, 0);
    }
}

#[test]
fn the_order_never_depends_on_a_bindings_resolved_value() {
    // Ties are broken by canonical form without resolved values: by binding name, whatever the values are.
    for (a, b) in [(true, false), (false, true)] {
        let (result, _) = ordered(
            json!({"all": [
                {"flag": {"config": "test.b", "resolved": a}},
                {"flag": {"config": "test.a", "resolved": b}}
            ]}),
            None,
        );
        assert_eq!(
            result,
            json!({"all": [
                {"flag": {"config": "test.a", "resolved": b}},
                {"flag": {"config": "test.b", "resolved": a}}
            ]})
        );
    }
    // A match whose mode is bound counts as a prefix match, even when it resolves to an exact one.
    for resolved in [false, true] {
        let bound =
            json!({"name": {"matches": {"values": ["a"], "prefix": {"config": "test.mode", "resolved": resolved}}}});
        let (result, _) = ordered(json!({"any": [bound.clone(), {"intake_name": {"eq": "b"}}]}), None);
        assert_eq!(result, json!({"any": [{"intake_name": {"eq": "b"}}, bound]}));
    }
}

#[test]
fn a_profile_orders_operands_by_their_expected_cost_per_decision() {
    let rarely_false = predicate(json!({"host": {"eq": "a"}}));
    let often_false = predicate(json!({"name": {"eq": "b"}}));
    let always_true = predicate(json!({"const": true}));
    let profile = OperandProfile::new()
        .with_observation(&rarely_false, 100, 99)
        .with_observation(&often_false, 100, 10)
        .with_observation(&always_true, 100, 100);

    // Without the profile, equal costs fall back to canonical form, and the constant comes first.
    let all = json!({"all": [{"name": {"eq": "b"}}, {"const": true}, {"host": {"eq": "a"}}]});
    assert_eq!(
        ordered(all.clone(), None).0,
        json!({"all": [{"const": true}, {"host": {"eq": "a"}}, {"name": {"eq": "b"}}]})
    );
    // An `all` is decided by a false operand: the one that is often false comes first, and the constant, which never
    // decides it, last.
    assert_eq!(
        ordered(all, Some(&profile)).0,
        json!({"all": [{"name": {"eq": "b"}}, {"host": {"eq": "a"}}, {"const": true}]})
    );
    // An `any` is decided by a true one: the constant, at the cost of 1 per decision, then the one that is often true.
    assert_eq!(
        ordered(
            json!({"any": [{"name": {"eq": "b"}}, {"host": {"eq": "a"}}, {"const": true}]}),
            Some(&profile)
        )
        .0,
        json!({"any": [{"const": true}, {"host": {"eq": "a"}}, {"name": {"eq": "b"}}]})
    );
    // An operand the profile did not observe is assumed to decide half of its evaluations: a tag value match (40 per
    // evaluation, so 80 per decision) after the name match that decides 90% of them (about 9), before the host match
    // that decides 1% (800).
    assert_eq!(
        ordered(
            json!({"all": [{"host": {"eq": "a"}}, {"tag": {"key": "env", "value": {"eq": "prod"}}}, {"name": {"eq": "b"}}]}),
            Some(&profile)
        )
        .0,
        json!({"all": [{"name": {"eq": "b"}}, {"tag": {"key": "env", "value": {"eq": "prod"}}}, {"host": {"eq": "a"}}]})
    );
}

#[test]
fn profiles_identify_operands_by_canonical_form_without_resolved_values() {
    let mut profile = OperandProfile::new();
    assert!(profile.is_empty());
    let observed = predicate(
        json!({"name": {"matches": {"values": {"config": "test.names", "resolved": ["a"]},
                                                         "prefix": false}}}),
    );
    let other_value = predicate(
        json!({"name": {"matches": {"values": {"config": "test.names", "resolved": ["b"]},
                                                            "prefix": false}}}),
    );
    profile.observe(&observed, 10, 4);
    profile.observe(&other_value, 10, 20);
    // Both observations are of one operand; matches are capped at evaluations.
    assert_eq!(
        profile.observation(&predicate(
            json!({"name": {"matches": {"values": {"config": "test.names"}, "prefix": false}}})
        )),
        Some(Observation {
            evaluated: 20,
            matched: 14
        })
    );
    let unordered = predicate(json!({"all": [{"has_tag": "b"}, {"has_tag": "a"}]}));
    let reordered = predicate(json!({"all": [{"has_tag": "a"}, {"has_tag": "b"}]}));
    profile.observe(&unordered, 1, 1);
    assert_eq!(
        profile.observation(&reordered),
        Some(Observation {
            evaluated: 1,
            matched: 1
        })
    );
    assert!(!profile.is_empty());
}

#[test]
fn every_predicate_leaf_and_composition_is_pure() {
    // No leaf reads anything but the event and the plan's parameters today, so every junction may be reordered.
    for value in [
        json!({"all": [{"name": {"eq": "a"}}, {"not": {"any": [{"has_tag": "x"}, {"flag": true}]}}]}),
        json!({"any": [
            {"is_empty": []},
            {"kind_in": "all"},
            {"host_unset": {}},
            {"origin_tag": {"key": "pod", "value": {"prefix": "web"}}}
        ]}),
    ] {
        assert!(is_pure(&predicate(value.clone())), "{}", value);
    }
}

#[test]
fn large_observations_compare_exactly_after_scaling() {
    assert_eq!(scaled(3, 7), (3, 7));
    assert_eq!(scaled(u64::MAX, u64::MAX), (u64::MAX >> 34, u64::MAX >> 34));
    let (decided, evaluated) = scaled(1, u64::MAX);
    assert_eq!(decided, 0);
    assert!(evaluated <= MAX_COUNT);
}

// R7 on plans.

/// A node with native provenance of its own.
fn node(id: &str, op: &str, params: Value) -> Value {
    json!({"id": id, "op": op, "params": params, "provenance": {"native": id}})
}

fn namer(nodes: &[NodeId]) -> Result<NativeComponentId, String> {
    NativeComponentId::new(format!("region_{}", nodes[0].local())).map_err(|error| error.to_string())
}

/// A plan from an OTLP source through a kernel to a split on `predicate`, with an output on its `true` port and a
/// drop on its `false` port; validated, with its frame.
fn split_plan(predicate: Value) -> (Plan, ValidationReport, Frame, OperatorRegistry) {
    let fingerprint = PlanHash::of_bytes(b"o0").to_string();
    let plan = Plan::from_value(json!({
        "ir_version": 1,
        "nodes": [
            node("src", "source", json!({"binding": "otlp"})),
            node("k", "host_enrichment", json!({})),
            node("s", "split", json!({"predicate": predicate})),
            node("yes", "emit", json!({"output": "o0"})),
            node("no", "drop", json!({})),
        ],
        "edges": [
            {"from": "src.metrics", "to": "k.in"},
            {"from": "k.out", "to": "s.in"},
            {"from": "s.true", "to": "yes.in"},
            {"from": "s.false", "to": "no.in"},
        ],
        "outputs": [{"id": "o0", "accepts": {"kinds": "all", "timestamps": "mixed"}, "fingerprint": fingerprint,
                     "recipients": ["out/o0"]}],
    }))
    .unwrap();
    let registry = OperatorRegistry::native();
    let report = Validator::new(&registry, &catalog()).validate(&plan);
    assert!(report.is_valid(), "{}", report);
    let frame = Frame::compute(&plan, &registry, &namer).unwrap();
    (plan, report, frame, registry)
}

#[test]
fn r7_rewrites_one_nodes_predicate_and_records_why() {
    let (plan, report, frame, registry) = split_plan(json!({"any": [{"has_tag": "env"}, {"name": {"eq": "a"}}]}));
    let rewritten = R7
        .apply(&RewriteContext::new(&registry, &frame, &report), &plan)
        .unwrap();
    let mut expected = plan.clone();
    expected.nodes[2].params.insert(
        "predicate".parse().unwrap(),
        json!({"any": [{"name": {"eq": "a"}}, {"has_tag": "env"}]}),
    );
    assert_eq!(rewritten.plan, expected);
    assert_eq!(rewritten.nodes, vec!["s".parse::<NodeId>().unwrap()]);
    assert!(rewritten.telemetry.is_empty());
    assert!(!rewritten.capacity_rule);
    assert_eq!(
        rewritten.justification,
        "operands pure: every leaf of the predicate of `s` reads only the event and the plan's parameters, and its \
         operator reads no signals; order: 1 junction reordered by the static cost order (const, kind_in, flag and \
         is_empty, exact matches, prefix matches, tag scans), so the operand cheapest to decide is evaluated first; the \
         canonical form, and with it R2 and the duplicate analysis, is unchanged; capacity: not relied on, and no \
         record is routed differently"
    );
}

#[test]
fn the_default_program_writes_its_predicates_in_the_static_order() {
    // Lowering writes every junction cheapest first, so R7 changes nothing in either default-program fixture.
    let registry = OperatorRegistry::native();
    for plan in [
        Plan::from_json(DEFAULT_PROGRAM_JSON).unwrap(),
        Plan::from_value(with_opaque_gateways(DEFAULT_PROGRAM_JSON)).unwrap(),
    ] {
        let report = Validator::new(&registry, &default_program_catalog()).validate(&plan);
        let frame = Frame::compute(&plan, &registry, &default_program_slot_name).unwrap();
        assert!(R7
            .apply(&RewriteContext::new(&registry, &frame, &report), &plan)
            .is_none());
    }
}

#[test]
fn explain_lists_an_applied_reordering() {
    let (plan, report, frame, registry) =
        split_plan(json!({"all": [{"tag": {"key": "env", "value": {"eq": "prod"}}}, {"kind_in": ["gauge"]}]}));
    let compiled = compile(&plan, &report, &registry, &frame).unwrap();
    let r7 = compiled
        .rewrites
        .iter()
        .filter(|record| record.id == RewriteId::R7)
        .collect::<Vec<_>>();
    assert_eq!(r7.len(), 1);
    assert_eq!(r7[0].outcome, RewriteOutcome::Applied);
    let split = compiled.plan.node(&"s".parse().unwrap()).unwrap();
    assert_eq!(
        split.params.get("predicate"),
        Some(&json!({"all": [{"kind_in": ["gauge"]}, {"tag": {"key": "env", "value": {"eq": "prod"}}}]}))
    );
    let text = Explain::of(&plan, &registry, &report)
        .with_rewrites(&compiled.rewrites)
        .to_text();
    assert!(
        text.contains("  R7 applied: s\n    justification operands pure: every leaf of the predicate of `s`"),
        "{}",
        text
    );
}

#[test]
fn a_reordered_program_has_a_fingerprint_of_its_own_so_that_regions_run_it() {
    // Canonical form sorts operands, so only the order written tells the reordered program from the original. A region
    // keeps the program it runs when the candidate's fingerprint is equal, which must therefore mean the same order:
    // otherwise a degraded boot program followed by a valid candidate, or a new operand profile, would never run the
    // new order while the candidate's records said it did.
    let (plan, report, frame, registry) =
        split_plan(json!({"all": [{"tag": {"key": "env", "value": {"eq": "prod"}}}, {"kind_in": ["gauge"]}]}));
    let reordered = compile(&plan, &report, &registry, &frame).unwrap();
    assert!(reordered
        .rewrites
        .iter()
        .any(|record| record.id == RewriteId::R7 && record.is_applied()));
    let written = crate::compile::compile_unvalidated(&plan, &registry, &frame).unwrap();
    assert!(written.rewrites.is_empty());

    let runs_s = |program: &crate::compile::RegionProgram| {
        program
            .decisions()
            .iter()
            .any(|decision| decision.root().node == "s".parse::<NodeId>().unwrap())
    };
    let reuse = crate::install::kernel_reuse(Some(&written.programs), &reordered.programs);
    assert_eq!(reuse.len(), reordered.programs.len());
    assert!(reordered.programs.iter().any(|program| runs_s(program)));
    for (slot, program) in reuse.iter().zip(&reordered.programs) {
        // Only the program that runs the split changes.
        assert_eq!(slot.unchanged, !runs_s(program), "slot {}", slot.slot);
    }
    // Compiling again gives the same fingerprints: they change with the order, and nothing else here.
    let again = compile(&plan, &report, &registry, &frame).unwrap();
    for (a, b) in reordered.programs.iter().zip(&again.programs) {
        assert_eq!(a.fingerprint(), b.fingerprint());
    }
}

// Random plans.

/// Returns a profile with an observation for each operand of a junction in the plan's predicates, drawn in turn from
/// `observations`.
fn profile_of(plan: &Plan, registry: &OperatorRegistry, observations: &[(u64, u64)]) -> OperandProfile {
    fn operands<'p>(predicate: &'p Predicate, out: &mut Vec<&'p Predicate>) {
        match predicate {
            Predicate::All(inner) | Predicate::Any(inner) => {
                out.extend(inner);
                inner.iter().for_each(|operand| operands(operand, out));
            }
            Predicate::Not(inner) => operands(inner, out),
            _ => {}
        }
    }
    let mut profile = OperandProfile::new();
    if observations.is_empty() {
        return profile;
    }
    let ops = plan
        .nodes
        .iter()
        .filter_map(|node| registry.configure(node.op.as_str(), &node.params).ok())
        .collect::<Vec<_>>();
    let mut found = Vec::new();
    for op in &ops {
        if let Some(predicate) = op.params().predicate("predicate") {
            operands(predicate, &mut found);
        }
    }
    for (operand, (evaluated, matched)) in found.into_iter().zip(observations.iter().cycle()) {
        profile.observe(operand, *evaluated, *matched);
    }
    profile
}

fn arb_observations() -> impl Strategy<Value = Vec<(u64, u64)>> {
    vec((0u64..1_000, 0u64..1_000), 0..12)
}

/// Checks what every application of R7 to a random plan must keep.
fn check_obligations(
    case: &Case, optimized: &Optimized, profile: Option<&OperandProfile>,
) -> Result<(), TestCaseError> {
    prop_assert!(optimized.converged, "R7 did not converge");
    let registry = &case.registry;
    for record in &optimized.records {
        prop_assert_eq!(record.id, RewriteId::R7);
        prop_assert!(record.is_applied(), "R7 skipped {:?}", record);
        prop_assert_eq!(record.nodes.len(), 1);
        prop_assert!(record.telemetry.is_empty());
        prop_assert!(!record.capacity_rule);
    }
    let rewritten = &optimized.plan;
    // Only predicate parameters change, and no canonical form does: structure, outputs, and everything R2 and the
    // duplicate analysis compare stay.
    prop_assert_eq!(&rewritten.edges, &case.plan.edges);
    prop_assert_eq!(&rewritten.outputs, &case.plan.outputs);
    prop_assert_eq!(rewritten.nodes.len(), case.plan.nodes.len());
    for (before, after) in case.plan.nodes.iter().zip(&rewritten.nodes) {
        prop_assert_eq!(&before.id, &after.id);
        prop_assert_eq!(&before.op, &after.op);
        let configure = |node: &crate::ir::Node| registry.configure(node.op.as_str(), &node.params).unwrap();
        let (old, new) = (configure(before), configure(after));
        match (old.params().predicate("predicate"), new.params().predicate("predicate")) {
            (Some(old), Some(new)) => {
                prop_assert_eq!(old.canonical(), new.canonical(), "`{}` changed meaning", before.id);
                prop_assert_eq!(static_cost(old), static_cost(new));
            }
            (None, None) => prop_assert_eq!(before, after),
            _ => prop_assert!(false, "`{}` gained or lost its predicate", before.id),
        }
    }
    // The rewritten plan is valid and runs in the boot frame.
    let mut ctx = RewriteContext::new(registry, &case.frame, &case.report);
    if let Some(profile) = profile {
        ctx = ctx.with_profile(profile);
    }
    let placement =
        place_unvalidated(&case.plan, registry, &case.frame).map_err(|error| TestCaseError::fail(error.to_string()))?;
    let (report, _) = check(&ctx, &placement, rewritten).map_err(TestCaseError::fail)?;
    // A fixed point, and a deterministic one: R7 proposes nothing more, and compiling again gives the same plan.
    prop_assert!(
        R7.apply(&ctx.with(&report, &optimized.records), rewritten).is_none(),
        "R7 still applies"
    );
    prop_assert_eq!(&case.optimize(&R7, profile).plan, rewritten);
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn property_test_r7_changes_no_output_of_random_plans(
        shape in arb_shape(arb_split_predicate()),
        batches in arb_batches(),
        observations in arb_observations(),
    ) {
        let case = Case::new(&shape)?;
        let profile = (!observations.is_empty()).then(|| profile_of(&case.plan, &case.registry, &observations));
        let optimized = case.optimize(&R7, profile.as_ref());
        check_obligations(&case, &optimized, profile.as_ref())?;
        let (before, after) = check_equivalent(&case, &case.plan, &optimized.plan, &batches)?;
        check_same_order(&case.plan, &before, &after)?;
        // Every node receives what it received: no loss changes.
        for node in &case.plan.nodes {
            prop_assert_eq!(before.loss(&node.id), after.loss(&node.id), "loss of `{}`", node.id);
        }
    }

    #[test]
    fn property_test_r7_under_capacity_pressure_delivers_and_refuses_what_the_original_does(
        shape in arb_shape(arb_split_predicate()),
        batches in arb_batches(),
        observations in arb_observations(),
        choices in arb_bounds(),
    ) {
        let case = Case::new(&shape)?;
        let profile = (!observations.is_empty()).then(|| profile_of(&case.plan, &case.registry, &observations));
        let optimized = case.optimize(&R7, profile.as_ref());
        check_obligations(&case, &optimized, profile.as_ref())?;

        // Reordering operands changes neither routing nor the order in which records reach an output, so even bounds
        // that cut inside a batch refuse the same records.
        let arbitrary = arbitrary_bounds(&case.plan, &batches, &choices);
        check_under_capacity(&case, &case.plan, &optimized.plan, &batches, &arbitrary)?;
        let unbounded = case.run(&trace(&case.plan, &batches, &[]))?;
        let aligned = batch_aligned_bounds(&case.plan, &unbounded, &batches, &choices);
        check_under_capacity(&case, &case.plan, &optimized.plan, &batches, &aligned)?;
    }
}

#[test]
fn random_plans_exercise_every_part_of_r7() {
    // The property tests hold vacuously unless R7 reorders many generated plans, and profiles order some differently
    // from the static order.
    let mut runner = TestRunner::deterministic();
    let strategy = (arb_shape(arb_split_predicate()), vec((0u64..1_000, 0u64..1_000), 1..12));
    let (mut plans, mut applied, mut profiled_differently) = (0, 0, 0);
    for _ in 0..300 {
        let (shape, observations) = strategy.new_tree(&mut runner).unwrap().current();
        let case = Case::new(&shape).unwrap();
        plans += 1;
        let statically = case.optimize(&R7, None);
        if statically.changed() {
            applied += 1;
        }
        let profile = profile_of(&case.plan, &case.registry, &observations);
        if case.optimize(&R7, Some(&profile)).plan != statically.plan {
            profiled_differently += 1;
        }
        // Every split's predicate is ordered, whatever R7 did.
        for node in statically
            .plan
            .nodes
            .iter()
            .filter(|node| node.op.as_str() == names::SPLIT)
        {
            let op = case.registry.configure(node.op.as_str(), &node.params).unwrap();
            let mut changed = 0;
            let predicate = op.params().predicate("predicate").unwrap();
            Order { profile: None }.apply(predicate, &mut changed);
            assert_eq!(changed, 0, "`{}` is not ordered: {:?}", node.id, predicate);
        }
    }
    assert!(applied * 3 > plans, "R7 applied to {} of {} plans", applied, plans);
    assert!(
        profiled_differently * 10 > plans,
        "profiles changed {} of {} plans",
        profiled_differently,
        plans
    );
}
