//! Interleaved flushes (V7b, 3.7): a tick or boundary flush of an instance not on lanes is a pass, which runs in steps
//! while the region admits the batches that arrive meanwhile, and whose output, with what the batches flush on touch,
//! equals that of the flush completed before them.
//!
//! Until aggregation's own kernel admits while advancing, every state owner's kernel runs inside the harness's stand-in
//! ([`AdmittingStandIn`][crate::runtime::harness::AdmittingStandIn]), which completes a pending pass when a batch
//! reaches it.

use std::time::Duration;

use proptest::{collection::vec, prelude::*, sample::select};
use saluki_core::{
    data_model::event::metric::Metric,
    execution::{Budget, LossClass, Progress, RegionDriver, WorkKind},
};
use saluki_metrics::{test::TestRecorder, MetricsBuilder};
use serde_json::json;

use super::{gauge, one_context_per_call, plan, t, Direct};
use crate::{
    equivalence::{compare, OutputPoints},
    interpret::Trace,
    ir::{NodeId, Plan},
    limits::values::FLUSH_STEP_PERIOD,
    model::Kind,
    operators::OperatorRegistry,
    runtime::{
        harness::{HarnessOptions, Interleaving, LaneTimeEvent},
        transition_tests::{self, aggregation, dogstatsd, node, Replay},
        PlanTelemetry, PLAN_LANE_FLUSH_SECONDS, PLAN_LANE_FLUSH_TOUCHED_CONTEXTS_TOTAL,
    },
    testing::arb_metrics_with_kind_siblings,
};

/// Returns the driver of `plan`'s isolation slot, interleaving its flushes, with aggregation's own kernel, or with
/// `Some(room)` a stand-in that says `room` new contexts fit while a pass is pending.
fn interleaving(plan: &Plan, room: Option<usize>) -> Direct {
    let stand_in = Interleaving { room, background: None }.stand_in();
    Direct::configured(plan, OperatorRegistry::native(), |config| {
        let config = config.with_interleaving(true);
        match stand_in {
            Some(wrap) => config.with_state_kernel_wrap(wrap),
            None => config,
        }
    })
}

/// Three gauges at 3, and a heartbeat at 15, so that the tick at 15 flushes all three.
fn three_gauges(direct: &mut Direct) {
    direct.process(t(3_000), ["a", "b", "c"].iter().map(|name| gauge(name, 1.0)).collect());
    assert_eq!(direct.process(t(15_000), Vec::new()), Progress::Done);
}

#[test]
fn a_pass_lets_batches_in_on_pace_and_runs_ahead_of_them_once_behind() {
    // The tick at 15 flushes three gauges one context per step. Its first step lets batches in again with the pass
    // pending, due for its next step a step period later; a step on pace does the same. A step a whole step period past
    // its due time finds the pass behind: it holds admission, and yields until the pass completes.
    let mut direct = interleaving(&plan(), None);
    three_gauges(&mut direct);
    let tick = Duration::from_secs(15);
    let (progress, mut emitted) = direct.advance(tick, one_context_per_call());
    assert_eq!(progress, Progress::Done, "the pass lets batches in");
    assert!(direct.driver.accepts_input());
    assert!(direct.driver.has_background_work());
    assert_eq!(
        direct.driver.next_deadline(),
        Some(direct.start + tick + FLUSH_STEP_PERIOD)
    );

    let (progress, written) = direct.advance(tick + FLUSH_STEP_PERIOD, one_context_per_call());
    emitted += written;
    assert_eq!(progress, Progress::Done, "a step at its due time is on pace");
    assert_eq!(
        direct.driver.next_deadline(),
        Some(direct.start + tick + FLUSH_STEP_PERIOD * 2)
    );

    let late = tick + FLUSH_STEP_PERIOD * 4;
    let (progress, written) = direct.advance(late, one_context_per_call());
    emitted += written;
    assert_eq!(
        progress,
        Progress::Yielded,
        "a pass behind its pace runs ahead of admission"
    );
    assert!(!direct.driver.accepts_input());
    assert!(!direct.driver.has_background_work());
    loop {
        let (progress, written) = direct.resume(late, one_context_per_call());
        emitted += written;
        if progress != Progress::Yielded {
            break;
        }
    }
    assert_eq!(emitted, 3, "each gauge once");
    assert!(direct.driver.accepts_input());
    assert!(!direct.driver.has_background_work());
    // The instance's next tick is due once the pass completed: one interval after the one that started it.
    assert_eq!(direct.driver.next_deadline(), Some(direct.start + tick * 2));
}

#[test]
fn a_batch_admitted_while_a_pass_is_pending_delivers_what_it_flushed_on_touch_as_the_steps_output() {
    // The pass of the tick at 15 has flushed one of four gauges when a batch at 15.5 reaches all four: aggregation
    // flushes the other three on touch, which the batch's call delivers, as the lane counts. The pass completes at its
    // next step, emitting nothing more, and the batch's gauges land in [10, 20), which the tick at 30 flushes. (Four
    // contexts leave room in aggregation's table for the batch's four, which the room check counts as new.)
    let mut direct = interleaving(&plan(), None);
    let names = ["a", "b", "c", "d"];
    direct.process(t(3_000), names.iter().map(|name| gauge(name, 1.0)).collect());
    assert_eq!(direct.process(t(15_000), Vec::new()), Progress::Done);
    let tick = Duration::from_secs(15);
    let (progress, first) = direct.advance(tick, one_context_per_call());
    assert_eq!((progress, first), (Progress::Done, 1));

    let header = direct.header(t(15_500));
    let batch = names.iter().map(|name| gauge(name, 2.0)).collect();
    let (progress, touched) = direct.send(header, batch, Budget::unlimited());
    assert_eq!(
        (progress, touched),
        (Progress::Done, 3),
        "the rest of the pass, flushed on touch"
    );
    let (progress, rest) = direct.advance(tick + FLUSH_STEP_PERIOD, one_context_per_call());
    assert_eq!((progress, rest), (Progress::Done, 0));
    assert!(!direct.driver.has_background_work(), "the pass completed");
    direct.driver.publish_telemetry();
    let slot = direct.frame.slot(direct.slot).expect("the slot").name.to_string();
    assert_eq!(
        direct
            .telemetry
            .lane_value(&slot, PLAN_LANE_FLUSH_TOUCHED_CONTEXTS_TOTAL, 0),
        3
    );
    assert_eq!(direct.process(t(30_000), Vec::new()), Progress::Done);
    let (progress, next) = direct.advance(tick * 2, Budget::unlimited());
    assert_eq!(
        (progress, next),
        (Progress::Done, 4),
        "the batch's gauges, at the next tick"
    );
}

#[test]
fn a_batch_that_could_need_more_room_than_the_state_owner_has_waits_for_the_pass() {
    // The stand-in says no new context fits while a pass is pending: a batch that reaches the state owner waits, the
    // pass runs ahead of admission until it completes, and the batch enters after it. A header-only batch, which
    // creates no context, enters at once.
    let mut direct = interleaving(&plan(), Some(0));
    three_gauges(&mut direct);
    let tick = Duration::from_secs(15);
    assert_eq!(direct.advance(tick, one_context_per_call()), (Progress::Done, 1));
    let header = direct.header(t(15_200));
    assert_eq!(
        direct.send(header, Vec::new(), one_context_per_call()),
        (Progress::Done, 0)
    );
    assert!(
        direct.driver.has_background_work(),
        "a heartbeat enters during the pass"
    );

    let header = direct.header(t(15_500));
    let (progress, mut emitted) = direct.send(header, vec![gauge("d", 1.0)], one_context_per_call());
    assert_eq!(progress, Progress::Yielded, "the batch waits");
    assert!(!direct.driver.accepts_input());
    loop {
        let (progress, written) = direct.resume(tick, one_context_per_call());
        emitted += written;
        if progress != Progress::Yielded {
            break;
        }
    }
    assert_eq!(emitted, 2, "the rest of the pass, before the batch");
    assert!(direct.driver.accepts_input());
    assert!(!direct.driver.has_background_work());
    assert_eq!(
        direct.snapshot(),
        ["d"],
        "the pass removed the gauges it flushed, and the batch entered after it"
    );
}

#[test]
fn a_gated_tick_a_header_released_fires_once_however_many_batches_its_pass_lets_in() {
    // The tick due at 15 finds the watermark at 3 and waits at the gate. The batch at 16 releases it: its pass flushes
    // one of three gauges and lets the batch in. The tick stays gated until its pass completes, so the batch at 16.5,
    // whose header the gate reads too, must not release it again: it enters at once, and the tick flushes once.
    let recorder = TestRecorder::default();
    let lane = {
        let _local = metrics::set_default_local_recorder(&recorder);
        PlanTelemetry::new(MetricsBuilder::default()).state_lane("dsd_agg", 0)
    };
    let mut direct = Direct::configured(&plan(), OperatorRegistry::native(), |config| {
        config.with_interleaving(true).with_state_lane_telemetry(lane)
    });
    direct.process(t(3_000), ["a", "b", "c"].iter().map(|name| gauge(name, 1.0)).collect());
    let tick = Duration::from_secs(15);
    assert_eq!(direct.advance(tick, one_context_per_call()), (Progress::Done, 0));
    assert!(!direct.driver.has_background_work(), "the tick waits at the gate");

    let header = direct.header(t(16_000));
    let (progress, mut emitted) = direct.send(header, vec![gauge("d", 1.0)], one_context_per_call());
    assert_eq!(
        (progress, emitted),
        (Progress::Done, 1),
        "the released tick's first step"
    );
    assert!(direct.driver.has_background_work(), "its pass is pending");

    let header = direct.header(t(16_500));
    let (during, written) = direct.send(header, vec![gauge("e", 1.0)], one_context_per_call());
    emitted += written;
    let mut progress = during;
    while progress == Progress::Yielded {
        let (next, written) = direct.resume(tick, Budget::unlimited());
        (progress, emitted) = (next, emitted + written);
    }
    while direct.driver.has_background_work() {
        let (_, written) = direct.advance(tick, Budget::unlimited());
        emitted += written;
    }
    let ticks = direct
        .driver
        .take_time_events()
        .into_iter()
        .filter(|event| matches!(event, LaneTimeEvent::Ticked { .. }))
        .count();
    assert_eq!(ticks, 1, "the tick at 15 fires once");
    assert_eq!(during, Progress::Done, "the batch enters during the pass");
    assert_eq!(emitted, 3, "each gauge once");
    assert_eq!(direct.driver.next_deadline(), Some(direct.start + tick * 2));
    // The lane times the drain of the tick the header released, as it does one a deadline fired.
    let timed = recorder
        .histogram_keys()
        .into_iter()
        .find(|key| key.name() == PLAN_LANE_FLUSH_SECONDS)
        .and_then(|key| recorder.histogram(key))
        .map_or(0, |samples| samples.len());
    assert_eq!(timed, 1, "one drain timed");
}

#[test]
fn a_pass_whose_instances_next_tick_is_already_due_runs_ahead_of_admission() {
    // The tick due at 15 waits at the gate with the watermark at 3, and its grace has long ended when time-driven work
    // next runs, at 31: it fires at the watermark's second, and its pass would let batches in, but the instance's next
    // tick, due at 30, has passed. A batch let in now would enter before that tick, as no native flush order has it, so
    // the pass runs ahead of admission until it completes, and the next tick is due at once.
    let mut direct = interleaving(&plan(), None);
    direct.process(t(3_000), ["a", "b", "c"].iter().map(|name| gauge(name, 1.0)).collect());
    let tick = Duration::from_secs(15);
    assert_eq!(direct.advance(tick, one_context_per_call()), (Progress::Done, 0));
    let late = Duration::from_secs(31);
    let (progress, _) = direct.advance(late, one_context_per_call());
    assert_eq!(progress, Progress::Yielded, "the pass runs ahead of admission");
    assert!(!direct.driver.accepts_input());
    while direct.resume(late, one_context_per_call()).0 == Progress::Yielded {}
    assert!(direct.driver.accepts_input());
    assert!(!direct.driver.has_background_work());
    assert_eq!(direct.driver.next_deadline(), Some(direct.start + tick * 2));
}

// The property test: random traces through a frame of region drivers whose flushes interleave with admission, against
// the interpreter and the same frame with every drain holding admission.

/// The plan around the aggregation the traces drive.
#[derive(Clone, Debug)]
struct Shape {
    /// Whether a kind filter before aggregation lets only counters and gauges through.
    filtered: bool,
    /// Aggregation's context limit.
    context_limit: u64,
    /// Aggregation's counter expiry.
    counter_expiry: &'static str,
    /// Aggregation's flush interval: a second, so that every flush tick finds the next one due, or longer than the
    /// gaps between steps, so that passes let batches in and a gated tick's pass can be pending when a header arrives.
    flush_interval: &'static str,
}

fn arb_shape() -> impl Strategy<Value = Shape> {
    (
        any::<bool>(),
        prop_oneof![1 => Just(0u64), 3 => 1u64..4, 2 => Just(1_000)],
        prop_oneof![Just("0s"), Just("5s"), Just("5m")],
        prop_oneof![Just("1s"), Just("15s")],
    )
        .prop_map(|(filtered, context_limit, counter_expiry, flush_interval)| Shape {
            filtered,
            context_limit,
            counter_expiry,
            flush_interval,
        })
}

/// Returns the plan of `shape`, aggregating over `window`.
fn shaped(shape: &Shape, window: &str) -> Plan {
    let mut params = aggregation(window);
    params["context_limit"] = json!(shape.context_limit);
    params["counter_expiry"] = json!(shape.counter_expiry);
    params["flush_interval"] = json!(shape.flush_interval);
    let mut nodes = vec![
        dogstatsd("mixed"),
        node("agg", "aggregate", params),
        node("out", "emit", json!({"output": "a"})),
    ];
    let mut edges = vec!["agg.out -> out.in"];
    if shape.filtered {
        nodes.extend([
            node(
                "kinds",
                "split",
                json!({"predicate": {"kind_in": ["counter", "gauge"]}}),
            ),
            node("kinds_drop", "drop", json!({})),
        ]);
        edges.extend([
            "src.metrics -> kinds.in",
            "kinds.true -> agg.in",
            "kinds.false -> kinds_drop.in",
        ]);
    } else {
        edges.push("src.metrics -> agg.in");
    }
    transition_tests::plan(nodes, &edges, &["a"])
}

/// One step of a trace.
#[derive(Clone, Debug)]
enum Step {
    /// A batch admitted.
    Admit(Vec<Metric>),
    /// A flush tick.
    Advance,
    /// An installation of the plan aggregating over another window: a cut.
    Install(&'static str),
    /// The source's clock jumps back an hour, past the reset threshold, before the next stamp.
    Reset,
}

fn arb_step() -> impl Strategy<Value = (u64, Step)> {
    (
        0u64..12,
        prop_oneof![
            // DogStatsD admits every kind but rates.
            8 => arb_metrics_with_kind_siblings(1..4).prop_map(|metrics| {
                Step::Admit(metrics.into_iter().filter(|metric| Kind::of(metric.values()) != Kind::Rate).collect())
            }),
            4 => Just(Step::Advance),
            1 => select(vec!["10s", "30s"]).prop_map(Step::Install),
            1 => Just(Step::Reset),
        ],
    )
}

/// Builds the trace of `steps`, each `gap` seconds after the previous one, over the plan of `shape`, from 7,200 on.
///
/// A flush tick at the time of the previous one is left out, as in R5b's property test: the interpreter ticks at every
/// `advance`, a region once per flush interval. At most one reset jumps the clock back, and none with a flush interval
/// longer than a second.
fn trace_of(shape: &Shape, steps: &[(u64, Step)]) -> Trace {
    let mut trace = Trace::new().install(shaped(shape, "10s"));
    let mut now = 7_200;
    let mut last_tick = Some(now);
    let mut reset = false;
    for (gap, step) in steps {
        now += gap;
        trace = match step {
            Step::Admit(metrics) => trace.admit(src(), t_s(now), metrics.clone()),
            Step::Advance if last_tick == Some(now) => trace,
            Step::Advance => {
                last_tick = Some(now);
                trace.advance(t_s(now))
            }
            Step::Install(window) => trace.install(shaped(shape, window)),
            // With the longer flush interval a pass can still be pending when a reset's header arrives, which is
            // known to differ from blocking drains (an idle counter's zero after the reset), and left out here.
            Step::Reset if reset || shape.flush_interval != "1s" => trace,
            Step::Reset => {
                reset = true;
                now -= 3_600;
                last_tick = None;
                trace
            }
        };
    }
    // Every bucket closes, and every idle counter expires or synthesizes its zeros, before the trace ends.
    trace.advance(t_s(now + 320)).close()
}

/// Returns the time `relative` seconds after the transition traces' base time.
fn t_s(relative: u64) -> saluki_core::execution::AdmissionTime {
    transition_tests::t(relative)
}

fn src() -> NodeId {
    "src".parse().unwrap()
}

/// Aggregation's own kernel half the time, and otherwise a stand-in that completes a pass when a batch reaches it, with
/// room for none, a few, or any number of new contexts.
fn arb_interleaving() -> impl Strategy<Value = Interleaving> {
    (
        prop_oneof![3 => Just(None), 1 => Just(Some(0usize)), 1 => Just(Some(2)), 1 => Just(Some(usize::MAX))],
        proptest::option::of(any::<u64>()),
    )
        .prop_map(|(room, background)| Interleaving { room, background })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn property_test_interleaved_drains_match_the_interpreter(
        shape in arb_shape(),
        steps in vec(arb_step(), 1..24),
        interleaving in arb_interleaving(),
        step_contexts in 1u64..4,
        isolate_state in any::<bool>(),
        heartbeats_at_ticks in any::<bool>(),
    ) {
        let trace = trace_of(&shape, &steps);
        let blocking = HarnessOptions {
            monotonic_across_jumps: Some(Duration::from_secs(1)),
            isolate_state,
            ..HarnessOptions::default()
        };
        let interleaved = HarnessOptions {
            interleaving: Some(interleaving),
            turn_budget: Budget::unlimited().with_limit(WorkKind::StateWork, step_contexts),
            ..blocking
        };
        let start = t_s(7_200);
        // Without heartbeats at ticks the watermark lags them, and ticks wait at the gate for a header to release them.
        let (run, mut interleaved) = Replay::run_with(&trace, heartbeats_at_ticks, start, interleaved);
        let (_, mut blocking) = Replay::run_with(&trace, heartbeats_at_ticks, start, blocking);

        // Every output receives what the frame delivers with every drain holding admission, and what the interpreter
        // delivers where the interpreter models the frame's ticks: with a heartbeat at each, and a tick at every advance,
        // as a one-second flush interval has it. (Otherwise the two can disagree whether drains interleave or not.)
        let expected = OutputPoints::from_run(&run);
        let against_interpreter = heartbeats_at_ticks && shape.flush_interval == "1s";
        if against_interpreter {
            if let Err(mismatch) = compare(&expected, blocking.harness.outputs()) {
                prop_assert!(false, "blocking drains differ from the interpreter:\n{}", mismatch);
            }
            if let Err(mismatch) = compare(&expected, interleaved.harness.outputs()) {
                prop_assert!(false, "interleaved drains differ from the interpreter:\n{}", mismatch);
            }
        }
        if let Err(mismatch) = compare(blocking.harness.outputs(), interleaved.harness.outputs()) {
            prop_assert!(false, "interleaved drains differ from blocking ones:\n{}", mismatch);
        }

        // Aggregation loses the same points to its context limit: no admission during a pass is refused where it
        // would have been admitted after it.
        interleaved.harness.publish();
        blocking.harness.publish();
        let agg: NodeId = "agg".parse().unwrap();
        if against_interpreter {
            prop_assert_eq!(
                interleaved.harness.telemetry().loss("agg", LossClass::Capacity),
                run.loss(&agg).loss(LossClass::Capacity).points
            );
        }
        prop_assert_eq!(
            interleaved.harness.telemetry().loss("agg", LossClass::Capacity),
            blocking.harness.telemetry().loss("agg", LossClass::Capacity)
        );

        // No tick fires twice, as a header releasing a gated tick again while its pass lets batches in would make it: a
        // second flush at the same time mostly emits nothing, so the outputs alone rarely tell.
        let names = interleaved.harness.state_slots().map(|slot| slot.name.to_string()).collect::<Vec<_>>();
        for name in names {
            let mut ticks = std::collections::BTreeSet::new();
            for (lane, event) in interleaved.harness.slot_time_events(&name) {
                if let LaneTimeEvent::Ticked { created_by, epoch, scheduled, .. } = event {
                    prop_assert!(
                        ticks.insert((lane, created_by, epoch, scheduled)),
                        "slot {} fired the tick due at {:?} of generation {}'s lineage in epoch {} twice",
                        name, scheduled, created_by, epoch
                    );
                }
            }
        }
    }
}
