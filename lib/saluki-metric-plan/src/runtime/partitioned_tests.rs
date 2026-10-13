//! Partitioned execution against its oracles: frames on lanes run by a [`LanedFrameHarness`], checked against the
//! reference interpreter in trace order, against the arrival replay of what the state lanes saw and the ticks they
//! reported, against the interpreter's candidates under interleaving, and by the capacity and loss balance checks
//! (6.3, 6.4); and the fixtures of 6.9 at the frame level.
//!
//! The harness's own tests run here on frames whose state slot runs one lane, and the properties and fixtures on state
//! lanes, producer lanes, and keyed exits that coalesce, against the real drivers.
//!
//! Generated traces tick every second in every timeline of the source's clock: the harnesses' monotonic clock moves a
//! second across each of the clock's jumps back ([`HarnessOptions::monotonic_across_jumps`]), and every producer lane
//! heartbeats at each tick, stated in the trace, except a lane that stays quiet for some ticks after another lane's
//! reset, which keeps state lanes' reset windows open across ticks and at the close. Plans rename, prefix, and strip a
//! tag before aggregation, so that contexts collide after their renames ([`Shape`]).
//!
//! The properties exclude what the lanes count as outside the equivalence they keep: ticks the lanes resolved apart
//! from their shared tick record, and reset windows that overflowed ([`FrameHarness::ticks_resolved_apart`],
//! [`FrameHarness::reset_overflows`]). Those read the lanes' own counters, which reach the harness through the
//! telemetry handles it gives each lane, a slot's only lane too. The arrival replay places
//! the ticks the state lanes reported they fired, and the recorder's own model of their ticks cross-checks them. Exact
//! properties generate no input whose effect depends on the order of several points (`without_races`), since
//! coalescing orders what several points send one lane by buffer, not by call.
//!
//! Which state lane a context goes to follows its context key, a hash seeded once per process: a failure on several
//! state lanes reproduces in a process that routes the trace's contexts alike, so a saved regression may take a few
//! runs to fail again, and the tests that pin one down use a delay, which names the lane, not a seeded schedule.

use std::{collections::BTreeMap, time::Duration};

use proptest::{collection::vec, prelude::*};
use saluki_core::{
    data_model::{
        event::metric::{context::Context, Metric, MetricValues},
        tags::TagSet,
    },
    execution::{AdmissionTime, GenerationId, LossClass},
};
use serde_json::{json, Value};

use super::{
    harness::{DeliverySchedule, FrameHarness, HarnessOptions, LaneTimeEvent, LanedFrameHarness},
    transition_tests::{aggregation, catalog, dogstatsd, node, plan, report, t, B},
};
use crate::{
    compile::{KeyGroups, LanePolicy},
    equivalence::{
        check_balance, check_capacity, compare, compare_partitioned, Candidates, CapacityBalance, CapacityVerdict,
        ContextIdentity, OutputPoints, Point, PointKind, PointValue, ReorderedVerdict, Tolerance,
    },
    interpret::{Interpreter, RunOutput, Trace, TraceEvent},
    ir::{NodeId, OutputId, Plan},
    operators::OperatorRegistry,
};

fn src() -> NodeId {
    "src".parse().unwrap()
}

fn output() -> OutputId {
    OutputId::new("a").unwrap()
}

/// `src -> keep -> agg -> post -> emit a`: aggregation with `params` behind a fused slot, whose keyed exit routes to
/// the state lanes, and a fused slot after it, which runs the post-aggregation lanes.
fn kept_with(params: Value) -> Plan {
    plan(
        vec![
            dogstatsd("untimestamped"),
            node("keep", "split", json!({"predicate": {"const": true}})),
            node("dropped", "drop", json!({})),
            node("agg", "aggregate", params),
            node("post", "split", json!({"predicate": {"const": true}})),
            node("post_dropped", "drop", json!({})),
            node("out", "emit", json!({"output": "a"})),
        ],
        &[
            "src.metrics -> keep.in",
            "keep.true -> agg.in",
            "keep.false -> dropped.in",
            "agg.out -> post.in",
            "post.true -> out.in",
            "post.false -> post_dropped.in",
        ],
        &["a"],
    )
}

fn kept() -> Plan {
    kept_with(aggregation("10s"))
}

fn counter(name: String, value: f64) -> Metric {
    Metric::counter(Context::from_parts(name, TagSet::default()), value)
}

fn harness_on(
    boot: &Plan, lanes: LanePolicy, schedule: DeliverySchedule, options: HarnessOptions,
) -> LanedFrameHarness {
    LanedFrameHarness::new(
        boot,
        &report(boot),
        OperatorRegistry::native(),
        options,
        t(0),
        lanes,
        schedule,
    )
    .unwrap_or_else(|error| panic!("{}", error))
}

/// Runs `trace` through the interpreter with `src` on `lanes` lanes, every lane sending a heartbeat at each tick.
fn interpreted(trace: &Trace, lanes: u16, candidates: bool) -> RunOutput {
    let registry = OperatorRegistry::native();
    let catalog = catalog();
    let interpreter = Interpreter::new(&registry, &catalog)
        .with_source_lanes(src(), lanes)
        .with_heartbeats_at_ticks();
    let interpreter = if candidates {
        interpreter.with_candidates()
    } else {
        interpreter
    };
    interpreter.run(trace).unwrap()
}

/// Runs `trace`'s stamps, ticks, and close through a harness on lanes, as [`Trace`] states them, every lane of `src`
/// sending a heartbeat at each tick, as the interpreter's heartbeats at ticks do. The trace installs only its boot
/// plan.
fn run_harness(harness: &mut FrameHarness, trace: &Trace) {
    let mut clock = t(0);
    for event in &trace.events()[1..] {
        match event {
            TraceEvent::Admit {
                lane,
                admitted_at,
                batch,
                ..
            } => harness.admit_at_lane("src", *lane, *admitted_at, batch.clone()),
            TraceEvent::Heartbeat { lane, admitted_at, .. } => harness.heartbeat_at_lane("src", *lane, *admitted_at),
            TraceEvent::Advance { clock: at } => {
                harness.advance(*at);
                clock = *at;
            }
            TraceEvent::Close => harness.close(clock),
            other => panic!(
                "the lane properties' traces have no `{}` after their boot plan",
                other.name()
            ),
        }
    }
}

fn assert_equal_outputs(expected: &OutputPoints, actual: &OutputPoints, what: &str) {
    if let Err(mismatch) = compare(expected, actual) {
        panic!("{} differ:\n{}", what, mismatch);
    }
}

// The harness on frames whose state slot runs one lane.

#[test]
fn a_harness_refuses_a_frame_that_runs_none_of_the_state_lanes_it_was_asked_for() {
    // `src -> agg -> emit`: aggregation fed straight by the one producer point keeps one lane, by the frame's rules, so
    // a harness asked for two state lanes would run one without saying so. The same plan behind a fused slot runs
    // two.
    let straight = plan(
        vec![
            dogstatsd("untimestamped"),
            node("agg", "aggregate", aggregation("10s")),
            node("out", "emit", json!({"output": "a"})),
        ],
        &["src.metrics -> agg.in", "agg.out -> out.in"],
        &["a"],
    );
    let lanes = LanePolicy {
        dogstatsd: 1,
        state: 2,
        post: 1,
    };
    let refused = LanedFrameHarness::new(
        &straight,
        &report(&straight),
        OperatorRegistry::native(),
        HarnessOptions::default(),
        t(0),
        lanes,
        DeliverySchedule::TraceOrder,
    )
    .err()
    .expect("a frame on one state lane is refused");
    assert!(
        refused.contains("no isolation slot runs the 2 state lanes asked for"),
        "{}",
        refused
    );
    let harness = harness_on(&kept(), lanes, DeliverySchedule::TraceOrder, HarnessOptions::default());
    let isolation = harness
        .frame()
        .slots()
        .iter()
        .filter(|slot| slot.is_isolation())
        .map(|slot| slot.lanes)
        .collect::<Vec<_>>();
    assert_eq!(isolation, vec![2]);
}

#[test]
fn post_aggregation_lanes_spread_a_flush_over_their_lanes_and_deliver_what_the_interpreter_does() {
    // 2,100 contexts flush in several buffers, which the state slot spreads over the two post-aggregation lanes in
    // rotation; what the output receives is the interpreter's, and both lanes took part.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    let _guard = runtime.enter();
    let mut params = aggregation("10s");
    params["context_limit"] = json!(5_000);
    let boot = kept_with(params);
    let lanes = LanePolicy {
        dogstatsd: 1,
        state: 1,
        post: 2,
    };
    let options = HarnessOptions {
        heartbeats_at_ticks: true,
        ..HarnessOptions::default()
    };
    let mut trace = Trace::new().install(boot.clone());
    for batch in 0..3u64 {
        let metrics = (0..700)
            .map(|index| counter(format!("c{}", batch * 700 + index), 1.0))
            .collect();
        trace = trace.admit(src(), t(1 + batch), metrics);
    }
    let trace = trace.advance(t(11)).close();
    let mut harness = harness_on(&boot, lanes, DeliverySchedule::TraceOrder, options);
    assert_eq!(
        harness.frame().slots()[2].lanes,
        2,
        "the post-aggregation slot runs two lanes"
    );
    run_harness(&mut harness, &trace);
    let run = interpreted(&trace, 1, false);
    assert_equal_outputs(
        &OutputPoints::from_run(&run),
        harness.outputs(),
        "the frame on lanes and the interpreter",
    );
    assert_eq!(harness.outputs().count(&output()), 2_100);
    let post = harness.frame().slots()[2].name.to_string();
    for lane in 0..2 {
        assert!(
            harness.lane_arrivals(&post, lane).0 > 0,
            "post-aggregation lane {} received part of the flush",
            lane
        );
    }
}

#[test]
fn a_seeded_schedule_interleaves_what_the_lanes_receive_and_delivers_the_same_points() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    let _guard = runtime.enter();
    let boot = kept();
    let lanes = LanePolicy {
        dogstatsd: 1,
        state: 1,
        post: 2,
    };
    let options = HarnessOptions {
        heartbeats_at_ticks: true,
        ..HarnessOptions::default()
    };
    let mut trace = Trace::new().install(boot.clone());
    for second in 1..30u64 {
        let metrics = (0..40)
            .map(|index| counter(format!("c{}", (second * 7 + index) % 90), 1.0))
            .collect();
        trace = trace.admit(src(), t(second), metrics);
        if second % 5 == 0 {
            trace = trace.advance(t(second));
        }
    }
    let trace = trace.advance(t(41)).close();
    let expected = OutputPoints::from_run(&interpreted(&trace, 1, false));
    for seed in [1, 7, 42] {
        let mut harness = harness_on(&boot, lanes, DeliverySchedule::Seeded(seed), options);
        run_harness(&mut harness, &trace);
        assert_equal_outputs(&expected, harness.outputs(), &format!("seed {}", seed));
    }
}

#[test]
fn a_delay_holds_one_points_buffers_toward_one_lane_until_the_point_stamped_later() {
    // What the state slot receives from `src` through the fused slot's exit waits until `src` stamps a second after
    // it: the first batch at 1 is let through by the heartbeat at 2.2, the second, at 1.5, and what follows it on the
    // path, by the heartbeat at 3. The heartbeat at 11 still waits at the tick, which gates, and arrives when the
    // input ends, releasing the tick.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    let _guard = runtime.enter();
    let boot = kept();
    let point = boot_point(&boot);
    let schedule = DeliverySchedule::Delay {
        point,
        lane: 0,
        by: Duration::from_secs(1),
    };
    let mut harness = harness_on(&boot, LanePolicy::ONE, schedule, HarnessOptions::default());
    let agg = harness.frame().slots()[1].name.to_string();
    harness.admit("src", t(1), vec![counter("c".to_string(), 1.0)]);
    harness.admit("src", ms(1_500), vec![counter("c".to_string(), 1.0)]);
    assert_eq!(harness.arrivals(&agg).0, 0, "both batches wait");
    harness.heartbeat("src", ms(2_200));
    assert_eq!(harness.arrivals(&agg).0, 1, "the first batch is let through");
    harness.heartbeat("src", t(3));
    assert_eq!(harness.arrivals(&agg).0, 2, "the second batch is let through, in order");
    harness.heartbeat("src", t(11));
    harness.advance(t(11));
    assert!(
        harness.outputs().count(&output()) == 0,
        "the tick waits for the delayed heartbeat"
    );
    harness.close(t(11));
    let trace = Trace::new()
        .install(boot.clone())
        .admit(src(), t(1), vec![counter("c".to_string(), 1.0)])
        .admit(src(), ms(1_500), vec![counter("c".to_string(), 1.0)])
        .heartbeat(src(), ms(2_200))
        .heartbeat(src(), t(3))
        .heartbeat(src(), t(11))
        .advance(t(11))
        .close();
    assert_equal_outputs(
        &OutputPoints::from_run(&interpreted(&trace, 1, false)),
        harness.outputs(),
        "the delayed frame",
    );
}

/// Returns the admission point of `src`'s first lane in the frame of `boot`.
fn boot_point(boot: &Plan) -> saluki_core::execution::AdmissionPointId {
    let harness = harness_on(
        boot,
        LanePolicy::ONE,
        DeliverySchedule::TraceOrder,
        HarnessOptions::default(),
    );
    harness
        .frame()
        .admission_point_at(
            &src(),
            &crate::ir::PortName::from_static(crate::operators::METRICS_PORT),
        )
        .and_then(|point| point.lane_id(0))
        .expect("the source has an admission point")
}

fn ms(relative_millis: u64) -> AdmissionTime {
    AdmissionTime::from_unix_nanos((B * 1_000 + relative_millis) * 1_000_000)
}

#[test]
fn the_arrival_replay_of_a_state_lane_runs_to_what_the_frame_delivered() {
    // One state lane: its log is everything it received, in order, with its ticks placed by the shared rules; the
    // interpreter, in arrival order, delivers on it what the frame delivered.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    let _guard = runtime.enter();
    let boot = kept();
    let options = HarnessOptions {
        heartbeats_at_ticks: true,
        ..HarnessOptions::default()
    };
    let mut trace = Trace::new().install(boot.clone());
    for second in 1..24u64 {
        let metrics = vec![
            counter(format!("c{}", second % 3), 1.0),
            Metric::gauge(
                Context::from_parts(format!("g{}", second % 2), TagSet::default()),
                second as f64,
            ),
        ];
        trace = trace.admit(src(), ms(second * 1_000 + 300), metrics);
        if second % 4 == 0 {
            trace = trace.advance(t(second + 1));
        }
    }
    let trace = trace.advance(t(31)).close();
    let mut harness = harness_on(&boot, LanePolicy::ONE, DeliverySchedule::TraceOrder, options);
    run_harness(&mut harness, &trace);
    let replay = harness.replay().build().expect("one lane's replay is valid");
    assert!(
        replay
            .events()
            .iter()
            .any(|event| matches!(event, TraceEvent::Advance { .. })),
        "the replay places the lane's ticks"
    );
    let registry = OperatorRegistry::native();
    let catalog = catalog();
    let run = Interpreter::new(&registry, &catalog)
        .in_arrival_order()
        .run(&replay)
        .unwrap();
    assert_equal_outputs(
        &OutputPoints::from_run(&run),
        harness.outputs(),
        "the replay and the frame",
    );
    assert_equal_outputs(
        &OutputPoints::from_run(&interpreted(&trace, 1, false)),
        harness.outputs(),
        "the trace and the frame",
    );
}

#[test]
fn recorded_budgets_hold_under_the_capacity_check() {
    // Three contexts fit: of five new ones, two are refused, each refusal justified by the units its own operation saw
    // in use, and the global budget holds the three it granted.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    let _guard = runtime.enter();
    let boot = kept();
    let options = HarnessOptions {
        state_capacity: Some(3),
        record_budgets: true,
        ..HarnessOptions::default()
    };
    let mut harness = harness_on(&boot, LanePolicy::ONE, DeliverySchedule::TraceOrder, options);
    harness.admit(
        "src",
        t(1),
        (0..5).map(|index| counter(format!("c{}", index), 1.0)).collect(),
    );
    let traces = harness.budget_traces();
    assert_eq!(traces[0].0, "global");
    assert_eq!(traces[0].1.capacity, 3);
    for (budget, trace) in &traces {
        assert_eq!(check_capacity(trace), CapacityVerdict::Holds, "budget `{}`", budget);
    }
    assert_eq!(traces[0].1.held(), 3);
    assert!(
        traces.iter().any(|(budget, _)| budget == "agg"),
        "the node's budget is recorded"
    );
    harness.publish();
    assert_eq!(harness.telemetry().loss("agg", LossClass::Capacity), 2);
    assert!(
        harness_on(
            &boot,
            LanePolicy::ONE,
            DeliverySchedule::TraceOrder,
            HarnessOptions::default()
        )
        .budget_traces()
        .is_empty(),
        "budgets record only when asked to"
    );
}

#[test]
fn the_monotonic_clock_moves_a_step_across_a_jump_back_so_that_regions_tick_in_every_timeline() {
    // With the monotonic clock moving a second across the source clock's jump back, the advance after the reset finds a
    // tick due, and the region flushes the new timeline's buckets there, as the interpreter does; following the wall
    // clock from the start, the region's next tick waits until the wall clock passes 7,202 again.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    let _guard = runtime.enter();
    let boot = kept_with(aggregation("1s"));
    let trace = Trace::new()
        .install(boot.clone())
        .admit(src(), ms(7_201_500), vec![counter("c".to_string(), 1.0)])
        .advance(t(7_202))
        .admit(src(), ms(3_601_500), vec![counter("c".to_string(), 2.0)])
        .advance(t(3_602))
        .advance(t(3_603));
    let expected = OutputPoints::from_run(&interpreted(&trace, 1, false));
    // [7201] at 7,202, [3601] at 3,602, and its idle zero, [3602], at 3,603.
    assert_eq!(expected.count(&output()), 3, "both timelines' buckets flush");
    for (step, delivered) in [(Some(Duration::from_secs(1)), 3), (None, 1)] {
        let options = HarnessOptions {
            heartbeats_at_ticks: true,
            monotonic_across_jumps: step,
            ..HarnessOptions::default()
        };
        let mut harness = harness_on(&boot, LanePolicy::ONE, DeliverySchedule::TraceOrder, options);
        run_harness(&mut harness, &trace);
        assert_eq!(harness.outputs().count(&output()), delivered, "{:?}", step);
        if delivered == 3 {
            assert_equal_outputs(&expected, harness.outputs(), "the frame ticking in every timeline");
        }
    }
}

// The fixtures of 6.9 on state lanes. Their expectations are hand-written, as the interpreter's fixtures', in
// `interpret::lane_tests`, are for the one core; on lanes they must hold as well.

/// Returns a context named `prefix` and a number whose key the key groups of `lanes` state lanes give lane `lane`.
fn name_on_lane(prefix: &str, lane: u16, lanes: u16) -> String {
    let groups = KeyGroups::new(lanes);
    (0..)
        .map(|index| format!("{}{}", prefix, index))
        .find(|name| groups.lane_of(Context::from_parts(name.clone(), TagSet::default()).key().as_u64()) == lane)
        .expect("some name lands on every lane")
}

/// One record an output received: its generation, its admission time relative to `B` in milliseconds, and the record.
type Received = (u64, u64, String);

/// Returns, sorted, what `output` received in `harness` and in `run`: generation, admission time relative to `B` in
/// milliseconds, and the record.
fn both(harness: &FrameHarness, run: &RunOutput) -> (Vec<Received>, Vec<Received>) {
    let describe = super::transition_tests::describe;
    let mut framed = harness
        .delivered()
        .iter()
        .map(|record| {
            let at = record
                .admitted_at
                .map_or(0, |at| at.as_unix_nanos() / 1_000_000 - B * 1_000);
            (
                record.generation.map_or(0, |generation| generation.get()),
                at,
                describe(&record.metric),
            )
        })
        .collect::<Vec<_>>();
    let mut interpreted = run
        .output(&output())
        .iter()
        .map(|record| {
            let at = record
                .admitted_at
                .map_or(0, |at| at.as_unix_nanos() / 1_000_000 - B * 1_000);
            (record.generation.get(), at, describe(&record.metric))
        })
        .collect::<Vec<_>>();
    framed.sort();
    interpreted.sort();
    (framed, interpreted)
}

/// Returns the records `expected` lists, sorted, with `{name}` replaced by each name in turn.
fn received(expected: &[(u64, u64, String)]) -> Vec<Received> {
    let mut expected = expected.to_vec();
    expected.sort();
    expected
}

/// Runs fixture X1's trace, one of its closing flushes, through the interpreter and through two state lanes, `a` on
/// state lane 0 and `g` and `b` on state lane 1, and returns what each delivered, sorted.
fn fixture_x1_on_lanes(trace: &Trace, start: AdmissionTime) -> (Vec<Received>, Vec<Received>) {
    let lanes = LanePolicy {
        dogstatsd: 1,
        state: 2,
        post: 1,
    };
    let options = HarnessOptions {
        monotonic_across_jumps: Some(Duration::from_secs(1)),
        ..HarnessOptions::default()
    };
    let (run, replay) =
        super::transition_tests::Replay::run_on_lanes(trace, true, start, options, lanes, DeliverySchedule::TraceOrder);
    both(&replay.harness, &run)
}

#[test]
fn fixture_x1_a_lane_new_to_a_lineage_gains_zeros_from_the_lineages_last_flush_in_any_lane() {
    // 10-second buckets, 15-second ticks. Lane 0 holds counter `a`, flushed at 15, 30, and 45; lane 1 holds only gauge
    // `g` until the flush at 15, and is empty at 30.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    let _guard = runtime.enter();
    let (a, g, b) = (
        name_on_lane("a", 0, 2),
        name_on_lane("g", 1, 2),
        name_on_lane("b", 1, 2),
    );
    let mut params = aggregation("10s");
    params["flush_interval"] = json!("15s");
    let ten = kept_with(params.clone());
    params["window"] = json!("30s");
    let thirty = kept_with(params);
    let record = |generation: u64, at: u64, name: &str, points: &str| (generation, at, format!("{} {}", name, points));

    // The tick case: `b` arrives in lane 1 at 31, after the lineage's flushes at 15 and 30, at both of which lane 1
    // held nothing of `b`. Its first flush, at 45, starts its zeros at align(30), the lineage's last flush in any lane:
    // `b` holds [30, 40) and gains no zero before it, at 10 or 20, where lane 1's own flushes would start it.
    let at_rest = |base: u64| {
        Trace::new()
            .install(ten.clone())
            .admit(
                src(),
                t(base + 5),
                vec![
                    counter(a.clone(), 1.0),
                    Metric::gauge(Context::from_parts(g.clone(), TagSet::default()), 1.0),
                ],
            )
            .advance(t(base + 15))
            .advance(t(base + 30))
    };
    let ticked = at_rest(0)
        .admit(src(), t(31), vec![counter(b.clone(), 1.0)])
        .advance(t(45))
        .close();
    let (framed, interpreted) = fixture_x1_on_lanes(&ticked, t(0));
    assert_eq!(framed, interpreted);
    assert_eq!(
        interpreted,
        received(&[
            record(1, 15_000, &a, "rate/10s [0:1]"),
            record(1, 15_000, &g, "gauge [0:1]"),
            record(1, 30_000, &a, "rate/10s [10:0, 20:0]"),
            record(1, 45_000, &a, "rate/10s [30:0]"),
            record(1, 45_000, &b, "rate/10s [30:1]"),
        ])
    );

    // The closing cases: `a` is flushed at 45 as well, and `b` arrives in lane 1 at 47. Lane 1's closing flush starts
    // `b`'s zeros at align(45), so `b` gains one zero, at 50, never at 10, 20, or 30, which its own last flush, at 15,
    // would give: at a cut that retires the lineage at T = 60, at a reset that drains it at the watermark before it,
    // 60.5, and at shutdown, which closes it at the latest stamp, 60.5.
    let closing = |base: u64| {
        at_rest(base)
            .advance(t(base + 45))
            .admit(src(), t(base + 47), vec![counter(b.clone(), 1.0)])
    };
    // Every record of the closing cases, in the timeline `base` seconds on, the closing flush stamped at `at`.
    let closing_records = |base: u64, at: u64| {
        let shifted =
            |at: u64, name: &str, points: &str| record(1, base * 1_000 + at, name, &shift_points(points, base));
        received(&[
            shifted(15_000, &a, "rate/10s [0:1]"),
            shifted(15_000, &g, "gauge [0:1]"),
            shifted(30_000, &a, "rate/10s [10:0, 20:0]"),
            shifted(45_000, &a, "rate/10s [30:0]"),
            shifted(at, &a, "rate/10s [40:0, 50:0]"),
            shifted(at, &b, "rate/10s [40:1, 50:0]"),
        ])
    };

    let cut = closing(0).install(thirty).heartbeat(src(), ms(60_500));
    let (framed, interpreted) = fixture_x1_on_lanes(&cut, t(0));
    assert_eq!(framed, interpreted);
    assert_eq!(interpreted, closing_records(0, 60_000), "the cut");

    // The reset and shutdown cases run in the timeline an hour on, so that the reset jumps back an hour.
    let base = 3_600;
    let reset = closing(base)
        .heartbeat(src(), ms((base + 60) * 1_000 + 500))
        .heartbeat(src(), t(1));
    let (framed, interpreted) = fixture_x1_on_lanes(&reset, t(base));
    assert_eq!(framed, interpreted);
    assert_eq!(interpreted, closing_records(base, 60_500), "the reset");

    let shutdown = closing(base).heartbeat(src(), ms((base + 60) * 1_000 + 500)).close();
    let (framed, interpreted) = fixture_x1_on_lanes(&shutdown, t(base));
    assert_eq!(framed, interpreted);
    assert_eq!(interpreted, closing_records(base, 60_500), "shutdown");
}

/// Returns `points`, `[bucket:value, ...]` with buckets relative to `B`, with every bucket `base` seconds later.
fn shift_points(points: &str, base: u64) -> String {
    let Some((kind, rest)) = points.split_once(" [") else {
        return points.to_string();
    };
    let shifted = rest
        .trim_end_matches(']')
        .split(", ")
        .map(|point| {
            let (bucket, value) = point.split_once(':').expect("a point is bucket:value");
            format!("{}:{}", bucket.parse::<u64>().expect("a bucket") + base, value)
        })
        .collect::<Vec<_>>();
    format!("{} [{}]", kind, shifted.join(", "))
}

/// Counts what the aggregation kernels log at their context limit on this thread while installed as the default
/// subscriber: each warning that the limit was reached, and each recovery.
#[derive(Clone, Default)]
struct BreachLog {
    counts: std::sync::Arc<std::sync::Mutex<(usize, usize)>>,
}

impl BreachLog {
    /// Returns the warnings and recoveries logged so far.
    fn counts(&self) -> (usize, usize) {
        *self.counts.lock().unwrap()
    }
}

impl tracing::Subscriber for BreachLog {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }

    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }

    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}

    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}

    fn event(&self, event: &tracing::Event<'_>) {
        struct Message(String);
        impl tracing::field::Visit for Message {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                if field.name() == "message" {
                    self.0 = format!("{:?}", value);
                }
            }
        }
        let mut message = Message(String::new());
        event.record(&mut message);
        let mut counts = self.counts.lock().unwrap();
        if message.0.contains("Context limit reached") {
            counts.0 += 1;
        } else if message.0.contains("Context limit no longer exceeded") {
            counts.1 += 1;
        }
    }

    fn enter(&self, _: &tracing::span::Id) {}

    fn exit(&self, _: &tracing::span::Id) {}
}

#[test]
fn fixture_x2_contexts_beyond_the_limit_across_four_lanes_are_refused_once_each_and_warned_of_once() {
    // Four state lanes, a context limit of 3, five new gauges at once: exactly two are refused, the global and the
    // node budget never hold more than three, and every refusal is justified. The lanes share one breach episode:
    // one warning across the four lanes, and one recovery once the flush at 10 freed the gauges and the next ones fit.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    let _guard = runtime.enter();
    let mut params = aggregation("10s");
    params["context_limit"] = json!(3);
    let boot = kept_with(params);
    let options = HarnessOptions {
        state_capacity: Some(3),
        record_budgets: true,
        heartbeats_at_ticks: true,
        ..HarnessOptions::default()
    };
    let lanes = LanePolicy {
        dogstatsd: 1,
        state: 4,
        post: 1,
    };
    let log = BreachLog::default();
    tracing::subscriber::with_default(log.clone(), || {
        let mut harness = harness_on(&boot, lanes, DeliverySchedule::TraceOrder, options);
        let gauges = |names: std::ops::Range<usize>| {
            names
                .map(|index| Metric::gauge(Context::from_parts(format!("g{}", index), TagSet::default()), 1.0))
                .collect::<Vec<_>>()
        };
        harness.admit("src", t(1), gauges(0..5));
        // The keyed exit holds what it routes until a tick sends it.
        harness.advance(t(2));
        for (budget, trace) in harness.budget_traces() {
            let trace = if budget == "agg" { trace.with_limit(3) } else { trace };
            assert_eq!(check_capacity(&trace), CapacityVerdict::Holds, "budget `{}`", budget);
            assert_eq!(trace.held(), 3, "budget `{}`", budget);
        }
        harness.publish();
        assert_eq!(harness.telemetry().loss("agg", LossClass::Capacity), 2);
        assert_eq!(log.counts(), (1, 0), "one warning across the lanes");
        for second in 3..=11 {
            harness.advance(t(second));
        }
        harness.admit("src", t(12), gauges(5..6));
        harness.advance(t(13));
        assert_eq!(log.counts(), (1, 1), "one recovery across the lanes");
    });
}

#[test]
fn fixture_x3_the_flush_count_rises_once_per_non_empty_tick_with_four_lanes() {
    // Four state lanes each hold a gauge at a tick, and none once [0, 10) flushed: `aggregate_flushes_total` counts one
    // flush per tick at which some lane held contexts, through the lineage's shared flush log, and none at a tick at
    // which every lane is empty, as one core does. The count is read where every lane's kernel writes it: the same
    // unlabeled handle. The harness advances one second at a time, each a tick.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    let _guard = runtime.enter();
    let boot = kept();
    let names = (0..4).map(|lane| name_on_lane("c", lane, 4)).collect::<Vec<_>>();
    let lanes = LanePolicy {
        dogstatsd: 1,
        state: 4,
        post: 1,
    };
    let options = HarnessOptions {
        heartbeats_at_ticks: true,
        ..HarnessOptions::default()
    };
    let recorder = saluki_metrics::test::TestRecorder::default();
    let _local = metrics::set_default_local_recorder(&recorder);
    let flushes = || flush_count(&recorder);
    let mut harness = harness_on(&boot, lanes, DeliverySchedule::TraceOrder, options);
    harness.advance(t(1));
    harness.publish();
    let before = flushes();
    let gauges = names
        .iter()
        .map(|name| Metric::gauge(Context::from_parts(name.clone(), TagSet::default()), 1.0))
        .collect();
    harness.admit("src", ms(1_500), gauges);
    harness.advance(t(2));
    harness.publish();
    assert_eq!(
        flushes(),
        before + 1,
        "one tick at which four lanes held contexts counts once"
    );
    for second in 3..=11 {
        harness.advance(t(second));
    }
    assert_eq!(
        harness.outputs().count(&output()),
        4,
        "every lane flushed its gauge at 10"
    );
    harness.publish();
    let flushed = flushes();
    harness.advance(t(12));
    harness.publish();
    assert_eq!(flushes(), flushed, "a tick at which every lane is empty counts nothing");
}

/// Returns `aggregate_flushes_total` as `recorder` holds it: the sum of every handle of that name.
fn flush_count(recorder: &saluki_metrics::test::TestRecorder) -> u64 {
    recorder
        .counters()
        .into_iter()
        .filter(|(key, _)| key.name() == "aggregate_flushes_total")
        .map(|(_, value)| value)
        .sum::<u64>()
}

#[test]
fn fixture_x4_a_gauge_from_two_producer_lanes_is_the_last_value_of_one_of_them() {
    // Gauge `g` in [100, 110), on state lane 0: producer lane 0 sends 1 then 2, producer lane 1 sends 3. In trace order
    // producer lane 1's 3 arrives last; with producer lane 0's buffers toward `g`'s lane held until producer lane 0
    // stamped 1.5 seconds later, its 1 and 2 arrive after the 3, and 2 is last. Either is the last value of one lane;
    // lane 0's first, 1, is never delivered.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    let _guard = runtime.enter();
    let boot = kept();
    let name = name_on_lane("g", 0, 2);
    let gauge = |value| Metric::gauge(Context::from_parts(name.clone(), TagSet::default()), value);
    let trace = Trace::new()
        .install(boot.clone())
        .admit_at_lane(src(), 0, t(101), vec![gauge(1.0)])
        .admit_at_lane(src(), 0, t(102), vec![gauge(2.0)])
        .admit_at_lane(src(), 1, t(103), vec![gauge(3.0)])
        .advance(t(111))
        .close();
    let run = interpreted(&trace, 2, true);
    for (schedule, last) in [
        (DeliverySchedule::TraceOrder, 3.0),
        (
            DeliverySchedule::Delay {
                point: saluki_core::execution::AdmissionPointId::new(0),
                lane: 0,
                by: Duration::from_millis(1_500),
            },
            2.0,
        ),
    ] {
        let mut harness = harness_on(
            &boot,
            LanePolicy {
                dogstatsd: 2,
                state: 2,
                post: 1,
            },
            schedule,
            HarnessOptions {
                heartbeats_at_ticks: true,
                ..HarnessOptions::default()
            },
        );
        run_harness(&mut harness, &trace);
        let records = harness
            .delivered()
            .iter()
            .map(|record| super::transition_tests::describe(&record.metric))
            .collect::<Vec<_>>();
        assert_eq!(
            records,
            vec![format!("{} gauge [100:{}]", name, last)],
            "{:?}",
            schedule
        );
        assert_eq!(
            compare_partitioned(
                &OutputPoints::from_run(&run),
                harness.outputs(),
                run.candidates(),
                Tolerance::from_limits()
            ),
            ReorderedVerdict::Equivalent,
            "{:?}",
            schedule
        );
    }
}

#[test]
fn fixture_x5_a_delayed_buffer_from_another_producer_lane_never_moves_a_counters_last_seen_back() {
    // Counter `c` from producer lane 1 at 100, then producer lane 0's buffer stamped at 99.5, delayed past it at the
    // state lane: `last_seen` stays 100, so the idle counter's last zero is at 399, as the interpreter's fixture X5
    // has it.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    let _guard = runtime.enter();
    let mut params = aggregation("1s");
    params["counter_expiry"] = json!("5m");
    let boot = kept_with(params);
    let trace = Trace::new()
        .install(boot.clone())
        .admit_at_lane(src(), 0, ms(99_500), vec![counter("c".to_string(), 1.0)])
        .admit_at_lane(src(), 1, t(100), vec![counter("c".to_string(), 1.0)])
        .advance(t(101))
        .advance(t(400))
        .advance(t(401));
    let schedule = DeliverySchedule::Delay {
        point: saluki_core::execution::AdmissionPointId::new(0),
        lane: 0,
        by: Duration::from_millis(600),
    };
    let lanes = LanePolicy {
        dogstatsd: 2,
        state: 1,
        post: 1,
    };
    let (run, replay) = super::transition_tests::replay_on_lanes(&trace, true, t(0), lanes, schedule);
    let (framed, interpreted) = both(&replay, &run);
    assert_eq!(framed, interpreted);
    let last_zero = interpreted
        .iter()
        .filter(|(_, at, _)| *at == 400_000)
        .map(|(_, _, record)| record.clone())
        .collect::<Vec<_>>();
    assert!(
        last_zero.iter().any(|record| record.ends_with("399:0]")),
        "{:?}",
        last_zero
    );
}

#[test]
fn fixture_x6_contexts_a_tag_projection_merges_reach_one_state_lane_and_are_one_series() {
    // `x{env:a,pod:1}` and `x{env:a,pod:2}`, `pod` stripped before aggregation: routed by the key after the
    // projection, both reach one state lane, which holds one series with one point per bucket, at any lane count.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    let _guard = runtime.enter();
    let boot = plan(
        vec![
            dogstatsd("untimestamped"),
            node(
                "strip",
                "tag_projection",
                json!({"rules": [{"metric_name": "x", "action": "exclude", "tags": ["pod"]}], "value_rules": [],
                       "cache_capacity": 1000}),
            ),
            node("agg", "aggregate", aggregation("10s")),
            node("out", "emit", json!({"output": "a"})),
        ],
        &["src.metrics -> strip.in", "strip.out -> agg.in", "agg.out -> out.in"],
        &["a"],
    );
    let pod = |pod: &'static str| {
        Metric::counter(
            Context::from_static_parts(
                "x",
                if pod == "1" {
                    &["env:a", "pod:1"]
                } else {
                    &["env:a", "pod:2"]
                },
            ),
            1.0,
        )
    };
    for state in [2u16, 4, 8] {
        let mut harness = harness_on(
            &boot,
            LanePolicy {
                dogstatsd: 2,
                state,
                post: 1,
            },
            DeliverySchedule::TraceOrder,
            HarnessOptions {
                heartbeats_at_ticks: true,
                ..HarnessOptions::default()
            },
        );
        harness.admit_at_lane("src", 0, t(101), vec![pod("1")]);
        harness.admit_at_lane("src", 1, t(102), vec![pod("2")]);
        harness.advance(t(111));
        let records = harness.delivered();
        assert_eq!(records.len(), 1, "{} state lanes: {:?}", state, records);
        assert_eq!(
            super::transition_tests::describe(&records[0].metric),
            "x rate/10s [100:2]",
            "{} state lanes",
            state
        );
    }
}

#[test]
fn fixture_x7_the_bucket_open_at_the_shared_close_time_is_discarded_in_every_lane() {
    // The interpreter's fixture X7 on two state lanes: one close time for the lineage, 7,210.1, at which lane 0's
    // [7200, 7210) closes and lane 1's [7210, 7220) is discarded.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    let _guard = runtime.enter();
    let boot = kept();
    let (c, d) = (name_on_lane("c", 0, 2), name_on_lane("d", 1, 2));
    let trace = Trace::new()
        .install(boot.clone())
        .admit_at_lane(src(), 0, ms(7_209_900), vec![counter(c.clone(), 1.0)])
        .admit_at_lane(src(), 1, ms(7_210_100), vec![counter(d, 2.0)])
        .close();
    let lanes = LanePolicy {
        dogstatsd: 2,
        state: 2,
        post: 1,
    };
    let (run, mut replay) =
        super::transition_tests::replay_on_lanes(&trace, false, t(7_200), lanes, DeliverySchedule::TraceOrder);
    let (framed, interpreted) = both(&replay, &run);
    assert_eq!(framed, interpreted);
    assert_eq!(interpreted, vec![(1, 7_210_100, format!("{} rate/10s [7200:1]", c))]);
    replay.publish();
    assert_eq!(replay.telemetry().loss("agg", LossClass::Shutdown), 1);
}

#[test]
fn a_lane_whose_catch_up_deadlines_run_ahead_of_the_others_duplicates_no_idle_zero() {
    // After the clock jumps 31 seconds, each state lane has 31 ticks due. One lane resolving all of them before the
    // other resolved the first, as a lane task that runs ahead does, leaves the other a straggler behind the ticks the
    // lineage's records keep; the idle counters' zeros must still be delivered once each, as one core delivers them
    // (F4, which the first harness's sequential deadlines showed).
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    let _guard = runtime.enter();
    let (c, d) = (name_on_lane("c", 0, 2), name_on_lane("d", 1, 2));
    let boot = kept();
    let trace = Trace::new()
        .install(boot)
        .admit(src(), ms(100_500), vec![counter(c, 1.0), counter(d, 1.0)])
        .advance(t(101))
        .advance(t(132))
        .advance(t(133))
        .close();
    let options = HarnessOptions {
        deadlines_lane_by_lane: true,
        ..HarnessOptions::default()
    };
    let lanes = LanePolicy {
        dogstatsd: 1,
        state: 2,
        post: 1,
    };
    let (run, replay) = super::transition_tests::Replay::run_on_lanes(
        &trace,
        true,
        t(99),
        options,
        lanes,
        DeliverySchedule::TraceOrder,
    );
    let (framed, interpreted) = both(&replay.harness, &run);
    assert_eq!(framed, interpreted);
}

#[test]
fn a_window_open_across_ticks_counts_the_flushes_one_lane_counts() {
    // Producer lane 1 resets, then producer lane 0 resets while producer lane 1 stays quiet for one tick, so that the
    // window over epochs 1 and 2 stays open across the tick at 101, a cut armed meanwhile. Every lane holds its ticks
    // while its window is open, and once it closes its lineages catch up the ticks the window held, whichever of their
    // instances continues them: one the window created for the newest epoch, or the one it drained. So one and two
    // state lanes count the same flushes, and what the output receives is the same.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    let _guard = runtime.enter();
    let shape = Shape {
        mapper: false,
        prefix: false,
        stripped: Vec::new(),
        expiry: "0s",
        limit: 1_000,
        dropped: None,
    };
    let pick = || Pick {
        name: 0,
        gauge: false,
        value: 1.0,
        pod: 0,
        owner: None,
    };
    let admit = |lane| Step::Admit {
        lane,
        gap_ms: 0,
        picks: vec![pick()],
    };
    let steps = vec![
        admit(0),
        Step::Reset { lane: 1, quiet: 0 },
        admit(1),
        Step::Tick,
        Step::Reset { lane: 0, quiet: 1 },
        admit(0),
        Step::Tick,
        Step::Cut,
        Step::Reset { lane: 0, quiet: 0 },
        Step::Burst {
            lane: 0,
            gap_ms: 0,
            pick: pick(),
        },
    ];
    let trace = build_trace(&shape.plan("10s"), &without_races(&steps), 2, Some(&shape.plan("5s")));
    let one = LanePolicy {
        dogstatsd: 2,
        state: 1,
        post: 1,
    };
    let (a, a_counts) = counted(|| replayed(&trace, one, DeliverySchedule::TraceOrder, |_| {}).1);
    let (b, b_counts) = counted(|| {
        replayed(
            &trace,
            LanePolicy { state: 2, ..one },
            DeliverySchedule::TraceOrder,
            |_| {},
        )
        .1
    });
    assert_equal_outputs(a.outputs(), b.outputs(), "one and two state lanes");
    assert_eq!(a_counts.flushes, b_counts.flushes, "flushes at one and two state lanes");
}

#[test]
fn a_reset_overflow_during_a_cut_loses_nothing_of_the_epochs_it_leaves_open() {
    // Producer lane 0 resets, a cut is armed, and lane 0 admits in epoch 1, which the retiring lineage still serves;
    // then four more resets, lane 1 quiet throughout, so that the fifth leaves five older epochs open and closes epoch
    // 0 at once. The lineage's bound instance, epoch 0's, continues into the newest epoch and crosses there, while its
    // instance of epoch 1 still waits for the window: what that instance holds must still drain at the window's close,
    // under the generation it served, as the interpreter has it. An overflow loses only later batches of the epoch it
    // closes, and this trace has none.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    let _guard = runtime.enter();
    let shape = Shape {
        mapper: false,
        prefix: false,
        stripped: Vec::new(),
        expiry: "0s",
        limit: 1_000,
        dropped: None,
    };
    let admit = || Step::Admit {
        lane: 0,
        gap_ms: 0,
        picks: vec![Pick {
            name: 0,
            gauge: false,
            value: 1.0,
            pod: 0,
            owner: None,
        }],
    };
    let reset = |lane| Step::Reset { lane, quiet: 0 };
    let steps = vec![
        admit(),
        reset(0),
        Step::Cut,
        admit(),
        reset(0),
        admit(),
        reset(2),
        reset(0),
        reset(3),
    ];
    let trace = build_trace(&shape.plan("10s"), &without_races(&steps), 4, Some(&shape.plan("5s")));
    for state in [1, 2] {
        let lanes = LanePolicy {
            dogstatsd: 4,
            state,
            post: 1,
        };
        let (run, mut harness) = replayed(&trace, lanes, DeliverySchedule::TraceOrder, |_| {});
        assert!(
            harness.reset_overflows() > 0,
            "the fifth reset overflows the window at {:?}",
            lanes
        );
        assert_equal_outputs(
            &OutputPoints::from_run(&run),
            harness.outputs(),
            &format!("the interpreter and {:?}", lanes),
        );
    }
}

#[test]
fn a_lane_that_reaches_an_epoch_after_the_others_left_it_ticks_on_the_epochs_record() {
    // One producer point resets twice, and state lane 0 hears of each reset five seconds late, so that lane 1 drains
    // epoch 1 and continues the lineage in epoch 2, the last holder of epoch 1's record, before lane 0 continues the
    // lineage in epoch 1. Lane 0 must tick the epoch on the record lane 1 left, at its flush times and its instants,
    // as the shared record of one lineage and epoch has it whatever order the lanes see the resets in: not on a
    // record of its own, numbered afresh, whose seconds it would read from the wall clock of epoch 2.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    let _guard = runtime.enter();
    let shape = Shape {
        mapper: false,
        prefix: false,
        stripped: Vec::new(),
        expiry: "0s",
        limit: 1_000,
        dropped: None,
    };
    let admit = || Step::Admit {
        lane: 0,
        gap_ms: 0,
        picks: vec![Pick {
            name: 0,
            gauge: false,
            value: 1.0,
            pod: 0,
            owner: None,
        }],
    };
    let reset = || Step::Reset { lane: 0, quiet: 0 };
    let steps = vec![
        admit(),
        Step::Tick,
        Step::Tick,
        reset(),
        admit(),
        Step::Tick,
        Step::Tick,
        Step::Tick,
        reset(),
        admit(),
        Step::Tick,
    ];
    let trace = build_trace(&shape.plan("10s"), &without_races(&steps), 1, None);
    let lanes = LanePolicy {
        dogstatsd: 1,
        state: 2,
        post: 1,
    };
    let schedule = DeliverySchedule::DelayFrom {
        point: saluki_core::execution::AdmissionPointId::new(0),
        lane: 0,
        epoch: 1,
        by: Duration::from_secs(5),
    };
    let (run, mut harness) = replayed(&trace, lanes, schedule, |_| {});
    let verdict = compare_partitioned(
        &OutputPoints::from_run(&run),
        harness.outputs(),
        &Candidates::new(),
        Tolerance::from_limits(),
    );
    assert_eq!(verdict, ReorderedVerdict::Equivalent);
    let slot = harness
        .frame()
        .slots()
        .iter()
        .find(|slot| slot.is_isolation())
        .map(|slot| slot.name.to_string())
        .unwrap();
    let mut flushes = BTreeMap::new();
    for (lane, event) in harness.slot_time_events(&slot) {
        if let LaneTimeEvent::Ticked {
            epoch: 1,
            ordinal: Some(ordinal),
            flush,
            ..
        } = event
        {
            let known = *flushes.entry(ordinal).or_insert(flush);
            assert_eq!(
                flush, known,
                "lane {} fired tick {} of epoch 1 at another flush time",
                lane, ordinal
            );
        }
    }
    let lane_times = check_lane_times(&mut harness);
    assert!(lane_times.is_ok(), "{}", lane_times.unwrap_err());
}

/// Returns the smallest even name number whose counter the key groups of `lanes` state lanes give `lane`: an even
/// counter is no producer lane's own ([`without_races`] leaves it as it is), so its context is the one routed.
fn counter_on_lane(lane: u16, lanes: u16) -> usize {
    let groups = KeyGroups::new(lanes);
    (0..)
        .step_by(2)
        .find(|name| groups.lane_of(counter_pick(*name).metric().context().key().as_u64()) == lane)
        .expect("some name lands on every lane")
}

/// A counter of the name numbered `name`, at 1, from any producer lane.
fn counter_pick(name: usize) -> Pick {
    Pick {
        name,
        gauge: false,
        value: 1.0,
        pod: 0,
        owner: None,
    }
}

/// The shape of the reset tests below: no renaming, no projection, counters that never expire.
fn plain_shape() -> Shape {
    Shape {
        mapper: false,
        prefix: false,
        stripped: Vec::new(),
        expiry: "0s",
        limit: 1_000,
        dropped: None,
    }
}

/// Returns the name of the isolation slot of a harness's frame.
fn isolation_slot(harness: &FrameHarness) -> String {
    harness
        .frame()
        .slots()
        .iter()
        .find(|slot| slot.is_isolation())
        .map(|slot| slot.name.to_string())
        .expect("the frame has an isolation slot")
}

#[test]
fn a_lane_that_reaches_an_epoch_after_the_others_left_it_counts_its_flushes_on_the_epochs_record() {
    // The trace above, with a context on each of the two state lanes: lane 1 drains epoch 1, its last holder, and
    // continues the lineage in epoch 2 before lane 0, which hears of each reset five seconds late, reaches epoch 1.
    // Lane 0 must flush the epoch on the state owner's record lane 1 left as well as on its tick record: its closing
    // flush counts no flush lane 1 counted, and its zeros start from lane 1's flushes, so the lineage counts the
    // flushes one lane counts, in trace order or not.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    let _guard = runtime.enter();
    let (a, b) = (counter_on_lane(0, 2), counter_on_lane(1, 2));
    let admit = || Step::Admit {
        lane: 0,
        gap_ms: 0,
        picks: vec![counter_pick(a), counter_pick(b)],
    };
    let reset = || Step::Reset { lane: 0, quiet: 0 };
    let steps = vec![
        admit(),
        Step::Tick,
        Step::Tick,
        reset(),
        admit(),
        Step::Tick,
        Step::Tick,
        Step::Tick,
        reset(),
        admit(),
        Step::Tick,
    ];
    let trace = build_trace(&plain_shape().plan("10s"), &without_races(&steps), 1, None);
    let one = LanePolicy {
        dogstatsd: 1,
        state: 1,
        post: 1,
    };
    let two = LanePolicy { state: 2, ..one };
    let late = DeliverySchedule::DelayFrom {
        point: saluki_core::execution::AdmissionPointId::new(0),
        lane: 0,
        epoch: 1,
        by: Duration::from_secs(5),
    };
    let (_, one_counts) = counted(|| replayed(&trace, one, DeliverySchedule::TraceOrder, |_| {}).1);
    let (_, two_counts) = counted(|| replayed(&trace, two, DeliverySchedule::TraceOrder, |_| {}).1);
    let (mut harness, late_counts) = counted(|| replayed(&trace, two, late, |_| {}).1);
    assert_eq!(
        one_counts.flushes, two_counts.flushes,
        "one and two state lanes in trace order"
    );
    assert_eq!(
        one_counts.flushes, late_counts.flushes,
        "two state lanes, one hearing of the resets late"
    );
    let run = reference(&trace, 1, false);
    let verdict = compare_partitioned(
        &OutputPoints::from_run(&run),
        harness.outputs(),
        &Candidates::new(),
        Tolerance::from_limits(),
    );
    assert_eq!(verdict, ReorderedVerdict::Equivalent);
    let lane_times = check_lane_times(&mut harness);
    assert!(lane_times.is_ok(), "{}", lane_times.unwrap_err());
}

#[test]
fn a_lane_that_reaches_an_epoch_after_another_retired_its_lineage_there_ticks_after_that_lanes_ordinals() {
    // A reset, then a cut that state lane 1 crosses in epoch 1, its lineage's last holder there, before state lane 0
    // hears of the reset, nine seconds late. Lane 0 continues the old lineage in epoch 1 on the record lane 1 left,
    // after the newest ordinal lane 1 resolved, never renumbering the ordinals lane 1 fired from the start; and it
    // flushes on the state owner's record lane 1 left, so that the epoch's closing flushes, lane 1's final retirement
    // among them, count once. Lane 0 still fires its own ordinals after lane 1's, at grace expiries of its lagging
    // watermark, which counts the ticks as resolved apart; each counts once, as no other lane flushed it.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    let _guard = runtime.enter();
    let shape = plain_shape();
    let (a, b) = (counter_on_lane(0, 2), counter_on_lane(1, 2));
    let admit = || Step::Admit {
        lane: 0,
        gap_ms: 0,
        picks: vec![counter_pick(a), counter_pick(b)],
    };
    let mut steps = vec![
        admit(),
        Step::Tick,
        Step::Tick,
        Step::Reset { lane: 0, quiet: 0 },
        admit(),
        Step::Tick,
        Step::Tick,
        Step::Cut,
        admit(),
    ];
    steps.extend(std::iter::repeat_n(Step::Tick, 12));
    steps.extend([admit(), Step::Tick]);
    let trace = build_trace(&shape.plan("10s"), &without_races(&steps), 1, Some(&shape.plan("5s")));
    let two = LanePolicy {
        dogstatsd: 1,
        state: 2,
        post: 1,
    };
    let late = DeliverySchedule::DelayFrom {
        point: saluki_core::execution::AdmissionPointId::new(0),
        lane: 0,
        epoch: 1,
        by: Duration::from_secs(9),
    };
    let (_, two_counts) = counted(|| replayed(&trace, two, DeliverySchedule::TraceOrder, |_| {}).1);
    let (mut harness, late_counts) = counted(|| replayed(&trace, two, late, |_| {}).1);
    let slot = isolation_slot(&harness);
    let old = GenerationId::new(1);
    let mut fired = BTreeMap::<u16, Vec<(u64, std::time::Instant)>>::new();
    let mut crossed = BTreeMap::new();
    let mut continued = None;
    for (lane, event) in harness.slot_time_events(&slot) {
        match event {
            LaneTimeEvent::Ticked {
                created_by,
                epoch: 1,
                ordinal: Some(ordinal),
                scheduled,
                ..
            } if created_by == old => fired.entry(lane).or_default().push((ordinal, scheduled)),
            LaneTimeEvent::Crossed {
                created_by, epoch: 1, ..
            } if created_by == old => {
                crossed.insert(lane, ());
            }
            LaneTimeEvent::Continued {
                created_by,
                from: 0,
                epoch: 1,
                next_at,
                ..
            } if created_by == old && lane == 0 => continued = next_at,
            _ => {}
        }
    }
    assert_eq!(crossed.len(), 2, "both lanes crossed in epoch 1");
    let lane_1 = &fired[&1];
    let lane_0 = fired.get(&0).cloned().unwrap_or_default();
    let (newest, last_at) = lane_1.iter().copied().max().expect("lane 1 ticked epoch 1");
    let continued = continued.expect("lane 0 continued the old lineage in epoch 1");
    assert!(
        continued > last_at,
        "lane 0 continued at {:?}, at or before lane 1's last tick of epoch 1, at {:?}",
        continued,
        last_at
    );
    assert!(
        lane_0.iter().all(|(ordinal, _)| *ordinal > newest),
        "lane 0 fired {:?} of the ordinals lane 1 fired through {}",
        lane_0,
        newest
    );
    // Every flush is counted once: what both lanes count in trace order, and the ordinals lane 0 alone fired.
    assert_eq!(late_counts.flushes, two_counts.flushes + lane_0.len() as u64);
    assert!(
        harness.ticks_resolved_apart() > 0,
        "lane 0's grace expiries are resolved apart"
    );
}

#[test]
fn a_second_reset_while_a_lane_holds_the_first_window_open_keeps_the_lineages_schedule() {
    // Two producer lanes, and state lane 0 hears producer lane 1's epoch 1 ten seconds late, so that its window over
    // epochs 0 and 1 is still open when producer lane 0 resets again, into epoch 2, while state lane 1 has closed its
    // window and ticked epoch 1. The lineage's record of epoch 2 starts at the current deadline of its record of epoch
    // 1, the newest epoch older than it, after the ordinals state lane 1 fired there, not at epoch 0's: one core keeps
    // its schedule across a reset, and no lane fires epoch 2's ticks at the instants of epoch 1's. The lineage counts
    // the flushes one lane counts.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    let _guard = runtime.enter();
    let (x, y) = (counter_on_lane(0, 2), counter_on_lane(1, 2));
    let admit = || Step::Admit {
        lane: 0,
        gap_ms: 0,
        picks: vec![counter_pick(x), counter_pick(y)],
    };
    let reset = || Step::Reset { lane: 0, quiet: 0 };
    let first_window = vec![
        admit(),
        Step::Tick,
        Step::Tick,
        reset(),
        admit(),
        Step::Tick,
        Step::Tick,
        Step::Tick,
        Step::Tick,
        reset(),
    ];
    // With data of epoch 2 at once, state lane 0 creates its instance of epoch 2 inside its window; with ticks before
    // it, both windows close first, and the lane continues the lineage through its drained instance of epoch 0.
    let cases = [
        ("data at once", vec![admit(), Step::Tick, Step::Tick]),
        ("ticks first", vec![Step::Tick, Step::Tick, admit(), Step::Tick]),
    ];
    let lanes = LanePolicy {
        dogstatsd: 2,
        state: 2,
        post: 1,
    };
    let late = DeliverySchedule::DelayFrom {
        point: saluki_core::execution::AdmissionPointId::new(1),
        lane: 0,
        epoch: 1,
        by: Duration::from_secs(10),
    };
    for (case, after) in cases {
        let steps = [first_window.clone(), after].concat();
        let trace = build_trace(&plain_shape().plan("10s"), &without_races(&steps), 2, None);
        let (_, one) = counted(|| {
            replayed(
                &trace,
                LanePolicy { state: 1, ..lanes },
                DeliverySchedule::TraceOrder,
                |_| {},
            )
            .1
        });
        let (_, in_order) = counted(|| replayed(&trace, lanes, DeliverySchedule::TraceOrder, |_| {}).1);
        let (harness, delayed) = counted(|| replayed(&trace, lanes, late, |_| {}).1);
        let slot = isolation_slot(&harness);
        let mut scheduled = BTreeMap::<u32, Vec<std::time::Instant>>::new();
        for (_, event) in harness.slot_time_events(&slot) {
            if let LaneTimeEvent::Ticked {
                epoch,
                ordinal: Some(_),
                scheduled: at,
                ..
            } = event
            {
                scheduled.entry(epoch).or_default().push(at);
            }
        }
        let last_of_one = scheduled[&1].iter().max().copied().expect("epoch 1 ticked");
        let first_of_two = scheduled[&2].iter().min().copied().expect("epoch 2 ticked");
        assert!(
            first_of_two > last_of_one,
            "{}: epoch 2's first tick at {:?}, at or before epoch 1's last, at {:?}",
            case,
            first_of_two,
            last_of_one
        );
        assert_eq!(
            one.flushes, in_order.flushes,
            "{}: one and two state lanes in trace order",
            case
        );
        assert_eq!(
            one.flushes, delayed.flushes,
            "{}: two state lanes, one holding the first window open",
            case
        );
    }
}

// The properties of 6.4 at the frame level.

/// The names that may be admitted as a gauge or as a counter: every other name has one kind, so that most series
/// compare their values under interleaving, and a kind conflict is the exception.
const CONFLICTED_NAMES: usize = 1;

/// The events of a burst, as many as one batch holds: enough to fill a routed buffer at any number of state lanes.
const BURST: usize = crate::limits::values::BATCH_SIZE;

/// The most names a generated trace draws from.
const MAX_CARDINALITY: usize = 60;

/// The producer lanes generated steps stamp at, before they are folded onto a frame's.
const STEP_LANES: u16 = 4;

/// Returns the first digit of a name's number.
fn first_digit(name: usize) -> usize {
    usize::from(name.to_string().as_bytes()[0] - b'0')
}

/// Returns whether the name numbered `name` is a gauge's: by its first digit, so that the names the mapper makes
/// collide share their kind, except for a conflicted name, which takes `drawn`.
fn gauge_name(name: usize, drawn: bool) -> bool {
    if name < CONFLICTED_NAMES {
        drawn
    } else {
        first_digit(name) % 2 == 1
    }
}

/// One metric a generated batch carries: which name of the cardinality's vocabulary, which kind, its value, and its
/// `pod` tag; with `owner`, a name only that producer lane admits.
#[derive(Clone, Debug)]
struct Pick {
    name: usize,
    gauge: bool,
    value: f64,
    pod: u8,
    owner: Option<u16>,
}

impl Pick {
    /// Returns the metric's name, before any renaming: `m<n>`, `m<n>.<lane>` for a lane's own counter, `g<n>.<lane>`
    /// for a lane's own gauge.
    fn base_name(&self) -> String {
        match (self.owner, self.gauge) {
            (Some(lane), true) => format!("g{}.{}", self.name, lane),
            (Some(lane), false) => format!("m{}.{}", self.name, lane),
            (None, _) => format!("m{}", self.name),
        }
    }

    /// Returns the metric.
    fn metric(&self) -> Metric {
        let tags = TagSet::from(saluki_core::data_model::tags::Tag::from(format!("pod:{}", self.pod)));
        let context = Context::from_parts(self.base_name(), tags);
        match self.gauge {
            true => Metric::gauge(context, self.value),
            false => Metric::counter(context, self.value),
        }
    }
}

/// One step of a generated trace, in stamp order: a batch, a burst of one name, or a heartbeat at a producer lane after
/// a gap; a tick at the next whole second; a backward jump of the source's clock beyond the reset threshold, after
/// which every other producer lane stays quiet for `quiet` ticks (all of them, at `u8::MAX`); or the installation of
/// the trace's second plan, a cut.
#[derive(Clone, Debug)]
enum Step {
    Admit { lane: u16, gap_ms: u64, picks: Vec<Pick> },
    Burst { lane: u16, gap_ms: u64, pick: Pick },
    Beat { lane: u16, gap_ms: u64 },
    Tick,
    Reset { lane: u16, quiet: u8 },
    Cut,
}

impl Step {
    /// Returns `true` if the step stamps at a producer lane.
    fn stamps(&self) -> bool {
        matches!(
            self,
            Self::Admit { .. } | Self::Burst { .. } | Self::Beat { .. } | Self::Reset { .. }
        )
    }
}

fn arb_pick(cardinality: usize) -> impl Strategy<Value = Pick> {
    (0..cardinality, any::<bool>(), 1u8..10, 0u8..2).prop_map(|(name, drawn, value, pod)| Pick {
        name,
        gauge: gauge_name(name, drawn),
        value: f64::from(value),
        pod,
        owner: None,
    })
}

/// Returns the strategy of one generated step at `lanes` producer lanes over `cardinality` names, with resets and cuts
/// if asked for.
fn arb_step(lanes: u16, cardinality: usize, resets: bool, cuts: bool) -> BoxedStrategy<Step> {
    let admit = (0..lanes, 0u64..700, vec(arb_pick(cardinality), 1..12))
        .prop_map(|(lane, gap_ms, picks)| Step::Admit { lane, gap_ms, picks })
        .boxed();
    let burst = (0..lanes, 0u64..700, arb_pick(cardinality))
        .prop_map(|(lane, gap_ms, pick)| Step::Burst { lane, gap_ms, pick })
        .boxed();
    let beat = (0..lanes, 0u64..1_500)
        .prop_map(|(lane, gap_ms)| Step::Beat { lane, gap_ms })
        .boxed();
    let mut steps = vec![(6, admit), (2, beat), (2, Just(Step::Tick).boxed()), (1, burst)];
    if resets {
        let quiet = prop_oneof![3 => 0u8..4, 1 => Just(u8::MAX)];
        let reset = (0..lanes, quiet)
            .prop_map(|(lane, quiet)| Step::Reset { lane, quiet })
            .boxed();
        steps.push((1, reset));
    }
    if cuts {
        steps.push((1, Just(Step::Cut).boxed()));
    }
    prop::strategy::Union::new_weighted(steps).boxed()
}

/// The seconds between the timelines a generated trace's resets jump back over: an hour, far beyond the reset
/// threshold, and far more than a trace's steps take.
const TIMELINE_SECS: u64 = 3_600;

/// The first second of the newest timeline of a generated trace.
const NEWEST_TIMELINE: u64 = 100;

/// Returns the trace of generated steps over `boot` at `lanes` producer lanes, `cut` installed at the trace's cuts, if
/// it has any.
///
/// Each reset jumps back an hour, to the start of the next timeline, so a trace with `r` resets starts `r` hours ahead
/// of its last timeline. Time advances a second at a time in every timeline: every whole second is a tick, at which
/// every producer lane heartbeats first, stated in the trace, as a running source's flush timer has each lane's
/// admission point heartbeat, unless the lane is quiet after another lane's reset, which it stays until it stamps. A
/// tick step moves time to the next whole second; every stamp after a tick is at or after it. The trace ends advancing
/// through the 31 seconds after its last stamp, which closes every bucket, and closes.
///
/// Generated traces run on harnesses whose monotonic clock moves a second across each jump back, so that a tick falls
/// due at every advance ([`generated_options`]).
fn build_trace(boot: &Plan, steps: &[Step], lanes: u16, cut: Option<&Plan>) -> Trace {
    let resets = steps.iter().filter(|step| matches!(step, Step::Reset { .. })).count() as u64;
    let start_of = |timeline: u64| (NEWEST_TIMELINE + timeline * TIMELINE_SECS) * 1_000;
    let mut timeline = resets;
    let mut trace = Trace::new().install(boot.clone());
    let mut now = start_of(timeline);
    // The last whole second the trace advanced to.
    let mut ticked = now / 1_000 - 1;
    // Per producer lane, the ticks it still skips the heartbeat of.
    let mut quiet = vec![0u8; usize::from(lanes)];
    for step in steps {
        match step {
            Step::Admit { lane, gap_ms, picks } => {
                now += gap_ms;
                trace = ticks(trace, &mut ticked, &mut quiet, now / 1_000);
                quiet[usize::from(*lane)] = 0;
                let metrics = picks.iter().map(Pick::metric).collect();
                trace = trace.admit_at_lane(src(), *lane, ms(now), metrics);
            }
            Step::Burst { lane, gap_ms, pick } => {
                now += gap_ms;
                trace = ticks(trace, &mut ticked, &mut quiet, now / 1_000);
                quiet[usize::from(*lane)] = 0;
                let metrics = std::iter::repeat_n(pick.metric(), BURST).collect();
                trace = trace.admit_at_lane(src(), *lane, ms(now), metrics);
            }
            Step::Beat { lane, gap_ms } => {
                now += gap_ms;
                trace = ticks(trace, &mut ticked, &mut quiet, now / 1_000);
                quiet[usize::from(*lane)] = 0;
                trace = trace.heartbeat_at_lane(src(), *lane, ms(now));
            }
            Step::Tick => {
                let next = (now / 1_000 + 1).max(ticked + 1);
                trace = ticks(trace, &mut ticked, &mut quiet, next);
                now = now.max(next * 1_000);
            }
            Step::Reset { lane, quiet: skipped } => {
                timeline -= 1;
                now = start_of(timeline) + 1;
                ticked = now / 1_000;
                for (other, quiet) in quiet.iter_mut().enumerate() {
                    *quiet = if other == usize::from(*lane) { 0 } else { *skipped };
                }
                trace = trace.heartbeat_at_lane(src(), *lane, ms(now));
            }
            Step::Cut => {
                if let Some(cut) = cut {
                    trace = trace.install(cut.clone());
                }
            }
        }
    }
    let end = (now / 1_000 + 1).max(ticked + 1) + 31;
    ticks(trace, &mut ticked, &mut quiet, end).close()
}

/// Advances `trace` through every whole second after `ticked` through `until`, each a tick at which every producer
/// lane that is not quiet heartbeats first.
fn ticks(mut trace: Trace, ticked: &mut u64, quiet: &mut [u8], until: u64) -> Trace {
    for second in *ticked + 1..=until {
        for (lane, quiet) in quiet.iter_mut().enumerate() {
            match *quiet {
                0 => trace = trace.heartbeat_at_lane(src(), lane as u16, t(second)),
                u8::MAX => {}
                left => *quiet = left - 1,
            }
        }
        trace = trace.advance(t(second));
    }
    *ticked = (*ticked).max(until);
    trace
}

/// Returns where the harness of a generated trace starts: just before its newest timeline, which a harness whose
/// monotonic clock steps across jumps starts from as well as from just before its first ([`generated_options`]).
fn trace_start() -> AdmissionTime {
    t(NEWEST_TIMELINE - 1)
}

/// The harness options of generated traces: the monotonic clock moves a second across each jump of the clock, as a
/// tick does, and the trace states every heartbeat.
fn generated_options() -> HarnessOptions {
    HarnessOptions {
        monotonic_across_jumps: Some(Duration::from_secs(1)),
        ..HarnessOptions::default()
    }
}

/// The lane policies the properties draw from: producer lanes from 1, 2, 4; state lanes from 1, 2, 3, 4, 8; post
/// lanes from 1, 3.
fn arb_lanes() -> impl Strategy<Value = LanePolicy> {
    (
        prop::sample::select(vec![1u16, 2, 4]),
        prop::sample::select(vec![1u16, 2, 3, 4, 8]),
        prop::sample::select(vec![1u16, 3]),
    )
        .prop_map(|(dogstatsd, state, post)| LanePolicy { dogstatsd, state, post })
}

/// What the plans of the properties do before aggregation, and how aggregation and the filter after it behave.
#[derive(Clone, Debug)]
struct Shape {
    /// Whether the mapper renames every name `m<digit><digits>` to `r<digit>`, so that names sharing their first digit
    /// collide.
    mapper: bool,
    /// Whether every name gets the prefix `ns.` after the mapper, unless it has it: every kernel before aggregation
    /// leaves what it already renamed or stripped as it is, since an arrival replay runs what the state lanes saw
    /// through the whole plan again.
    prefix: bool,
    /// The first digits of the names whose `pod` tag the tag projection strips after both, from counters, which
    /// makes their two contexts collide; no projection if empty.
    stripped: Vec<usize>,
    /// The counter expiry.
    expiry: &'static str,
    /// The context limit.
    limit: u64,
    /// The number of the name whose records the filter after aggregation drops, which R5b pushes into aggregation.
    dropped: Option<usize>,
}

impl Shape {
    /// Returns the name a metric named `name` has at aggregation: renamed by the mapper and prefixed.
    fn renamed(&self, name: &str) -> String {
        let mapped = match (self.mapper, name.strip_prefix('m')) {
            (true, Some(digits)) if !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit()) => {
                format!("r{}", &digits[..1])
            }
            _ => name.to_string(),
        };
        if self.prefix {
            format!("ns.{}", mapped)
        } else {
            mapped
        }
    }

    /// Returns the plan of the shape, aggregating over `window`.
    fn plan(&self, window: &str) -> Plan {
        let mut params = aggregation(window);
        params["counter_expiry"] = json!(self.expiry);
        params["context_limit"] = json!(self.limit);
        let mut nodes = vec![
            dogstatsd("untimestamped"),
            node("keep", "split", json!({"predicate": {"const": true}})),
            node("dropped", "drop", json!({})),
        ];
        let mut edges = vec![
            "src.metrics -> keep.in".to_string(),
            "keep.false -> dropped.in".to_string(),
        ];
        let mut feed = "keep.true".to_string();
        let mut then = |nodes: &mut Vec<Value>, edges: &mut Vec<String>, id: &str, op: &str, params: Value| {
            nodes.push(node(id, op, params));
            edges.push(format!("{} -> {}.in", feed, id));
            feed = format!("{}.out", id);
        };
        if self.mapper {
            let profiles = json!([{"name": "collide", "prefix": "m", "mappings": [
                {"match": "m(\\d)\\d*", "match_type": "regex", "name": "r${1}"}
            ]}]);
            then(
                &mut nodes,
                &mut edges,
                "mapper",
                "dogstatsd_mapper",
                json!({"profiles": profiles, "cache_size": 64, "string_interner_bytes": 65536}),
            );
        }
        if self.prefix {
            then(
                &mut nodes,
                &mut edges,
                "prefix",
                "namespace_prefix",
                json!({"namespace": "ns", "exclusions": ["ns."]}),
            );
        }
        if !self.stripped.is_empty() {
            then(
                &mut nodes,
                &mut edges,
                "strip",
                "tag_projection",
                json!({"rules": self.stripping_rules(), "value_rules": [], "cache_capacity": 1000}),
            );
        }
        nodes.push(node("agg", "aggregate", params));
        edges.push(format!("{} -> agg.in", feed));
        let predicate = match self.dropped {
            Some(name) => json!({"name": {"eq": self.renamed(&format!("m{}", name))}}),
            None => json!({"const": true}),
        };
        nodes.push(node("post", "split", json!({"predicate": predicate})));
        nodes.push(node("post_dropped", "drop", json!({})));
        nodes.push(node("out", "emit", json!({"output": "a"})));
        edges.push("agg.out -> post.in".to_string());
        if self.dropped.is_some() {
            edges.push("post.true -> post_dropped.in".to_string());
            edges.push("post.false -> out.in".to_string());
        } else {
            edges.push("post.true -> out.in".to_string());
            edges.push("post.false -> post_dropped.in".to_string());
        }
        let edges = edges.iter().map(String::as_str).collect::<Vec<_>>();
        plan(nodes, &edges, &["a"])
    }

    /// Returns the tag projection's rules: `pod` excluded from every name, as it reaches the projection, of a number
    /// whose first digit the shape strips.
    fn stripping_rules(&self) -> Vec<Value> {
        let mut names = std::collections::BTreeSet::new();
        for name in (0..MAX_CARDINALITY).filter(|name| self.stripped.contains(&first_digit(*name))) {
            names.insert(self.renamed(&format!("m{}", name)));
            for lane in 0..STEP_LANES {
                names.insert(self.renamed(&format!("m{}.{}", name, lane)));
                names.insert(self.renamed(&format!("g{}.{}", name, lane)));
            }
        }
        names
            .into_iter()
            .map(|name| json!({"metric_name": name, "action": "exclude", "tags": ["pod"]}))
            .collect()
    }
}

/// The shapes the properties draw from: a mapper whose renames collide, a prefix, and a projection that strips a tag,
/// each or not, before aggregation with idle counters expiring after 0 seconds or 5 minutes.
fn arb_shape() -> impl Strategy<Value = Shape> {
    (
        any::<bool>(),
        any::<bool>(),
        prop_oneof![
            Just(Vec::new()),
            prop::collection::btree_set(0usize..10, 1..4).prop_map(Vec::from_iter)
        ],
        prop::sample::select(vec!["0s", "5m"]),
    )
        .prop_map(|(mapper, prefix, stripped, expiry)| Shape {
            mapper,
            prefix,
            stripped,
            expiry,
            limit: 1_000,
            dropped: None,
        })
}

/// Runs a generated trace through the interpreter, with `src` on the frame's producer lanes, and through a frame on
/// `lanes` delivering as `schedule` says, following the interpreter's installations, with [`generated_options`] and
/// `adjust`.
fn replayed(
    trace: &Trace, lanes: LanePolicy, schedule: DeliverySchedule, adjust: impl FnOnce(&mut HarnessOptions),
) -> (RunOutput, FrameHarness) {
    let mut options = generated_options();
    adjust(&mut options);
    let (run, replay) =
        super::transition_tests::Replay::run_on_lanes(trace, false, trace_start(), options, lanes, schedule);
    (run, replay.harness)
}

/// Runs a generated trace through the interpreter, with `src` on `lanes` producer lanes, in candidate mode if asked.
fn reference(trace: &Trace, lanes: u16, candidates: bool) -> RunOutput {
    let registry = OperatorRegistry::native();
    let catalog = catalog();
    let interpreter = Interpreter::new(&registry, &catalog).with_source_lanes(src(), lanes);
    let interpreter = if candidates {
        interpreter.with_candidates()
    } else {
        interpreter
    };
    interpreter.run(trace).unwrap()
}

/// One record an output received, as the stamps comparisons see it: its flush time and its points, which the oracle
/// compares by their contexts' parts, not by how a context holds its tags.
///
/// The generation that stamps time-driven output is the one the schedule gives a drain when it runs (6.1): a reset
/// window's drain, which the header closing the window starts, runs later at lanes whose keyed exits held that header
/// across an installation that retains the lineage, and is then stamped with the retaining generation, whose program
/// delivers the same records. So generated traces with installations compare flush times and points, not generations.
type Stamped = (Option<AdmissionTime>, Vec<Point>);

/// Returns every record the frame's outputs received, with its flush time, sorted.
fn stamped(harness: &FrameHarness) -> Vec<Stamped> {
    let mut stamped = harness
        .delivered()
        .iter()
        .map(|record| (record.admitted_at, Point::flatten(&record.metric)))
        .collect::<Vec<_>>();
    stamped.sort();
    stamped
}

/// Returns every record the interpreter's outputs received, with its flush time, sorted.
fn stamped_run(run: &RunOutput) -> Vec<Stamped> {
    let mut stamped = run
        .output(&output())
        .iter()
        .map(|record| (record.admitted_at, Point::flatten(&record.metric)))
        .collect::<Vec<_>>();
    stamped.sort();
    stamped
}

/// Returns `true` if a point is an idle counter's zero: a counter's or a rate's point of value zero, as no admission of
/// the properties' values, all positive, fills.
fn idle_zero(point: &Point) -> bool {
    matches!(point.kind, PointKind::Counter | PointKind::Rate { .. })
        && matches!(point.value, PointValue::Scalar(value) if value.get() == 0.0)
}

/// Returns the loss balance of a run of `trace` through aggregation over `width` seconds, under `shape`: the buckets
/// and values admitted, what the output received without idle-counter zeros, and the losses `loss` reports for
/// aggregation, by class.
fn balance(
    trace: &Trace, shape: &Shape, width: u64, delivered: &OutputPoints, loss: impl Fn(LossClass) -> u64,
) -> CapacityBalance {
    let mut buckets = std::collections::BTreeSet::new();
    let mut admitted_contexts = std::collections::BTreeSet::new();
    let mut values = 0;
    for event in trace.events() {
        let TraceEvent::Admit { admitted_at, batch, .. } = event else {
            continue;
        };
        for metric in batch {
            let name = shape.renamed(metric.context().name());
            // The projection strips counters' tags, and distributions', not gauges'.
            let projected = matches!(
                metric.values(),
                MetricValues::Counter(_) | MetricValues::Distribution(_)
            );
            let stripped = projected
                && shape.stripped.iter().any(|digit| {
                    name.trim_start_matches("ns.")
                        .trim_start_matches(['m', 'g', 'r'])
                        .bytes()
                        .next()
                        .is_some_and(|byte| usize::from(byte - b'0') == *digit)
                });
            let tags = if stripped {
                TagSet::default()
            } else {
                metric.context().tags().clone()
            };
            let identity = ContextIdentity::of(&Metric::gauge(Context::from_parts(name, tags), 0.0));
            buckets.insert((identity.clone(), admitted_at.as_unix_secs() / width));
            admitted_contexts.insert(identity);
            values += 1;
        }
    }
    let delivered_points = delivered
        .points(&output())
        .filter(|(point, _)| !idle_zero(point))
        .map(|(_, count)| count as u64)
        .sum();
    let emitted_contexts = delivered
        .points(&output())
        .map(|(point, _)| point.context.clone())
        .collect();
    CapacityBalance {
        admitted: buckets.len() as u64,
        admitted_at_most: values,
        delivered: delivered_points,
        capacity_loss: loss(LossClass::Capacity),
        policy_loss: loss(LossClass::Policy),
        shutdown_loss: loss(LossClass::Shutdown),
        admitted_contexts,
        emitted_contexts,
    }
}

/// Compares what a harness's outputs received with the arrival replay of what its state lanes saw, run through the
/// interpreter in arrival order, if the execution has a valid replay: none if a reset window overflowed, which the
/// lanes count and which loses what still reaches the epoch it closed, or if a lane's timing decided a tick, which the
/// lanes count, and the replay excludes.
///
/// When the lanes count no tick resolved apart, the recorder's model of their ticks must agree with what they did
/// ([`FrameHarness::replay_disagreement`]), and their replay must be valid. The comparison is exact unless the lanes
/// counted a tick they resolved apart from their shared tick record (6.1): a flush whose idle-counter zeros started
/// from the lane's own last flush, a flush at the lane's own watermark once a grace expired, or a straggler. Each of
/// those changes only which idle buckets of a counter get zeros, so such a case is compared by [`compare_partitioned`]
/// without candidates, which is exact but for that, rather than excluded.
fn against_replay(harness: &mut FrameHarness, lanes: u16) -> Option<Result<(), String>> {
    if harness.reset_overflows() > 0 {
        return None;
    }
    let apart = harness.ticks_resolved_apart();
    if apart == 0 {
        if let Some(disagreement) = harness.replay_disagreement() {
            return Some(Err(format!(
                "the recorder's model of the lanes' ticks disagrees with the ticks they reported: {}",
                disagreement
            )));
        }
    }
    let replay = match harness.replay().build() {
        Ok(replay) => replay,
        Err(_) if apart > 0 => return None,
        Err(excluded) => {
            return Some(Err(format!(
                "the replay is excluded although the lanes resolved every tick alike: {}",
                excluded.reason
            )))
        }
    };
    let registry = OperatorRegistry::native();
    let catalog = catalog();
    let run = Interpreter::new(&registry, &catalog)
        .with_source_lanes(src(), lanes)
        .in_arrival_order()
        .run(&replay)
        .unwrap();
    let expected = OutputPoints::from_run(&run);
    if apart == 0 {
        return Some(compare(&expected, harness.outputs()).map_err(|mismatch| mismatch.to_string()));
    }
    Some(
        match compare_partitioned(
            &expected,
            harness.outputs(),
            &Candidates::new(),
            Tolerance::from_limits(),
        ) {
            ReorderedVerdict::Equivalent => Ok(()),
            verdict => Err(format!("with ticks resolved apart: {:?}", verdict)),
        },
    )
}

/// Checks the time a frame's state lanes reported: per epoch, every lane drained the epoch at the same watermark; every
/// lane that continued its lineage from one epoch in a newer one resumed from the old record's next instant, one for
/// every lane; no lane fired one tick of an instance twice, so that a lane holds one instance per lineage and epoch;
/// after an instance's crossing or reset drain, nothing more of it; and, unless a lane counted a tick it resolved apart
/// from its lineage's record, every lane that fired a tick of a lineage's record fired it at the record's flush time.
fn check_lane_times(harness: &mut FrameHarness) -> Result<(), String> {
    let agreed = harness.ticks_resolved_apart() == 0;
    let Some(slot) = harness.frame().slots().iter().find(|slot| slot.is_isolation()) else {
        return Ok(());
    };
    let events = harness.slot_time_events(slot.name.as_str());
    let mut drained = BTreeMap::<(GenerationId, u32), Option<AdmissionTime>>::new();
    let mut resumed = BTreeMap::<(GenerationId, u32, u32), Option<std::time::Instant>>::new();
    let mut flushes = BTreeMap::<(GenerationId, u32, u64), AdmissionTime>::new();
    let mut fired = std::collections::BTreeSet::new();
    let mut ended = std::collections::BTreeSet::new();
    for (lane, event) in events {
        match event {
            LaneTimeEvent::ResetDrained { created_by, epoch, pre } => {
                if let Some(known) = drained.insert((created_by, epoch), pre).filter(|known| *known != pre) {
                    return Err(format!(
                        "state lane {} drained epoch {} of the lineage generation {} created at {:?}, another lane at \
                         {:?}",
                        lane, epoch, created_by, pre, known
                    ));
                }
                ended.insert((lane, created_by, epoch));
            }
            LaneTimeEvent::Crossed { created_by, epoch, .. } => {
                ended.insert((lane, created_by, epoch));
            }
            LaneTimeEvent::Ticked {
                created_by,
                epoch,
                ordinal,
                flush,
                ..
            } => {
                if ended.contains(&(lane, created_by, epoch)) {
                    return Err(format!(
                        "state lane {} ticked the instance of generation {} in epoch {} after its crossing or drain",
                        lane, created_by, epoch
                    ));
                }
                if let Some(ordinal) = ordinal {
                    if !fired.insert((lane, created_by, epoch, ordinal)) {
                        return Err(format!(
                            "state lane {} fired tick {} of the instance of generation {} in epoch {} twice",
                            lane, ordinal, created_by, epoch
                        ));
                    }
                    let known = flushes.insert((created_by, epoch, ordinal), flush);
                    if let Some(known) = known.filter(|known| agreed && *known != flush) {
                        return Err(format!(
                            "state lane {} fired tick {} of generation {}'s lineage in epoch {} at {}, another lane at \
                             {}, though no lane counted a tick resolved apart",
                            lane, ordinal, created_by, epoch, flush, known
                        ));
                    }
                }
            }
            // Where the successor record starts is the first lane's to set: a lane continuing from another epoch's
            // record, or a fresh instance of the newer epoch, may have started it, so its next instant is not checked.
            LaneTimeEvent::Continued {
                created_by,
                from,
                epoch,
                resume_at,
                ..
            } => {
                let key = (created_by, from, epoch);
                if let Some(first) = resumed.insert(key, resume_at).filter(|first| *first != resume_at) {
                    return Err(format!(
                        "state lane {} continues generation {}'s lineage from epoch {} in epoch {} at {:?}, another \
                         lane at {:?}, though the old record's next instant is one",
                        lane, created_by, from, epoch, resume_at, first
                    ));
                }
            }
        }
    }
    Ok(())
}

/// Counts what one run's state owners count that no output shows: their flushes, and the warnings and recoveries of
/// their context limit, logged on this thread.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct OwnerCounts {
    flushes: u64,
    breaches: (usize, usize),
}

/// Runs `run` with a metrics recorder and a log counter installed, publishes what it returns, and returns it with what
/// its state owners counted.
fn counted(run: impl FnOnce() -> FrameHarness) -> (FrameHarness, OwnerCounts) {
    let recorder = saluki_metrics::test::TestRecorder::default();
    let log = BreachLog::default();
    let mut harness = {
        let _local = metrics::set_default_local_recorder(&recorder);
        let mut harness = tracing::subscriber::with_default(log.clone(), run);
        harness.publish();
        harness
    };
    harness.publish();
    let counts = OwnerCounts {
        flushes: flush_count(&recorder),
        breaches: log.counts(),
    };
    (harness, counts)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn property_test_lanes_equal_interpreter_in_trace_order(
        shape in arb_shape(),
        lanes in arb_lanes(),
        steps in prop_oneof![
            vec(arb_step(STEP_LANES, 3, false, false), 1..40),
            vec(arb_step(STEP_LANES, 60, false, false), 1..40),
        ],
    ) {
        // Plans that rename, prefix, and strip a tag before aggregation, so that contexts collide after their
        // renames, low and high cardinality, bursts that fill routed buffers, idle gaps, open routed buffers flushed
        // before every tick: exact, flush times included, with no input whose effect depends on the order of several
        // points (`without_races`).
        let runtime = tokio::runtime::Builder::new_current_thread().enable_time().build().unwrap();
        let _guard = runtime.enter();
        let steps = without_races(&on_producer_lanes(&steps, lanes.dogstatsd));
        let trace = build_trace(&shape.plan("10s"), &steps, lanes.dogstatsd, None);
        let (run, mut harness) = replayed(&trace, lanes, DeliverySchedule::TraceOrder, |_| {});
        let result = compare(&OutputPoints::from_run(&run), harness.outputs());
        let apart = harness.ticks_resolved_apart();
        prop_assert!(result.is_ok(), "{:?}, {} ticks resolved apart: {}", lanes, apart, result.unwrap_err());
        prop_assert_eq!(stamped(&harness), stamped_run(&run), "flush times at {:?}", lanes);
        let lane_times = check_lane_times(&mut harness);
        prop_assert!(lane_times.is_ok(), "{:?}: {}", lanes, lane_times.unwrap_err());
    }

    #[test]
    fn property_test_lanes_equal_interpreter_in_arrival_order(
        shape in arb_shape(),
        lanes in arb_lanes(),
        steps in vec(arb_step(STEP_LANES, 12, false, false), 1..40),
        seed in any::<u64>(),
    ) {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_time().build().unwrap();
        let _guard = runtime.enter();
        let steps = on_producer_lanes(&steps, lanes.dogstatsd);
        let trace = build_trace(&shape.plan("10s"), &steps, lanes.dogstatsd, None);
        let (_, mut harness) = replayed(&trace, lanes, DeliverySchedule::Seeded(seed), |_| {});
        // Exact against the replay of what the state lanes saw and the ticks they reported, but for the idle zeros of
        // ticks resolved apart.
        if let Some(result) = against_replay(&mut harness, lanes.dogstatsd) {
            prop_assert!(result.is_ok(), "{:?}, seed {}: {}", lanes, seed, result.unwrap_err());
        }
        let lane_times = check_lane_times(&mut harness);
        prop_assert!(lane_times.is_ok(), "{:?}, seed {}: {}", lanes, seed, lane_times.unwrap_err());
    }

    #[test]
    fn property_test_lane_count_invariance(
        shape in arb_shape(),
        dropped in any::<bool>(),
        pressure in any::<bool>(),
        state in prop::sample::select(vec![2u16, 3, 4, 8]),
        steps in vec(arb_step(2, 12, true, true), 1..40),
    ) {
        // One trace and one schedule, trace order, which fixes the order each context's events reach the state slot
        // identically at one state lane and at `state`, with resets and cuts: bit-equal outputs at the same flush
        // times, equal flush counts, equal counts of what R5b suppresses, and no breach. Coalescing at `state` lanes
        // orders what several points send one lane by buffer, not by call, so no input's effect depends on that order
        // (`without_races`).
        //
        // Resets keep every lane heartbeating at the next tick, so that no reset window stays open across a tick: one
        // that does makes the lineage on state lanes catch up the ticks it held in a burst, each counted as a flush
        // (F6, `a_window_open_across_ticks_counts_the_flushes_one_lane_counts`).
        //
        // Under pressure, a context limit of 3 over gauges, which no flush keeps, without resets or cuts: which
        // contexts win at the limit is the lanes' race (capacity behavior), but how many points are delivered, the
        // ticks at which some context is held, which count as flushes, and the breach episodes, one warning and one
        // recovery each across the lanes, are not. (A counter that won would be held after its flush, and count more
        // flushes.)
        let runtime = tokio::runtime::Builder::new_current_thread().enable_time().build().unwrap();
        let _guard = runtime.enter();
        let shape = Shape {
            dropped: dropped.then_some(2),
            limit: if pressure { 3 } else { shape.limit },
            ..shape
        };
        // A cut is armed by the source's clock, which the first stamp starts.
        let first = steps.iter().position(Step::stamps).unwrap_or(steps.len());
        let steps = steps
            .iter()
            .enumerate()
            .filter(|(index, step)| match step {
                Step::Cut => !pressure && *index > first,
                Step::Reset { .. } => !pressure,
                _ => true,
            })
            .map(|(_, step)| match step {
                Step::Reset { lane, .. } => Step::Reset { lane: *lane, quiet: 0 },
                step if pressure => as_gauges(step.clone()),
                step => step.clone(),
            })
            .collect::<Vec<_>>();
        let steps = without_races(&steps);
        let trace = build_trace(&shape.plan("10s"), &steps, 2, Some(&shape.plan("5s")));
        let one = LanePolicy { dogstatsd: 2, state: 1, post: 1 };
        let many = LanePolicy { state, ..one };
        let in_order = DeliverySchedule::TraceOrder;
        let (mut a, a_counts) = counted(|| replayed(&trace, one, in_order, |_| {}).1);
        let (mut b, b_counts) = counted(|| replayed(&trace, many, in_order, |_| {}).1);
        // A reset window that overflowed loses what still reaches its oldest epoch (counted), which only coalescing
        // at `state` lanes holds back.
        prop_assume!(b.reset_overflows() == 0);
        prop_assert_eq!(a_counts.flushes, b_counts.flushes, "flushes at 1 and {} state lanes", state);
        prop_assert_eq!(a_counts.breaches, b_counts.breaches, "breach episodes at 1 and {} state lanes", state);
        if pressure {
            let count = |harness: &FrameHarness| harness.outputs().count(&output());
            prop_assert_eq!(count(&a), count(&b), "points delivered at 1 and {} state lanes", state);
        } else {
            let result = compare(a.outputs(), b.outputs());
            prop_assert!(result.is_ok(), "1 and {} state lanes: {}", state, result.unwrap_err());
            prop_assert_eq!(stamped(&a), stamped(&b), "flush times at 1 and {} state lanes", state);
            prop_assert_eq!(a_counts.breaches, (0, 0));
            for harness in [&mut a, &mut b] {
                harness.publish();
            }
            for (port, node) in [("true", "post"), ("false", "post")] {
                prop_assert_eq!(
                    a.telemetry().port_events(node, port),
                    b.telemetry().port_events(node, port),
                    "`{}.{}` at 1 and {} state lanes", node, port, state
                );
            }
            prop_assert_eq!(
                a.telemetry().loss("post_dropped", LossClass::Policy),
                b.telemetry().loss("post_dropped", LossClass::Policy),
                "suppressed at 1 and {} state lanes", state
            );
        }
    }

    #[test]
    fn property_test_lanes_under_interleaving(
        shape in arb_shape(),
        lanes in arb_lanes(),
        steps in vec(arb_step(STEP_LANES, 12, false, false), 1..40),
        seed in any::<u64>(),
    ) {
        // Seeded schedules against the interpreter in candidate mode on the original trace, kind conflicts on few names
        // (`CONFLICTED_NAMES`): every other series' values compared; and the loss balance of both runs, what a kind
        // replacement dropped counted as policy loss.
        let runtime = tokio::runtime::Builder::new_current_thread().enable_time().build().unwrap();
        let _guard = runtime.enter();
        let steps = on_producer_lanes(&steps, lanes.dogstatsd);
        let trace = build_trace(&shape.plan("10s"), &steps, lanes.dogstatsd, None);
        let (_, mut harness) = replayed(&trace, lanes, DeliverySchedule::Seeded(seed), |_| {});
        let run = reference(&trace, lanes.dogstatsd, true);
        // A tick resolved apart changes only idle zeros, which the comparison lets differ.
        let verdict = compare_partitioned(
            &OutputPoints::from_run(&run), harness.outputs(), run.candidates(), Tolerance::from_limits(),
        );
        prop_assert_eq!(verdict, ReorderedVerdict::Equivalent, "{:?}, seed {}", lanes, seed);
        harness.publish();
        let framed = balance(&trace, &shape, 10, harness.outputs(), |class| harness.telemetry().loss("agg", class));
        prop_assert_eq!(check_balance(&framed), CapacityVerdict::Holds, "{:?}, seed {}", lanes, seed);
        let agg: NodeId = "agg".parse().unwrap();
        let interpreted = balance(
            &trace, &shape, 10, &OutputPoints::from_run(&run), |class| run.loss(&agg).loss(class).points,
        );
        prop_assert_eq!(check_balance(&interpreted), CapacityVerdict::Holds, "the interpreter");
    }

    #[test]
    fn property_test_idle_counters_across_lanes(
        expiry in prop::sample::select(vec!["0s", "5m"]),
        counters in 1usize..4,
        gaps in vec((0u16..2, 0u64..25_000, 0u8..12), 1..12),
        delayed in any::<bool>(),
    ) {
        // One to three counters over eight state lanes, so most lanes are empty at most ticks; gaps longer than the
        // window; a cut, and a reset, between some ticks. In trace order, exact at one and at eight state lanes and
        // against the interpreter, flush times included, so that every closing flush (a crossing, a reset drain,
        // shutdown) emits only the zeros one core emits. With `delayed`, producer lane 1's buffers toward state lane 0
        // lag behind by more than a flush interval: the lanes fall back and count it, and the values still equal the
        // interpreter's but for idle zeros.
        let runtime = tokio::runtime::Builder::new_current_thread().enable_time().build().unwrap();
        let _guard = runtime.enter();
        let shape = Shape {
            mapper: false,
            prefix: false,
            stripped: Vec::new(),
            expiry,
            limit: 1_000,
            dropped: None,
        };
        let mut steps = Vec::new();
        for (index, (lane, gap_ms, between)) in gaps.iter().enumerate() {
            steps.push(Step::Admit {
                lane: *lane,
                gap_ms: *gap_ms,
                picks: vec![Pick { name: index % counters * 2, gauge: false, value: 1.0, pod: 0, owner: None }],
            });
            steps.push(Step::Tick);
            match between {
                0 if index > 0 => steps.push(Step::Cut),
                1 => steps.push(Step::Reset { lane: *lane, quiet: 0 }),
                _ => {}
            }
        }
        let trace = build_trace(&shape.plan("10s"), &steps, 2, Some(&shape.plan("5s")));
        let lanes = LanePolicy { dogstatsd: 2, state: 8, post: 1 };
        let (run, one) = replayed(&trace, LanePolicy { state: 1, ..lanes }, DeliverySchedule::TraceOrder, |_| {});
        prop_assert!(compare(&OutputPoints::from_run(&run), one.outputs()).is_ok());
        prop_assert_eq!(stamped(&one), stamped_run(&run), "flush times at one state lane");
        if !delayed {
            let (_, many) = replayed(&trace, lanes, DeliverySchedule::TraceOrder, |_| {});
            let result = compare(&OutputPoints::from_run(&run), many.outputs());
            prop_assert!(result.is_ok(), "eight lanes: {}", result.unwrap_err());
            prop_assert_eq!(stamped(&many), stamped_run(&run), "flush times at eight state lanes");
        } else {
            let schedule = DeliverySchedule::Delay {
                point: saluki_core::execution::AdmissionPointId::new(1),
                lane: 0,
                by: Duration::from_secs(12),
            };
            let (_, mut many) = replayed(&trace, lanes, schedule, |_| {});
            prop_assert!(many.ticks_resolved_apart() > 0, "the delayed lane's fallbacks are counted");
            let verdict = compare_partitioned(
                &OutputPoints::from_run(&run), many.outputs(), &Candidates::new(), Tolerance::from_limits(),
            );
            prop_assert_eq!(verdict, ReorderedVerdict::Equivalent, "the delayed lanes against the interpreter");
        }
    }

    #[test]
    fn property_test_multi_point_reset(
        shape in arb_shape(),
        lanes in arb_lanes(),
        steps in vec(arb_step(STEP_LANES, 6, true, true), 1..48),
        seed in any::<u64>(),
    ) {
        // Resets at any producer lane, with lanes that stay quiet after another lane's reset, so that state lanes'
        // windows stay open across ticks and at the close; ticks in every timeline, inside the spread of the windows'
        // openings; bursts that fill routed buffers, also as the first call of a new epoch; cuts, a reset during one;
        // low cardinality, resets inside windows, and lanes that skip an epoch. In trace order exact against the
        // interpreter, flush times included, so that every lane drains an epoch at the interpreter's watermark; every
        // lane drains each epoch at one watermark, and resumes at the old record's next instant. With a seeded
        // schedule, exact against the arrival replay whenever every lane resolved each tick alike (otherwise counted,
        // and compared but for idle zeros), when the trace has no cut, which a replay of one lineage cannot state; and
        // with a cut, equal to the interpreter but for idle zeros.
        let runtime = tokio::runtime::Builder::new_current_thread().enable_time().build().unwrap();
        let _guard = runtime.enter();
        let first = steps.iter().position(Step::stamps).unwrap_or(steps.len());
        let steps = steps
            .iter()
            .enumerate()
            .filter(|(index, step)| !matches!(step, Step::Cut) || *index > first)
            .map(|(_, step)| step.clone())
            .collect::<Vec<_>>();
        let steps = without_races(&on_producer_lanes(&steps, lanes.dogstatsd));
        let cuts = steps.iter().any(|step| matches!(step, Step::Cut));
        let trace = build_trace(&shape.plan("10s"), &steps, lanes.dogstatsd, Some(&shape.plan("5s")));
        let (run, mut ordered) = replayed(&trace, lanes, DeliverySchedule::TraceOrder, |_| {});
        // Unless five resets came before every point switched, which closes the oldest epoch at once, at whatever
        // watermark each lane has for it then, and loses what still reaches it (counted); what an overflow must keep
        // is `a_reset_overflow_during_a_cut_loses_nothing_of_the_epochs_it_leaves_open`'s (F7).
        if ordered.reset_overflows() == 0 {
            let result = compare(&OutputPoints::from_run(&run), ordered.outputs());
            prop_assert!(result.is_ok(), "{:?} in trace order: {}", lanes, result.unwrap_err());
            prop_assert_eq!(stamped(&ordered), stamped_run(&run), "flush times at {:?} in trace order", lanes);
            let lane_times = check_lane_times(&mut ordered);
            prop_assert!(lane_times.is_ok(), "{:?} in trace order: {}", lanes, lane_times.unwrap_err());
            // The recorder's model of one lineage's ticks agrees with what the lanes reported, also while a quiet
            // lane holds a window open across ticks, which trace order does more often than a seeded schedule.
            if !cuts && ordered.ticks_resolved_apart() == 0 {
                let disagreement = ordered.replay_disagreement().unwrap_or_default();
                prop_assert!(disagreement.is_empty(), "{:?} in trace order: {}", lanes, disagreement);
            }
        }
        let (_, mut seeded) = replayed(&trace, lanes, DeliverySchedule::Seeded(seed), |_| {});
        if seeded.reset_overflows() > 0 {
            return Ok(());
        }
        let lane_times = check_lane_times(&mut seeded);
        prop_assert!(lane_times.is_ok(), "{:?}, seed {}: {}", lanes, seed, lane_times.unwrap_err());
        if cuts {
            let verdict = compare_partitioned(
                &OutputPoints::from_run(&run), seeded.outputs(), &Candidates::new(), Tolerance::from_limits(),
            );
            prop_assert_eq!(verdict, ReorderedVerdict::Equivalent, "{:?}, seed {}", lanes, seed);
        } else if let Some(result) = against_replay(&mut seeded, lanes.dogstatsd) {
            prop_assert!(result.is_ok(), "{:?}, seed {}, the replay: {}", lanes, seed, result.unwrap_err());
        }
    }

    #[test]
    fn property_test_cut_with_lanes(
        state in prop::sample::select(vec![2u16, 4]),
        before in vec(arb_step(2, 12, false, false), 1..16),
        after in vec(arb_step(2, 12, false, false), 1..16),
        reset in any::<bool>(),
        delay_ms in 0u64..400,
    ) {
        // A cut from 10 to 5 seconds mid-trace, with producer lane 1's buffers toward state lane 1 delayed, so that the
        // state lanes cross at different times, and, with `reset`, a reset during the cut. Against the interpreter,
        // with no input whose effect depends on the order of the points (`without_races`), which the delay changes:
        // every value exact, sums with no tolerance, and, whenever the lanes resolved every tick alike, at its flush
        // time. Idle zeros are not compared: the delayed lane's tick waits for producer lane 1's heartbeat, and a
        // counter that arrives from producer lane 0 meanwhile gains a zero where its tick closes a bucket, as one core
        // it reached in that order would give it, and as the interpreter, ticking in trace order, does not. At each
        // lane one instance per lineage and epoch, which ticks nothing after its crossing or reset drain, and once
        // every lane crossed, no context held twice.
        let runtime = tokio::runtime::Builder::new_current_thread().enable_time().build().unwrap();
        let _guard = runtime.enter();
        let shape = Shape {
            mapper: false,
            prefix: false,
            stripped: Vec::new(),
            expiry: "5m",
            limit: 1_000,
            dropped: None,
        };
        let mut steps = before;
        // A cut is armed by the source's clock, which the first stamp starts.
        if !steps.iter().any(Step::stamps) {
            steps.push(Step::Beat { lane: 0, gap_ms: 0 });
        }
        steps.push(Step::Cut);
        if reset {
            steps.push(Step::Reset { lane: 0, quiet: 0 });
        }
        steps.extend(after);
        let steps = without_races(&steps);
        let trace = build_trace(&shape.plan("10s"), &steps, 2, Some(&shape.plan("5s")));
        let lanes = LanePolicy { dogstatsd: 2, state, post: 1 };
        let schedule = DeliverySchedule::Delay {
            point: saluki_core::execution::AdmissionPointId::new(1),
            lane: 1,
            by: Duration::from_millis(delay_ms),
        };
        let (run, mut harness) = replayed(&trace, lanes, schedule, |_| {});
        let exact = Tolerance { relative: 0.0 };
        let verdict = compare_partitioned(&OutputPoints::from_run(&run), harness.outputs(), &Candidates::new(), exact);
        prop_assert_eq!(verdict, ReorderedVerdict::Equivalent, "{:?}", lanes);
        if harness.ticks_resolved_apart() == 0 {
            let values = |stamped: Vec<Stamped>| {
                stamped
                    .into_iter()
                    .flat_map(|(at, points)| {
                        points.into_iter().filter(|point| !idle_zero(point)).map(move |point| (at, point))
                    })
                    .collect::<std::collections::BTreeSet<_>>()
            };
            prop_assert_eq!(values(stamped(&harness)), values(stamped_run(&run)), "flush times at {:?}", lanes);
        }
        let lane_times = check_lane_times(&mut harness);
        prop_assert!(lane_times.is_ok(), "{:?}: {}", lanes, lane_times.unwrap_err());
        let slot = harness.frame().slots().iter().find(|slot| slot.is_isolation()).map(|slot| slot.name.to_string());
        if let Some(slot) = slot {
            harness.publish();
            for lane in 0..state {
                let overlap = harness.telemetry().lane_gauge(&slot, "plan_lane_crossing_overlap_contexts", lane);
                prop_assert_eq!(overlap, 0.0, "state lane {} holds contexts twice once every lane crossed", lane);
            }
        }
    }

    #[test]
    fn property_test_partitioned_capacity(
        state in prop::sample::select(vec![2u16, 4, 8]),
        limit in prop::sample::select(vec![0u64, 1, 2, 3, 1_000]),
        capacity in 1u64..8,
        steps in vec(arb_step(2, 40, false, false), 1..40),
    ) {
        // Contexts beyond the context limit and the global capacity, a node limit below the capacity, and churn as
        // gauges flush away: every budget's recorded operations hold under the capacity check, the node's against
        // its limit, and every admitted bucket is delivered or counted as lost, the state owners emitting no context
        // they did not admit.
        let runtime = tokio::runtime::Builder::new_current_thread().enable_time().build().unwrap();
        let _guard = runtime.enter();
        let shape = Shape {
            mapper: false,
            prefix: false,
            stripped: Vec::new(),
            expiry: "5m",
            limit,
            dropped: None,
        };
        let trace = build_trace(&shape.plan("10s"), &steps, 2, None);
        let lanes = LanePolicy { dogstatsd: 2, state, post: 1 };
        let (_, mut harness) = replayed(&trace, lanes, DeliverySchedule::TraceOrder, |options| {
            options.state_capacity = Some(capacity);
            options.record_budgets = true;
        });
        for (budget, recorded) in harness.budget_traces() {
            let recorded = if budget == "agg" { recorded.with_limit(limit) } else { recorded };
            let verdict = check_capacity(&recorded);
            prop_assert_eq!(verdict, CapacityVerdict::Holds, "budget `{}` at {} state lanes", budget, state);
        }
        harness.publish();
        let accounted = balance(&trace, &shape, 10, harness.outputs(), |class| harness.telemetry().loss("agg", class));
        prop_assert_eq!(check_balance(&accounted), CapacityVerdict::Holds, "at {} state lanes", state);
    }
}

/// Returns generated steps without input whose effect depends on the order in which batches reach a state lane: every
/// gauge is renamed to a gauge name of its producer lane alone, and every counter of an odd name to a counter name of
/// its producer lane alone, so that a gauge's last value follows one point's order, which every schedule keeps, and no
/// name has two kinds, while counters of even names, whose sums do not depend on the order, are shared by the points.
///
/// A real run orders several points' batches at a state lane by when their buffers arrive, and coalescing sends a
/// buffer per lane and point, so even in trace order a lane receives a point's open buffer after another point's later
/// calls. The interpreter orders them by the trace: the two agree exactly only on input that does not depend on it. A
/// kind replacement depends on order even within one point: it replaces the values of every open bucket, so whether a
/// tick that waits for another point's heartbeat closed a bucket before the other kind arrived decides what it drops.
fn without_races(steps: &[Step]) -> Vec<Step> {
    let owned = |lane: u16, pick: Pick| Pick {
        owner: (pick.gauge || pick.name % 2 == 1).then_some(lane),
        ..pick
    };
    steps
        .iter()
        .cloned()
        .map(|step| match step {
            Step::Admit { lane, gap_ms, picks } => Step::Admit {
                lane,
                gap_ms,
                picks: picks.into_iter().map(|pick| owned(lane, pick)).collect(),
            },
            Step::Burst { lane, gap_ms, pick } => Step::Burst {
                lane,
                gap_ms,
                pick: owned(lane, pick),
            },
            other => other,
        })
        .collect()
}

/// Returns a generated step with every metric it admits a gauge.
fn as_gauges(step: Step) -> Step {
    let gauge = |pick: Pick| Pick { gauge: true, ..pick };
    match step {
        Step::Admit { lane, gap_ms, picks } => Step::Admit {
            lane,
            gap_ms,
            picks: picks.into_iter().map(gauge).collect(),
        },
        Step::Burst { lane, gap_ms, pick } => Step::Burst {
            lane,
            gap_ms,
            pick: gauge(pick),
        },
        other => other,
    }
}

/// Returns generated steps with their producer lanes folded onto `lanes`.
fn on_producer_lanes(steps: &[Step], lanes: u16) -> Vec<Step> {
    steps
        .iter()
        .cloned()
        .map(|step| match step {
            Step::Admit { lane, gap_ms, picks } => Step::Admit {
                lane: lane % lanes,
                gap_ms,
                picks,
            },
            Step::Burst { lane, gap_ms, pick } => Step::Burst {
                lane: lane % lanes,
                gap_ms,
                pick,
            },
            Step::Beat { lane, gap_ms } => Step::Beat {
                lane: lane % lanes,
                gap_ms,
            },
            Step::Reset { lane, quiet } => Step::Reset {
                lane: lane % lanes,
                quiet,
            },
            other => other,
        })
        .collect()
}
