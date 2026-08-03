use std::collections::{BTreeMap, BTreeSet};

use proptest::prelude::*;

use super::*;
use crate::capture::MetricKind;

fn ctx(name: &str, tags: &[&str], kind: MetricKind) -> Arc<Context> {
    Arc::new(Context {
        name: name.to_string(),
        tagset: tags.iter().map(|t| (*t).to_string()).collect(),
        kind,
    })
}

/// A lane set in the order the store yields it, sorted by context.
fn lane<'a>(entries: &[(&'a Arc<Context>, i64)]) -> Vec<(&'a Arc<Context>, EpochSeconds)> {
    let mut sorted: Vec<_> = entries
        .iter()
        .map(|(c, seen)| (*c, EpochSeconds::from_epoch_secs(*seen)))
        .collect();
    sorted.sort_by(|(a, _), (b, _)| a.as_ref().cmp(b.as_ref()));
    sorted
}

fn names(difference: &[Diverging<'_>]) -> Vec<(Target, String)> {
    difference.iter().map(|d| (d.lane, d.context.name.clone())).collect()
}

/// Arbitrary contexts drawn from a small pool, so the two lanes overlap often enough to be interesting.
fn pool() -> Vec<Arc<Context>> {
    vec![
        ctx("a", &["env:test"], MetricKind::Count),
        ctx("b", &["env:test"], MetricKind::Gauge),
        ctx("c", &["env:test"], MetricKind::Rate),
        ctx("d", &[], MetricKind::Count),
    ]
}

proptest! {
    // Invariant 27. Neither lane is privileged. Swapping them yields the same set of contexts, with
    // each member's lane flipped.
    #[test]
    fn property_test_difference_is_symmetric(a_mask in 0u8..16, b_mask in 0u8..16) {
        let pool = pool();
        let pick = |mask: u8| -> Vec<(&Arc<Context>, i64)> {
            pool.iter().enumerate().filter(|(i, _)| mask >> i & 1 == 1).map(|(_, c)| (c, 100)).collect()
        };
        let (a, b) = (lane(&pick(a_mask)), lane(&pick(b_mask)));
        let now = EpochSeconds::from_epoch_secs(200);

        let forward: BTreeSet<String> = difference(&a, &b, now).iter().map(|d| d.context.name.clone()).collect();
        let backward: BTreeSet<String> = difference(&b, &a, now).iter().map(|d| d.context.name.clone()).collect();

        prop_assert_eq!(forward, backward);
    }

    // Invariant 28. A lane compared with itself diverges nowhere, whatever it holds.
    #[test]
    fn property_test_a_lane_never_differs_from_itself(mask in 0u8..16) {
        let pool = pool();
        let entries: Vec<(&Arc<Context>, i64)> =
            pool.iter().enumerate().filter(|(i, _)| mask >> i & 1 == 1).map(|(_, c)| (c, 100)).collect();
        let side = lane(&entries);

        prop_assert!(difference(&side, &side, EpochSeconds::from_epoch_secs(200)).is_empty());
    }

    // Invariant 29. Membership is exactly exclusive-or. A context on both lanes is absent from the
    // difference and a context on one lane is present, with no third case.
    #[test]
    fn property_test_membership_is_exclusive_or(a_mask in 0u8..16, b_mask in 0u8..16) {
        let pool = pool();
        let pick = |mask: u8| -> Vec<(&Arc<Context>, i64)> {
            pool.iter().enumerate().filter(|(i, _)| mask >> i & 1 == 1).map(|(_, c)| (c, 100)).collect()
        };
        let (a, b) = (lane(&pick(a_mask)), lane(&pick(b_mask)));
        let found: BTreeSet<String> =
            difference(&a, &b, EpochSeconds::from_epoch_secs(200)).iter().map(|d| d.context.name.clone()).collect();

        for (i, context) in pool.iter().enumerate() {
            let on_a = a_mask >> i & 1 == 1;
            let on_b = b_mask >> i & 1 == 1;
            prop_assert_eq!(found.contains(&context.name), on_a != on_b, "{}", context.name);
        }
    }
}

// Invariant 30. The tagset is a set, so tag order never decides identity and two lanes listing the
// same tags differently agree.
#[test]
fn tag_order_does_not_decide_identity() {
    let a = ctx("requests", &["b:2", "a:1"], MetricKind::Count);
    let b = ctx("requests", &["a:1", "b:2"], MetricKind::Count);

    let difference = difference(
        &lane(&[(&a, 100)]),
        &lane(&[(&b, 100)]),
        EpochSeconds::from_epoch_secs(200),
    );

    assert!(difference.is_empty());
}

// Invariant 31. Kind is part of identity. The same name and tags flushed as different types are two
// contexts, so a lane that changed a metric's type diverges on both.
#[test]
fn kind_is_part_of_identity() {
    let count = ctx("requests", &["host:h"], MetricKind::Count);
    let gauge = ctx("requests", &["host:h"], MetricKind::Gauge);

    let difference = difference(
        &lane(&[(&count, 100)]),
        &lane(&[(&gauge, 100)]),
        EpochSeconds::from_epoch_secs(200),
    );

    assert_eq!(difference.len(), 2);
}

// A member carries the lane that holds it, so triage reads the direction without a second lookup.
// The lane says where the context was observed, not which side is at fault.
#[test]
fn a_member_names_the_lane_that_holds_it() {
    let agent_only = ctx("agent.only", &[], MetricKind::Count);
    let adp_only = ctx("adp.only", &[], MetricKind::Gauge);

    let difference = difference(
        &lane(&[(&agent_only, 94)]),
        &lane(&[(&adp_only, 97)]),
        EpochSeconds::from_epoch_secs(100),
    );

    assert_eq!(
        names(&difference),
        vec![
            (Target::Adp, "adp.only".to_string()),
            (Target::Agent, "agent.only".to_string()),
        ]
    );
    assert_eq!(difference[0].age_secs, 3);
    assert_eq!(difference[1].age_secs, 6);
}

// A member counts as overdue only once the intake clock has moved further than the budget past its
// first sighting. At the budget exactly it is still in flight, which keeps the eventually_ check from
// firing on a lane that is merely mid-flush.
#[test]
fn overdue_counts_only_past_the_budget() {
    let context = ctx("adp.requests", &[], MetricKind::Count);
    let at = |now: i64| {
        difference(
            &lane(&[(&context, 100)]),
            &lane(&[]),
            EpochSeconds::from_epoch_secs(now),
        )
    };

    assert_eq!(overdue(&at(160), 60), 0);
    assert_eq!(overdue(&at(161), 60), 1);
}

// Both lanes empty is not a divergence, and the caller must not read that as agreement. The endpoint
// reports the compared count alongside, so a run that saw nothing is distinguishable from a healthy one.
#[test]
fn two_empty_lanes_diverge_nowhere() {
    assert!(difference(&lane(&[]), &lane(&[]), EpochSeconds::from_epoch_secs(200)).is_empty());
}

// The result is ordered by context, not by arrival, so a replay produces the same exemplar list.
#[test]
fn ordering_is_by_context_not_arrival() {
    let z = ctx("z.metric", &[], MetricKind::Count);
    let a = ctx("a.metric", &[], MetricKind::Count);
    let mut entries = BTreeMap::new();
    entries.insert("z", &z);
    entries.insert("a", &a);

    let difference = difference(
        &lane(&[(&z, 100), (&a, 100)]),
        &lane(&[]),
        EpochSeconds::from_epoch_secs(200),
    );

    assert_eq!(
        difference.iter().map(|d| d.context.name.as_str()).collect::<Vec<_>>(),
        vec!["a.metric", "z.metric"]
    );
}
