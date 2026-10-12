//! The normative transition traces through a frame of region drivers: the stage 6 design's traces 1-15, each run by
//! the reference interpreter and replayed through a [`FrameHarness`], whose isolation slot runs aggregation as the
//! production topology does. Both must deliver exactly what the trace's expectation, written by hand from the
//! transition protocol, says: which generation every record is delivered under, at which flush time (its `t_a`), and
//! with which points.
//!
//! The traces are those of `interpret::transition_tests`, with the same expectations, written over plans
//! `source -> aggregate -> emit` with a 10-second window unless stated. Two things differ from the interpreter's form:
//!
//! - **Flush ticks.** The interpreter ticks at every `advance`; a region ticks every flush interval from its start. The
//!   plans here flush every second, so every `advance` finds a tick due, and the ticks that a jump of the clock makes
//!   due fire at once, at the advance's wall-clock second, as one tick at that time would: a flush at the time of the
//!   previous one closes nothing and synthesizes no zeros. The interpreter's output does not depend on the flush
//!   interval, which the matching expectations show.
//! - **Installations.** The interpreter decides installations itself (immediately, at a cut, or after the cut in
//!   flight). The replay performs what it decided on the regions as the installer does: it prepares the candidate,
//!   then commits it or arms its cut, finalizes the cut once every admission point feeding state passed it, starts a
//!   waiting installation then, and cancels a cut as the trace says. The cut's time the harness computes must equal
//!   the interpreter's.
//!
//! The traces whose design form needs a queue between regions (7, 11 before publication, and 13) also run in their
//! queued form here, where batches are held back between the pre-aggregation region and the isolation slot (trace 7
//! in `state_tests`). Trace 14's irrevocable phase needs two upstream admission points, which an isolation slot does
//! not have until stage 9 (`FrameError::StateOwnerPoints`); it runs in the interpreter only.

use std::{collections::BTreeMap, time::Duration};

use saluki_core::{
    data_model::event::metric::{context::Context, Metric, MetricMetadata, MetricValues},
    execution::{AdmissionTime, LossClass},
};
use serde_json::{json, Value};
use stringtheory::MetaString;

use super::harness::{DeliveredRecord, FrameHarness, HarnessOptions};
use crate::{
    canonical::PlanHash,
    install::diff,
    interpret::{InstallOutcome, Interpreter, RunOutput, Trace, TraceEvent},
    ir::{NodeId, OutputId, Plan, RecipientId},
    limits,
    model::PortType,
    operators::{names, OperatorRegistry},
    validate::{CatalogOutput, MemoryCatalog, ValidationReport, Validator},
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

fn id(value: &str) -> NodeId {
    value.parse().unwrap()
}

fn src() -> NodeId {
    id("src")
}

fn fingerprint(output: &str) -> PlanHash {
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

/// The parameters of an aggregation with the given window, a flush every second, and native defaults otherwise.
pub(super) fn aggregation(window: &str) -> Value {
    json!({
        "window": window, "flush_interval": "1s", "context_limit": 1000, "counter_expiry": "5m",
        "flush_open_windows": false, "histogram_aggregates": ["max", "count"], "histogram_percentiles": [],
        "copy_to_distribution": false, "copy_to_distribution_prefix": "",
    })
}

pub(super) fn node(id: &str, op: &str, params: Value) -> Value {
    json!({"id": id, "op": op, "params": params, "provenance": {"native": id}})
}

/// Builds a valid plan from nodes, edges written as `from -> to`, and the outputs it declares.
pub(super) fn plan(nodes: Vec<Value>, edges: &[&str], outputs: &[&str]) -> Plan {
    let edges = edges
        .iter()
        .map(|edge| {
            let (from, to) = edge.split_once(" -> ").unwrap();
            json!({"from": from, "to": to})
        })
        .collect::<Vec<_>>();
    let outputs = outputs
        .iter()
        .map(|output| {
            json!({"id": output, "accepts": {"kinds": "all", "timestamps": "mixed"},
                   "fingerprint": fingerprint(output).to_string(), "recipients": [format!("out/{}", output)]})
        })
        .collect::<Vec<_>>();
    let plan = Plan::from_value(json!({"ir_version": 1, "nodes": nodes, "edges": edges, "outputs": outputs})).unwrap();
    report(&plan);
    plan
}

pub(super) fn report(plan: &Plan) -> ValidationReport {
    let report = Validator::new(&OperatorRegistry::native(), &catalog()).validate(plan);
    assert!(report.is_valid(), "the fixture plan is invalid:\n{}", report);
    report
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
fn aggregated_with(params: Value, output: &str, timestamps: &str) -> Plan {
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
fn aggregated(window: &str, output: &str) -> Plan {
    aggregated_with(aggregation(window), output, "untimestamped")
}

/// `src -> agg -> emit` to `a` and `b`, through a copy: the boot plan of traces whose later plans send to either.
fn aggregated_to_both(window: &str) -> Plan {
    plan(
        vec![
            dogstatsd("untimestamped"),
            node("agg", "aggregate", aggregation(window)),
            node("copy", "tee", json!({"ports": 2})),
            node("out", "emit", json!({"output": "a"})),
            node("out_b", "emit", json!({"output": "b"})),
        ],
        &[
            "src.metrics -> agg.in",
            "agg.out -> copy.in",
            "copy.0 -> out.in",
            "copy.1 -> out_b.in",
        ],
        &["a", "b"],
    )
}

fn counter(name: &'static str, value: f64) -> Metric {
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
fn gauge_at(name: &'static str, relative: u64, value: f64) -> Metric {
    Metric::gauge(Context::from_static_name(name), (B + relative, value))
}

/// One delivered record: the generation that delivered it, its admission time in milliseconds relative to `B` (a
/// flush time), and the record, as `name kind [bucket:value, ...]`, buckets relative to `B`, with its unit if it has
/// one.
type Delivered = (u64, u64, String);

fn describe(metric: &Metric) -> String {
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

fn relative_millis(at: AdmissionTime) -> u64 {
    at.as_unix_nanos() / 1_000_000 - B * 1_000
}

/// Returns what `output` received in the interpreter's run, sorted: the order of records within one flush is not
/// observable.
fn interpreted(run: &RunOutput, output: &str) -> Vec<Delivered> {
    let mut delivered = run
        .output(&OutputId::new(output).unwrap())
        .iter()
        .map(|record| {
            let at = record.admitted_at.expect("time-driven output is stamped");
            (record.generation.get(), relative_millis(at), describe(&record.metric))
        })
        .collect::<Vec<_>>();
    delivered.sort();
    delivered
}

/// Returns what `output` received through the frame, sorted.
fn framed(harness: &FrameHarness, output: &str) -> Vec<Delivered> {
    let output = OutputId::new(output).unwrap();
    let mut delivered = harness
        .delivered()
        .iter()
        .filter(|record: &&DeliveredRecord| record.output == output)
        .map(|record| {
            let at = record.admitted_at.expect("time-driven output is stamped");
            let generation = record.generation.expect("time-driven output carries a generation");
            (generation.get(), relative_millis(at), describe(&record.metric))
        })
        .collect::<Vec<_>>();
    delivered.sort();
    delivered
}

fn expected(records: &[(u64, u64, &str)]) -> Vec<Delivered> {
    let mut expected = records
        .iter()
        .map(|(generation, at, record)| (*generation, *at, record.to_string()))
        .collect::<Vec<_>>();
    expected.sort();
    expected
}

/// A trace replayed through a frame of region drivers, following the installations its interpreter run decided.
pub(super) struct Replay {
    pub(super) harness: FrameHarness,
    /// The plan of the active generation, and of the generation a cut in flight replaces.
    active: Plan,
    replaced: Option<Plan>,
    /// The installation waiting for the cut in flight, with its event's position: the latest wins.
    waiting: Option<(usize, Plan)>,
    /// The latest admission time of each source, for heartbeats at ticks.
    latest: BTreeMap<NodeId, AdmissionTime>,
    /// The clock of the latest `advance`, the wall clock the interpreter reads when it arms a cut.
    clock: AdmissionTime,
    heartbeats_at_ticks: bool,
}

impl Replay {
    /// Runs `trace` through the interpreter, with a heartbeat from every source at every tick if `heartbeats_at_ticks`,
    /// and through a frame whose regions start at `start`, which must be no later than the trace's first stamp. Returns
    /// both.
    fn run(trace: &Trace, heartbeats_at_ticks: bool, start: AdmissionTime) -> (RunOutput, Self) {
        Self::run_with(trace, heartbeats_at_ticks, start, HarnessOptions::default())
    }

    /// Runs `trace` as [`run`][Self::run] does, with the harness's `options`; the interpreter's state budget is the
    /// harness's.
    pub(super) fn run_with(
        trace: &Trace, heartbeats_at_ticks: bool, start: AdmissionTime, options: HarnessOptions,
    ) -> (RunOutput, Self) {
        let registry = OperatorRegistry::native();
        let catalog = catalog();
        let interpreter = Interpreter::new(&registry, &catalog);
        let interpreter = match options.state_capacity {
            Some(capacity) => interpreter.with_state_budget(capacity),
            None => interpreter,
        };
        let interpreter = if heartbeats_at_ticks {
            interpreter.with_heartbeats_at_ticks()
        } else {
            interpreter
        };
        let run = interpreter.run(trace).unwrap();
        let replay = Self::follow(trace, &run, heartbeats_at_ticks, start, options);
        (run, replay)
    }

    /// Replays `trace` through the frame harness, following the interpreter's `run`: each installation as the
    /// interpreter decided it (committed, armed at its cut, waiting, cancelled, or refused).
    fn follow(
        trace: &Trace, run: &RunOutput, heartbeats_at_ticks: bool, start: AdmissionTime, options: HarnessOptions,
    ) -> Self {
        let outcomes = run
            .installs()
            .iter()
            .map(|record| (record.event, &record.outcome))
            .collect::<BTreeMap<_, _>>();
        let events = trace.events();
        let TraceEvent::Install { plan: boot } = &events[0] else {
            panic!("a trace starts with its boot plan's installation")
        };
        let boot = Plan::clone(boot);
        let harness = FrameHarness::starting_at(&boot, &report(&boot), OperatorRegistry::native(), options, start);
        let mut replay = Self {
            harness,
            active: boot,
            replaced: None,
            waiting: None,
            latest: BTreeMap::new(),
            clock: start,
            heartbeats_at_ticks,
        };
        for (position, event) in events.iter().enumerate().skip(1) {
            match event {
                TraceEvent::Admit {
                    source,
                    admitted_at,
                    batch,
                } => {
                    replay.stamped(source, *admitted_at);
                    replay
                        .harness
                        .admit(source.local().as_str(), *admitted_at, batch.clone());
                    replay.finalize_if_passed(&outcomes);
                }
                TraceEvent::Heartbeat { source, admitted_at } => replay.heartbeat(source, *admitted_at, &outcomes),
                TraceEvent::Advance { clock } => {
                    // As the interpreter does: idle sources heartbeat at the tick's clock first, which can finalize a
                    // cut, and start the installation waiting for it, before the tick.
                    if replay.heartbeats_at_ticks {
                        let stale = replay
                            .sources()
                            .into_iter()
                            .filter(|source| replay.latest.get(source).is_none_or(|latest| latest < clock))
                            .collect::<Vec<_>>();
                        for source in stale {
                            replay.heartbeat(&source, *clock, &outcomes);
                        }
                    }
                    replay.harness.advance(*clock);
                    replay.clock = *clock;
                }
                TraceEvent::Install { plan } => {
                    let plan = Plan::clone(plan);
                    match outcomes[&position] {
                        InstallOutcome::Rejected(_) => {}
                        _ if replay.replaced.is_some() => replay.waiting = Some((position, plan)),
                        _ => replay.start(position, plan, &outcomes),
                    }
                }
                TraceEvent::Cancel => match outcomes[&position] {
                    InstallOutcome::Cancelled => {
                        assert!(replay.harness.cancel_cut(), "event {}: the cut is cancelled", position);
                        replay.active = replay.replaced.take().expect("a cut is in flight");
                        // A cancel discards the installation waiting for the cut.
                        replay.waiting = None;
                    }
                    InstallOutcome::CancelRefused => {
                        assert!(
                            !replay.harness.cancel_cut(),
                            "event {}: the cancel is refused",
                            position
                        );
                    }
                    other => panic!("event {}: a cancel's outcome is {:?}", position, other),
                },
                TraceEvent::Close => {
                    let clock = replay.clock;
                    replay.harness.close(clock);
                }
                other => panic!("event {}: the replay runs no `{}`", position, other.name()),
            }
        }
        replay
    }

    /// Returns the sources of the active plan.
    fn sources(&self) -> Vec<NodeId> {
        self.active
            .nodes
            .iter()
            .filter(|node| node.op.as_str() == names::SOURCE)
            .map(|node| node.id.clone())
            .collect()
    }

    /// Records a stamp of `source` at `at`, as its admission clock keeps its latest time: a jump back beyond the reset
    /// threshold restarts it.
    fn stamped(&mut self, source: &NodeId, at: AdmissionTime) {
        let threshold = limits::values::CLOCK_RESET_THRESHOLD.as_nanos() as u64;
        let latest = self.latest.entry(source.clone()).or_insert(at);
        if at > *latest || latest.as_unix_nanos() - at.as_unix_nanos() > threshold {
            *latest = at;
        }
    }

    fn heartbeat(&mut self, source: &NodeId, at: AdmissionTime, outcomes: &BTreeMap<usize, &InstallOutcome>) {
        self.stamped(source, at);
        self.harness.heartbeat(source.local().as_str(), at);
        self.finalize_if_passed(outcomes);
    }

    /// Prepares the installation at `position`, then commits it or arms its cut, as its interpreter run decided.
    fn start(&mut self, position: usize, plan: Plan, outcomes: &BTreeMap<usize, &InstallOutcome>) {
        let cut = self.harness.prepare(&plan, &report(&plan));
        match outcomes[&position] {
            InstallOutcome::Cut { at, .. } => {
                assert!(
                    cut,
                    "event {}: the interpreter cut, so the regions' bindings do",
                    position
                );
                // As the installer does: the widths of the state owners whose lineages the cut starts or ends.
                let widths = diff(&self.active, &plan, &OperatorRegistry::native())
                    .cut()
                    .expect("the installation is a cut")
                    .widths;
                // The interpreter arms with the clock of its latest tick.
                let clock = self.clock;
                self.harness.set_wall(clock);
                assert_eq!(self.harness.arm_cut(&widths), *at, "event {}: the cut's time", position);
                self.replaced = Some(std::mem::replace(&mut self.active, plan));
            }
            InstallOutcome::Active { .. } => {
                assert!(
                    !cut,
                    "event {}: the interpreter retained, so the regions' bindings do",
                    position
                );
                self.harness.commit();
                self.active = plan;
            }
            other => panic!("event {}: an installation that starts is {:?}", position, other),
        }
    }

    /// Finalizes the cut in flight once every admission point feeding state passed it, and then starts the
    /// installation waiting for it.
    fn finalize_if_passed(&mut self, outcomes: &BTreeMap<usize, &InstallOutcome>) {
        if self.replaced.is_none() || !self.harness.cut_passed() {
            return;
        }
        self.harness.finalize_cut();
        self.replaced = None;
        if let Some((position, plan)) = self.waiting.take() {
            self.start(position, plan, outcomes);
        }
    }
}

/// Checks that the interpreter and the frame both delivered `records` to `output`.
#[track_caller]
fn assert_delivered(run: &RunOutput, replay: &Replay, output: &str, records: &[(u64, u64, &str)]) {
    let expected = expected(records);
    assert_eq!(interpreted(run, output), expected, "the interpreter's `{}`", output);
    assert_eq!(framed(&replay.harness, output), expected, "the frame's `{}`", output);
}

#[test]
fn trace_1_a_backlog_at_the_cut_lands_in_the_old_lineage() {
    let trace = Trace::new()
        .install(aggregated("10s", "a"))
        .admit(src(), t(95), vec![counter("c", 1.0)])
        .heartbeat(src(), ms(98_500))
        .install(aggregated("5s", "a"))
        .advance(t(101))
        .admit(src(), t(99), vec![counter("c", 2.0)])
        .heartbeat(src(), ms(100_500))
        .heartbeat(src(), t(116))
        .advance(t(116));
    let (run, replay) = Replay::run(&trace, false, t(90));
    assert_delivered(&run, &replay, "a", &[(1, 100_000, "c rate/10s [90:3]")]);
}

#[test]
fn trace_2_a_late_batch_without_a_cut_lands_in_its_open_bucket() {
    let trace = Trace::new()
        .install(aggregated("10s", "a"))
        .admit(src(), t(95), vec![counter("c", 1.0)])
        .advance(t(101))
        .admit(src(), t(99), vec![counter("c", 2.0)])
        .heartbeat(src(), ms(100_500))
        .advance(t(116));
    let (run, replay) = Replay::run(&trace, false, t(90));
    assert_delivered(&run, &replay, "a", &[(1, 100_000, "c rate/10s [90:3]")]);
}

#[test]
fn trace_3_a_changed_width_cuts_at_the_common_boundary() {
    let trace = Trace::new()
        .install(aggregated("10s", "a"))
        .admit(src(), t(95), vec![counter_with_unit("c", 1.0, "second")])
        .install(aggregated("30s", "a"))
        .advance(t(101))
        .admit(src(), t(105), vec![counter_with_unit("c", 1.0, "second")])
        .advance(t(111))
        .admit(src(), t(115), vec![counter_with_unit("c", 1.0, "second")])
        .admit(src(), t(121), vec![counter_with_unit("c", 1.0, "millisecond")])
        .advance(t(151));
    let (run, replay) = Replay::run(&trace, true, t(90));
    assert_delivered(
        &run,
        &replay,
        "a",
        &[
            (1, 101_000, "c rate/10s [90:1] unit=second"),
            (1, 111_000, "c rate/10s [100:1] unit=second"),
            (1, 120_000, "c rate/10s [110:1] unit=second"),
            (2, 151_000, "c rate/30s [120:1] unit=millisecond"),
        ],
    );
}

#[test]
fn trace_4_a_changed_destination_retains_and_flushes_through_the_new_plan() {
    // The frame is the boot plan's, which must already deliver to `b`: a copy to both outputs, replaced by an emit to
    // `b` alone.
    let only_b = plan(
        vec![
            dogstatsd("untimestamped"),
            node("agg", "aggregate", aggregation("10s")),
            node("copy", "tee", json!({"ports": 2})),
            node("out_b", "emit", json!({"output": "b"})),
            node("idle", "drop", json!({})),
        ],
        &[
            "src.metrics -> agg.in",
            "agg.out -> copy.in",
            "copy.0 -> idle.in",
            "copy.1 -> out_b.in",
        ],
        &["a", "b"],
    );
    let trace = Trace::new()
        .install(aggregated_to_both("10s"))
        .admit(src(), t(95), vec![counter("c", 1.0)])
        .install(only_b)
        .advance(t(101));
    let (run, replay) = Replay::run(&trace, true, t(90));
    assert_delivered(&run, &replay, "a", &[]);
    assert_delivered(&run, &replay, "b", &[(2, 101_000, "c rate/10s [90:1]")]);
}

#[test]
fn trace_5_a_small_backward_jump_is_absorbed_by_the_admission_clock() {
    // The admission clock is at 103 when the wall clock reads 97: it stamps 103 again, so both batches land in
    // [100, 110). The interpreter takes stamps as given, and a stamp that moves back within the reset threshold is
    // ill-formed there, so its trace stamps 103 twice; the frame's admission point stamps the batch at 97 itself.
    let trace = Trace::new()
        .install(aggregated("10s", "a"))
        .admit(src(), t(103), vec![counter("c", 1.0)])
        .admit(src(), t(103), vec![counter("c", 2.0)])
        .advance(t(111));
    let (run, replay) = Replay::run(&trace, true, t(100));
    assert_delivered(&run, &replay, "a", &[(1, 111_000, "c rate/10s [100:3]")]);

    let boot = aggregated("10s", "a");
    let mut harness = FrameHarness::starting_at(
        &boot,
        &report(&boot),
        OperatorRegistry::native(),
        HarnessOptions::default(),
        t(100),
    );
    harness.admit("src", t(103), vec![counter("c", 1.0)]);
    harness.admit("src", t(97), vec![counter("c", 2.0)]);
    harness.heartbeat("src", t(111));
    harness.advance(t(111));
    assert_eq!(framed(&harness, "a"), expected(&[(1, 111_000, "c rate/10s [100:3]")]));
}

#[test]
fn trace_6_a_large_backward_jump_resets_and_closes_every_open_bucket() {
    let trace = Trace::new()
        .install(aggregated("10s", "a"))
        .admit(src(), t(7_185), vec![counter("i", 1.0)])
        .admit(src(), t(7_201), vec![counter("c", 1.0)])
        .admit(src(), t(3_603), vec![counter("c", 2.0)])
        .advance(t(3_615))
        .advance(t(3_630));
    let (run, replay) = Replay::run(&trace, true, t(3_600));
    assert_delivered(
        &run,
        &replay,
        "a",
        &[
            (1, 7_201_000, "i rate/10s [7180:1]"),
            (1, 7_201_000, "c rate/10s [7200:1]"),
            (1, 3_615_000, "c rate/10s [3600:2]"),
            (1, 3_630_000, "c rate/10s [3610:0, 3620:0]"),
        ],
    );
}

#[test]
fn trace_7_a_rollback_is_a_new_installation_with_its_own_cut() {
    // Without a queue between regions, each crossing happens at the first stamp of the generation replacing the
    // lineage, so the two lineages of one key never coexist here; `state_tests` holds the queued form, where they do.
    let trace = Trace::new()
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
        .advance(t(121));
    let (run, replay) = Replay::run(&trace, false, t(90));
    assert_delivered(
        &run,
        &replay,
        "a",
        &[
            (1, 100_000, "c rate/10s [90:1]"),
            (2, 110_000, "c rate/5s [100:1, 105:1]"),
            (3, 121_000, "c rate/10s [110:1]"),
        ],
    );
}

#[test]
fn trace_8_a_late_timestamped_point_belongs_to_the_new_generation() {
    let trace = Trace::new()
        .install(aggregated_with(aggregation("10s"), "a", "mixed"))
        .heartbeat(src(), ms(98_500))
        .install(aggregated_with(aggregation("5s"), "a", "mixed"))
        .admit(src(), t(103), vec![gauge_at("g", 95, 1.0)])
        .heartbeat(src(), t(106))
        .advance(t(106));
    let (run, replay) = Replay::run(&trace, false, t(90));
    assert_delivered(&run, &replay, "a", &[(2, 106_000, "g gauge [95:1]")]);
}

#[test]
fn trace_9_a_future_timestamped_point_is_flushed_at_the_old_lineages_retirement() {
    let trace = Trace::new()
        .install(aggregated_with(aggregation("10s"), "a", "mixed"))
        .admit(src(), t(98), vec![gauge_at("g", 130, 1.0)])
        .heartbeat(src(), ms(98_500))
        .install(aggregated_with(aggregation("5s"), "a", "mixed"))
        .heartbeat(src(), ms(100_500))
        .heartbeat(src(), t(141))
        .advance(t(141));
    let (run, replay) = Replay::run(&trace, false, t(90));
    assert_delivered(&run, &replay, "a", &[(1, 100_000, "g gauge [130:1]")]);
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
    let (run, replay) = Replay::run(
        &armed().cancel().heartbeat(src(), ms(100_500)).advance(t(101)),
        true,
        t(90),
    );
    assert_delivered(&run, &replay, "a", &[(1, 101_000, "c rate/10s [90:1]")]);
    // The aborted generation's lineage is gone from the isolation slot.
    assert_eq!(
        replay
            .harness
            .drivers()
            .iter()
            .map(|driver| driver.lineages().len())
            .sum::<usize>(),
        1
    );

    let (run, replay) = Replay::run(
        &armed().heartbeat(src(), ms(100_500)).cancel().advance(t(101)),
        true,
        t(90),
    );
    assert_delivered(&run, &replay, "a", &[(1, 100_000, "c rate/10s [90:1]")]);
}

#[test]
fn trace_11_a_reset_after_publication_switches_the_point_to_the_new_generation() {
    let trace = Trace::new()
        .install(aggregated("10s", "a"))
        .admit(src(), t(7_195), vec![counter("c", 1.0)])
        .heartbeat(src(), ms(7_198_500))
        .install(aggregated("5s", "a"))
        .heartbeat(src(), t(3_601))
        .admit(src(), t(3_602), vec![counter("c", 2.0)])
        .heartbeat(src(), t(3_606))
        .advance(t(3_606));
    let (run, replay) = Replay::run(&trace, false, t(3_590));
    assert_delivered(
        &run,
        &replay,
        "a",
        &[
            (1, 7_198_500, "c rate/10s [7190:1]"),
            (2, 3_606_000, "c rate/5s [3600:2]"),
        ],
    );

    // A reset before publication: the cut's time and the epochs it records come from the clock after the reset.
    let trace = Trace::new()
        .install(aggregated("10s", "a"))
        .admit(src(), t(7_201), vec![counter("c", 1.0)])
        .heartbeat(src(), t(3_601))
        .install(aggregated("5s", "a"))
        .admit(src(), t(3_605), vec![counter("c", 1.0)])
        .heartbeat(src(), ms(3_610_500));
    let (run, replay) = Replay::run(&trace, false, t(3_590));
    assert_delivered(
        &run,
        &replay,
        "a",
        &[
            (1, 7_201_000, "c rate/10s [7200:1]"),
            (1, 3_610_000, "c rate/10s [3600:1]"),
        ],
    );
}

/// `src -> keep -> agg -> copy -> emit` to `a` and `b` (or to `b` alone, `retargeted`), with a filter before
/// aggregation, so that a region runs before the isolation slot and batches can wait between the two.
fn filtered(window: &str, retargeted: bool) -> Plan {
    let mut nodes = vec![
        dogstatsd("untimestamped"),
        node("keep", "split", json!({"predicate": {"name": {"prefix": "c"}}})),
        node("dropped", "drop", json!({})),
        node("agg", "aggregate", aggregation(window)),
        node("copy", "tee", json!({"ports": 2})),
        node("out_b", "emit", json!({"output": "b"})),
    ];
    let mut edges = vec![
        "src.metrics -> keep.in",
        "keep.true -> agg.in",
        "keep.false -> dropped.in",
        "agg.out -> copy.in",
        "copy.1 -> out_b.in",
    ];
    if retargeted {
        nodes.push(node("idle", "drop", json!({})));
        edges.push("copy.0 -> idle.in");
    } else {
        nodes.push(node("out", "emit", json!({"output": "a"})));
        edges.push("copy.0 -> out.in");
    }
    plan(nodes, &edges, &["a", "b"])
}

fn filtered_harness(boot: &Plan, start: AdmissionTime) -> FrameHarness {
    FrameHarness::starting_at(
        boot,
        &report(boot),
        OperatorRegistry::native(),
        HarnessOptions::default(),
        start,
    )
}

#[test]
fn trace_11_queued_epoch_zero_batches_are_the_old_lineages_and_flushed_unclamped() {
    // The point stamps 7,201 (epoch 0), then resets to 3,601 (epoch 1), while both batches wait between the filter's
    // region and the isolation slot; the cut is armed with the post-reset clock and the wall clock at 3,605 (T = 3,610,
    // epochs {p: 1}). The queued epoch-0 batch is pre-cut whatever its `t_a`: the old lineage aggregates it, and the
    // reset drain at the first epoch-1 header flushes it at 7,201, unclamped (no flush moves back, nothing trips a
    // guard). The old lineage then crosses at the first header at or after 3,610, holding the epoch-1 count, and the
    // new lineage holds what came after.
    let (ten, five) = (filtered("10s", false), filtered("5s", false));
    let mut harness = filtered_harness(&ten, t(3_590));
    harness.hold_between_slots();
    harness.admit("src", t(7_201), vec![counter("c", 1.0)]);
    harness.admit("src", t(3_601), vec![counter("c", 1.0)]);
    assert_eq!(harness.held(), 2, "both batches wait before the isolation slot");
    assert!(harness.prepare(&five, &report(&five)));
    harness.set_wall(t(3_605));
    assert_eq!(
        harness.arm_cut(&[Duration::from_secs(10), Duration::from_secs(5)]),
        t(3_610)
    );
    harness.stop_holding();
    harness.release();
    harness.heartbeat("src", ms(3_610_500));
    assert!(harness.cut_passed());
    harness.finalize_cut();
    harness.admit("src", t(3_612), vec![counter("c", 1.0)]);
    harness.close(t(3_620));
    let records = expected(&[
        (1, 7_201_000, "c rate/10s [7200:1]"),
        (1, 3_610_000, "c rate/10s [3600:1]"),
        (2, 3_620_000, "c rate/5s [3610:1]"),
    ]);
    assert_eq!(framed(&harness, "a"), records);
    assert_eq!(framed(&harness, "b"), records);
}

/// `src -> by_name`, whose `a.` names `agg_a` aggregates over 10 seconds, and, `with_b`, whose other names `agg_b`
/// aggregates over `window_b`, the two outputs joined into output `a`; without `agg_b` they are dropped.
fn by_name(with_b: bool, window_b: &str) -> Plan {
    let mut nodes = vec![
        dogstatsd("untimestamped"),
        node("by_name", "split", json!({"predicate": {"name": {"prefix": "a."}}})),
        node("agg_a", "aggregate", aggregation("10s")),
        node("out_a", "emit", json!({"output": "a"})),
    ];
    let mut edges = vec!["src.metrics -> by_name.in", "by_name.true -> agg_a.in"];
    if with_b {
        nodes.extend([
            node("agg_b", "aggregate", aggregation(window_b)),
            node("joined", "merge", json!({"inputs": 2})),
        ]);
        edges.extend([
            "by_name.false -> agg_b.in",
            "agg_a.out -> joined.0",
            "agg_b.out -> joined.1",
            "joined.out -> out_a.in",
        ]);
    } else {
        nodes.push(node("rest", "drop", json!({})));
        edges.extend(["by_name.false -> rest.in", "agg_a.out -> out_a.in"]);
    }
    plan(nodes, &edges, &["a"])
}

#[test]
fn trace_12_an_added_state_owner_starts_its_lineage_at_the_cut() {
    // The second plan adds `agg_b`, whose output joins `agg_a`'s, as a frame runs a second state owner over the boot
    // plan's edge into its isolation slot: a cut at 100, while `agg_a` is retained throughout. `b.y` is dropped
    // before the cut, and aggregated by `agg_b` from [100, 110) on. `agg_a`'s flush at 99, before any point switched to
    // the new generation, is the old generation's, which cancelling the cut would keep; after the cut, its state, from
    // both generations, is flushed together under the newest.
    let split = |with_second: bool| by_name(with_second, "10s");
    let trace = Trace::new()
        .install(split(false))
        .admit(src(), t(85), vec![counter("a.x", 1.0)])
        .admit(src(), t(95), vec![counter("a.x", 1.0), counter("b.y", 1.0)])
        .heartbeat(src(), ms(98_500))
        .install(split(true))
        .advance(t(99))
        .admit(src(), t(99), vec![counter("a.x", 1.0), counter("b.y", 1.0)])
        .admit(src(), t(101), vec![counter("a.x", 1.0), counter("b.y", 1.0)])
        .advance(t(111));
    let (run, mut replay) = Replay::run(&trace, true, t(80));
    assert_delivered(
        &run,
        &replay,
        "a",
        &[
            (1, 99_000, "a.x rate/10s [80:1]"),
            (2, 111_000, "a.x rate/10s [90:2, 100:1]"),
            (2, 111_000, "b.y rate/10s [100:1]"),
        ],
    );
    replay.harness.publish();
    assert_eq!(replay.harness.telemetry().loss("rest", LossClass::Policy), 2);
    assert_eq!(run.loss(&id("rest")).loss(LossClass::Policy).points, 2);
}

#[test]
fn trace_13_a_retain_after_the_cut_flushes_the_old_lineage_under_the_generation_it_served() {
    let trace = Trace::new()
        .install(aggregated_to_both("10s"))
        .admit(src(), t(95), vec![counter("c", 1.0)])
        .heartbeat(src(), ms(98_500))
        .install(aggregated_to_both("5s"))
        .heartbeat(src(), ms(100_100))
        .install(retargeted_to_b("5s"))
        .admit(src(), t(101), vec![counter("c", 1.0)])
        .advance(t(106))
        .advance(t(121));
    let (run, replay) = Replay::run(&trace, true, t(90));
    assert_delivered(&run, &replay, "a", &[(1, 100_000, "c rate/10s [90:1]")]);
    assert_delivered(
        &run,
        &replay,
        "b",
        &[
            (1, 100_000, "c rate/10s [90:1]"),
            (3, 106_000, "c rate/5s [100:1]"),
            (3, 121_000, "c rate/5s [105:0, 110:0, 115:0]"),
        ],
    );
}

/// `src -> agg -> copy`, the copy's first port dropped and its second emitted to `b`: aggregation retained with a new
/// destination.
fn retargeted_to_b(window: &str) -> Plan {
    plan(
        vec![
            dogstatsd("untimestamped"),
            node("agg", "aggregate", aggregation(window)),
            node("copy", "tee", json!({"ports": 2})),
            node("out_b", "emit", json!({"output": "b"})),
            node("idle", "drop", json!({})),
        ],
        &[
            "src.metrics -> agg.in",
            "agg.out -> copy.in",
            "copy.0 -> idle.in",
            "copy.1 -> out_b.in",
        ],
        &["a", "b"],
    )
}

#[test]
fn trace_13_queued_the_old_lineage_crosses_after_a_later_retain_and_still_flushes_under_its_own_generation() {
    // The cut at 100 finalizes at 100.1, when the heartbeat passes the point; the heartbeat then waits before the
    // isolation slot past 106, so the old lineage has not crossed when a `retain` (generation 3, to `b` alone) commits.
    // Generation 2 is no longer in the schedule, but the retiring lineage holds it strongly: once the heartbeat
    // arrives, the old lineage makes its final flush at 100, under generation 1, the one it served, with no zero at or
    // after 100. The tick at 106 waits at the gate for the watermark, which the heartbeat at 121 raises past it before
    // the tick at 121: the new lineage flushes [100, 105) at 106, and its zeros at 121.
    let (ten, five, retained) = (filtered("10s", false), filtered("5s", false), filtered("5s", true));
    let mut harness = filtered_harness(&ten, t(90));
    harness.admit("src", t(95), vec![counter("c", 1.0)]);
    harness.heartbeat("src", ms(98_500));
    assert!(harness.prepare(&five, &report(&five)));
    assert_eq!(
        harness.arm_cut(&[Duration::from_secs(5), Duration::from_secs(10)]),
        t(100)
    );
    harness.hold_between_slots();
    harness.heartbeat("src", ms(100_100));
    assert_eq!(harness.held(), 1, "the heartbeat waits before the isolation slot");
    assert!(harness.cut_passed());
    harness.finalize_cut();
    assert!(
        !harness.prepare(&retained, &report(&retained)),
        "a change of destination retains"
    );
    harness.commit();
    harness.admit("src", t(101), vec![counter("c", 1.0)]);
    harness.advance(t(106));
    assert!(framed(&harness, "a").is_empty(), "nothing has crossed yet");
    harness.stop_holding();
    harness.release();
    harness.heartbeat("src", t(121));
    harness.advance(t(121));
    assert_eq!(framed(&harness, "a"), expected(&[(1, 100_000, "c rate/10s [90:1]")]));
    assert_eq!(
        framed(&harness, "b"),
        expected(&[
            (1, 100_000, "c rate/10s [90:1]"),
            (3, 106_000, "c rate/5s [100:1]"),
            (3, 121_000, "c rate/5s [105:0, 110:0, 115:0]"),
        ])
    );
    let lineages = harness
        .drivers()
        .iter()
        .flat_map(|driver| driver.lineages())
        .map(|(_, created_by, retired)| (created_by.get(), retired))
        .collect::<Vec<_>>();
    // The old lineage retired: it holds nothing and binds nothing new, and stays until generation 1's program is
    // released, which the harness never does.
    assert_eq!(lineages, vec![(1, true), (2, false)]);
}

#[test]
fn trace_14_a_revocation_cancels_an_armed_cut_and_the_default_program_retains_its_lineage() {
    // While armed, the revocation cancels the artifact's cut, and the default program installs again, retaining its
    // lineage: nothing of the artifact is delivered. (The irrevocable phase needs two upstream admission points; see
    // the module documentation.)
    let trace = Trace::new()
        .install(aggregated("10s", "a"))
        .admit(src(), t(95), vec![counter("c", 1.0)])
        .install(aggregated("30s", "a"))
        .cancel()
        .install(aggregated("10s", "a"))
        .advance(t(101));
    let (run, replay) = Replay::run(&trace, true, t(90));
    assert_delivered(&run, &replay, "a", &[(3, 101_000, "c rate/10s [90:1]")]);
}

#[test]
fn trace_15_re_compositions_during_a_cut_wait_and_the_last_commits_as_retain() {
    let recomposed = |context_limit: u64| {
        let mut params = aggregation("30s");
        params["context_limit"] = json!(context_limit);
        aggregated_with(params, "a", "untimestamped")
    };
    let trace = Trace::new()
        .install(aggregated("10s", "a"))
        .admit(src(), t(95), vec![counter("c", 1.0)])
        .install(recomposed(1000))
        .install(recomposed(999))
        .install(recomposed(998))
        .install(recomposed(1))
        .admit(src(), t(121), vec![counter("c", 1.0)])
        .admit(src(), t(122), vec![counter("c", 1.0), counter("d", 1.0)])
        .advance(t(151));
    let (run, mut replay) = Replay::run(&trace, true, t(90));
    assert_delivered(
        &run,
        &replay,
        "a",
        &[(1, 120_000, "c rate/10s [90:1]"), (3, 151_000, "c rate/30s [120:2]")],
    );
    // The last re-composition's context limit of 1 applies to the retained lineage from its commit on: `d`, new at
    // 122, is capacity loss, as in the interpreter.
    replay.harness.publish();
    assert_eq!(replay.harness.telemetry().loss("agg", LossClass::Capacity), 1);
    assert_eq!(run.loss(&id("agg")).loss(LossClass::Capacity).points, 1);
}

// Budgets of state.

/// `src -> agg -> emit` to `a`, aggregating over `window` with a context limit of `context_limit`.
fn limited(window: &str, context_limit: u64) -> Plan {
    let mut params = aggregation(window);
    params["context_limit"] = json!(context_limit);
    aggregated_with(params, "a", "untimestamped")
}

fn counters(names: &[&'static str]) -> Vec<Metric> {
    names.iter().map(|name| counter(name, 1.0)).collect()
}

#[test]
fn a_cut_at_seventy_percent_of_the_state_budget_loses_no_context_to_capacity() {
    // Ten contexts fit, between every lineage. The old lineage holds seven when the cut to 5 seconds is armed (T =
    // 100); the first batch of the new generation, seven new contexts, crosses: the old lineage makes its final flush
    // at 100, releasing its seven, before the new lineage admits any. Fourteen contexts never compete for ten.
    let old = ["c0", "c1", "c2", "c3", "c4", "c5", "c6"];
    let new = ["d0", "d1", "d2", "d3", "d4", "d5", "d6"];
    let trace = Trace::new()
        .install(limited("10s", 10))
        .admit(src(), t(95), counters(&old))
        .install(limited("5s", 10))
        .admit(src(), t(101), counters(&new))
        .advance(t(111));
    let options = HarnessOptions {
        state_capacity: Some(10),
        ..HarnessOptions::default()
    };
    let (run, mut replay) = Replay::run_with(&trace, true, t(90), options);
    let mut records = old
        .iter()
        .map(|name| (1, 100_000, format!("{} rate/10s [90:1]", name)))
        .collect::<Vec<_>>();
    records.extend(new.iter().map(|name| (2, 111_000, format!("{} rate/5s [100:1]", name))));
    let records = records
        .iter()
        .map(|(generation, at, record)| (*generation, *at, record.as_str()))
        .collect::<Vec<_>>();
    assert_delivered(&run, &replay, "a", &records);
    replay.harness.publish();
    assert_eq!(replay.harness.telemetry().loss("agg", LossClass::Capacity), 0);
    assert_eq!(run.loss(&id("agg")).loss(LossClass::Capacity).points, 0);
    // The old lineage released its seven contexts at the crossing: the budget holds the new lineage's seven, which it
    // keeps until its counters expire, after five minutes.
    assert_eq!(replay.harness.budgets().global().used(), 7);
}

#[test]
fn state_owners_share_the_global_budget_and_each_keeps_to_its_own_limit() {
    // Two aggregations over one point, each in an isolation slot of its own, share a budget of three contexts. `agg_b`
    // may hold one context: `b.y` is its capacity loss. `agg_a` may hold ten, but the budget is full after `a.x`,
    // `a.y`, and `b.x`: `a.z` is its capacity loss. Every lineage's limit and the shared budget apply, in the frame as
    // in the interpreter.
    let mut limit_one = aggregation("10s");
    limit_one["context_limit"] = json!(1);
    let mut limit_ten = aggregation("10s");
    limit_ten["context_limit"] = json!(10);
    let both = plan(
        vec![
            dogstatsd("untimestamped"),
            node("by_name", "split", json!({"predicate": {"name": {"prefix": "a."}}})),
            node("agg_a", "aggregate", limit_ten),
            node("agg_b", "aggregate", limit_one),
            node("out_a", "emit", json!({"output": "a"})),
            node("out_b", "emit", json!({"output": "b"})),
        ],
        &[
            "src.metrics -> by_name.in",
            "by_name.true -> agg_a.in",
            "by_name.false -> agg_b.in",
            "agg_a.out -> out_a.in",
            "agg_b.out -> out_b.in",
        ],
        &["a", "b"],
    );
    let trace = Trace::new()
        .install(both)
        .admit(src(), t(1), counters(&["a.x", "a.y", "b.x", "b.y"]))
        .admit(src(), t(2), counters(&["a.z", "a.x"]))
        .advance(t(15));
    let options = HarnessOptions {
        state_capacity: Some(3),
        ..HarnessOptions::default()
    };
    let (run, mut replay) = Replay::run_with(&trace, true, t(0), options);
    assert_delivered(
        &run,
        &replay,
        "a",
        &[(1, 15_000, "a.x rate/10s [0:2]"), (1, 15_000, "a.y rate/10s [0:1]")],
    );
    assert_delivered(&run, &replay, "b", &[(1, 15_000, "b.x rate/10s [0:1]")]);
    replay.harness.publish();
    for node in ["agg_a", "agg_b"] {
        assert_eq!(
            replay.harness.telemetry().loss(node, LossClass::Capacity),
            1,
            "{}",
            node
        );
        assert_eq!(run.loss(&id(node)).loss(LossClass::Capacity).points, 1, "{}", node);
    }
}

// Beyond the normative traces: the slot's watermark, resets during a gated tick, and the order of drains.

#[test]
fn a_cut_that_adds_a_state_owner_of_another_width_lines_up_with_the_widths_of_the_owners_it_changes() {
    // `agg_a` (10 seconds) is retained and `agg_b` (5 seconds) added: only `agg_b`'s lineage starts at the cut, so the
    // cut lines up with 5 seconds alone, at 7205, the first multiple of 5 a second after the latest stamp, 7201.
    // `agg_b`, empty at the tick at 7202, records no flush there, so its first flush adds no zeros.
    let trace = Trace::new()
        .install(by_name(false, "5s"))
        .admit(src(), t(7_201), vec![counter("a.x", 1.0), counter("b.y", 1.0)])
        .install(by_name(true, "5s"))
        .advance(t(7_202))
        .admit(src(), t(7_206), vec![counter("a.x", 1.0), counter("b.y", 1.0)])
        .advance(t(7_216));
    let (run, replay) = Replay::run(&trace, true, t(7_200));
    let InstallOutcome::Cut { at, .. } = &run.installs()[1].outcome else {
        panic!("the second plan installs at a cut: {:?}", run.installs())
    };
    assert_eq!(*at, t(7_205));
    assert_delivered(
        &run,
        &replay,
        "a",
        &[
            (2, 7_216_000, "a.x rate/10s [7200:2]"),
            (2, 7_216_000, "b.y rate/5s [7205:1]"),
        ],
    );
}

#[test]
fn a_lineage_a_cut_starts_gates_its_first_tick_on_the_slots_watermark() {
    // The isolation slot keeps one watermark: the 5-second lineage the cut at 7210 starts gates its first tick, at
    // 7212, on the slot's 7207, as the old lineage does, rather than skipping it. The tick fires at the end of its
    // grace at the watermark's 7211, a flush the new lineage records since it holds `c0` by then, so its flush at
    // shutdown gives `c1`, first seen at 7216.9, a zero back to align(7211) (the native rule for idle counters), and
    // discards the open [7215, 7220).
    let trace = Trace::new()
        .install(aggregated("10s", "a"))
        .advance(t(7_200))
        .admit(src(), t(7_207), vec![counter("c0", 1.0)])
        .install(aggregated("5s", "a"))
        .advance(t(7_212))
        .admit(src(), ms(7_211_900), vec![counter("c0", 1.0)])
        .advance(ms(7_217_500))
        .admit(src(), ms(7_216_900), vec![counter("c1", 1.0)])
        .close();
    let (run, replay) = Replay::run(&trace, false, t(7_190));
    assert_delivered(
        &run,
        &replay,
        "a",
        &[
            (1, 7_210_000, "c0 rate/10s [7200:1]"),
            (2, 7_217_500, "c0 rate/5s [7210:1]"),
            (2, 7_217_500, "c1 rate/5s [7210:0]"),
        ],
    );
}

#[test]
fn a_reset_while_a_tick_is_gated_drops_the_tick_and_the_first_flush_after_it_adds_no_zeros() {
    // The tick at 7201.2 gates on the watermark's 7200.5; an hour's backward jump at 3601.3 resets the point within
    // its grace. The reset drain flushes `c0` at the watermark before it and empties the lineage, and the gated tick,
    // whose second is the old timeline's, is dropped. The next tick, at 7204, gates on the new timeline and fires at
    // the end of its grace at the watermark's 3630: it is the lineage's first flush since the reset, so it adds no
    // zeros.
    let trace = Trace::new()
        .install(aggregated("10s", "a"))
        .advance(t(7_200))
        .admit(src(), ms(7_200_500), vec![counter("c0", 1.0)])
        .advance(ms(7_201_200))
        .admit(src(), ms(3_601_300), vec![counter("n", 1.0)])
        .heartbeat(src(), t(3_602))
        .advance(t(7_204))
        .admit(src(), t(3_612), vec![counter("m", 1.0)])
        .heartbeat(src(), t(3_630))
        .advance(t(7_210));
    let (run, mut replay) = Replay::run(&trace, false, t(7_190));
    assert_delivered(
        &run,
        &replay,
        "a",
        &[
            (1, 7_200_500, "c0 rate/10s [7200:1]"),
            (1, 3_630_000, "n rate/10s [3600:1]"),
            (1, 3_630_000, "m rate/10s [3610:1]"),
        ],
    );
    // Only the tick at 7204 ran to the end of its grace; the dropped one never did.
    replay.harness.publish();
    assert_eq!(
        replay
            .harness
            .telemetry()
            .value("agg_region", crate::runtime::PLAN_STATE_TICK_GRACE_EXPIRED_TOTAL),
        1
    );
}

#[test]
fn drains_that_flush_at_different_times_are_stamped_in_plan_order_of_their_state_owners() {
    // `agg_b` joins `agg_a` in the boot frame's isolation slot at a cut at 7200; `agg_a`'s window then changes to 5
    // seconds at a cut at 7210, which gives it a new lineage, created after `agg_b`'s; and a third plan removes `agg_b`
    // at a cut at 7220, during which input ends. At shutdown `agg_a` closes at 7221 and the retiring `agg_b` at the
    // cut's 7220, both under generation 3. The output clock latches each stamp to the latest before it, so the order of
    // the two drains decides `b.y`'s: in plan order, `agg_a` drains first, and `b.y` is latched to 7221, whatever order
    // the region holds the lineages in.
    let by_name_over = |window_a: &str, with_b: bool| {
        let mut plan = by_name(with_b, "10s");
        let agg_a = plan
            .nodes
            .iter_mut()
            .find(|node| node.id == id("agg_a"))
            .expect("the plan aggregates `a.` names");
        agg_a.params = serde_json::from_value(aggregation(window_a)).unwrap();
        plan
    };
    let trace = Trace::new()
        .install(by_name_over("10s", false))
        .admit(src(), t(7_195), vec![counter("a.x", 1.0)])
        .install(by_name_over("10s", true))
        .admit(src(), t(7_201), vec![counter("a.x", 1.0), counter("b.y", 1.0)])
        .install(by_name_over("5s", true))
        .admit(src(), t(7_211), vec![counter("a.x", 1.0), counter("b.y", 1.0)])
        .install(by_name_over("5s", false))
        .advance(t(7_221))
        .close();
    let (run, replay) = Replay::run(&trace, false, t(7_190));
    let cuts = run
        .installs()
        .iter()
        .filter_map(|record| match &record.outcome {
            InstallOutcome::Cut { at, .. } => Some(*at),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(cuts, vec![t(7_200), t(7_210), t(7_220)]);
    assert_delivered(
        &run,
        &replay,
        "a",
        &[
            (2, 7_210_000, "a.x rate/10s [7190:1, 7200:1]"),
            (3, 7_221_000, "a.x rate/5s [7210:1]"),
            (3, 7_221_000, "b.y rate/10s [7200:1, 7210:1]"),
        ],
    );
}
