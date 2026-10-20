//! Traces 16 to 19 of version 2's V7a: a state owner over several sources, a second aggregation after the first, a
//! chain of aggregations in steady state, and cuts through a chain, run through the interpreter, whose rules the frame
//! harness replays.
//!
//! The fixtures and conventions are the transition traces' ([`super::transition_tests`]): times relative to `B`, in
//! milliseconds in expectations, a cut margin of 1 second. A state owner's output is a derived point of its own, which
//! the state owners downstream observe: they admit its batches before the ticks its stamps release.

use saluki_core::{
    data_model::event::metric::{context::Context, Metric},
    execution::GenerationId,
};
use serde_json::json;

use super::transition_tests::{
    aggregation, catalog, counter, delivered, dogstatsd, expected, gauge_at, id, installs, ms, node, outcomes, plan,
    run_queued, run_with_heartbeats, src, t, two_sources,
};
use crate::{
    interpret::{Interpreter, Trace},
    ir::Plan,
    operators::OperatorRegistry,
};

/// An untimestamped gauge.
fn gauge(name: &'static str, value: f64) -> Metric {
    Metric::gauge(Context::from_static_name(name), value)
}

/// `src -> first -> second -> emit` to `a`: an aggregation over `first` whose output a second aggregation, over
/// `second`, aggregates again.
fn chain(first: &str, second: &str) -> Plan {
    plan(
        vec![
            dogstatsd("untimestamped"),
            node("first", "aggregate", aggregation(first)),
            node("second", "aggregate", aggregation(second)),
            node("out", "emit", json!({"output": "a"})),
        ],
        &[
            "src.metrics -> first.in",
            "first.out -> second.in",
            "second.out -> out.in",
        ],
        &["a"],
    )
}

/// The default program's shape, `dsd -> dsd_agg -> joined <- otlp`, emitting everything `joined` carries to `a`, and a
/// copy of it to `b`: with `again`, aggregated a second time, over a minute, or otherwise as it is.
fn aggregated_after(again: bool) -> Plan {
    let mut nodes = vec![
        node(
            "dsd",
            "source",
            json!({"binding": "dogstatsd", "timestamps": "untimestamped"}),
        ),
        node("otlp", "source", json!({"binding": "otlp"})),
        node("dsd_agg", "aggregate", aggregation("10s")),
        node("joined", "merge", json!({"inputs": 2})),
        node("t", "tee", json!({"ports": 2})),
        node("out_a", "emit", json!({"output": "a"})),
        node("out_b", "emit", json!({"output": "b"})),
    ];
    let mut edges = vec![
        "dsd.metrics -> dsd_agg.in",
        "dsd_agg.out -> joined.0",
        "otlp.metrics -> joined.1",
        "joined.out -> t.in",
        "t.0 -> out_a.in",
    ];
    if again {
        nodes.push(node("again", "aggregate", aggregation("1m")));
        edges.extend(["t.1 -> again.in", "again.out -> out_b.in"]);
    } else {
        edges.push("t.1 -> out_b.in");
    }
    plan(nodes, &edges, &["a", "b"])
}

#[test]
fn trace_16_a_state_owner_over_two_sources_flushes_at_the_earliest_and_drains_once_per_reset_source() {
    let registry = OperatorRegistry::native();
    let catalog = catalog();
    let run = |trace: Trace| Interpreter::new(&registry, &catalog).run(&trace).unwrap();

    // The watermark is the minimum: the tick at 101 waits at the gate while `otlp` is at 95, `dsd`'s heartbeat at 101
    // leaves it there, and `otlp`'s heartbeat at 101 releases it at its second.
    let minimum = run(Trace::new()
        .install(two_sources("10s"))
        .admit(id("dsd"), t(95), vec![counter("c", 1.0)])
        .heartbeat(id("otlp"), t(95))
        .advance(t(101))
        .heartbeat(id("dsd"), t(101)));
    assert_eq!(delivered(&minimum, "a"), expected(&[]));
    assert_eq!(minimum.watermark(&id("agg")), Some(t(95)));
    let released = run(Trace::new()
        .install(two_sources("10s"))
        .admit(id("dsd"), t(95), vec![counter("c", 1.0)])
        .heartbeat(id("otlp"), t(95))
        .advance(t(101))
        .heartbeat(id("dsd"), t(101))
        .heartbeat(id("otlp"), t(101)));
    assert_eq!(
        delivered(&released, "a"),
        expected(&[(1, 101_000, "c rate/10s [90:1]")])
    );

    // An idle source heartbeating every second keeps the ticks on time: each fires at its second, within the grace.
    let idle = run(Trace::new()
        .install(two_sources("10s"))
        .admit(id("dsd"), ms(99_500), vec![counter("c", 1.0)])
        .heartbeat(id("otlp"), t(100))
        .advance(t(101))
        .heartbeat(id("dsd"), t(101))
        .heartbeat(id("otlp"), t(101))
        .admit(id("dsd"), ms(110_500), vec![counter("c", 2.0)])
        .heartbeat(id("otlp"), t(111))
        .advance(t(111))
        .heartbeat(id("dsd"), t(111)));
    assert_eq!(
        delivered(&idle, "a"),
        expected(&[(1, 101_000, "c rate/10s [90:1]"), (1, 111_000, "c rate/10s [100:0]")])
    );

    // A reset at one source drains everything the state owner holds, at the watermark before it, and starts its next
    // epoch, which the other source's later reset ends with a second drain.
    let resets = run(Trace::new()
        .install(two_sources("10s"))
        .admit(id("dsd"), t(500), vec![counter("c", 1.0)])
        .heartbeat(id("otlp"), t(500))
        .admit(id("dsd"), t(95), vec![counter("x", 1.0)])
        .heartbeat(id("otlp"), t(96)));
    assert_eq!(
        delivered(&resets, "a"),
        expected(&[(1, 500_000, "c rate/10s [500:1]"), (1, 95_000, "x rate/10s [90:1]")])
    );
}

#[test]
fn trace_17_a_second_aggregation_after_the_first_follows_every_source_and_the_first_ones_output() {
    // `again` is over `otlp`'s point and `dsd_agg`'s derived point, not `dsd`'s. Every point `dsd` admits reaches `b`
    // once, through both aggregations, and every point `otlp` admits once, through `again`.
    // A flush emits its contexts in no particular order, so this runs once, and sorts what each output received.
    let registry = OperatorRegistry::native();
    let catalog = catalog();
    let run = Interpreter::new(&registry, &catalog)
        .with_heartbeats_at_ticks()
        .run(
            &Trace::new()
                .install(aggregated_after(true))
                .admit(id("dsd"), t(5), vec![gauge("g", 1.0)])
                .admit(id("otlp"), t(6), vec![gauge_at("o", 6, 10.0)])
                .advance(t(11))
                .admit(id("dsd"), t(25), vec![gauge("g", 2.0)])
                .advance(t(31))
                .admit(id("otlp"), t(45), vec![gauge_at("o", 45, 20.0)])
                .admit(id("dsd"), t(55), vec![gauge("g", 4.0)])
                .advance(t(51))
                .advance(t(61))
                .advance(t(71)),
        )
        .unwrap();
    let mut a = delivered(&run, "a");
    a.sort();
    assert_eq!(
        a,
        expected(&[
            (1, 6_000, "o gauge [6:10]"),
            (1, 11_000, "g gauge [0:1]"),
            (1, 31_000, "g gauge [20:2]"),
            (1, 45_000, "o gauge [45:20]"),
            (1, 61_000, "g gauge [50:4]"),
        ])
    );
    let mut b = delivered(&run, "b");
    b.sort();
    assert_eq!(
        b,
        expected(&[
            (1, 61_000, "g gauge [0:1, 20:2, 50:4]"),
            (1, 61_000, "o gauge [6:10, 45:20]")
        ])
    );
    assert_eq!(run.watermark(&id("again")), Some(t(71)));
}

#[test]
fn trace_18_a_chain_in_steady_state_flushes_every_contribution_of_the_first_in_the_seconds_bucket() {
    // With every source heartbeating at each tick, `second`'s tick at 61 waits for `first`'s at 61, admits its output,
    // the bucket [50, 60), and then flushes [0, 60) with every one of `first`'s buckets in it.
    let run = run_with_heartbeats(
        Trace::new()
            .install(chain("10s", "1m"))
            .admit(src(), t(1), vec![gauge("g", 1.0)])
            .advance(t(11))
            .admit(src(), t(15), vec![gauge("g", 2.0)])
            .advance(t(21))
            .advance(t(31))
            .advance(t(41))
            .admit(src(), t(55), vec![gauge("g", 4.0)])
            .advance(t(51))
            .advance(t(61))
            .advance(t(71))
            .close(),
    );
    assert_eq!(
        delivered(&run, "a"),
        expected(&[(1, 61_000, "g gauge [0:1, 10:2, 50:4]")])
    );

    // Without heartbeats at ticks, `second`'s first tick is skipped, its watermark unknown until `first`'s progress
    // at 61, and its tick at 71 waits at the gate for `first`'s, which the stamp at 71.5 releases: `second` admits
    // `first`'s output first.
    let run = run_queued(
        Trace::new()
            .install(chain("10s", "1m"))
            .admit(src(), t(55), vec![gauge("g", 1.0)])
            .heartbeat(src(), t(61))
            .advance(t(61))
            .admit(src(), t(65), vec![gauge("g", 2.0)])
            .advance(t(71))
            .heartbeat(src(), ms(71_500)),
    );
    assert_eq!(delivered(&run, "a"), expected(&[(1, 71_000, "g gauge [50:1]")]));
    assert_eq!(run.watermark(&id("second")), Some(t(71)));
}

/// Runs the cut from `chain(old)` to `chain(new)`, installed with the latest stamp at 98.5, with gauges admitted on
/// both sides of the cut and every source heartbeating at each tick.
fn cut_through_chain(old: (&str, &str), new: (&str, &str)) -> super::RunOutput {
    run_with_heartbeats(
        Trace::new()
            .install(chain(old.0, old.1))
            .admit(src(), t(85), vec![gauge("g", 1.0)])
            .advance(t(91))
            .admit(src(), t(95), vec![gauge("g", 2.0)])
            .heartbeat(src(), ms(98_500))
            .install(chain(new.0, new.1))
            .advance(t(101))
            .admit(src(), t(105), vec![gauge("g", 4.0)])
            .advance(t(111))
            .admit(src(), t(115), vec![gauge("g", 8.0)])
            .advance(t(121))
            .admit(src(), t(125), vec![gauge("g", 16.0)])
            .advance(t(131))
            .advance(t(151))
            .advance(t(181))
            .close(),
    )
}

#[test]
fn trace_19_a_cut_through_a_chain_delivers_every_point_once_under_the_expected_generation() {
    // Both cut, T = 120: `first`'s old lineage makes its final flush at 120 under generation 1, which `second`'s old
    // lineage admits; it crosses at `first`'s first stamp of generation 2, the progress of its tick at 121, and makes
    // its own final flush at 120.
    let both = cut_through_chain(("10s", "30s"), ("5s", "1m"));
    assert_eq!(installs(&both), outcomes(&[(0, "active 1"), (5, "cut 2 at 120000")]));
    assert_eq!(
        delivered(&both, "a"),
        expected(&[
            (1, 101_000, "g gauge [80:1]"),
            (1, 120_000, "g gauge [90:2, 100:4, 110:8]"),
            (2, 181_000, "g gauge [125:16]"),
        ])
    );

    // The upstream cut only, T = 100: `second` keeps its lineage, and buckets by timestamp what `first`'s old lineage
    // sent before the cut and its new one after.
    let upstream = cut_through_chain(("10s", "1m"), ("5s", "1m"));
    assert_eq!(
        installs(&upstream),
        outcomes(&[(0, "active 1"), (5, "cut 2 at 100000")])
    );
    assert_eq!(
        delivered(&upstream, "a"),
        expected(&[
            (2, 121_000, "g gauge [80:1, 90:2, 105:4, 115:8]"),
            (2, 181_000, "g gauge [125:16]"),
        ])
    );

    // The downstream cut only, T = 120, which lines up with `first`'s width too: `first` keeps its lineage and makes a
    // boundary flush at 120 under generation 1 once its watermark reaches it, so that its bucket [110, 120) reaches
    // `second`'s old lineage, not its tick at 121, under generation 2.
    let downstream = cut_through_chain(("10s", "30s"), ("10s", "1m"));
    assert_eq!(
        installs(&downstream),
        outcomes(&[(0, "active 1"), (5, "cut 2 at 120000")])
    );
    assert_eq!(
        delivered(&downstream, "a"),
        expected(&[
            (1, 101_000, "g gauge [80:1]"),
            (1, 120_000, "g gauge [90:2, 100:4, 110:8]"),
            (2, 181_000, "g gauge [120:16]"),
        ])
    );
}

#[test]
fn trace_17_without_a_tick_after_the_cut_a_second_aggregation_retires_the_generation_it_replaces() {
    // `again` starts at a cut at 60, which `dsd_agg` owes a boundary flush at 60 under generation 1. No tick follows
    // the cut, as with clocked aggregation's hour-long flush interval: the boundary flush, which the stamps at 65 make
    // due, is `dsd_agg`'s last drain under generation 1, and generation 1 retires once it is out. What `dsd` admitted
    // before 60 reaches `b` under generation 1, through `dsd_agg`'s flushes, never through `again`.
    let registry = OperatorRegistry::native();
    let catalog = catalog();
    let run = Interpreter::new(&registry, &catalog)
        .run(
            &Trace::new()
                .install(aggregated_after(false))
                .admit(id("dsd"), t(5), vec![gauge("g", 1.0)])
                .admit(id("otlp"), t(6), vec![gauge_at("o", 6, 10.0)])
                .advance(t(11))
                .install(aggregated_after(true))
                .admit(id("dsd"), t(55), vec![gauge("g", 2.0)])
                .heartbeat(id("otlp"), t(65))
                .heartbeat(id("dsd"), t(65)),
        )
        .unwrap();
    assert_eq!(installs(&run), outcomes(&[(0, "active 1"), (4, "cut 2 at 60000")]));
    assert_eq!(
        delivered(&run, "b"),
        expected(&[
            (1, 6_000, "o gauge [6:10]"),
            (1, 11_000, "g gauge [0:1]"),
            (1, 60_000, "g gauge [50:2]"),
        ])
    );
    assert_eq!(run.live(), [GenerationId::new(2)]);
}

#[test]
fn trace_17_a_second_aggregation_a_later_program_drops_crosses_at_the_first_ones_boundary_flush() {
    // `again` starts at a cut at 60, and a later program drops it at a cut at 120, which `dsd_agg` owes a boundary
    // flush at 120 under generation 2. The new program no longer routes `dsd_agg`'s output to `again`, and no tick of
    // `dsd_agg` follows: its boundary flush is its last drain under generation 2, after which it stamps generation 3's
    // progress at once, at 120. `again`'s old lineage, which `otlp` already switched, crosses at it, and makes its final
    // flush at 120 with everything both points sent it; generation 2 then retires.
    let registry = OperatorRegistry::native();
    let catalog = catalog();
    let run = Interpreter::new(&registry, &catalog)
        .run(
            &Trace::new()
                .install(aggregated_after(false))
                .admit(id("dsd"), t(5), vec![gauge("g", 1.0)])
                .heartbeat(id("dsd"), t(11))
                .advance(t(11))
                .install(aggregated_after(true))
                .admit(id("otlp"), t(65), vec![gauge_at("o", 65, 40.0)])
                .admit(id("dsd"), t(66), vec![gauge("g", 8.0)])
                .advance(t(71))
                .install(aggregated_after(false))
                .admit(id("dsd"), t(115), vec![gauge("g", 16.0)])
                .heartbeat(id("otlp"), t(125))
                .heartbeat(id("dsd"), t(125)),
        )
        .unwrap();
    assert_eq!(
        installs(&run),
        outcomes(&[(0, "active 1"), (4, "cut 2 at 60000"), (8, "cut 3 at 120000")])
    );
    assert_eq!(
        delivered(&run, "b"),
        expected(&[
            (1, 11_000, "g gauge [0:1]"),
            (2, 120_000, "o gauge [65:40]"),
            (2, 120_000, "g gauge [60:8, 110:16]"),
        ])
    );
    assert_eq!(run.live(), [GenerationId::new(3)]);
}
