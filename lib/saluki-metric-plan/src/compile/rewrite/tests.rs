use std::collections::{BTreeMap, BTreeSet};

use super::*;
use crate::{
    compile::compile,
    testing::{default_program_catalog, default_program_slot_name, DEFAULT_PROGRAM_JSON},
    validate::Validator,
};

fn default_program() -> (Plan, ValidationReport, Frame) {
    let plan = Plan::from_json(DEFAULT_PROGRAM_JSON).unwrap();
    let registry = OperatorRegistry::native();
    let report = Validator::new(&registry, &default_program_catalog()).validate(&plan);
    let frame = Frame::compute(&plan, &registry, &default_program_slot_name).unwrap();
    (plan, report, frame)
}

#[test]
fn every_rewrite_identifier_is_listed_once_in_order() {
    // The match is exhaustive: a new identifier fails to compile here until it is placed, and listed in `ALL`.
    let position = |id: RewriteId| match id {
        RewriteId::R2 => 0,
        RewriteId::R3 => 1,
        RewriteId::R4 => 2,
        RewriteId::R5a => 3,
        RewriteId::R6 => 4,
        RewriteId::R7 => 5,
    };
    for (index, id) in RewriteId::ALL.into_iter().enumerate() {
        assert_eq!(position(id), index, "{}", id);
    }
    let names = RewriteId::ALL.map(RewriteId::as_str);
    assert_eq!(names, ["R2", "R3", "R4", "R5a", "R6", "R7"]);
}

#[test]
fn the_pipeline_runs_every_logical_rewrite_once_per_pass_in_numeric_order() {
    let ids = PIPELINE.iter().map(|rewrite| rewrite.id()).collect::<Vec<_>>();
    assert_eq!(ids, vec![RewriteId::R3, RewriteId::R5a, RewriteId::R6, RewriteId::R7]);
}

#[test]
fn optimizing_the_default_program_changes_nothing_while_no_rewrite_applies() {
    let (plan, report, frame) = default_program();
    let registry = OperatorRegistry::native();
    let optimized = optimize(&plan, &RewriteContext::new(&registry, &frame, &report));
    assert_eq!(optimized.plan, plan);
    assert!(optimized.records.is_empty());
    assert!(optimized.converged);
    assert!(!optimized.changed());

    let compiled = compile(&plan, &report, &registry, &frame).unwrap();
    assert_eq!(*compiled.plan, plan);
    // Only decision-time rewrites apply, which the programs' decisions record.
    assert!(compiled
        .rewrites
        .iter()
        .all(|record| matches!(record.id, RewriteId::R2 | RewriteId::R3 | RewriteId::R4)));
}

#[test]
fn rewritten_plans_revalidate_without_a_catalog_as_the_catalog_validated_them() {
    let (plan, report, _) = default_program();
    let revalidated = revalidate(&OperatorRegistry::native(), &plan);
    assert!(revalidated.is_valid());
    assert_eq!(
        revalidated.port_types().collect::<Vec<_>>(),
        report.port_types().collect::<Vec<_>>()
    );
}

// The whole pipeline, over both families of random plans: `harness`'s, built around filters (R5a and R6 sites), and
// `random_plans`'s, built around constant and reorderable predicates (R3 and R7 sites).

mod pipeline {
    use proptest::{prelude::*, strategy::ValueTree as _, test_runner::TestRunner};

    use super::super::{harness, random_plans};
    use super::*;

    /// Runs the pipeline over a `random_plans` case, as compilation does.
    fn optimized(case: &random_plans::Case) -> Optimized {
        optimize(
            &case.plan,
            &RewriteContext::new(&case.registry, &case.frame, &case.report),
        )
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(256))]

        #[test]
        fn property_test_the_pipeline_keeps_what_plans_around_filters_deliver_under_capacity_too(
            shape in harness::arb_shape(),
            params in harness::arb_plan_params(),
            trace in harness::arb_trace(),
        ) {
            let case = harness::Case::new(&shape, &params).map_err(TestCaseError::fail)?;
            let optimized = optimize(&case.plan, &case.context());
            prop_assert!(optimized.converged, "the pipeline did not reach a fixed point: {:?}", optimized.records);
            let structure = case.check_structure(&optimized);
            prop_assert!(structure.is_ok(), "{:?}", structure.err());
            let equivalent = case.check_equivalent(&optimized.plan, &trace);
            prop_assert!(equivalent.is_ok(), "{:?}", equivalent.err());
            let capacity = case.check_capacity(&optimized.plan, &trace);
            prop_assert!(capacity.is_ok(), "{:?}", capacity.err());
        }

        #[test]
        fn property_test_the_pipeline_keeps_what_plans_around_constant_and_reorderable_predicates_deliver(
            shape in random_plans::arb_shape(random_plans::arb_split_predicate()),
            batches in random_plans::arb_batches(),
            choices in random_plans::arb_bounds(),
        ) {
            let case = random_plans::Case::new(&shape)?;
            let optimized = optimized(&case);
            prop_assert!(optimized.converged, "the pipeline did not reach a fixed point: {:?}", optimized.records);
            let (before, after) = random_plans::check_equivalent(&case, &case.plan, &optimized.plan, &batches)?;
            // These plans have no operator that loses to capacity, and every rewrite keeps each remaining port's
            // records in order, so the pipeline delivers every output's records in the same order, and refuses the
            // same records under any bound, even one that cuts inside a batch.
            random_plans::check_same_order(&case.plan, &before, &after)?;
            let bounds = random_plans::arbitrary_bounds(&case.plan, &batches, &choices);
            random_plans::check_under_capacity(&case, &case.plan, &optimized.plan, &batches, &bounds)?;
        }
    }

    #[test]
    fn random_plans_of_both_families_exercise_every_logical_rewrite_through_the_pipeline() {
        // The pipeline's property tests hold vacuously unless every rewrite applies to many generated plans, together
        // with others, and some plans get several.
        let mut applied = BTreeMap::<RewriteId, usize>::new();
        let (mut several, mut skipped) = (0, 0);
        let mut count = |optimized: &Optimized| {
            let mut ids = BTreeSet::new();
            for record in &optimized.records {
                if record.is_applied() {
                    *applied.entry(record.id).or_default() += 1;
                    ids.insert(record.id);
                } else {
                    skipped += 1;
                }
            }
            several += usize::from(ids.len() > 1);
        };
        for (case, _) in harness::sample(256) {
            count(&optimize(&case.plan, &case.context()));
        }
        let mut runner = TestRunner::deterministic();
        let strategy = random_plans::arb_shape(random_plans::arb_split_predicate());
        for _ in 0..256 {
            let shape = strategy.new_tree(&mut runner).unwrap().current();
            count(&optimized(&random_plans::Case::new(&shape).unwrap()));
        }
        eprintln!(
            "applied {:?}, {} plans with several rewrites, {} skipped",
            applied, several, skipped
        );
        // R6's sites, `tee`s whose every port starts with an equivalent filter, are the rarest, and R5a moves some of
        // their filters first.
        for (id, least) in [
            (RewriteId::R3, 50),
            (RewriteId::R5a, 50),
            (RewriteId::R6, 5),
            (RewriteId::R7, 50),
        ] {
            let count = applied.get(&id).copied().unwrap_or_default();
            assert!(count >= least, "{} applied {} times", id, count);
        }
        assert_eq!(applied.len(), PIPELINE.len());
        assert!(several >= 50, "{} plans with several rewrites", several);
        assert!(skipped < 50, "{} skipped applications", skipped);
    }
}
