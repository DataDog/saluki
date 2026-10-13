//! Sources with lanes in the interpreter: lanes stamp by one clock, a state owner's watermark waits for every lane, and
//! a reset at one lane runs as a reset window that the last lane to switch closes, with the instances per epoch and the
//! cut rules of the stage 9 design (5.6); candidate mode; and the hand-written fixtures of 6.9 whose expectation is the
//! one core's.
//!
//! Each expectation is written by hand from those rules, over plans `src -> agg -> emit` with the transition traces'
//! aggregation (10-second window, native defaults), times relative to `B`.

use std::time::Duration;

use saluki_core::{
    data_model::event::metric::{context::Context, Metric, MetricMetadata, MetricValues},
    execution::{AdmissionPointId, AdmissionTime, LossClass},
};
use serde_json::json;
use stringtheory::MetaString;

use super::transition_tests::{
    aggregated, aggregated_with, aggregation, catalog, counter, delivered, describe, expected, installs, ms, node,
    outcomes, src, t, Delivered, B,
};
use crate::{
    equivalence::{compare_partitioned, OutputPoints, PointKind, ReorderedVerdict, Tolerance},
    interpret::{test_operators::test_registry, InterpretErrorKind, Interpreter, RunOutput, Trace},
    ir::{OutputId, Plan},
    operators::OperatorRegistry,
    validate::Validator,
};

/// Runs `trace` with `src` on `lanes` lanes, every lane sending a heartbeat at each tick if `heartbeats_at_ticks`.
fn run_on(trace: &Trace, lanes: u16, heartbeats_at_ticks: bool) -> RunOutput {
    let registry = OperatorRegistry::native();
    let catalog = catalog();
    let interpreter = Interpreter::new(&registry, &catalog).with_source_lanes(src(), lanes);
    let interpreter = if heartbeats_at_ticks {
        interpreter.with_heartbeats_at_ticks()
    } else {
        interpreter
    };
    interpreter.run(trace).unwrap()
}

/// Returns what `output` received, sorted: the order of records within one flush is not part of the expectation.
fn received(run: &RunOutput, output: &str) -> Vec<Delivered> {
    let mut records = delivered(run, output);
    records.sort();
    records
}

fn sorted(records: &[(u64, u64, &str)]) -> Vec<Delivered> {
    let mut records = expected(records);
    records.sort();
    records
}

fn admit(trace: Trace, lane: u16, at: AdmissionTime, batch: Vec<Metric>) -> Trace {
    trace.admit_at_lane(src(), lane, at, batch)
}

fn beat(trace: Trace, lane: u16, at: AdmissionTime) -> Trace {
    trace.heartbeat_at_lane(src(), lane, at)
}

#[test]
fn lanes_stamp_by_one_clock_and_only_each_lanes_own_times_must_not_move_back() {
    let registry = OperatorRegistry::native();
    let catalog = catalog();
    let interpreter = Interpreter::new(&registry, &catalog).with_source_lanes(src(), 2);
    let base = || {
        admit(
            Trace::new().install(aggregated("10s", "a")),
            0,
            t(101),
            vec![counter("c", 1.0)],
        )
    };

    // Lane 1's stamp reaches the state owner after a later one of lane 0's, as lanes' paths deliver: within the
    // threshold, that is no reset, and the stamp lands in its own bucket.
    let trace = admit(base(), 1, ms(100_500), vec![counter("c", 2.0)]);
    let trace = beat(beat(trace, 1, t(111)), 0, t(111)).advance(t(111));
    let run = interpreter.run(&trace).unwrap();
    assert_eq!(received(&run, "a"), sorted(&[(1, 111_000, "c rate/10s [100:3]")]));

    // A lane's own stamp moving back is one no clock makes.
    let error = interpreter
        .run(&admit(admit(base(), 1, t(105), vec![]), 1, t(104), vec![]))
        .unwrap_err();
    assert_eq!(error.event, Some(3));
    assert!(
        matches!(error.kind, InterpretErrorKind::AdmissionTimeMovedBack { ref point, .. } if point == "src.metrics#1"),
        "{}",
        error
    );

    // A lane the source does not have.
    let error = interpreter.run(&admit(base(), 2, t(102), vec![])).unwrap_err();
    assert!(
        matches!(error.kind, InterpretErrorKind::UnknownLane { lane: 2, lanes: 2, .. }),
        "{}",
        error
    );
    assert!(error.to_string().contains("with_source_lanes"), "{}", error);
    let error = Interpreter::new(&registry, &catalog)
        .run(&admit(base(), 1, t(102), vec![]))
        .unwrap_err();
    assert!(
        matches!(error.kind, InterpretErrorKind::UnknownLane { lane: 1, lanes: 1, .. }),
        "{}",
        error
    );
}

#[test]
fn a_tick_waits_for_every_lane_and_fires_at_its_second_once_the_slowest_lane_reached_it() {
    // Lane 1 has stamped nothing at the tick at 101, so the watermark is unknown and the tick is skipped; at 102 it is
    // at lane 1's 99, so the tick gates; lane 1's heartbeat at 102 releases it, at its second.
    let trace = admit(
        Trace::new().install(aggregated("10s", "a")),
        0,
        t(95),
        vec![counter("c", 1.0)],
    );
    let trace = beat(trace, 0, t(102)).advance(t(101));
    let trace = beat(trace, 1, t(99)).advance(t(102));
    let trace = beat(trace, 1, t(102));
    let run = run_on(&trace, 2, false);
    assert_eq!(received(&run, "a"), sorted(&[(1, 102_000, "c rate/10s [90:1]")]));
    assert_eq!(run.watermark(&"agg".parse().unwrap()), Some(t(102)));
}

#[test]
fn a_reset_at_one_lane_opens_a_window_that_the_last_lane_to_switch_closes() {
    // Lane 0 jumps back an hour at 3,601: the source's clock resets once, for both lanes. Lane 1, whose last stamp was
    // at 7,195, has not switched, so the window opens: the old epoch's instance waits, and the new epoch's data, 2 and
    // 1 at 3,601 and 3,603, goes to a fresh instance. Lane 1's heartbeat at 3,604 closes the window: the old instance
    // drains at the old epoch's watermark, the earlier of the lanes' final times, 7,185, emitting everything it holds,
    // and the lineage continues on the fresh instance, whose [3600, 3610) closes at the tick at 3,615.
    //
    // Draining at the first lane's reset, as one point's reset does, would have mixed the new epoch's counts into
    // the old instance or flushed them at lane 1's switch, at 3,603.
    let trace = Trace::new().install(aggregated("10s", "a"));
    let trace = admit(trace, 0, t(7_185), vec![counter("i", 1.0)]);
    let trace = admit(trace, 1, t(7_195), vec![counter("c", 1.0)]);
    let trace = admit(trace, 0, t(3_601), vec![counter("c", 2.0)]);
    let trace = admit(trace, 0, t(3_603), vec![counter("c", 1.0)]);
    let trace = beat(trace, 1, t(3_604)).advance(t(3_615));
    let run = run_on(&trace, 2, true);
    assert_eq!(
        received(&run, "a"),
        sorted(&[
            (1, 7_185_000, "i rate/10s [7180:1]"),
            (1, 7_185_000, "c rate/10s [7190:1]"),
            (1, 3_615_000, "c rate/10s [3600:3]"),
        ])
    );
}

#[test]
fn a_window_spanning_two_resets_closes_both_epochs_oldest_first_for_a_lane_that_skipped_one() {
    // Lane 0 resets twice, at 3,601 and again at 10, while lane 1 stamps nothing; lane 1's next stamp, at 20, is in the
    // newest epoch, past the middle one without a stamp there. The window then closes both older epochs, oldest first:
    // the first epoch at the lanes' earlier final time, 7,185, and the middle one at lane 0's 3,602 alone, lane 1
    // having stamped nothing in it. The lineage continues on the newest epoch's instance.
    let trace = Trace::new().install(aggregated("10s", "a"));
    let trace = admit(trace, 0, t(7_185), vec![counter("c", 1.0)]);
    let trace = beat(trace, 1, t(7_190));
    let trace = beat(trace, 0, t(3_601));
    let trace = admit(trace, 0, t(3_602), vec![counter("c", 1.0)]);
    let trace = beat(trace, 0, t(10));
    let trace = admit(trace, 0, t(15), vec![counter("c", 5.0)]);
    let trace = beat(trace, 1, t(20)).advance(t(25));
    let run = run_on(&trace, 2, true);
    assert_eq!(
        received(&run, "a"),
        sorted(&[
            (1, 7_185_000, "c rate/10s [7180:1]"),
            (1, 3_602_000, "c rate/10s [3600:1]"),
            (1, 25_000, "c rate/10s [10:5]"),
        ])
    );
}

#[test]
fn a_reset_after_a_cuts_publication_is_the_old_lineages_final_retirement_when_its_window_closes() {
    // Trace T-L3 in the interpreter's form. The cut to 5 seconds is armed at 7,198.5 (T = 7,200); lane 1 stamps the new
    // generation in the old epoch, at 7,200.5, with 5 in the new lineage's [7200, 7205). Lane 0's clock then resets: a
    // reset after publication is the new generation's, and opens a window. The old lineage does not cross yet: its
    // crossing waits for the window's close, at lane 1's switch, where it drains at the old epoch's watermark, 7,198.5,
    // clamped before the cut, and retires. The new lineage's old-epoch instance drains at the same watermark, not
    // clamped, since it does not retire, and the lineage continues on the instance the new epoch's first data created.
    let trace = Trace::new().install(aggregated("10s", "a"));
    let trace = admit(trace, 0, t(7_195), vec![counter("c", 1.0)]);
    let trace = beat(trace, 1, t(7_196));
    let trace = beat(trace, 0, ms(7_198_500)).install(aggregated("5s", "a"));
    let trace = admit(trace, 1, ms(7_200_500), vec![counter("c", 5.0)]);
    let trace = beat(trace, 0, t(3_601));
    let trace = admit(trace, 0, t(3_602), vec![counter("c", 2.0)]);
    let trace = beat(trace, 1, t(3_603));
    let trace = beat(beat(trace, 0, t(3_606)), 1, t(3_606)).advance(t(3_606));
    let run = run_on(&trace, 2, false);
    assert_eq!(installs(&run), outcomes(&[(0, "active 1"), (4, "cut 2 at 7200000")]));
    assert_eq!(
        received(&run, "a"),
        sorted(&[
            (1, 7_198_500, "c rate/10s [7190:1]"),
            (2, 7_198_500, "c rate/5s [7200:5]"),
            (2, 3_606_000, "c rate/5s [3600:2]"),
        ])
    );
}

#[test]
fn a_cut_finalizes_once_every_lane_passed_it_though_a_reset_window_still_holds_the_lineage_it_retires() {
    // T-L3's trace with a third plan, of 10 seconds again, installed while the cut to 5 seconds waits. Lane 1 passes
    // the cut by stamping its new generation at 7,200.5, and lane 0 by resetting at 3,601: every lane has passed it
    // then, by its own stamps, as the installer sees it, though lane 1 has not switched epochs and the window holds the
    // old lineage. So the cut finalizes at 3,601 and the waiting plan starts at once, its cut armed from the clock then
    // (T = 3,610); waiting for the window's close, at lane 1's switch at 3,615, would have armed it at 3,620.
    //
    // The old lineage still crosses at the window's close, through the program it served: its reset drain, at the old
    // epoch's watermark, 7,198.5, before its cut, is its final retirement. The 5-second lineage's old-epoch instance
    // drains at the same watermark, unclamped, since its own cut is published in the new epoch, and its new-epoch
    // instance holds lane 0's 2 until both lanes pass the third plan's cut, which it crosses at 3,610.
    let trace = Trace::new().install(aggregated("10s", "a"));
    let trace = admit(trace, 0, t(7_195), vec![counter("c", 1.0)]);
    let trace = beat(trace, 1, t(7_196));
    let trace = beat(trace, 0, ms(7_198_500))
        .install(aggregated("5s", "a"))
        .install(aggregated("10s", "a"));
    let trace = admit(trace, 1, ms(7_200_500), vec![counter("c", 5.0)]);
    let trace = beat(trace, 0, t(3_601));
    let trace = admit(trace, 0, t(3_602), vec![counter("c", 2.0)]);
    let trace = beat(trace, 1, t(3_615));
    let trace = beat(beat(trace, 0, t(3_621)), 1, t(3_621)).advance(t(3_621));
    let run = run_on(&trace, 2, false);
    assert_eq!(
        installs(&run),
        outcomes(&[(0, "active 1"), (4, "cut 2 at 7200000"), (5, "cut 3 at 3610000")])
    );
    assert_eq!(
        received(&run, "a"),
        sorted(&[
            (1, 7_198_500, "c rate/10s [7190:1]"),
            (2, 7_198_500, "c rate/5s [7200:5]"),
            (2, 3_610_000, "c rate/5s [3600:2]"),
        ])
    );
}

#[test]
fn an_instance_a_window_created_before_the_cut_retires_with_its_lineage_and_drains_under_the_generation_it_served() {
    // Lane 0 resets at 3,700 and admits `m` there, which a fresh instance of the old lineage takes beside the bound
    // one, whose epoch the window holds; then the cut to 5 seconds is armed (T = 3,710, both lanes recorded in epoch
    // 1). Lane 0 resets again, at 100, and lane 1, which skipped epoch 1, closes the window over both older epochs at
    // 101. By then every lane passed the cut and it finalized. The fresh instance retires with the lineage it stands
    // beside: its drain, at epoch 1's watermark, 3,700, which lane 0's stamps alone make, runs under the generation it
    // served, as the bound instance's does, at 7,300; draining it under the cut's new generation, which does not bind
    // its lineage, would lose it.
    let trace = Trace::new().install(aggregated("10s", "a"));
    let trace = admit(trace, 0, t(7_300), vec![counter("m", 1.0)]);
    let trace = beat(trace, 1, t(7_300));
    let trace = beat(trace, 0, t(3_700));
    let trace = admit(trace, 0, t(3_700), vec![counter("m", 1.0)]).install(aggregated("5s", "a"));
    let trace = beat(beat(trace, 0, t(100)), 1, t(101));
    let run = run_on(&trace, 2, false);
    assert_eq!(installs(&run), outcomes(&[(0, "active 1"), (5, "cut 2 at 3710000")]));
    assert_eq!(
        received(&run, "a"),
        sorted(&[
            (1, 7_300_000, "m rate/10s [7300:1]"),
            (1, 3_700_000, "m rate/10s [3700:1]"),
        ])
    );
}

#[test]
fn a_retiring_lineage_never_flushes_past_its_cut_though_a_lane_never_stamped() {
    // The cut to 5 seconds is armed at 95 (T = 100) while lane 1 has stamped nothing, so the cut never passes and the
    // old lineage retires only at shutdown. Its data is of the cut's publication epoch, the epoch the cut recorded for
    // both lanes, so its final flush is clamped to the cut: [90, 100) at 100, not at the close time, 130, which a lane
    // that has not stamped yet would otherwise leave unclamped, as no state lane does.
    let trace = Trace::new().install(aggregated("10s", "a"));
    let trace = admit(trace, 0, t(95), vec![counter("c", 1.0)]).install(aggregated("5s", "a"));
    let trace = admit(trace, 0, t(97), vec![counter("c", 1.0)]).advance(t(130)).close();
    let run = run_on(&trace, 2, false);
    assert_eq!(installs(&run), outcomes(&[(0, "active 1"), (2, "cut 2 at 100000")]));
    assert_eq!(received(&run, "a"), sorted(&[(1, 100_000, "c rate/10s [90:2]")]));
}

#[test]
fn a_cut_armed_inside_a_window_retires_the_old_lineage_through_its_newer_epochs_instance() {
    // Trace 11's reset before publication, on two lanes whose windows stay open: lane 0 resets at 3,601 while lane 1 is
    // still in the old epoch, and the cut is armed then, from the source's clock (T = 3,610, both lanes in the new
    // epoch). Lane 0's batch at 3,605 is the old generation's, in the new epoch: it reaches a fresh instance of the old
    // lineage, which keeps the cut's mark. Lane 1's switch closes the window: the old epoch drains at 7,201, unclamped,
    // since the cut was published after it, and the old lineage continues on the fresh instance, which crosses once
    // both lanes stamped the new generation, flushing [3600, 3610) at the cut.
    let trace = Trace::new().install(aggregated("10s", "a"));
    let trace = admit(trace, 0, t(7_201), vec![counter("c", 1.0)]);
    let trace = beat(trace, 1, t(7_201));
    let trace = beat(trace, 0, t(3_601)).install(aggregated("5s", "a"));
    let trace = admit(trace, 0, t(3_605), vec![counter("c", 1.0)]);
    let trace = beat(trace, 1, t(3_606));
    let trace = beat(beat(trace, 0, ms(3_610_500)), 1, ms(3_610_500));
    let run = run_on(&trace, 2, false);
    assert_eq!(installs(&run), outcomes(&[(0, "active 1"), (4, "cut 2 at 3610000")]));
    assert_eq!(
        received(&run, "a"),
        sorted(&[
            (1, 7_201_000, "c rate/10s [7200:1]"),
            (1, 3_610_000, "c rate/10s [3600:1]")
        ])
    );
}

#[test]
fn a_lineage_a_cut_starts_inside_a_window_ticks_only_once_every_lane_switched_to_its_epoch() {
    // The cut to 5 seconds is armed while lane 1 is still in the old epoch (T = 3,610, both lanes recorded in the new
    // epoch), and lane 0 admits `d` to the new lineage at 3,611. The tick at 3,616 finds the new lineage's epoch
    // without lane 1, so it is skipped, though lane 0 has reached 3,616: [3610, 3615) closes only at the tick at 3,621,
    // after lane 1 switched at 3,617 and closed the window, draining the old epoch at 7,201.
    let trace = Trace::new().install(aggregated("10s", "a"));
    let trace = admit(trace, 0, t(7_201), vec![counter("c", 1.0)]);
    let trace = beat(trace, 1, t(7_201));
    let trace = beat(trace, 0, t(3_601)).install(aggregated("5s", "a"));
    let trace = admit(trace, 0, t(3_611), vec![counter("d", 7.0)]);
    let trace = beat(trace, 0, t(3_616)).advance(t(3_616));
    let trace = beat(trace, 1, t(3_617));
    let trace = beat(beat(trace, 0, t(3_621)), 1, t(3_621)).advance(t(3_621));
    let run = run_on(&trace, 2, false);
    assert_eq!(installs(&run), outcomes(&[(0, "active 1"), (4, "cut 2 at 3610000")]));
    assert_eq!(
        received(&run, "a"),
        sorted(&[
            (1, 7_201_000, "c rate/10s [7200:1]"),
            (2, 3_621_000, "d rate/5s [3610:7]")
        ])
    );
}

#[test]
fn shutdown_inside_a_window_closes_it_first_and_closes_the_new_timeline_at_its_own_time() {
    // Lane 1 never switches before input ends: shutdown closes the window first, the old epoch at its watermark so far,
    // 7,185, which is exact since every lane's last stamp arrived, and then closes the new epoch's instance at its own
    // latest time, 3,601, not the old timeline's: its open [3600, 3610) is discarded.
    let trace = Trace::new().install(aggregated("10s", "a"));
    let trace = admit(trace, 0, t(7_185), vec![counter("c", 1.0)]);
    let trace = beat(trace, 1, t(7_195));
    let trace = admit(trace, 0, t(3_601), vec![counter("c", 2.0)]).close();
    let run = run_on(&trace, 2, false);
    assert_eq!(received(&run, "a"), sorted(&[(1, 7_185_000, "c rate/10s [7180:1]")]));
    assert_eq!(run.loss(&"agg".parse().unwrap()).loss(LossClass::Shutdown).points, 1);
}

#[test]
fn fixture_x7_the_bucket_open_at_the_close_time_is_discarded_whichever_lane_stamped_into_it() {
    // One close time for the state owner, the later of every lane's latest stamp and the clock: 7,210.1. Lane 0's
    // [7200, 7210) closes; lane 1's [7210, 7220) is open then, and discarded as shutdown loss.
    let trace = Trace::new().install(aggregated("10s", "a"));
    let trace = admit(trace, 0, ms(7_209_900), vec![counter("c", 1.0)]);
    let trace = admit(trace, 1, ms(7_210_100), vec![counter("d", 2.0)]).close();
    let run = run_on(&trace, 2, false);
    assert_eq!(received(&run, "a"), sorted(&[(1, 7_210_100, "c rate/10s [7200:1]")]));
    assert_eq!(run.loss(&"agg".parse().unwrap()).loss(LossClass::Shutdown).points, 1);
}

#[test]
fn fixture_x5_a_delayed_buffer_from_another_lane_never_moves_a_counters_last_seen_back() {
    // Counter `c` from lane 1 at 100, then a buffer lane 0 stamped at 99.5 that arrives after it. `last_seen` is the
    // latest admission second, 100, so with a 5-minute expiry the idle counter gains a zero up to the bucket at 399,
    // and none at 400; assignment would have made it 99, and the last zero 398.
    let mut params = aggregation("1s");
    params["counter_expiry"] = json!("5m");
    let trace = Trace::new().install(aggregated_with(params, "a", "untimestamped"));
    let trace = admit(trace, 1, t(100), vec![counter("c", 1.0)]);
    let trace = admit(trace, 0, ms(99_500), vec![counter("c", 1.0)])
        .advance(t(101))
        .advance(t(400))
        .advance(t(401));
    let run = run_on(&trace, 2, true);
    let zeros = (101..=399).map(|second| format!("{}:0", second)).collect::<Vec<_>>();
    let idle = format!("c rate/1s [{}]", zeros.join(", "));
    assert_eq!(
        received(&run, "a"),
        sorted(&[(1, 101_000, "c rate/1s [99:1, 100:1]"), (1, 400_000, idle.as_str())])
    );
}

#[test]
fn fixture_x1_an_idle_counter_new_to_the_lineage_gains_zeros_from_the_lineages_last_flush() {
    // The one core's expectation of X1's closing case, which every lane's shared flush log must reproduce: 10-second
    // buckets, 15-second ticks. `a` is flushed at 15, 30, and 45; `g` is held only until the flush at 15; `b` arrives
    // at 47. The cut to 30 seconds at T = 60 retires the lineage: its final flush starts `b`'s zeros at align(45) = 40,
    // so `b` gains one zero, at 50, never at 10, 20, or 30.
    let trace = Trace::new()
        .install(aggregated("10s", "a"))
        .admit(src(), t(5), vec![counter("a", 1.0), gauge("g", 1.0)])
        .advance(t(15))
        .advance(t(30))
        .advance(t(45))
        .admit(src(), t(47), vec![counter("b", 1.0)])
        .install(aggregated("30s", "a"))
        .heartbeat(src(), ms(60_500));
    let run = run_on(&trace, 1, true);
    assert_eq!(installs(&run), outcomes(&[(0, "active 1"), (6, "cut 2 at 60000")]));
    assert_eq!(
        received(&run, "a"),
        sorted(&[
            (1, 15_000, "a rate/10s [0:1]"),
            (1, 15_000, "g gauge [0:1]"),
            (1, 30_000, "a rate/10s [10:0, 20:0]"),
            (1, 45_000, "a rate/10s [30:0]"),
            (1, 60_000, "a rate/10s [40:0, 50:0]"),
            (1, 60_000, "b rate/10s [40:1, 50:0]"),
        ])
    );
}

#[test]
fn fixture_x6_contexts_a_tag_projection_merges_are_one_series_at_any_lane_count() {
    // `x{env:a,pod:1}` and `x{env:a,pod:2}` from two lanes, `pod` stripped before aggregation: one series, one point
    // per bucket, whichever lanes the two arrived on (not the duplicate a per-pipeline aggregator would produce).
    let registry = OperatorRegistry::native();
    let catalog = catalog();
    let plan = Plan::from_value(json!({
        "ir_version": 1,
        "nodes": [
            node("src", "source", json!({"binding": "dogstatsd", "timestamps": "untimestamped"})),
            node("strip", "tag_projection", json!({
                "rules": [{"metric_name": "x", "action": "exclude", "tags": ["pod"]}],
                "value_rules": [],
                "cache_capacity": 1000,
            })),
            node("agg", "aggregate", aggregation("10s")),
            node("out", "emit", json!({"output": "a"})),
        ],
        "edges": [
            {"from": "src.metrics", "to": "strip.in"},
            {"from": "strip.out", "to": "agg.in"},
            {"from": "agg.out", "to": "out.in"},
        ],
        "outputs": [{"id": "a", "accepts": {"kinds": "all", "timestamps": "mixed"},
                     "fingerprint": super::transition_tests::fingerprint("a").to_string(), "recipients": ["out/a"]}],
    }))
    .unwrap();
    let report = Validator::new(&registry, &catalog).validate(&plan);
    assert!(report.is_valid(), "the fixture plan is invalid:\n{}", report);
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
    for lanes in [1u16, 2, 4] {
        let trace = Trace::new()
            .install(plan.clone())
            .admit_at_lane(src(), 0, t(101), vec![pod("1")])
            .admit_at_lane(src(), lanes - 1, t(102), vec![pod("2")])
            .advance(t(111));
        let run = Interpreter::new(&registry, &catalog)
            .with_source_lanes(src(), lanes)
            .with_heartbeats_at_ticks()
            .run(&trace)
            .unwrap();
        let records = run.output(&OutputId::new("a").unwrap());
        assert_eq!(records.len(), 1, "{} lanes: {:?}", lanes, records);
        let tags = records[0]
            .metric
            .context()
            .tags()
            .into_iter()
            .map(|tag| tag.as_str().to_string())
            .collect::<Vec<_>>();
        assert_eq!(tags, vec!["env:a".to_string()], "{} lanes", lanes);
        assert_eq!(describe(&records[0].metric), "x rate/10s [100:2]", "{} lanes", lanes);
    }
}

/// `src -> agg -> rename -> emit a`, where `rename` prefixes every name with `p.` after aggregation, for candidates
/// that travel through a kernel downstream of the state owner.
fn renamed_after_aggregation() -> (Plan, OperatorRegistry) {
    let registry = test_registry();
    let plan = Plan::from_value(json!({
        "ir_version": 1,
        "nodes": [
            node("src", "source", json!({"binding": "dogstatsd", "timestamps": "untimestamped"})),
            node("agg", "aggregate", aggregation("10s")),
            node("rename", "test_prefix", json!({"prefix": "p."})),
            node("out", "emit", json!({"output": "a"})),
        ],
        "edges": [
            {"from": "src.metrics", "to": "agg.in"},
            {"from": "agg.out", "to": "rename.in"},
            {"from": "rename.out", "to": "out.in"},
        ],
        "outputs": [{"id": "a", "accepts": {"kinds": "all", "timestamps": "mixed"},
                     "fingerprint": super::transition_tests::fingerprint("a").to_string(), "recipients": ["out/a"]}],
    }))
    .unwrap();
    let report = Validator::new(&registry, &catalog()).validate(&plan);
    assert!(report.is_valid(), "the fixture plan is invalid:\n{}", report);
    (plan, registry)
}

fn gauge(name: &'static str, value: f64) -> Metric {
    Metric::gauge(Context::from_static_name(name), value)
}

#[test]
fn fixture_x4_candidate_mode_records_each_lanes_last_gauge_value_in_the_bucket() {
    // Gauge `g` in [100, 110): lane 0 admits 1 and then 2, lane 1 admits 3. The interpreter delivers the last arrival,
    // 3; the candidates are each lane's last value, 2 and 3, carried through the rename after aggregation to the
    // output's `p.g`. A partitioned execution may deliver either; never lane 0's first value, 1.
    let (plan, registry) = renamed_after_aggregation();
    let catalog = catalog();
    let trace = Trace::new()
        .install(plan)
        .admit_at_lane(src(), 0, t(101), vec![gauge("g", 1.0)])
        .admit_at_lane(src(), 0, t(102), vec![gauge("g", 2.0)])
        .admit_at_lane(src(), 1, t(103), vec![gauge("g", 3.0)])
        .advance(t(111));
    let run = Interpreter::new(&registry, &catalog)
        .with_source_lanes(src(), 2)
        .with_heartbeats_at_ticks()
        .with_candidates()
        .run(&trace)
        .unwrap();
    let output = OutputId::new("a").unwrap();
    let records = run.output(&output);
    assert_eq!(records.len(), 1);
    assert_eq!(describe(&records[0].metric), "p.g gauge [100:3]");

    let candidates = run.candidates();
    let buckets = candidates.buckets().collect::<Vec<_>>();
    assert_eq!(buckets.len(), 1, "{:?}", buckets);
    let ((at, context, timestamp), bucket) = buckets[0];
    assert_eq!(at, &output);
    assert_eq!(context.name, "p.g", "the output's own view of the series");
    assert_eq!(timestamp.map(|timestamp| timestamp.get()), Some(B + 100));
    let lanes = bucket
        .gauges()
        .iter()
        .map(|(point, value)| (*point, value.get()))
        .collect::<Vec<_>>();
    assert_eq!(
        lanes,
        vec![(AdmissionPointId::new(0), 2.0), (AdmissionPointId::new(1), 3.0)]
    );
    assert_eq!(bucket.kinds.iter().copied().collect::<Vec<_>>(), vec![PointKind::Gauge]);
    assert!(!bucket.has_kind_conflict());

    let expected = OutputPoints::from_run(&run);
    let delivering = |value: f64| {
        let mut metric = records[0].metric.clone();
        *metric.values_mut() = MetricValues::gauge((B + 100, value));
        OutputPoints::new().with(&output, [&metric])
    };
    let tolerance = Tolerance::from_limits();
    for (value, equivalent) in [(3.0, true), (2.0, true), (1.0, false), (4.0, false)] {
        let verdict = compare_partitioned(&expected, &delivering(value), candidates, tolerance);
        assert_eq!(
            verdict == ReorderedVerdict::Equivalent,
            equivalent,
            "{}: {:?}",
            value,
            verdict
        );
    }
}

#[test]
fn candidate_mode_records_every_kind_and_metadata_an_entry_was_admitted_with() {
    // `m` arrives as a gauge with unit `second` from lane 0 and as a counter with unit `byte` from lane 1: the entry's
    // kind and first metadata depend on which arrived last and first, so both are candidates, each kind as aggregation
    // emits it, the counter as a rate over the bucket width.
    let (plan, registry) = renamed_after_aggregation();
    let catalog = catalog();
    let with_unit = |values: MetricValues, unit: &'static str| {
        Metric::from_parts(
            Context::from_static_name("m"),
            values,
            MetricMetadata::default().with_unit(MetaString::from_static(unit)),
        )
    };
    let trace = Trace::new()
        .install(plan)
        .admit_at_lane(src(), 0, t(101), vec![with_unit(MetricValues::gauge(1.0), "second")])
        .admit_at_lane(src(), 1, t(102), vec![with_unit(MetricValues::counter(4.0), "byte")])
        .advance(t(111));
    let run = Interpreter::new(&registry, &catalog)
        .with_source_lanes(src(), 2)
        .with_heartbeats_at_ticks()
        .with_candidates()
        .run(&trace)
        .unwrap();
    let buckets = run.candidates().buckets().collect::<Vec<_>>();
    assert_eq!(buckets.len(), 1, "{:?}", buckets);
    let bucket = buckets[0].1;
    assert!(bucket.has_kind_conflict());
    let rate = PointKind::Rate {
        interval: Duration::from_secs(10),
    };
    assert_eq!(
        bucket.kinds.iter().copied().collect::<Vec<_>>(),
        vec![rate, PointKind::Gauge]
    );
    // Each lane's last admission into the bucket: the kinds the bucket may hold.
    assert_eq!(
        bucket.emitted_kinds().into_iter().collect::<Vec<_>>(),
        vec![rate, PointKind::Gauge]
    );
    let order = |lane: u32| bucket.last[&AdmissionPointId::new(lane)].order;
    assert!(order(0) < order(1), "lane 1's admission came later");
    let units = bucket
        .metadata
        .iter()
        .map(|metadata| metadata.unit.clone())
        .collect::<Vec<_>>();
    assert_eq!(units, vec!["byte".to_string(), "second".to_string()]);

    // Without candidate mode nothing is recorded.
    let plain = Interpreter::new(&registry, &catalog)
        .with_source_lanes(src(), 2)
        .with_heartbeats_at_ticks()
        .run(&trace)
        .unwrap();
    assert!(plain.candidates().is_empty());
}

#[test]
fn a_kind_change_within_one_lanes_bucket_is_candidate_for_that_lanes_last_admission_only() {
    // Only lane 0 sends `c`, a counter at 101 and then a gauge of 4 at 102, into one bucket: the gauge replaced the
    // counter, and every interleaving keeps lane 0's order, so the bucket's only candidate is the gauge. A partitioned
    // run that delivered a rate there, a point at a timestamp nothing was admitted at, or nothing, differs.
    let (plan, registry) = renamed_after_aggregation();
    let catalog = catalog();
    let trace = Trace::new()
        .install(plan)
        .admit_at_lane(src(), 0, t(101), vec![counter("c", 1.0)])
        .admit_at_lane(src(), 0, t(102), vec![gauge("c", 4.0)])
        .heartbeat_at_lane(src(), 1, t(102))
        .advance(t(111));
    let run = Interpreter::new(&registry, &catalog)
        .with_source_lanes(src(), 2)
        .with_heartbeats_at_ticks()
        .with_candidates()
        .run(&trace)
        .unwrap();
    let output = OutputId::new("a").unwrap();
    let records = run.output(&output);
    assert_eq!(records.len(), 1);
    assert_eq!(describe(&records[0].metric), "p.c gauge [100:4]");
    let bucket = run
        .candidates()
        .buckets()
        .next()
        .map(|(_, bucket)| bucket.clone())
        .unwrap();
    assert!(bucket.has_kind_conflict());
    assert_eq!(
        bucket.emitted_kinds().into_iter().collect::<Vec<_>>(),
        vec![PointKind::Gauge]
    );

    let expected = OutputPoints::from_run(&run);
    let delivering = |values: MetricValues| {
        let mut metric = records[0].metric.clone();
        *metric.values_mut() = values;
        OutputPoints::new().with(&output, [&metric])
    };
    let rate = |timestamp: u64, value: f64| {
        MetricValues::rate(
            saluki_core::data_model::event::metric::ScalarPoints::from((timestamp, value)),
            Duration::from_secs(10),
        )
    };
    let tolerance = Tolerance::from_limits();
    let verdict = |points: &OutputPoints| compare_partitioned(&expected, points, run.candidates(), tolerance);
    assert_eq!(
        verdict(&delivering(MetricValues::gauge((B + 100, 4.0)))),
        ReorderedVerdict::Equivalent
    );
    for actual in [
        delivering(rate(B + 100, 1.0)),
        delivering(rate(100, 1_000.0)),
        OutputPoints::new(),
    ] {
        assert!(
            matches!(verdict(&actual), ReorderedVerdict::NotEquivalent(_)),
            "{:?}",
            actual.points(&output).collect::<Vec<_>>()
        );
    }
}

#[test]
fn candidate_mode_records_a_bucket_whose_points_a_kind_replaced_before_any_flush() {
    // Lane 0 admits `m` as a counter into [100, 110) at 105, then lane 1 as a gauge into [110, 120) at 111, before any
    // tick: the gauge replaced the counter, so the interpreter delivers only the gauge. A partitioned run in which the
    // counter arrived last delivers only the counter's rate, which some interleaving does: its bucket has candidates,
    // though the reference emitted nothing there.
    let (plan, registry) = renamed_after_aggregation();
    let catalog = catalog();
    let trace = Trace::new()
        .install(plan)
        .admit_at_lane(src(), 0, t(105), vec![counter("m", 1.0)])
        .admit_at_lane(src(), 1, t(111), vec![gauge("m", 3.0)])
        .heartbeat_at_lane(src(), 0, t(121))
        .heartbeat_at_lane(src(), 1, t(121))
        .advance(t(121));
    let run = Interpreter::new(&registry, &catalog)
        .with_source_lanes(src(), 2)
        .with_candidates()
        .run(&trace)
        .unwrap();
    let output = OutputId::new("a").unwrap();
    let records = run.output(&output);
    assert_eq!(
        records
            .iter()
            .map(|record| describe(&record.metric))
            .collect::<Vec<_>>(),
        vec!["p.m gauge [110:3]".to_string()]
    );
    let timestamps = run
        .candidates()
        .buckets()
        .map(|((_, _, timestamp), _)| timestamp.map_or(0, |timestamp| timestamp.get() - B))
        .collect::<Vec<_>>();
    assert_eq!(timestamps, vec![100, 110]);

    let expected = OutputPoints::from_run(&run);
    let mut counted = records[0].metric.clone();
    *counted.values_mut() = MetricValues::rate(
        saluki_core::data_model::event::metric::ScalarPoints::from((B + 100, 1.0)),
        Duration::from_secs(10),
    );
    let tolerance = Tolerance::from_limits();
    assert_eq!(
        compare_partitioned(
            &expected,
            &OutputPoints::new().with(&output, [&counted]),
            run.candidates(),
            tolerance
        ),
        ReorderedVerdict::Equivalent
    );
    assert!(matches!(
        compare_partitioned(&expected, &OutputPoints::new(), run.candidates(), tolerance),
        ReorderedVerdict::NotEquivalent(_)
    ));
}

#[test]
fn trace_l7_a_revocation_cancels_a_cut_armed_over_every_lane_until_one_lane_stamps_the_new_generation() {
    // D = 4. While the cut is armed and no lane has stamped its generation, the cancel restores the old generation for
    // every lane; once lane 3 has stamped it, the cancel is refused.
    let armed = || {
        let trace = Trace::new().install(aggregated("10s", "a"));
        let trace = (0..4).fold(trace, |trace, lane| {
            trace.admit_at_lane(src(), lane, t(95 + u64::from(lane)), vec![counter("c", 1.0)])
        });
        trace.install(aggregated("5s", "a"))
    };
    let cancelled = run_on(&armed().cancel().advance(t(101)), 4, true);
    assert_eq!(
        installs(&cancelled),
        outcomes(&[(0, "active 1"), (5, "cut 2 at 100000"), (6, "cancelled")])
    );
    assert_eq!(received(&cancelled, "a"), sorted(&[(1, 101_000, "c rate/10s [90:4]")]));

    let refused = run_on(&armed().heartbeat_at_lane(src(), 3, ms(100_500)).cancel(), 4, false);
    assert_eq!(
        installs(&refused),
        outcomes(&[(0, "active 1"), (5, "cut 2 at 100000"), (7, "cancel refused")])
    );
}
