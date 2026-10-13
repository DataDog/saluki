//! The specification's normative transition traces, and the stage 6 design's, run through the interpreter with
//! aggregation as a stateful kernel behind an isolation fence.
//!
//! Each trace is written over plans `source -> aggregate -> emit`, with a 10-second window unless stated, and each
//! expectation is written by hand from the transition protocol: which generation every record is delivered under, at
//! which flush time (its `t_a`), and with which points. Times are relative to `B`, a multiple of every width used, in
//! milliseconds where a trace needs fractions of a second. The cut margin is 1 second, so a cut armed when the latest
//! stamp is 98.5 is at 100 for any widths whose least common multiple divides 100.
//!
//! The interpreter has no queues: every stamp reaches every state owner at once, so a cut's crossing (the first
//! stamp of the new generation reaching the retiring lineage) is its finalization too, with one upstream point. The
//! traces whose design form holds batches back in a queue are written in the form that remains; the frame harness
//! runs their queued forms.

use saluki_core::{
    data_model::event::metric::{context::Context, Metric, MetricMetadata, MetricValues},
    execution::{AdmissionTime, LossClass, LossCount},
};
use serde_json::{json, Value};
use stringtheory::MetaString;

use crate::{
    canonical::{PlanHash, MAX_SAFE_INTEGER},
    interpret::{InstallOutcome, InstallRejection, InterpretErrorKind, Interpreter, RunOutput, Trace},
    ir::{NodeId, OutputId, ParamType, Plan, RecipientId},
    model::PortType,
    operators::OperatorRegistry,
    validate::{CatalogOutput, Liveness, MemoryCatalog, Validator},
};

/// The base time: a multiple of 300, and so of every allowed width.
pub(super) const B: u64 = 1_700_000_100;

/// Returns the time `relative_millis` milliseconds after `B`.
pub(super) fn ms(relative_millis: u64) -> AdmissionTime {
    AdmissionTime::from_unix_nanos((B * 1_000 + relative_millis) * 1_000_000)
}

/// Returns the time `relative` seconds after `B`.
pub(super) fn t(relative: u64) -> AdmissionTime {
    ms(relative * 1_000)
}

pub(super) fn id(value: &str) -> NodeId {
    value.parse().unwrap()
}

pub(super) fn src() -> NodeId {
    id("src")
}

pub(super) fn fingerprint(output: &str) -> PlanHash {
    PlanHash::of_bytes(output.as_bytes())
}

pub(super) fn catalog() -> MemoryCatalog {
    ["a", "b"].into_iter().fold(MemoryCatalog::new(), |catalog, output| {
        catalog.with_output(
            OutputId::new(output).unwrap(),
            CatalogOutput {
                accepts: PortType::ANY,
                fingerprint: fingerprint(output),
                recipients: vec![RecipientId::new(format!("out/{}", output)).unwrap()],
                legacy_fingerprint: None,
            },
        )
    })
}

/// The parameters of an aggregation with the given window and native defaults otherwise.
pub(super) fn aggregation(window: &str) -> Value {
    json!({
        "window": window, "flush_interval": "15s", "context_limit": 1000, "counter_expiry": "5m",
        "flush_open_windows": false, "histogram_aggregates": ["max", "count"], "histogram_percentiles": [],
        "copy_to_distribution": false, "copy_to_distribution_prefix": "",
    })
}

/// The parameters of an aggregation over `window` with the given context limit, as a re-composition after a native
/// change of the limit has them.
fn aggregation_limited(window: &str, context_limit: u64) -> Value {
    let mut params = aggregation(window);
    params["context_limit"] = json!(context_limit);
    params
}

pub(super) fn node(id: &str, op: &str, params: Value) -> Value {
    json!({"id": id, "op": op, "params": params, "provenance": {"native": id}})
}

pub(super) fn output_decls(outputs: &[&str]) -> Value {
    Value::Array(
        outputs
            .iter()
            .map(|output| {
                json!({"id": output, "accepts": {"kinds": "all", "timestamps": "mixed"},
                       "fingerprint": fingerprint(output).to_string(), "recipients": [format!("out/{}", output)]})
            })
            .collect(),
    )
}

/// Builds a plan from nodes, edges written as `from -> to`, and the outputs it declares, without validating it.
pub(super) fn unvalidated_plan(nodes: Vec<Value>, edges: &[&str], outputs: &[&str]) -> Plan {
    let edges = edges
        .iter()
        .map(|edge| {
            let (from, to) = edge.split_once(" -> ").unwrap();
            json!({"from": from, "to": to})
        })
        .collect::<Vec<_>>();
    Plan::from_value(json!({
        "ir_version": 1, "nodes": nodes, "edges": edges, "outputs": output_decls(outputs),
    }))
    .unwrap()
}

/// Builds a valid plan from nodes, edges written as `from -> to`, and the outputs it declares.
pub(super) fn plan(nodes: Vec<Value>, edges: &[&str], outputs: &[&str]) -> Plan {
    let plan = unvalidated_plan(nodes, edges, outputs);
    let report = Validator::new(&OperatorRegistry::native(), &catalog()).validate(&plan);
    assert!(report.is_valid(), "the fixture plan is invalid:\n{}", report);
    plan
}

/// A DogStatsD source, whose points are untimestamped unless `timestamps` says otherwise.
pub(super) fn dogstatsd(timestamps: &str) -> Value {
    node(
        "src",
        "source",
        json!({"binding": "dogstatsd", "timestamps": timestamps}),
    )
}

/// `src -> agg -> emit` to `output`, aggregating with `params`.
pub(super) fn aggregated_with(params: Value, output: &str, timestamps: &str) -> Plan {
    plan(
        vec![
            dogstatsd(timestamps),
            node("agg", "aggregate", params),
            node("out", "emit", json!({"output": output})),
        ],
        &["src.metrics -> agg.in", "agg.out -> out.in"],
        &[output],
    )
}

/// `src -> agg -> emit` to `output`, aggregating over `window`.
pub(super) fn aggregated(window: &str, output: &str) -> Plan {
    aggregated_with(aggregation(window), output, "untimestamped")
}

/// The lanes every trace also runs on: its stamps spread over three lanes of `src`, as a trace runs on producer lanes
/// ([`Trace::over_lanes`]).
const LANES: u16 = 3;

/// Runs a trace through the interpreter, and runs it again with its source's stamps spread over [`LANES`] lanes,
/// requiring both runs to deliver and decide the same: the expectations written for one point hold on lanes too.
fn interpreter_run(trace: Trace, heartbeats_at_ticks: bool) -> RunOutput {
    let registry = OperatorRegistry::native();
    let catalog = catalog();
    let run = |trace: &Trace, lanes: u16| {
        let interpreter = Interpreter::new(&registry, &catalog).with_source_lanes(src(), lanes);
        let interpreter = if heartbeats_at_ticks {
            interpreter.with_heartbeats_at_ticks()
        } else {
            interpreter
        };
        interpreter.run(trace).unwrap()
    };
    let one = run(&trace, 1);
    let spread = run(&trace.over_lanes(&src(), LANES), LANES);
    for output in ["a", "b"] {
        assert_eq!(
            delivered(&spread, output),
            delivered(&one, output),
            "`{}` on {} lanes",
            output,
            LANES
        );
    }
    // Mirrored heartbeats move the events after them, so a supersession names another position.
    let outcomes = |run: &RunOutput| {
        installs(run)
            .into_iter()
            .map(|(_, outcome)| match outcome.strip_prefix("superseded by ") {
                Some(_) => "superseded".to_string(),
                None => outcome,
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(outcomes(&spread), outcomes(&one), "installations on {} lanes", LANES);
    one
}

/// Runs a trace in which sources heartbeat only where the trace says.
fn run_queued(trace: Trace) -> RunOutput {
    interpreter_run(trace, false)
}

/// Runs a trace in which every source heartbeats at each tick, so that every tick fires at once.
fn run_with_heartbeats(trace: Trace) -> RunOutput {
    interpreter_run(trace, true)
}

pub(super) fn counter(name: &'static str, value: f64) -> Metric {
    Metric::counter(Context::from_static_name(name), value)
}

fn counter_with_unit(name: &'static str, value: f64, unit: &'static str) -> Metric {
    Metric::from_parts(
        Context::from_static_name(name),
        MetricValues::counter(value),
        MetricMetadata::default().with_unit(MetaString::from_static(unit)),
    )
}

/// A gauge with one point at `B + relative`.
pub(super) fn gauge_at(name: &'static str, relative: u64, value: f64) -> Metric {
    Metric::gauge(Context::from_static_name(name), (B + relative, value))
}

/// One delivered record: the generation that delivered it, its admission time in milliseconds relative to `B` (a
/// flush time), and the record, as `name kind [bucket:value, ...]`, buckets relative to `B`, with its unit if it has
/// one.
pub(super) type Delivered = (u64, u64, String);

pub(super) fn describe(metric: &Metric) -> String {
    let (kind, points) = match metric.values() {
        MetricValues::Rate(points, interval) => (format!("rate/{}s", interval.as_secs()), points),
        MetricValues::Gauge(points) => ("gauge".to_string(), points),
        other => panic!("unexpected values {:?}", other),
    };
    let mut points = points
        .into_iter()
        .map(|(ts, value)| (ts.map_or(0, |ts| ts.get() as i64 - B as i64), value))
        .collect::<Vec<_>>();
    points.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let points = points
        .into_iter()
        .map(|(ts, value)| format!("{}:{}", ts, value))
        .collect::<Vec<_>>();
    let unit = metric
        .metadata()
        .unit()
        .map(|unit| format!(" unit={}", unit))
        .unwrap_or_default();
    format!("{} {} [{}]{}", metric.context().name(), kind, points.join(", "), unit)
}

/// Returns what `output` received, in delivery order.
pub(super) fn delivered(run: &RunOutput, output: &str) -> Vec<Delivered> {
    run.output(&OutputId::new(output).unwrap())
        .iter()
        .map(|record| {
            let at = record
                .admitted_at
                .expect("time-driven output is stamped")
                .as_unix_nanos()
                / 1_000_000;
            (record.generation.get(), at - B * 1_000, describe(&record.metric))
        })
        .collect()
}

pub(super) fn expected(records: &[(u64, u64, &str)]) -> Vec<Delivered> {
    records
        .iter()
        .map(|(generation, at, record)| (*generation, *at, record.to_string()))
        .collect()
}

/// Returns each `install` and `cancel` event's position and outcome, written as `active 1`, `cut 2 at 100000`
/// (milliseconds relative to `B`), `cancelled`, `cancel refused`, `superseded by 4`, `waiting`, or `rejected`.
pub(super) fn installs(run: &RunOutput) -> Vec<(usize, String)> {
    run.installs()
        .iter()
        .map(|record| {
            let outcome = match &record.outcome {
                InstallOutcome::Active { generation } => format!("active {}", generation),
                InstallOutcome::Cut { generation, at } => {
                    format!("cut {} at {}", generation, at.as_unix_nanos() / 1_000_000 - B * 1_000)
                }
                InstallOutcome::Cancelled => "cancelled".to_string(),
                InstallOutcome::CancelRefused => "cancel refused".to_string(),
                InstallOutcome::Superseded { by } => format!("superseded by {}", by),
                InstallOutcome::Waiting => "waiting".to_string(),
                InstallOutcome::Rejected(rejection) => format!("rejected: {}", rejection),
            };
            (record.event, outcome)
        })
        .collect()
}

pub(super) fn outcomes(records: &[(usize, &str)]) -> Vec<(usize, String)> {
    records
        .iter()
        .map(|(event, outcome)| (*event, outcome.to_string()))
        .collect()
}

#[test]
fn trace_1_a_backlog_at_the_cut_lands_in_the_old_lineage() {
    // T = 100: the latest stamp is 98.5 at the arming, and widths 10 and 5 line up every 10 seconds. The tick at 101
    // finds the watermark at 98.5, so [90, 100) stays open, at the gate; the batch stamped 99 is the old generation's
    // and lands in it. The heartbeat at 100.5 is the new generation's first stamp: the old lineage makes its final
    // flush at 100 before it, with both counts, under the old generation. The new generation never sees them.
    let run = run_queued(
        Trace::new()
            .install(aggregated("10s", "a"))
            .admit(src(), t(95), vec![counter("c", 1.0)])
            .heartbeat(src(), ms(98_500))
            .install(aggregated("5s", "a"))
            .advance(t(101))
            .admit(src(), t(99), vec![counter("c", 2.0)])
            .heartbeat(src(), ms(100_500))
            .heartbeat(src(), t(116))
            .advance(t(116)),
    );
    assert_eq!(installs(&run), outcomes(&[(0, "active 1"), (3, "cut 2 at 100000")]));
    assert_eq!(delivered(&run, "a"), expected(&[(1, 100_000, "c rate/10s [90:3]")]));
}

#[test]
fn trace_2_a_late_batch_without_a_cut_lands_in_its_open_bucket() {
    // The same rule without a cut: the tick at 101 waits at the gate for the watermark, which neither the batch at 99
    // nor the heartbeat at 100.5 raises to 101; the tick at 116 comes after the 2-second grace, so the waiting tick
    // fires at the watermark's second, 100, closing [90, 100) with the late batch in it. No point is ever late.
    let run = run_queued(
        Trace::new()
            .install(aggregated("10s", "a"))
            .admit(src(), t(95), vec![counter("c", 1.0)])
            .advance(t(101))
            .admit(src(), t(99), vec![counter("c", 2.0)])
            .heartbeat(src(), ms(100_500))
            .advance(t(116)),
    );
    assert_eq!(delivered(&run, "a"), expected(&[(1, 100_000, "c rate/10s [90:3]")]));
}

#[test]
fn trace_3_a_changed_width_cuts_at_the_common_boundary() {
    // 10 -> 30 seconds at 95: T = 120, the first multiple of 30 at least a second away. The old lineage keeps flushing
    // through [110, 120), the last at the crossing, before the counter at 121 is admitted, with no zero at or after
    // 120. The new lineage starts at [120, 150) and closes it once the watermark reaches 150: its first flush emits no
    // zeros, and its entry records fresh metadata.
    let run = run_with_heartbeats(
        Trace::new()
            .install(aggregated("10s", "a"))
            .admit(src(), t(95), vec![counter_with_unit("c", 1.0, "second")])
            .install(aggregated("30s", "a"))
            .advance(t(101))
            .admit(src(), t(105), vec![counter_with_unit("c", 1.0, "second")])
            .advance(t(111))
            .admit(src(), t(115), vec![counter_with_unit("c", 1.0, "second")])
            .admit(src(), t(121), vec![counter_with_unit("c", 1.0, "millisecond")])
            .advance(t(151)),
    );
    assert_eq!(installs(&run), outcomes(&[(0, "active 1"), (2, "cut 2 at 120000")]));
    assert_eq!(
        delivered(&run, "a"),
        expected(&[
            (1, 101_000, "c rate/10s [90:1] unit=second"),
            (1, 111_000, "c rate/10s [100:1] unit=second"),
            (1, 120_000, "c rate/10s [110:1] unit=second"),
            (2, 151_000, "c rate/30s [120:1] unit=millisecond"),
        ])
    );
}

#[test]
fn trace_4_a_changed_destination_retains_and_flushes_through_the_new_plan() {
    // Output `a` replaced by `b` below aggregation: `retain`, active at once. The counter admitted before the change is
    // flushed to `b`, under the new generation, as native routing changes do.
    let run = run_with_heartbeats(
        Trace::new()
            .install(aggregated("10s", "a"))
            .admit(src(), t(95), vec![counter("c", 1.0)])
            .install(aggregated("10s", "b"))
            .advance(t(101)),
    );
    assert_eq!(installs(&run), outcomes(&[(0, "active 1"), (2, "active 2")]));
    assert_eq!(delivered(&run, "a"), expected(&[]));
    assert_eq!(delivered(&run, "b"), expected(&[(2, 101_000, "c rate/10s [90:1]")]));
}

#[test]
fn trace_5_a_small_backward_jump_is_absorbed_by_the_admission_clock() {
    // The admission clock is at 103 when the wall clock reads 97: it stamps 103 again, so both batches land in
    // [100, 110). An admission time that moves back by less than the reset threshold is never stamped.
    let run = run_with_heartbeats(
        Trace::new()
            .install(aggregated("10s", "a"))
            .admit(src(), t(103), vec![counter("c", 1.0)])
            .admit(src(), t(103), vec![counter("c", 2.0)])
            .advance(t(111)),
    );
    assert_eq!(delivered(&run, "a"), expected(&[(1, 111_000, "c rate/10s [100:3]")]));

    let registry = OperatorRegistry::native();
    let catalog = catalog();
    let error = Interpreter::new(&registry, &catalog)
        .run(
            &Trace::new()
                .install(aggregated("10s", "a"))
                .admit(src(), t(103), vec![counter("c", 1.0)])
                .admit(src(), t(97), vec![counter("c", 2.0)]),
        )
        .unwrap_err();
    assert_eq!(error.event, Some(2));
    assert!(
        matches!(error.kind, InterpretErrorKind::AdmissionTimeMovedBack { .. }),
        "{}",
        error
    );
}

#[test]
fn trace_6_a_large_backward_jump_resets_and_closes_every_open_bucket() {
    // The admission clock is at 7,201 when the wall clock jumps back to 3,603, 3,598 seconds, beyond the 300-second
    // reset threshold: the clock resets. Before the batch at 3,603 is admitted, aggregation flushes at the pre-reset
    // watermark, 7,201, closing [7180, 7190), and then emits everything it holds, the open [7200, 7210) included,
    // stamped 7,201; it continues empty. The idle counter `i` therefore gains no further zeros, and [3600, 3610)
    // closes later, at the tick at 3,615.
    let run = run_with_heartbeats(
        Trace::new()
            .install(aggregated("10s", "a"))
            .admit(src(), t(7_185), vec![counter("i", 1.0)])
            .admit(src(), t(7_201), vec![counter("c", 1.0)])
            .admit(src(), t(3_603), vec![counter("c", 2.0)])
            .advance(t(3_615))
            .advance(t(3_630)),
    );
    assert_eq!(
        delivered(&run, "a"),
        expected(&[
            (1, 7_201_000, "i rate/10s [7180:1]"),
            (1, 7_201_000, "c rate/10s [7200:1]"),
            (1, 3_615_000, "c rate/10s [3600:2]"),
            (1, 3_630_000, "c rate/10s [3610:0, 3620:0]"),
        ])
    );
}

#[test]
fn trace_7_a_rollback_is_a_new_installation_with_its_own_cut() {
    // Revision 2's width is 10 seconds; revision 3 changes it to 5, armed at 98.5 (T = 100); revision 4, revision 2's
    // plan again, is prepared at 105 with its own cut, at 110. Each retiring lineage makes its final flush at its cut
    // before the first batch of the generation replacing it, and nothing delivered is reverted: every count admitted
    // is delivered once.
    let run = run_queued(
        Trace::new()
            .install(aggregated("10s", "a"))
            .admit(src(), t(95), vec![counter("c", 1.0)])
            .heartbeat(src(), ms(98_500))
            .install(aggregated("5s", "a"))
            .admit(src(), ms(100_500), vec![counter("c", 1.0)])
            .heartbeat(src(), t(105))
            .install(aggregated("10s", "a"))
            .admit(src(), t(106), vec![counter("c", 1.0)])
            .admit(src(), ms(110_500), vec![counter("c", 1.0)])
            .heartbeat(src(), t(121))
            .advance(t(121)),
    );
    assert_eq!(
        installs(&run),
        outcomes(&[(0, "active 1"), (3, "cut 2 at 100000"), (6, "cut 3 at 110000")])
    );
    assert_eq!(
        delivered(&run, "a"),
        expected(&[
            (1, 100_000, "c rate/10s [90:1]"),
            (2, 110_000, "c rate/5s [100:1, 105:1]"),
            (3, 121_000, "c rate/10s [110:1]"),
        ])
    );
}

#[test]
fn trace_8_a_late_timestamped_point_belongs_to_the_new_generation() {
    // Stamped 103, after the cut at 100, so the new generation's: its timestamp, 95, puts it in a closed bucket, and
    // the new lineage emits it at its next flush, as native aggregation would.
    let run = run_queued(
        Trace::new()
            .install(aggregated_with(aggregation("10s"), "a", "mixed"))
            .heartbeat(src(), ms(98_500))
            .install(aggregated_with(aggregation("5s"), "a", "mixed"))
            .admit(src(), t(103), vec![gauge_at("g", 95, 1.0)])
            .heartbeat(src(), t(106))
            .advance(t(106)),
    );
    assert_eq!(installs(&run), outcomes(&[(0, "active 1"), (2, "cut 2 at 100000")]));
    assert_eq!(delivered(&run, "a"), expected(&[(2, 106_000, "g gauge [95:1]")]));
}

#[test]
fn trace_9_a_future_timestamped_point_is_flushed_at_the_old_lineages_retirement() {
    // Stamped 98, before the cut at 100, so the old generation's, with a timestamp of 130: its bucket ends after the
    // cut, so no flush of the old lineage closes it. The old lineage emits it when it retires, at the crossing.
    let run = run_queued(
        Trace::new()
            .install(aggregated_with(aggregation("10s"), "a", "mixed"))
            .admit(src(), t(98), vec![gauge_at("g", 130, 1.0)])
            .heartbeat(src(), ms(98_500))
            .install(aggregated_with(aggregation("5s"), "a", "mixed"))
            .heartbeat(src(), ms(100_500))
            .heartbeat(src(), t(141))
            .advance(t(141)),
    );
    assert_eq!(delivered(&run, "a"), expected(&[(1, 100_000, "g gauge [130:1]")]));
}

#[test]
fn trace_10_a_cut_is_cancelled_only_before_its_first_new_stamp() {
    let armed = || {
        Trace::new()
            .install(aggregated("10s", "a"))
            .admit(src(), t(95), vec![counter("c", 1.0)])
            .heartbeat(src(), ms(98_500))
            .install(aggregated("5s", "a"))
    };

    // Cancelled before any stamp of the new generation: the old generation stays, and nothing of the new one is
    // delivered.
    let cancelled = run_with_heartbeats(armed().cancel().heartbeat(src(), ms(100_500)).advance(t(101)));
    assert_eq!(
        installs(&cancelled),
        outcomes(&[(0, "active 1"), (3, "cut 2 at 100000"), (4, "cancelled")])
    );
    assert_eq!(
        delivered(&cancelled, "a"),
        expected(&[(1, 101_000, "c rate/10s [90:1]")])
    );

    // After the heartbeat at 100.5, the new generation has been admitted to: the cut proceeds.
    let refused = run_with_heartbeats(armed().heartbeat(src(), ms(100_500)).cancel().advance(t(101)));
    assert_eq!(
        installs(&refused),
        outcomes(&[(0, "active 1"), (3, "cut 2 at 100000"), (5, "cancel refused")])
    );
    assert_eq!(delivered(&refused, "a"), expected(&[(1, 100_000, "c rate/10s [90:1]")]));
}

#[test]
fn trace_11_a_reset_after_publication_switches_the_point_to_the_new_generation() {
    // The cut is armed at 7,198.5 (T = 7,200). The idle point's clock then resets to 3,601: a reset after
    // publication counts as reaching the cut, so the heartbeat is the new generation's. The old lineage first runs its
    // reset drain, at the pre-reset watermark clamped before the cut, emitting everything it holds, and then crosses,
    // holding nothing.
    let run = run_queued(
        Trace::new()
            .install(aggregated("10s", "a"))
            .admit(src(), t(7_195), vec![counter("c", 1.0)])
            .heartbeat(src(), ms(7_198_500))
            .install(aggregated("5s", "a"))
            .heartbeat(src(), t(3_601))
            .admit(src(), t(3_602), vec![counter("c", 2.0)])
            .heartbeat(src(), t(3_606))
            .advance(t(3_606)),
    );
    assert_eq!(installs(&run), outcomes(&[(0, "active 1"), (3, "cut 2 at 7200000")]));
    assert_eq!(
        delivered(&run, "a"),
        expected(&[
            (1, 7_198_500, "c rate/10s [7190:1]"),
            (2, 3_606_000, "c rate/5s [3600:2]")
        ])
    );

    // A reset before publication: the cut's time and the epochs it records come from the clock after the reset.
    let run = run_queued(
        Trace::new()
            .install(aggregated("10s", "a"))
            .admit(src(), t(7_201), vec![counter("c", 1.0)])
            .heartbeat(src(), t(3_601))
            .install(aggregated("5s", "a"))
            .admit(src(), t(3_605), vec![counter("c", 1.0)])
            .heartbeat(src(), ms(3_610_500)),
    );
    assert_eq!(installs(&run), outcomes(&[(0, "active 1"), (3, "cut 2 at 3610000")]));
    assert_eq!(
        delivered(&run, "a"),
        expected(&[
            (1, 7_201_000, "c rate/10s [7200:1]"),
            (1, 3_610_000, "c rate/10s [3600:1]")
        ])
    );
}

/// `src -> split -> agg_a -> emit a`, the split's other port dropped, or, `with_second`, aggregated by `agg_b` into
/// `b`.
fn split_aggregation(with_second: bool) -> Plan {
    let mut nodes = vec![
        dogstatsd("untimestamped"),
        node("by_name", "split", json!({"predicate": {"name": {"prefix": "a."}}})),
        node("agg_a", "aggregate", aggregation("10s")),
        node("out_a", "emit", json!({"output": "a"})),
    ];
    let mut edges = vec![
        "src.metrics -> by_name.in",
        "by_name.true -> agg_a.in",
        "agg_a.out -> out_a.in",
    ];
    let outputs: &[&str] = if with_second {
        nodes.extend([
            node("agg_b", "aggregate", aggregation("10s")),
            node("out_b", "emit", json!({"output": "b"})),
        ]);
        edges.extend(["by_name.false -> agg_b.in", "agg_b.out -> out_b.in"]);
        &["a", "b"]
    } else {
        nodes.push(node("rest", "drop", json!({})));
        edges.push("by_name.false -> rest.in");
        &["a"]
    };
    plan(nodes, &edges, outputs)
}

#[test]
fn trace_12_an_added_state_owner_starts_its_lineage_at_the_cut() {
    // The second plan adds `agg_b`: a cut at 100, while `agg_a` is retained throughout. `b.y` is dropped before the
    // cut, and aggregated by `agg_b` from [100, 110) on. `agg_a`'s flush at 99, before any point switched to the new
    // generation, is the old generation's, which cancelling the cut would keep; after the cut finalizes, its state,
    // from both generations, is flushed together under the newest.
    let run = run_with_heartbeats(
        Trace::new()
            .install(split_aggregation(false))
            .admit(src(), t(85), vec![counter("a.x", 1.0)])
            .admit(src(), t(95), vec![counter("a.x", 1.0), counter("b.y", 1.0)])
            .heartbeat(src(), ms(98_500))
            .install(split_aggregation(true))
            .advance(t(99))
            .admit(src(), t(99), vec![counter("a.x", 1.0), counter("b.y", 1.0)])
            .admit(src(), t(101), vec![counter("a.x", 1.0), counter("b.y", 1.0)])
            .advance(t(111)),
    );
    assert_eq!(installs(&run), outcomes(&[(0, "active 1"), (4, "cut 2 at 100000")]));
    assert_eq!(
        delivered(&run, "a"),
        expected(&[
            (1, 99_000, "a.x rate/10s [80:1]"),
            (2, 111_000, "a.x rate/10s [90:2, 100:1]"),
        ])
    );
    assert_eq!(delivered(&run, "b"), expected(&[(2, 111_000, "b.y rate/10s [100:1]")]));
    assert_eq!(
        run.loss(&id("rest")).loss(LossClass::Policy),
        LossCount { events: 2, points: 2 }
    );
}

#[test]
fn trace_13_a_retain_after_the_cut_flushes_the_old_lineage_under_the_generation_it_served() {
    // The cut at 100 finalizes at 100.1, where the old lineage makes its final flush, under generation 1 that it
    // served; a `retain` of the new plan to another output commits at 100.3 and flushes the new lineage's state there.
    // No zero at or after 100 comes from the old lineage.
    let run = run_with_heartbeats(
        Trace::new()
            .install(aggregated("10s", "a"))
            .admit(src(), t(95), vec![counter("c", 1.0)])
            .heartbeat(src(), ms(98_500))
            .install(aggregated("5s", "a"))
            .heartbeat(src(), ms(100_100))
            .install(aggregated("5s", "b"))
            .admit(src(), t(101), vec![counter("c", 1.0)])
            .advance(t(106))
            .advance(t(121)),
    );
    assert_eq!(
        installs(&run),
        outcomes(&[(0, "active 1"), (3, "cut 2 at 100000"), (5, "active 3")])
    );
    assert_eq!(delivered(&run, "a"), expected(&[(1, 100_000, "c rate/10s [90:1]")]));
    assert_eq!(
        delivered(&run, "b"),
        expected(&[
            (3, 106_000, "c rate/5s [100:1]"),
            (3, 121_000, "c rate/5s [105:0, 110:0, 115:0]"),
        ])
    );
}

/// `dsd -> merge <- otlp`, aggregated over `window` into `a`: one state owner with two upstream admission points.
fn two_sources(window: &str) -> Plan {
    two_sources_with(aggregation(window))
}

/// `dsd -> merge <- otlp`, aggregated with `params` into `a`.
fn two_sources_with(params: Value) -> Plan {
    plan(
        vec![
            node(
                "dsd",
                "source",
                json!({"binding": "dogstatsd", "timestamps": "untimestamped"}),
            ),
            node("otlp", "source", json!({"binding": "otlp"})),
            node("joined", "merge", json!({"inputs": 2})),
            node("agg", "aggregate", params),
            node("out", "emit", json!({"output": "a"})),
        ],
        &[
            "dsd.metrics -> joined.0",
            "otlp.metrics -> joined.1",
            "joined.out -> agg.in",
            "agg.out -> out.in",
        ],
        &["a"],
    )
}

#[test]
fn trace_14_a_revocation_cancels_an_armed_cut_or_waits_for_an_irrevocable_one() {
    // While armed: the revocation cancels the artifact's cut, and the default program installs again, retaining its
    // lineage; nothing of the artifact is delivered.
    let run = run_with_heartbeats(
        Trace::new()
            .install(aggregated("10s", "a"))
            .admit(src(), t(95), vec![counter("c", 1.0)])
            .install(aggregated("30s", "a"))
            .cancel()
            .install(aggregated("10s", "a"))
            .advance(t(101)),
    );
    assert_eq!(
        installs(&run),
        outcomes(&[
            (0, "active 1"),
            (2, "cut 2 at 120000"),
            (3, "cancelled"),
            (4, "active 3")
        ])
    );
    assert_eq!(delivered(&run, "a"), expected(&[(3, 101_000, "c rate/10s [90:1]")]));

    // While irrevocable: with two upstream points, the stamp at 121 at `dsd` makes the cut irrevocable but does not
    // finalize it, so the revocation's cancel is refused and the default program waits. When `otlp` too reaches the
    // cut, the old lineage retires and the artifact's plan is active; the default program then installs with its own
    // cut, at 150, and the artifact's lineage retires at it.
    let run = run_queued(
        Trace::new()
            .install(two_sources("10s"))
            .admit(id("dsd"), t(95), vec![counter("c", 1.0)])
            .heartbeat(id("otlp"), t(95))
            .install(two_sources("30s"))
            .admit(id("dsd"), t(121), vec![counter("c", 1.0)])
            .cancel()
            .install(two_sources("10s"))
            .heartbeat(id("otlp"), t(121))
            .admit(id("dsd"), t(151), vec![counter("c", 1.0)])
            .heartbeat(id("otlp"), t(151))
            .heartbeat(id("dsd"), t(161))
            .heartbeat(id("otlp"), t(161))
            .advance(t(161)),
    );
    assert_eq!(
        installs(&run),
        outcomes(&[
            (0, "active 1"),
            (3, "cut 2 at 120000"),
            (5, "cancel refused"),
            (6, "cut 3 at 150000")
        ])
    );
    assert_eq!(
        delivered(&run, "a"),
        expected(&[
            (1, 120_000, "c rate/10s [90:1]"),
            (2, 150_000, "c rate/30s [120:1]"),
            (3, 161_000, "c rate/10s [150:1]"),
        ])
    );
}

#[test]
fn trace_15_re_compositions_during_a_cut_wait_and_the_last_commits_as_retain() {
    // Revision 7 is armed with T = 120. Three native changes re-compose it before 120: each waits, replacing the one
    // before, so T stays 120 and there is one cut. Right after it finalizes, at 121, the last re-composition commits as
    // `retain`: the lineage keeps its state, and the new generation's context limit of 1 applies to it from then on.
    let recomposed = |context_limit: u64| {
        let mut params = aggregation("30s");
        params["context_limit"] = json!(context_limit);
        aggregated_with(params, "a", "untimestamped")
    };
    let run = run_with_heartbeats(
        Trace::new()
            .install(aggregated("10s", "a"))
            .admit(src(), t(95), vec![counter("c", 1.0)])
            .install(recomposed(1000))
            .install(recomposed(999))
            .install(recomposed(998))
            .install(recomposed(1))
            .admit(src(), t(121), vec![counter("c", 1.0)])
            .admit(src(), t(122), vec![counter("c", 1.0), counter("d", 1.0)])
            .advance(t(151)),
    );
    assert_eq!(
        installs(&run),
        outcomes(&[
            (0, "active 1"),
            (2, "cut 2 at 120000"),
            (3, "superseded by 4"),
            (4, "superseded by 5"),
            (5, "active 3"),
        ])
    );
    assert_eq!(
        delivered(&run, "a"),
        expected(&[(1, 120_000, "c rate/10s [90:1]"), (3, 151_000, "c rate/30s [120:2]")])
    );
    assert_eq!(
        run.loss(&id("agg")).loss(LossClass::Capacity),
        LossCount { events: 1, points: 1 }
    );
}

#[test]
fn an_installation_waiting_for_a_cut_starts_when_a_ticks_heartbeat_finalizes_it() {
    // The heartbeat at the tick at 121 is the cut's first new stamp: the old lineage retires, the cut finalizes, and
    // the waiting installation commits as `retain`, all before the tick, which then runs under it.
    let mut limited = aggregation("30s");
    limited["context_limit"] = json!(999);
    let run = run_with_heartbeats(
        Trace::new()
            .install(aggregated("10s", "a"))
            .admit(src(), t(95), vec![counter("c", 1.0)])
            .install(aggregated("30s", "a"))
            .install(aggregated_with(limited, "a", "untimestamped"))
            .advance(t(121))
            .admit(src(), t(125), vec![counter("c", 1.0)])
            .advance(t(151)),
    );
    assert_eq!(
        installs(&run),
        outcomes(&[(0, "active 1"), (2, "cut 2 at 120000"), (3, "active 3")])
    );
    assert_eq!(
        delivered(&run, "a"),
        expected(&[(1, 120_000, "c rate/10s [90:1]"), (3, 151_000, "c rate/30s [120:1]")])
    );
}

#[test]
fn a_cut_whose_widths_line_up_too_rarely_is_rejected_and_one_waiting_at_the_end_is_reported() {
    // 120 and 180 seconds line up every 360, more than a cut may wait for.
    let run = run_queued(
        Trace::new()
            .install(aggregated("2m", "a"))
            .install(aggregated("3m", "a")),
    );
    match &run.installs()[1].outcome {
        InstallOutcome::Rejected(rejection @ InstallRejection::CutWidths { .. }) => assert!(
            rejection
                .to_string()
                .contains("line up only every 360 seconds, more than the 300 seconds a cut may wait for"),
            "{}",
            rejection
        ),
        other => panic!("the installation should be rejected: {:?}", other),
    }

    // An installation still waiting for a cut when the trace ends never starts.
    let run = run_queued(
        Trace::new()
            .install(aggregated("10s", "a"))
            .heartbeat(src(), t(0))
            .install(aggregated("5s", "a"))
            .install(aggregated("5s", "b")),
    );
    assert_eq!(
        installs(&run),
        outcomes(&[(0, "active 1"), (2, "cut 2 at 10000"), (3, "waiting")])
    );
    // A plan is published with its outputs; the waiting plan's output `b` never was.
    assert!(
        run.outputs().all(|(output, _)| output.to_string() != "b"),
        "the waiting installation was published"
    );
}

#[test]
fn a_cancel_supersedes_the_installation_waiting_for_its_cut_so_only_the_cancellers_proposal_follows() {
    // A cut to 30 seconds is armed at T = 120, and a re-composition of its revision (a context limit of 999) waits for
    // it. The revision is then revoked while the cut is armed: the cancel succeeds and supersedes the re-composition,
    // and the default program installs as `retain` of generation 1's lineage. The revoked revision's plan is never
    // active, and generation 1's lineage never retires.
    let armed = || {
        Trace::new()
            .install(aggregated("10s", "a"))
            .admit(src(), t(95), vec![counter("c", 1.0)])
            .install(aggregated("30s", "a"))
            .install(aggregated_with(aggregation_limited("30s", 999), "a", "untimestamped"))
            .cancel()
    };
    let revoked = run_with_heartbeats(
        armed()
            .install(aggregated("10s", "a"))
            .advance(t(101))
            .advance(t(121))
            .advance(t(151)),
    );
    assert_eq!(
        installs(&revoked),
        outcomes(&[
            (0, "active 1"),
            (2, "cut 2 at 120000"),
            (3, "superseded by 4"),
            (4, "cancelled"),
            (5, "active 3"),
        ])
    );
    assert_eq!(
        delivered(&revoked, "a"),
        expected(&[
            (3, 101_000, "c rate/10s [90:1]"),
            (3, 121_000, "c rate/10s [100:0, 110:0]"),
            (3, 151_000, "c rate/10s [120:0, 130:0, 140:0]"),
        ])
    );

    // A newer revision while the cut is armed: the cancel supersedes the re-composition, and the newer revision is
    // prepared against generation 1, with a cut of its own at 100.
    let newer = run_with_heartbeats(
        armed()
            .install(aggregated("5s", "a"))
            .advance(t(101))
            .admit(src(), t(102), vec![counter("c", 1.0)])
            .advance(t(106)),
    );
    assert_eq!(
        installs(&newer),
        outcomes(&[
            (0, "active 1"),
            (2, "cut 2 at 120000"),
            (3, "superseded by 4"),
            (4, "cancelled"),
            (5, "cut 3 at 100000"),
        ])
    );
    assert_eq!(
        delivered(&newer, "a"),
        expected(&[(1, 100_000, "c rate/10s [90:1]"), (3, 106_000, "c rate/5s [100:1]")])
    );

    // A cancel that is refused, the cut being irrevocable, leaves the re-composition waiting, and the proposal that
    // follows it replaces it: the default program installs after the cut finalizes, with its own cut at 150, as in
    // trace 14.
    let refused = run_queued(
        Trace::new()
            .install(two_sources("10s"))
            .admit(id("dsd"), t(95), vec![counter("c", 1.0)])
            .heartbeat(id("otlp"), t(95))
            .install(two_sources("30s"))
            .install(two_sources_with(aggregation_limited("30s", 999)))
            .admit(id("dsd"), t(121), vec![counter("c", 1.0)])
            .cancel()
            .install(two_sources("10s"))
            .heartbeat(id("otlp"), t(121))
            .admit(id("dsd"), t(151), vec![counter("c", 1.0)])
            .heartbeat(id("otlp"), t(151))
            .heartbeat(id("dsd"), t(161))
            .heartbeat(id("otlp"), t(161))
            .advance(t(161)),
    );
    assert_eq!(
        installs(&refused),
        outcomes(&[
            (0, "active 1"),
            (3, "cut 2 at 120000"),
            (4, "superseded by 7"),
            (6, "cancel refused"),
            (7, "cut 3 at 150000"),
        ])
    );
    assert_eq!(
        delivered(&refused, "a"),
        expected(&[
            (1, 120_000, "c rate/10s [90:1]"),
            (2, 150_000, "c rate/30s [120:1]"),
            (3, 161_000, "c rate/10s [150:1]"),
        ])
    );
}

#[test]
fn a_proposal_is_validated_when_it_is_made_so_an_invalid_one_never_replaces_the_one_waiting() {
    // A re-composition routed to `b` waits for the cut at 120; a proposal with a 7-second window, which is not an
    // allowed bucket width, is rejected at once, as it would be with no cut in flight, and the re-composition commits
    // as `retain` when the cut finalizes at the tick at 121.
    let invalid = unvalidated_plan(
        vec![
            dogstatsd("untimestamped"),
            node("agg", "aggregate", aggregation("7s")),
            node("out", "emit", json!({"output": "a"})),
        ],
        &["src.metrics -> agg.in", "agg.out -> out.in"],
        &["a"],
    );
    let run = run_with_heartbeats(
        Trace::new()
            .install(aggregated("10s", "a"))
            .admit(src(), t(95), vec![counter("c", 1.0)])
            .install(aggregated("30s", "a"))
            .install(aggregated("30s", "b"))
            .install(invalid)
            .advance(t(121))
            .admit(src(), t(125), vec![counter("c", 1.0)])
            .advance(t(151)),
    );
    let rejected = run
        .installs()
        .iter()
        .filter(|record| {
            matches!(
                record.outcome,
                InstallOutcome::Rejected(InstallRejection::Invalid { .. })
            )
        })
        .map(|record| record.event)
        .collect::<Vec<_>>();
    assert_eq!(rejected, [4]);
    assert_eq!(
        installs(&run)
            .into_iter()
            .filter(|(event, _)| *event != 4)
            .collect::<Vec<_>>(),
        outcomes(&[(0, "active 1"), (2, "cut 2 at 120000"), (3, "active 3")])
    );
    assert_eq!(delivered(&run, "a"), expected(&[(1, 120_000, "c rate/10s [90:1]")]));
    assert_eq!(delivered(&run, "b"), expected(&[(3, 151_000, "c rate/30s [120:1]")]));
}

#[test]
fn a_retain_whose_parameters_do_not_build_its_kernel_is_rejected_as_a_first_installation_is() {
    // The second plan keeps the window, so its lineage would be retained, but binds the context limit to configuration
    // that has no value. As a first installation its kernel does not build; as a `retain` it is rejected the same way,
    // rather than running on generation 1's parameters, and generation 1 keeps its limit of 1,000.
    let registry = OperatorRegistry::native();
    let catalog = catalog().with_binding(
        "dogstatsd.aggregation.context_limit".parse().unwrap(),
        ParamType::Integer {
            min: 0,
            max: MAX_SAFE_INTEGER,
        },
        Liveness::Startup,
    );
    let mut params = aggregation("10s");
    params["context_limit"] = json!({"config": "dogstatsd.aggregation.context_limit"});
    let unresolved = unvalidated_plan(
        vec![
            dogstatsd("untimestamped"),
            node("agg", "aggregate", params),
            node("out", "emit", json!({"output": "b"})),
        ],
        &["src.metrics -> agg.in", "agg.out -> out.in"],
        &["b"],
    );
    let rejection = |run: &RunOutput, event: usize| match &run.installs()[event].outcome {
        InstallOutcome::Rejected(rejection @ InstallRejection::KernelBuild { .. }) => rejection.to_string(),
        other => panic!("the installation should be rejected by its kernel: {:?}", other),
    };

    let interpreter = Interpreter::new(&registry, &catalog).with_heartbeats_at_ticks();
    let first = interpreter.run(&Trace::new().install(unresolved.clone())).unwrap();
    assert!(
        rejection(&first, 0).contains("bound to configuration and has no resolved value"),
        "{}",
        rejection(&first, 0)
    );

    let retained = interpreter
        .run(
            &Trace::new()
                .install(aggregated("10s", "a"))
                .install(unresolved)
                .admit(src(), t(95), vec![counter("c", 1.0), counter("d", 1.0)])
                .advance(t(101)),
        )
        .unwrap();
    assert_eq!(installs(&retained)[0], (0, "active 1".to_string()));
    assert_eq!(rejection(&retained, 1), rejection(&first, 0));
    // A flush emits its contexts in no particular order.
    let mut flushed = delivered(&retained, "a");
    flushed.sort();
    assert_eq!(
        flushed,
        expected(&[(1, 101_000, "c rate/10s [90:1]"), (1, 101_000, "d rate/10s [90:1]")])
    );
    assert_eq!(delivered(&retained, "b"), expected(&[]));
}

#[test]
fn state_owners_over_one_admission_point_stamp_their_output_by_one_clock() {
    // `src -> tee -> {agg_a, agg_b}`; the second plan changes `agg_b` to 30 seconds (T = 120) and retains `agg_a`. The
    // tick at 111 waits at the gate for both, and the stamp at 121 crosses: `agg_b`'s old lineage makes its final
    // flush at 120, and then `agg_a`'s released tick flushes at 111, stamped no earlier than 120, as the isolation slot
    // that runs both state owners stamps its output.
    let tee = |window_b: &str| {
        plan(
            vec![
                dogstatsd("untimestamped"),
                node("t", "tee", json!({"ports": 2})),
                node("agg_a", "aggregate", aggregation("10s")),
                node("agg_b", "aggregate", aggregation(window_b)),
                node("out_a", "emit", json!({"output": "a"})),
                node("out_b", "emit", json!({"output": "b"})),
            ],
            &[
                "src.metrics -> t.in",
                "t.0 -> agg_a.in",
                "t.1 -> agg_b.in",
                "agg_a.out -> out_a.in",
                "agg_b.out -> out_b.in",
            ],
            &["a", "b"],
        )
    };
    let run = run_queued(
        Trace::new()
            .install(tee("10s"))
            .admit(src(), t(95), vec![counter("x", 1.0)])
            .install(tee("30s"))
            .heartbeat(src(), ms(110_500))
            .advance(t(111))
            .admit(src(), t(121), vec![counter("y", 1.0)]),
    );
    assert_eq!(installs(&run), outcomes(&[(0, "active 1"), (2, "cut 2 at 120000")]));
    assert_eq!(delivered(&run, "b"), expected(&[(1, 120_000, "x rate/10s [90:1]")]));
    assert_eq!(delivered(&run, "a"), expected(&[(2, 120_000, "x rate/10s [90:1]")]));
}

#[test]
fn every_lineage_draws_on_one_state_budget_and_a_cut_frees_the_old_lineages_share_first() {
    // A budget of 10 contexts, 7 held by the old lineage at the cut: it releases them at the crossing, before the new
    // generation's first batch admits 7 new ones, so the cut loses nothing to capacity.
    let names = ["c0", "c1", "c2", "c3", "c4", "c5", "c6"];
    let batch = |prefix: &'static str| {
        names
            .iter()
            .map(|name| counter(Box::leak(format!("{}.{}", prefix, name).into_boxed_str()), 1.0))
            .collect::<Vec<_>>()
    };
    let registry = OperatorRegistry::native();
    let catalog = catalog();
    let run = Interpreter::new(&registry, &catalog)
        .with_state_budget(10)
        .run(
            &Trace::new()
                .install(aggregated("10s", "a"))
                .admit(src(), t(95), batch("old"))
                .heartbeat(src(), ms(98_500))
                .install(aggregated("5s", "a"))
                .admit(src(), ms(100_500), batch("new")),
        )
        .unwrap();
    assert_eq!(run.loss(&id("agg")).loss(LossClass::Capacity), LossCount::default());
    assert_eq!(delivered(&run, "a").len(), 7, "the old lineage's final flush");

    // The budget bounds every lineage together: with 10 contexts held, an eleventh is dropped.
    let run = Interpreter::new(&registry, &catalog)
        .with_state_budget(10)
        .run(
            &Trace::new()
                .install(aggregated("10s", "a"))
                .admit(src(), t(95), batch("x"))
                .admit(src(), t(96), batch("y")),
        )
        .unwrap();
    assert_eq!(
        run.loss(&id("agg")).loss(LossClass::Capacity),
        LossCount { events: 4, points: 4 }
    );
}
