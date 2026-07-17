use std::collections::BTreeSet;

use datadog_protos::metrics::metric_payload::{MetricPoint, MetricType, Resource};
use proptest::prelude::*;
use proptest::strategy::BoxedStrategy;
use serde_json::json;

use super::*;

fn context(name: &str, tags: &[&str], kind: MetricKind) -> Context {
    Context {
        name: name.to_string(),
        tagset: tags.iter().map(|t| (*t).to_string()).collect(),
        kind,
    }
}

#[test]
fn contexts_serialize_to_the_flat_wire_shape() {
    let mut lanes = Lanes::default();
    lanes.record(
        Target::Adp,
        &[(
            context("requests", &["host:agent-host", "env:test"], MetricKind::Count),
            2,
        )],
        EpochSeconds::from_epoch_secs(1_000),
    );

    // Flat: name, tagset (a sorted set), kind as a snake_case token, first_seen and points as bare
    // numbers.
    let wire = serde_json::to_value(lanes.contexts(Target::Adp)).expect("serialize");
    assert_eq!(
        wire,
        json!([{
            "name": "requests",
            "tagset": ["env:test", "host:agent-host"],
            "kind": "count",
            "first_seen": 1_000,
            "points": 2,
        }])
    );
}

#[test]
fn re_recording_a_context_sums_points_and_keeps_first_seen() {
    let mut lanes = Lanes::default();
    let ctx = context("requests", &["host:h"], MetricKind::Count);
    lanes.record(Target::Adp, &[(ctx.clone(), 3)], EpochSeconds::from_epoch_secs(100));
    lanes.record(Target::Adp, &[(ctx, 4)], EpochSeconds::from_epoch_secs(200));

    let got = lanes.contexts(Target::Adp);
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].points, 7);
    assert_eq!(got[0].first_seen, EpochSeconds::from_epoch_secs(100));
}

#[test]
fn self_telemetry_points_are_never_counted() {
    let mut lanes = Lanes::default();
    lanes.record(
        Target::Adp,
        &[(context("datadog.agent.running", &[], MetricKind::Gauge), 9)],
        EpochSeconds::from_epoch_secs(0),
    );
    assert!(lanes.contexts(Target::Adp).is_empty());
}

#[test]
fn observe_series_counts_points_per_context() {
    // A two-point series contributes two points to its context, however stele groups them.
    let mut series = built_series("adp.requests", 1, 1);
    let mut second = MetricPoint::new();
    second.value = 2.0;
    second.timestamp = 1_600_000_060;
    series.points.push(second);
    let mut payload = MetricPayload::new();
    payload.series.push(series);

    let observed = observe_series(Target::Agent, payload);
    let points: u64 = observed
        .iter()
        .filter(|(c, _)| c.name == "adp.requests")
        .map(|(_, n)| n)
        .sum();
    assert_eq!(points, 2);
}

// --- generators ---

const NAME: &[&str] = &[
    "adp.requests",
    "dogstatsd.errors",
    "queue.depth",
    "workers.latency",
    "datadog.agent.running",
    "datadog.dogstatsd.packets",
    "datadog",
];

fn any_tag() -> impl Strategy<Value = String> {
    let word = || proptest::collection::vec(proptest::char::range('a', 'z'), 1..4);
    (word(), word()).prop_map(|(k, v)| {
        format!(
            "{}:{}",
            k.into_iter().collect::<String>(),
            v.into_iter().collect::<String>()
        )
    })
}

impl Arbitrary for MetricKind {
    type Parameters = ();
    type Strategy = BoxedStrategy<Self>;
    fn arbitrary_with((): ()) -> Self::Strategy {
        prop_oneof![
            Just(MetricKind::Count),
            Just(MetricKind::Rate),
            Just(MetricKind::Gauge),
            Just(MetricKind::Sketch),
        ]
        .boxed()
    }
}

impl Arbitrary for Target {
    type Parameters = ();
    type Strategy = BoxedStrategy<Self>;
    fn arbitrary_with((): ()) -> Self::Strategy {
        prop_oneof![Just(Target::Agent), Just(Target::Adp)].boxed()
    }
}

impl Arbitrary for EpochSeconds {
    type Parameters = ();
    type Strategy = BoxedStrategy<Self>;
    fn arbitrary_with((): ()) -> Self::Strategy {
        any::<i64>().prop_map(EpochSeconds::from_epoch_secs).boxed()
    }
}

impl Arbitrary for Context {
    type Parameters = ();
    type Strategy = BoxedStrategy<Self>;
    fn arbitrary_with((): ()) -> Self::Strategy {
        (
            proptest::sample::select(NAME),
            proptest::collection::btree_set(any_tag(), 0..4),
            any::<MetricKind>(),
        )
            .prop_map(|(name, tagset, kind)| Context {
                name: name.to_string(),
                tagset,
                kind,
            })
            .boxed()
    }
}

/// One record operation: which lane, the contexts in it, and when it arrived.
#[derive(Clone, Debug)]
struct Op {
    target: Target,
    contexts: Vec<Context>,
    now: EpochSeconds,
}

impl Arbitrary for Op {
    type Parameters = ();
    type Strategy = BoxedStrategy<Self>;
    fn arbitrary_with((): ()) -> Self::Strategy {
        (
            any::<Target>(),
            proptest::collection::vec(any::<Context>(), 0..8),
            any::<EpochSeconds>(),
        )
            .prop_map(|(target, contexts, now)| Op { target, contexts, now })
            .boxed()
    }
}

fn op_sequence() -> impl Strategy<Value = Vec<Op>> {
    proptest::collection::vec(any::<Op>(), 0..24)
}

fn context_batch() -> impl Strategy<Value = Vec<Context>> {
    proptest::collection::vec(any::<Context>(), 0..12)
}

/// Independent oracle: a lane holds each non-self-telemetry context that arrived on it, stamped at
/// the first op that carried it. Computed with a seen-set scan, not the recorder's own structure.
fn replay(ops: &[Op], lane: Target) -> BTreeSet<(Context, EpochSeconds)> {
    let mut seen = BTreeSet::new();
    let mut expected = BTreeSet::new();
    for op in ops {
        if op.target != lane {
            continue;
        }
        for context in &op.contexts {
            if context.name.starts_with("datadog.") {
                continue;
            }
            if seen.insert(context.clone()) {
                expected.insert((context.clone(), op.now));
            }
        }
    }
    expected
}

proptest! {
    // Model test: after any sequence of records, each lane equals an independent replay of the ops.
    // Subsumes lane isolation, per-context first-seen across times, idempotence, and self-telemetry
    // exclusion.
    #[test]
    fn lanes_match_a_replay_oracle(ops in op_sequence()) {
        let mut lanes = Lanes::default();
        for op in &ops {
            let batch: Vec<(Context, u64)> = op.contexts.iter().map(|c| (c.clone(), 1)).collect();
            lanes.record(op.target, &batch, op.now);
        }

        for lane in [Target::Agent, Target::Adp] {
            let got: BTreeSet<(Context, EpochSeconds)> =
                lanes.contexts(lane).into_iter().map(|c| (c.context, c.first_seen)).collect();
            prop_assert_eq!(got, replay(&ops, lane));
        }
    }

    // The returned count is exactly how many contexts the fold newly added, against a non-empty lane.
    #[test]
    fn record_returns_the_count_of_new_contexts(seed in context_batch(), batch in context_batch(), now in any::<EpochSeconds>()) {
        let mut lanes = Lanes::default();
        let seed: Vec<(Context, u64)> = seed.into_iter().map(|c| (c, 1)).collect();
        let batch: Vec<(Context, u64)> = batch.into_iter().map(|c| (c, 1)).collect();
        lanes.record(Target::Adp, &seed, EpochSeconds::from_epoch_secs(0));
        let before = lanes.contexts(Target::Adp).len();

        let added = lanes.record(Target::Adp, &batch, now);

        let after = lanes.contexts(Target::Adp).len();
        prop_assert_eq!(added, after - before);
    }
}

// --- production-parity per-series drops ---

fn built_series(name: &str, tags: usize, resources: usize) -> MetricSeries {
    let mut s = MetricSeries::new();
    s.set_metric(name.to_string());
    s.set_type(MetricType::COUNT);
    for i in 0..tags {
        s.tags.push(format!("k{i}:v"));
    }
    for i in 0..resources {
        let mut r = Resource::new();
        r.set_type("host".to_string());
        r.set_name(format!("h{i}"));
        s.resources.push(r);
    }
    let mut p = MetricPoint::new();
    p.value = 1.0;
    p.timestamp = 1_600_000_000;
    s.points.push(p);
    s
}

#[test]
fn series_kept_matches_propjoe_validation() {
    // Valid, and the count boundaries propjoe keeps.
    assert!(series_kept_by_intake(&built_series("adp.requests", 1, 1)));
    assert!(series_kept_by_intake(&built_series(&"a".repeat(350), 1, 1)));
    assert!(series_kept_by_intake(&built_series("ok", 100, 1)));
    assert!(series_kept_by_intake(&built_series("ok", 1, 500)));

    // Dropped: empty, no ASCII-alphabetic char, over the byte limit.
    assert!(!series_kept_by_intake(&built_series("", 1, 1)));
    assert!(!series_kept_by_intake(&built_series("123.456", 1, 1)));
    assert!(!series_kept_by_intake(&built_series(&"a".repeat(351), 1, 1)));
    // Dropped: over the tag and resource count thresholds.
    assert!(!series_kept_by_intake(&built_series("ok", 101, 1)));
    assert!(!series_kept_by_intake(&built_series("ok", 1, 501)));
}

#[test]
fn observe_series_drops_what_propjoe_drops() {
    let mut payload = MetricPayload::new();
    payload.series.push(built_series("adp.requests", 1, 1)); // kept
    payload.series.push(built_series("", 1, 1)); // empty name
    payload.series.push(built_series("999", 1, 1)); // no alpha
    payload.series.push(built_series("adp.toomanytags", 101, 1)); // tag flood

    let contexts = observe_series(Target::Agent, payload);
    let names: BTreeSet<&str> = contexts.iter().map(|(c, _)| c.name.as_str()).collect();
    assert_eq!(names, BTreeSet::from(["adp.requests"]));
}
