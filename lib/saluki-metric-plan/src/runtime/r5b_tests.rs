//! R5b, the push-down of a filter into the emission of the state owner above it, is exact through a frame of region
//! drivers: what every output receives, and the filter's own series, are what they are without it, and what the
//! reference interpreter, which applies no rewrite, delivers.
//!
//! Plans are `source -> [kind filter] -> aggregate -> [merge] -> filter -> emit`, the filter dropping what its
//! predicate matches, or what it does not; predicates read names, intake names, hosts, tags, origin tags, and kinds.
//! Traces mix every kind in one context, kind conflicts, rate intervals, timestamped points, context limits from zero
//! to a few contexts (capacity pressure), counter expiry from zero to minutes, and installations that change the
//! filter's predicate, which keep aggregation's state (`retain`). Every source heartbeats at each tick, so that the
//! frame's ticks and the interpreter's fire at the same times (see `transition_tests`).

use proptest::{collection::vec, prelude::*, sample::select};
use saluki_core::{
    data_model::event::metric::{context::Context, Metric},
    execution::LossClass,
};
use serde_json::{json, Value};

use super::{
    harness::HarnessOptions,
    transition_tests::{aggregation, dogstatsd, node, plan, report, t, Replay},
};
use crate::{
    compile::{compile, rewrite::RewriteId, Frame},
    equivalence::{compare, OutputPoints},
    interpret::{RunOutput, Trace},
    ir::{NativeComponentId, NodeId, Plan},
    model::Kind,
    operators::OperatorRegistry,
    predicate::{Predicate, StringMatch},
    testing::{arb_metrics_with_kind_siblings, arb_predicate, predicate_json, NAME_VOCABULARY},
};

/// The names a histogram's statistics take in the plans here (aggregates `max` and `count`), as suffixes of its own.
const STATISTIC_SUFFIXES: &[&str] = &["", ".max", ".count"];

/// The shape of a plan around its aggregation and its filter.
#[derive(Clone, Debug)]
struct Shape {
    /// Whether a kind filter before aggregation lets only these kinds through, dropping the rest.
    kinds: Option<Vec<&'static str>>,
    /// Whether a one-input `merge` stands between aggregation and the filter.
    merged: bool,
    /// Whether the filter drops what its predicate matches (its `true` port leads to the `drop`), or what it does not.
    drops_matching: bool,
    /// Aggregation's context limit.
    context_limit: u64,
    /// Aggregation's counter expiry.
    counter_expiry: &'static str,
}

fn arb_shape() -> impl Strategy<Value = Shape> {
    (
        prop_oneof![
            3 => Just(None),
            1 => Just(Some(vec!["counter", "gauge"])),
            1 => Just(Some(vec!["set", "distribution", "histogram"])),
        ],
        any::<bool>(),
        any::<bool>(),
        prop_oneof![1 => Just(0u64), 3 => 1u64..4, 2 => Just(1_000)],
        prop_oneof![Just("0s"), Just("5s"), Just("5m")],
    )
        .prop_map(|(kinds, merged, drops_matching, context_limit, counter_expiry)| Shape {
            kinds,
            merged,
            drops_matching,
            context_limit,
            counter_expiry,
        })
}

/// Returns the plan of `shape` whose filter judges records with `predicate`.
fn shaped(shape: &Shape, predicate: &Predicate) -> Plan {
    let mut params = aggregation("10s");
    params["context_limit"] = json!(shape.context_limit);
    params["counter_expiry"] = json!(shape.counter_expiry);
    let mut nodes = vec![
        dogstatsd("mixed"),
        node("agg", "aggregate", params),
        node("f", "split", json!({"predicate": predicate_json(predicate)})),
        node("f_drop", "drop", json!({})),
        node("out", "emit", json!({"output": "a"})),
    ];
    let mut edges = Vec::new();
    match &shape.kinds {
        Some(kinds) => {
            nodes.extend([
                node("kinds", "split", json!({"predicate": {"kind_in": kinds}})),
                node("kinds_drop", "drop", json!({})),
            ]);
            edges.extend([
                "src.metrics -> kinds.in".to_string(),
                "kinds.true -> agg.in".to_string(),
                "kinds.false -> kinds_drop.in".to_string(),
            ]);
        }
        None => edges.push("src.metrics -> agg.in".to_string()),
    }
    if shape.merged {
        nodes.push(node("joined", "merge", json!({"inputs": 1})));
        edges.extend(["agg.out -> joined.0".to_string(), "joined.out -> f.in".to_string()]);
    } else {
        edges.push("agg.out -> f.in".to_string());
    }
    let (dropped, kept) = if shape.drops_matching {
        ("true", "false")
    } else {
        ("false", "true")
    };
    edges.extend([format!("f.{} -> f_drop.in", dropped), format!("f.{} -> out.in", kept)]);
    let edges = edges.iter().map(String::as_str).collect::<Vec<_>>();
    plan(nodes, &edges, &["a"])
}

/// Returns a strategy for the filter's predicate: any predicate, or one that tells a histogram from its statistics.
///
/// R5b judges a histogram's statistics at emission only when the filter reads none of what they change (name, kind,
/// metadata), and leaves the rest of them to the filter. Predicates drawn at random seldom tell a histogram apart from
/// its statistics, so half of these read the names of histograms and of their statistics (`<name>`, `<name>.max`,
/// `<name>.count`), or kinds that split the statistics (a gauge's without the rate's), alone or beside another
/// predicate.
fn arb_filter_predicate() -> impl Strategy<Value = Predicate> {
    let statistic =
        (select(NAME_VOCABULARY), select(STATISTIC_SUFFIXES)).prop_map(|(name, suffix)| name.to_string() + suffix);
    let names = prop_oneof![
        statistic
            .clone()
            .prop_map(|name| Predicate::Name(StringMatch::Eq(name))),
        vec(statistic, 1..4).prop_map(|names| Predicate::Name(StringMatch::In(names))),
        select(NAME_VOCABULARY).prop_map(|name| Predicate::Name(StringMatch::Prefix(name.to_string()))),
    ];
    let kinds = select(vec![
        vec![Kind::Gauge],
        vec![Kind::Rate],
        vec![Kind::Gauge, Kind::Counter],
        vec![Kind::Rate, Kind::Distribution],
        vec![Kind::Gauge, Kind::Histogram],
    ])
    .prop_map(|kinds| Predicate::KindIn(kinds.into_iter().collect()));
    let telling = prop_oneof![names, kinds];
    prop_oneof![
        2 => arb_predicate(),
        1 => telling.clone(),
        1 => (telling, arb_predicate(), any::<bool>()).prop_map(|(telling, other, all)| {
            if all {
                Predicate::All(vec![telling, other])
            } else {
                Predicate::Any(vec![telling, other])
            }
        }),
    ]
}

/// One step of a trace: a batch admitted, a flush tick, or an installation of the plan with another predicate.
#[derive(Clone, Debug)]
enum Step {
    Admit(Vec<Metric>),
    Advance,
    Install(Predicate),
}

fn arb_step() -> impl Strategy<Value = (u64, Step)> {
    (
        0u64..12,
        prop_oneof![
            // DogStatsD admits every kind but rates.
            6 => arb_metrics_with_kind_siblings(1..4).prop_map(|metrics| {
                Step::Admit(metrics.into_iter().filter(|metric| Kind::of(metric.values()) != Kind::Rate).collect())
            }),
            3 => Just(Step::Advance),
            1 => arb_filter_predicate().prop_map(Step::Install),
        ],
    )
}

/// Builds the trace of `steps`, each `gap` seconds after the previous one, over the plan of `shape`.
///
/// A flush tick at the time of the previous one, or at the start, is left out: the interpreter ticks at every
/// `advance`, but the frame's regions tick once per flush interval (1 second) from their start, so such a tick would
/// flush in the interpreter only (see `transition_tests`).
fn trace_of(shape: &Shape, predicate: &Predicate, steps: &[(u64, Step)]) -> Trace {
    let mut trace = Trace::new().install(shaped(shape, predicate));
    let mut now = 0;
    // The frame starts at 0.
    let mut last_tick = Some(0);
    for (gap, step) in steps {
        now += gap;
        trace = match step {
            Step::Admit(metrics) => trace.admit(src(), t(now), metrics.clone()),
            Step::Advance if last_tick == Some(now) => trace,
            Step::Advance => {
                last_tick = Some(now);
                trace.advance(t(now))
            }
            Step::Install(predicate) => trace.install(shaped(shape, predicate)),
        };
    }
    // Every bucket closes, and every idle counter expires or synthesizes its zeros, before the trace ends.
    trace.advance(t(now + 320)).close()
}

fn src() -> NodeId {
    "src".parse().unwrap()
}

/// Runs a trace through the frame with and without R5b, and returns the interpreter's run with both replays.
fn runs(trace: &Trace) -> (RunOutput, Replay, Replay) {
    let (run, pushed) = Replay::run_with(trace, true, t(0), HarnessOptions::default());
    let plain = HarnessOptions {
        pushed_filters: false,
        ..HarnessOptions::default()
    };
    let (_, unpushed) = Replay::run_with(trace, true, t(0), plain);
    (run, pushed, unpushed)
}

/// Returns whether R5b applies to the plan in its own frame.
fn pushed(plan: &Plan) -> bool {
    let registry = OperatorRegistry::native();
    let namer =
        |nodes: &[NodeId]| NativeComponentId::new(format!("region_{}", nodes[0].local())).map_err(|e| e.to_string());
    let frame = Frame::compute(plan, &registry, &namer).unwrap();
    let compiled = compile(plan, &report(plan), &registry, &frame).unwrap();
    compiled
        .rewrites
        .iter()
        .any(|record| record.id == RewriteId::R5b && record.is_applied())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn property_test_r5b_exact(
        shape in arb_shape(),
        predicate in arb_filter_predicate(),
        steps in vec(arb_step(), 1..24),
    ) {
        let trace = trace_of(&shape, &predicate, &steps);
        let (run, mut pushed, mut unpushed) = runs(&trace);

        // Every output receives exactly what it receives without R5b, and what the interpreter delivers.
        let expected = OutputPoints::from_run(&run);
        if let Err(mismatch) = compare(&expected, pushed.harness.outputs()) {
            prop_assert!(false, "R5b differs from the interpreter:\n{}", mismatch);
        }
        if let Err(mismatch) = compare(unpushed.harness.outputs(), pushed.harness.outputs()) {
            prop_assert!(false, "R5b differs from the frame without it:\n{}", mismatch);
        }

        // The filter's own series count every record it would have dropped, as without R5b: what it sent to its drop
        // port, and their points as the drop's policy loss, which is also the interpreter's.
        pushed.harness.publish();
        unpushed.harness.publish();
        let drop_port = if shape.drops_matching { "true" } else { "false" };
        prop_assert_eq!(
            pushed.harness.telemetry().port_events("f", drop_port),
            unpushed.harness.telemetry().port_events("f", drop_port)
        );
        prop_assert_eq!(
            pushed.harness.telemetry().loss("f_drop", LossClass::Policy),
            unpushed.harness.telemetry().loss("f_drop", LossClass::Policy)
        );
        prop_assert_eq!(
            pushed.harness.telemetry().loss("f_drop", LossClass::Policy),
            run.loss(&"f_drop".parse().unwrap()).loss(LossClass::Policy).points
        );
        // Aggregation's own capacity loss, under the context limit, is untouched: R5b changes no state.
        prop_assert_eq!(
            pushed.harness.telemetry().loss("agg", LossClass::Capacity),
            run.loss(&"agg".parse().unwrap()).loss(LossClass::Capacity).points
        );
    }
}

#[test]
fn r5b_applies_to_the_filters_the_property_test_draws_and_suppresses_what_they_drop() {
    // A deterministic instance of the property test's plans, where R5b applies and suppresses records: a filter on the
    // name that drops `drop.me`, after a merge. Its series count both suppressed counters as dropped, as without R5b,
    // and the output receives only `keep`.
    let shape = Shape {
        kinds: None,
        merged: true,
        drops_matching: true,
        context_limit: 1_000,
        counter_expiry: "5m",
    };
    let predicate: Predicate = serde_json::from_value(json!({"name": {"eq": "drop.me"}})).unwrap();
    let plan = shaped(&shape, &predicate);
    assert!(pushed(&plan), "R5b pushes the name filter into aggregation");
    let counter = |name: &'static str| Metric::counter(Context::from_static_name(name), 1.0);
    let trace = Trace::new()
        .install(plan)
        .admit(src(), t(1), vec![counter("drop.me"), counter("keep")])
        .admit(src(), t(12), vec![counter("drop.me"), counter("keep")])
        .advance(t(15))
        .advance(t(25))
        .close();
    let (run, mut pushed, mut unpushed) = runs(&trace);
    let expected = OutputPoints::from_run(&run);
    assert!(compare(&expected, pushed.harness.outputs()).is_ok());
    assert_eq!(expected.count(&"a".parse().unwrap()), 2, "keep's [0] and [10]");
    pushed.harness.publish();
    unpushed.harness.publish();
    // The drop counts one record per flushed bucket of `drop.me`: [0] at 15 and [10] at 25, and its zero at 25 is not
    // a record of its own (it is a point of the [10, 20) record).
    for telemetry in [pushed.harness.telemetry(), unpushed.harness.telemetry()] {
        assert_eq!(telemetry.port_events("f", "true"), 2);
        assert_eq!(telemetry.loss("f_drop", LossClass::Policy), 2);
    }
    assert_eq!(run.loss(&"f_drop".parse().unwrap()).loss(LossClass::Policy).points, 2);
    // Without R5b aggregation emits the records the filter drops; with it they never leave aggregation, as the
    // rewrite's record says of aggregation's series.
    assert_eq!(unpushed.harness.telemetry().port_events("agg", "out"), 4);
    assert_eq!(pushed.harness.telemetry().port_events("agg", "out"), 2);
}

#[test]
fn r5b_leaves_histogram_statistics_to_the_filter_unless_it_reads_nothing_they_change() {
    // A histogram `h`, tagged `env:prod`, flushed once into its statistics `h.max` (a gauge) and `h.count` (a rate),
    // through a filter that drops what its predicate matches. (predicate, whether R5b judges the statistics at
    // emission, the statistics the output receives.)
    let cases: [(Value, bool, &[&str]); 5] = [
        // The histogram's own name: neither statistic has it.
        (json!({"name": {"eq": "h"}}), false, &["h.count", "h.max"]),
        // One statistic's name.
        (json!({"name": {"eq": "h.max"}}), false, &["h.count"]),
        // Kinds that split the statistics: the gauge goes, the rate stays, and the other way around.
        (json!({"kind_in": ["gauge"]}), false, &["h.count"]),
        (json!({"kind_in": ["rate"]}), false, &["h.max"]),
        // A tag, which every statistic keeps: judged at emission, both go.
        (json!({"tag": {"key": "env", "value": {"eq": "prod"}}}), true, &[]),
    ];
    let shape = Shape {
        kinds: None,
        merged: false,
        drops_matching: true,
        context_limit: 1_000,
        counter_expiry: "5m",
    };
    for (predicate, judged, expected) in cases {
        let what = predicate.to_string();
        let predicate: Predicate = serde_json::from_value(predicate).unwrap();
        let plan = shaped(&shape, &predicate);
        assert!(pushed(&plan), "{}: R5b applies", what);
        let histogram = Metric::histogram(Context::from_static_parts("h", &["env:prod"]), [1.0, 3.0]);
        let trace = Trace::new()
            .install(plan)
            .admit(src(), t(1), vec![histogram])
            .advance(t(15))
            .close();
        let (run, mut pushed, mut unpushed) = runs(&trace);
        let expected_points = OutputPoints::from_run(&run);
        assert!(
            compare(&expected_points, pushed.harness.outputs()).is_ok(),
            "{}: as the interpreter",
            what
        );
        let mut received = pushed
            .harness
            .delivered()
            .iter()
            .map(|record| record.metric.context().name().to_string())
            .collect::<Vec<_>>();
        received.sort();
        assert_eq!(received, expected, "{}", what);

        // The filter's series count both statistics either way; aggregation emits only those it does not suppress.
        pushed.harness.publish();
        unpushed.harness.publish();
        let dropped = 2 - expected.len() as u64;
        for telemetry in [pushed.harness.telemetry(), unpushed.harness.telemetry()] {
            assert_eq!(telemetry.port_events("f", "true"), dropped, "{}", what);
        }
        let emitted = if judged { 2 - dropped } else { 2 };
        assert_eq!(
            pushed.harness.telemetry().port_events("agg", "out"),
            emitted,
            "{}",
            what
        );
    }
}
