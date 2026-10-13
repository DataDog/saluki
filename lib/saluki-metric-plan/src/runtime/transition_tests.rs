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
    data_model::{
        event::metric::{context::Context, Metric, MetricMetadata, MetricValues},
        tags::TagSet,
    },
    execution::{AdmissionPointId, AdmissionTime, GenerationId, LossClass},
};
use serde_json::{json, Value};
use stringtheory::MetaString;

use super::harness::{
    DeliveredRecord, DeliverySchedule, FrameHarness, HarnessOptions, LaneTimeEvent, LanedFrameHarness,
};
use crate::{
    canonical::PlanHash,
    compile::{KeyGroups, LanePolicy},
    install::diff,
    interpret::{replay::LaneObservation, InstallOutcome, Interpreter, RunOutput, Trace, TraceEvent},
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

/// What a replay knows of one source's clock and each of its lanes' own stamps, as their admission points keep them:
/// one latest time and epoch per source, and each lane's latest time and epoch, for heartbeats at ticks.
#[derive(Default)]
struct ClockModel {
    latest: Option<AdmissionTime>,
    epoch: u32,
    lanes: BTreeMap<u16, (AdmissionTime, u32)>,
}

impl ClockModel {
    /// Records a stamp of `lane` at `at`: a jump back beyond the reset threshold from the clock's latest time resets
    /// the clock, for every lane.
    fn stamped(&mut self, lane: u16, at: AdmissionTime) {
        let threshold = limits::values::CLOCK_RESET_THRESHOLD.as_nanos() as u64;
        match self.latest {
            Some(latest) if at < latest && latest.as_unix_nanos() - at.as_unix_nanos() > threshold => {
                self.epoch = self.epoch.wrapping_add(1);
                self.latest = Some(at);
            }
            latest => self.latest = Some(latest.map_or(at, |latest| latest.max(at))),
        }
        let epoch = self.epoch;
        let own = self.lanes.entry(lane).or_insert((at, epoch));
        *own = match *own {
            (latest, own_epoch) if own_epoch == epoch => (latest.max(at), epoch),
            _ => (at, epoch),
        };
    }

    /// Returns `true` if `lane` has stamped nothing at or after `clock` in its clock's epoch.
    fn stale(&self, lane: u16, clock: AdmissionTime) -> bool {
        self.lanes
            .get(&lane)
            .is_none_or(|(latest, epoch)| *latest < clock || *epoch != self.epoch)
    }
}

/// A trace replayed through a frame of region drivers, following the installations its interpreter run decided.
pub(super) struct Replay {
    pub(super) harness: FrameHarness,
    /// The plan of the active generation, and of the generation a cut in flight replaces.
    active: Plan,
    replaced: Option<Plan>,
    /// The installation waiting for the cut in flight, with its event's position: the latest wins.
    waiting: Option<(usize, Plan)>,
    /// Each source's clock, for heartbeats at ticks.
    clocks: BTreeMap<NodeId, ClockModel>,
    /// The lanes of each source the frame runs on lanes.
    lanes: BTreeMap<NodeId, u16>,
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
        if let Some(lanes) = LANES.with(std::cell::Cell::get) {
            let trace = on_lanes(trace, lanes);
            return Self::run_on_lanes(
                &trace,
                heartbeats_at_ticks,
                start,
                options,
                lanes,
                DeliverySchedule::TraceOrder,
            );
        }
        let boot = boot_of(trace);
        let harness = FrameHarness::starting_at(&boot, &report(&boot), OperatorRegistry::native(), options, start);
        Self::run_on(trace, heartbeats_at_ticks, options, harness)
    }

    /// Runs `trace` as [`run_with`][Self::run_with] does, through a frame laid out on `lanes` without pipelining,
    /// delivering between lanes as `schedule` says, and through the interpreter with the frame's lanes of each source.
    pub(super) fn run_on_lanes(
        trace: &Trace, heartbeats_at_ticks: bool, start: AdmissionTime, options: HarnessOptions, lanes: LanePolicy,
        schedule: DeliverySchedule,
    ) -> (RunOutput, Self) {
        let boot = boot_of(trace);
        let harness = LanedFrameHarness::new(
            &boot,
            &report(&boot),
            OperatorRegistry::native(),
            options,
            start,
            lanes,
            schedule,
        )
        .unwrap_or_else(|error| panic!("{}", error))
        .into_harness();
        Self::run_on(trace, heartbeats_at_ticks, options, harness)
    }

    fn run_on(
        trace: &Trace, heartbeats_at_ticks: bool, options: HarnessOptions, harness: FrameHarness,
    ) -> (RunOutput, Self) {
        let registry = OperatorRegistry::native();
        let catalog = catalog();
        let lanes = harness
            .frame()
            .admission_points()
            .iter()
            .filter(|point| point.lanes > 1)
            .map(|point| (point.node.clone(), point.lanes))
            .collect::<BTreeMap<_, _>>();
        let interpreter = lanes
            .iter()
            .fold(Interpreter::new(&registry, &catalog), |interpreter, (source, lanes)| {
                interpreter.with_source_lanes(source.clone(), *lanes)
            });
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
        let replay = Self::follow(trace, &run, heartbeats_at_ticks, harness, lanes);
        (run, replay)
    }

    /// Replays `trace` through the frame harness, following the interpreter's `run`: each installation as the
    /// interpreter decided it (committed, armed at its cut, waiting, cancelled, or refused).
    fn follow(
        trace: &Trace, run: &RunOutput, heartbeats_at_ticks: bool, harness: FrameHarness, lanes: BTreeMap<NodeId, u16>,
    ) -> Self {
        let outcomes = run
            .installs()
            .iter()
            .map(|record| (record.event, &record.outcome))
            .collect::<BTreeMap<_, _>>();
        let events = trace.events();
        let boot = boot_of(trace);
        // The interpreter's clock before its first tick, which it arms a cut with.
        let clock = AdmissionTime::UNIX_EPOCH;
        let mut replay = Self {
            harness,
            active: boot,
            replaced: None,
            waiting: None,
            clocks: BTreeMap::new(),
            lanes,
            clock,
            heartbeats_at_ticks,
        };
        for (position, event) in events.iter().enumerate().skip(1) {
            match event {
                TraceEvent::Admit {
                    source,
                    lane,
                    admitted_at,
                    batch,
                } => {
                    replay
                        .clocks
                        .entry(source.clone())
                        .or_default()
                        .stamped(*lane, *admitted_at);
                    replay
                        .harness
                        .admit_at_lane(source.local().as_str(), *lane, *admitted_at, batch.clone());
                    replay.finalize_if_passed(&outcomes);
                }
                TraceEvent::Heartbeat {
                    source,
                    lane,
                    admitted_at,
                } => replay.heartbeat(source, *lane, *admitted_at, &outcomes),
                TraceEvent::Advance { clock } => {
                    // As the interpreter does: idle lanes heartbeat at the tick's clock first, which can finalize a
                    // cut, and start the installation waiting for it, before the tick.
                    if replay.heartbeats_at_ticks {
                        let stale = replay
                            .sources()
                            .into_iter()
                            .flat_map(|source| {
                                let lanes = replay.lanes.get(&source).copied().unwrap_or(1);
                                (0..lanes).map(move |lane| (source.clone(), lane))
                            })
                            .filter(|(source, lane)| {
                                replay
                                    .clocks
                                    .get(source)
                                    .is_none_or(|clock_model| clock_model.stale(*lane, *clock))
                            })
                            .collect::<Vec<_>>();
                        for (source, lane) in stale {
                            replay.heartbeat(&source, lane, *clock, &outcomes);
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

    fn heartbeat(
        &mut self, source: &NodeId, lane: u16, at: AdmissionTime, outcomes: &BTreeMap<usize, &InstallOutcome>,
    ) {
        self.clocks.entry(source.clone()).or_default().stamped(lane, at);
        self.harness.heartbeat_at_lane(source.local().as_str(), lane, at);
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

/// Runs `trace` through the interpreter and through a frame on `lanes`, as [`Replay::run_on_lanes`] does, with the
/// default options, and returns the interpreter's run and the frame's harness.
pub(super) fn replay_on_lanes(
    trace: &Trace, heartbeats_at_ticks: bool, start: AdmissionTime, lanes: LanePolicy, schedule: DeliverySchedule,
) -> (RunOutput, FrameHarness) {
    let (run, replay) = Replay::run_on_lanes(
        trace,
        heartbeats_at_ticks,
        start,
        HarnessOptions::default(),
        lanes,
        schedule,
    );
    (run, replay.harness)
}

/// Returns the plan a trace installs first, which boots its frame.
fn boot_of(trace: &Trace) -> Plan {
    match trace.events().first() {
        Some(TraceEvent::Install { plan }) => Plan::clone(plan),
        _ => panic!("a trace starts with its boot plan's installation"),
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

/// Returns the trace of [`drains_that_flush_at_different_times_are_stamped_in_plan_order_of_their_state_owners`],
/// whose `a.` counter is `a` and whose `b.` counter is `b`.
fn drains_at_different_times(a: &Metric, b: &Metric) -> Trace {
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
    Trace::new()
        .install(by_name_over("10s", false))
        .admit(src(), t(7_195), vec![a.clone()])
        .install(by_name_over("10s", true))
        .admit(src(), t(7_201), vec![a.clone(), b.clone()])
        .install(by_name_over("5s", true))
        .admit(src(), t(7_211), vec![a.clone(), b.clone()])
        .install(by_name_over("5s", false))
        .advance(t(7_221))
        .close()
}

#[test]
fn drains_that_flush_at_different_times_are_stamped_in_plan_order_of_their_state_owners() {
    // `agg_b` joins `agg_a` in the boot frame's isolation slot at a cut at 7200; `agg_a`'s window then changes to 5
    // seconds at a cut at 7210, which gives it a new lineage, created after `agg_b`'s; and a third plan removes `agg_b`
    // at a cut at 7220, during which input ends. At shutdown `agg_a` closes at 7221 and the retiring `agg_b` at the
    // cut's 7220, both under generation 3. The output clock latches each stamp to the latest before it, so the order of
    // the two drains decides `b.y`'s: in plan order, `agg_a` drains first, and `b.y` is latched to 7221, whatever order
    // the region holds the lineages in.
    let trace = drains_at_different_times(&counter("a.x", 1.0), &counter("b.y", 1.0));
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

#[test]
fn on_lanes_a_lanes_latch_stamps_its_drains_of_one_event_and_never_another_lanes() {
    // The trace above on the lanes the stage 6 traces run on: a state lane's latch takes the stamps of the drains that
    // lane emits. With `a.` and `b.` on one state lane, it emits `agg_a`'s drain at 7221 first, in plan order, and
    // latches `b.`'s to 7221, as the single latch does. On two, `b.`'s lane emits nothing of `agg_a`'s drain, so its
    // latch stamps `b.`'s drain at its own flush time, the retiring owner's cut at 7220: within one event, the drains
    // of two state owners of one slot flush at decreasing times, which per-lane latches stamp apart (6.7, as amended).
    LANES.with(|lanes| lanes.set(Some(TRACE_LANES)));
    let state = TRACE_LANES.state;
    for (b_lane, b_stamp) in [(0, 7_221_000), (1, 7_220_000)] {
        let a = counter_on_lane("a.x", 0, state, 1.0);
        let b = counter_on_lane("b.y", b_lane, state, 1.0);
        let (an, bn) = (name_of(&a), name_of(&b));
        let (run, replay) = Replay::run(&drains_at_different_times(&a, &b), false, t(7_190));
        let a_10s = format!("{} rate/10s [7190:1, 7200:1]", an);
        let a_5s = format!("{} rate/5s [7210:1]", an);
        let b_10s = format!("{} rate/10s [7200:1, 7210:1]", bn);
        let single = [
            (2, 7_210_000, a_10s.as_str()),
            (3, 7_221_000, a_5s.as_str()),
            (3, 7_221_000, b_10s.as_str()),
        ];
        assert_eq!(interpreted(&run, "a"), expected(&single), "the interpreter's `a`");
        let on_lanes = [
            (2, 7_210_000, a_10s.as_str()),
            (3, 7_221_000, a_5s.as_str()),
            (3, b_stamp, b_10s.as_str()),
        ];
        assert_eq!(
            framed(&replay.harness, "a"),
            expected(&on_lanes),
            "the frame's `a`, `b.` on state lane {}",
            b_lane
        );
    }
    LANES.with(|lanes| lanes.set(None));
}

#[test]
fn stage_6_tests_of_one_trace_each_deliver_their_expectations_on_lanes() {
    // The stage 6 tests of one trace each beyond traces 1 to 15, on the lanes the traces run on, on four state lanes,
    // and on two producer lanes into three state lanes: each holds its own expectations, as on one point. Two pin what
    // lanes make capacity behavior: a cut at 70% of the budget, where one point's crossing releases the old lineage's
    // contexts before the new one admits any, while on several points the two hold contexts at once between the first
    // point's switch and the last's (amendment 128); and two state owners sharing a budget, whose last unit goes to
    // whichever lane's context takes it first. The drains of two state owners of one slot that flush at decreasing
    // times are stamped per lane (above).
    for lanes in [
        TRACE_LANES,
        LanePolicy {
            dogstatsd: 3,
            state: 4,
            post: 1,
        },
        LanePolicy {
            dogstatsd: 2,
            state: 3,
            post: 2,
        },
    ] {
        LANES.with(|cell| cell.set(Some(lanes)));
        a_reset_while_a_tick_is_gated_drops_the_tick_and_the_first_flush_after_it_adds_no_zeros();
        a_lineage_a_cut_starts_gates_its_first_tick_on_the_slots_watermark();
        a_cut_that_adds_a_state_owner_of_another_width_lines_up_with_the_widths_of_the_owners_it_changes();
        LANES.with(|cell| cell.set(None));
    }
}

// The traces on lanes (6.7): stage 6's traces at D = 3 and P = 2 under 6.7's point mapping, and traces T-L1 to T-L7,
// on frames whose state slot runs on lanes, through the real region drivers.

/// The admission point of producer lane 1 of `src`: the frame numbers the source's lane points first.
const PRODUCER_LANE_1: AdmissionPointId = AdmissionPointId::new(1);

thread_local! {
    /// The lanes [`Replay::run_with`] runs its frame on, while a test replays the stage 6 traces on lanes; `None` runs
    /// them as written, on one lane per slot.
    static LANES: std::cell::Cell<Option<LanePolicy>> = const { std::cell::Cell::new(None) };
}

/// The producer, state, and post-aggregation lanes the stage 6 traces run on (6.7).
const TRACE_LANES: LanePolicy = LanePolicy {
    dogstatsd: 3,
    state: 2,
    post: 1,
};

/// Returns `plan` with a filter that keeps everything between the source and what the source feeds, so that its
/// aggregation runs behind a fused slot, whose keyed exit is the repartition edge: a point with lanes feeds no state
/// slot on lanes directly.
fn behind_a_fused_slot(plan: &Plan) -> Plan {
    let mut value = serde_json::to_value(plan).expect("a plan serializes");
    let mut fed = Vec::new();
    for edge in value["edges"].as_array_mut().expect("a plan has edges") {
        if edge["from"].as_str().is_some_and(|from| from.starts_with("src.")) {
            fed.push(edge["to"].clone());
            edge["to"] = json!("lanes_keep.in");
        }
    }
    let edges = value["edges"].as_array_mut().expect("a plan has edges");
    match fed.as_slice() {
        [to] => edges.push(json!({"from": "lanes_keep.true", "to": to})),
        _ => panic!("the stage 6 traces' plans feed one node from the source"),
    }
    edges.push(json!({"from": "lanes_keep.false", "to": "lanes_kept_none.in"}));
    let nodes = value["nodes"].as_array_mut().expect("a plan has nodes");
    nodes.push(node("lanes_keep", "split", json!({"predicate": {"const": true}})));
    nodes.push(node("lanes_kept_none", "drop", json!({})));
    let plan = Plan::from_value(value).expect("the plan with a filter in front is well-formed");
    report(&plan);
    plan
}

/// Returns a stage 6 trace as it runs on [`TRACE_LANES`]: every plan behind a fused slot, and its stamps spread over
/// the producer lanes, each mirrored as heartbeats of the other lanes ([`Trace::over_lanes`]).
fn on_lanes(trace: &Trace, lanes: LanePolicy) -> Trace {
    let plans = trace
        .events()
        .iter()
        .map(|event| match event {
            TraceEvent::Install { plan } => TraceEvent::Install {
                plan: std::sync::Arc::new(behind_a_fused_slot(plan)),
            },
            other => other.clone(),
        })
        .collect::<Trace>();
    plans.over_lanes(&src(), lanes.dogstatsd)
}

#[test]
fn stage_6_traces_deliver_their_expectations_on_three_producer_lanes_and_two_state_lanes() {
    // Each trace runs with its own hand-written expectations, through the interpreter on three lanes of `src` and
    // through a frame on `TRACE_LANES`: every state lane's watermark equals the single point's after each event, so
    // ticks, gates, expired graces, `W_pre`, and crossings happen at stage 6's instants and seconds. Traces 5 and 10
    // pin one-lane details (the admission clock's own stamp, the slot's lineage count), and the queued forms hold
    // buffers by hand; trace L3 runs trace 11's queued form on lanes.
    LANES.with(|lanes| lanes.set(Some(TRACE_LANES)));
    trace_1_a_backlog_at_the_cut_lands_in_the_old_lineage();
    trace_2_a_late_batch_without_a_cut_lands_in_its_open_bucket();
    trace_3_a_changed_width_cuts_at_the_common_boundary();
    trace_4_a_changed_destination_retains_and_flushes_through_the_new_plan();
    trace_6_a_large_backward_jump_resets_and_closes_every_open_bucket();
    trace_7_a_rollback_is_a_new_installation_with_its_own_cut();
    trace_8_a_late_timestamped_point_belongs_to_the_new_generation();
    trace_9_a_future_timestamped_point_is_flushed_at_the_old_lineages_retirement();
    trace_11_a_reset_after_publication_switches_the_point_to_the_new_generation();
    trace_12_an_added_state_owner_starts_its_lineage_at_the_cut();
    trace_13_a_retain_after_the_cut_flushes_the_old_lineage_under_the_generation_it_served();
    trace_14_a_revocation_cancels_an_armed_cut_and_the_default_program_retains_its_lineage();
    trace_15_re_compositions_during_a_cut_wait_and_the_last_commits_as_retain();
    LANES.with(|lanes| lanes.set(None));
}

#[test]
fn stage_6_traces_deliver_their_expectations_on_three_producer_lanes_and_one_state_lane() {
    // Increment 9.3's layout: one state lane fed by three producer points, through the producer slot's `One` exit,
    // whose several upstream points send their final headers before a newer epoch's and their generation changes at
    // once. The traces' expectations hold as on one point.
    LANES.with(|lanes| {
        lanes.set(Some(LanePolicy {
            state: 1,
            ..TRACE_LANES
        }))
    });
    trace_1_a_backlog_at_the_cut_lands_in_the_old_lineage();
    trace_2_a_late_batch_without_a_cut_lands_in_its_open_bucket();
    trace_3_a_changed_width_cuts_at_the_common_boundary();
    trace_4_a_changed_destination_retains_and_flushes_through_the_new_plan();
    trace_6_a_large_backward_jump_resets_and_closes_every_open_bucket();
    trace_7_a_rollback_is_a_new_installation_with_its_own_cut();
    trace_8_a_late_timestamped_point_belongs_to_the_new_generation();
    trace_9_a_future_timestamped_point_is_flushed_at_the_old_lineages_retirement();
    trace_11_a_reset_after_publication_switches_the_point_to_the_new_generation();
    trace_12_an_added_state_owner_starts_its_lineage_at_the_cut();
    trace_13_a_retain_after_the_cut_flushes_the_old_lineage_under_the_generation_it_served();
    trace_14_a_revocation_cancels_an_armed_cut_and_the_default_program_retains_its_lineage();
    trace_15_re_compositions_during_a_cut_wait_and_the_last_commits_as_retain();
    LANES.with(|lanes| lanes.set(None));
}

#[test]
fn stage_6_traces_deliver_their_expectations_through_the_laned_harness_on_one_lane() {
    // The machinery the traces on lanes run through, on one lane per slot: every plan behind a fused slot, laid out
    // without pipelining, run by a laned harness; each trace's own expectations hold.
    LANES.with(|lanes| lanes.set(Some(LanePolicy::ONE)));
    trace_1_a_backlog_at_the_cut_lands_in_the_old_lineage();
    trace_2_a_late_batch_without_a_cut_lands_in_its_open_bucket();
    trace_3_a_changed_width_cuts_at_the_common_boundary();
    trace_4_a_changed_destination_retains_and_flushes_through_the_new_plan();
    trace_6_a_large_backward_jump_resets_and_closes_every_open_bucket();
    trace_7_a_rollback_is_a_new_installation_with_its_own_cut();
    trace_8_a_late_timestamped_point_belongs_to_the_new_generation();
    trace_9_a_future_timestamped_point_is_flushed_at_the_old_lineages_retirement();
    trace_11_a_reset_after_publication_switches_the_point_to_the_new_generation();
    trace_12_an_added_state_owner_starts_its_lineage_at_the_cut();
    trace_13_a_retain_after_the_cut_flushes_the_old_lineage_under_the_generation_it_served();
    trace_14_a_revocation_cancels_an_armed_cut_and_the_default_program_retains_its_lineage();
    trace_15_re_compositions_during_a_cut_wait_and_the_last_commits_as_retain();
    LANES.with(|lanes| lanes.set(None));
}

/// Returns a counter of a context named `prefix` and a number, the first whose key the key groups of `lanes` state
/// lanes give lane `lane`, with the number of that name.
fn counter_on_lane(prefix: &str, lane: u16, lanes: u16, value: f64) -> Metric {
    let groups = KeyGroups::new(lanes);
    (0..)
        .map(|index| Context::from_parts(format!("{}{}", prefix, index), TagSet::default()))
        .find(|context| groups.lane_of(context.key().as_u64()) == lane)
        .map(|context| Metric::counter(context, value))
        .expect("some name lands on every lane")
}

/// Returns the name of a metric.
fn name_of(metric: &Metric) -> String {
    metric.context().name().to_string()
}

/// `src -> keep -> agg -> emit a`, aggregating over `window`, behind a fused slot.
fn kept(window: &str) -> Plan {
    behind_a_fused_slot(&aggregated(window, "a"))
}

#[test]
fn trace_l1_a_cut_crosses_each_state_lane_on_its_own_and_values_equal_one_lane() {
    // Two producer lanes, two state lanes, the cut to 5 seconds at T = 100. What state lane 1 receives from producer
    // lane 1 is delayed by three progress periods. Producer lane 0's new-generation batch with `x` and `y` reaches
    // each lane's new instance while its old one still holds them: at each lane a context seen on both sides of the
    // cut holds two units of the node's budget, one per instance, until the lane crosses. Lane 0 crosses at producer
    // lane 1's batch at 100.6; lane 1, which hears of it 300 ms later, only at producer lane 1's heartbeat at 101: the
    // lanes cross separately. Values equal the interpreter's, and one lane's.
    let (x, y) = (counter_on_lane("x", 0, 2, 1.0), counter_on_lane("y", 1, 2, 1.0));
    let trace = Trace::new()
        .install(kept("10s"))
        .admit_at_lane(src(), 0, t(95), vec![x.clone(), y.clone()])
        .heartbeat_at_lane(src(), 1, t(96))
        .heartbeat_at_lane(src(), 0, ms(98_500))
        .heartbeat_at_lane(src(), 1, ms(98_500))
        .install(kept("5s"))
        .admit_at_lane(src(), 0, ms(100_500), vec![x.clone(), y.clone()])
        .admit_at_lane(src(), 1, ms(100_600), vec![x.clone()])
        .heartbeat_at_lane(src(), 1, t(101))
        .advance(t(106))
        .close();
    let lanes = LanePolicy {
        dogstatsd: 2,
        state: 2,
        post: 1,
    };
    let delayed = DeliverySchedule::Delay {
        point: PRODUCER_LANE_1,
        lane: 1,
        by: limits::values::PROGRESS_PERIOD * 3,
    };
    let (run, replay) = Replay::run_on_lanes(&trace, true, t(90), HarnessOptions::default(), lanes, delayed);
    let (run_one, replay_one) = Replay::run(&on_one_lane(&trace), true, t(90));
    let records = interpreted(&run, "a");
    assert_eq!(framed(&replay.harness, "a"), records, "the frame on lanes");
    assert_eq!(interpreted(&run_one, "a"), records, "the interpreter on one lane");
    assert_eq!(framed(&replay_one.harness, "a"), records, "the frame on one lane");
    let names = [name_of(&x), name_of(&y)];
    assert!(
        interpreted(&run, "a")
            .iter()
            .any(|(generation, _, record)| *generation == 1 && record.starts_with(&names[1])),
        "the old lineage flushes `y` at its crossing"
    );

    // The crossings, step by step: the overlap each lane counts, the units the node's budget holds, and each lane's
    // crossing. Each step lets the keyed exits' holds expire, so that what a producer lane routed reaches the lanes.
    let boot = kept("10s");
    let options = HarnessOptions {
        record_budgets: true,
        ..HarnessOptions::default()
    };
    let mut harness = LanedFrameHarness::new(
        &boot,
        &report(&boot),
        OperatorRegistry::native(),
        options,
        t(90),
        lanes,
        delayed,
    )
    .unwrap();
    let slot = harness
        .frame()
        .slots()
        .iter()
        .find(|slot| slot.is_isolation())
        .map(|slot| slot.name.to_string())
        .expect("the frame has a state slot");
    let step = |harness: &mut LanedFrameHarness| {
        harness.elapse(Duration::from_millis(10));
        harness.publish();
        let overlap = (0..2)
            .map(|lane| {
                harness
                    .telemetry()
                    .lane_gauge(&slot, "plan_lane_crossing_overlap_contexts", lane)
            })
            .collect::<Vec<_>>();
        let held = harness
            .budget_traces()
            .into_iter()
            .find(|(budget, _)| budget == "agg")
            .map_or(0, |(_, trace)| trace.held());
        let crossed = (0..2)
            .map(|lane| {
                harness
                    .time_events(&slot, lane)
                    .iter()
                    .any(|event| matches!(event, LaneTimeEvent::Crossed { .. }))
            })
            .collect::<Vec<_>>();
        (overlap, held, crossed)
    };
    harness.admit_at_lane("src", 0, t(95), vec![x.clone(), y.clone()]);
    harness.heartbeat_at_lane("src", 1, t(96));
    harness.heartbeat_at_lane("src", 0, ms(98_500));
    harness.heartbeat_at_lane("src", 1, ms(98_500));
    let five = kept("5s");
    assert!(harness.prepare(&five, &report(&five)));
    assert_eq!(
        harness.arm_cut(&[Duration::from_secs(10), Duration::from_secs(5)]),
        t(100)
    );
    harness.admit_at_lane("src", 0, ms(100_500), vec![x.clone(), y.clone()]);
    assert_eq!(
        step(&mut harness),
        (vec![1.0, 1.0], 4, vec![false, false]),
        "each lane holds its context in both lineages"
    );
    harness.admit_at_lane("src", 1, ms(100_600), vec![x.clone()]);
    assert_eq!(
        step(&mut harness),
        (vec![0.0, 1.0], 3, vec![true, false]),
        "lane 0 crossed, lane 1 still holds `y` twice"
    );
    harness.heartbeat_at_lane("src", 1, t(101));
    assert_eq!(
        step(&mut harness),
        (vec![0.0, 0.0], 2, vec![true, true]),
        "lane 1 crossed"
    );
}

/// Returns `trace` with every stamp at lane 0: the one-lane run a run on lanes is compared with. The lanes' stamps are
/// in stamp order, so one point stamps them as well.
fn on_one_lane(trace: &Trace) -> Trace {
    trace
        .events()
        .iter()
        .map(|event| match event {
            TraceEvent::Admit {
                source,
                admitted_at,
                batch,
                ..
            } => TraceEvent::Admit {
                source: source.clone(),
                lane: 0,
                admitted_at: *admitted_at,
                batch: batch.clone(),
            },
            TraceEvent::Heartbeat {
                source, admitted_at, ..
            } => TraceEvent::Heartbeat {
                source: source.clone(),
                lane: 0,
                admitted_at: *admitted_at,
            },
            other => other.clone(),
        })
        .collect()
}

/// The admission point of producer lane 0 of `src`.
const PRODUCER_LANE_0: AdmissionPointId = AdmissionPointId::new(0);

#[test]
fn trace_l2_a_reset_drains_every_state_lane_at_the_exact_final_old_epoch_watermark() {
    // `x` lives on state lane 0, `y` on state lane 1; both flushed at the tick at 7,190. Producer lane 0 then sends
    // five old-epoch batches, the last, at 7,195, of `x` only, without a tick between, so that its keyed exit still
    // holds them when its clock resets at 3,601; producer lane 1, idle after its heartbeat at 7,196, closes the windows
    // with its heartbeat at 3,604. What producer lane 0 sends state lane 1 from its reset on waits until it stamped
    // 1.5 seconds later, so state lane 0 opens its window first, and the tick at 3,602 is due between the two lanes'
    // openings.
    //
    // State lane 1 learns producer lane 0's final old-epoch time, 7,195, from the header the exit forwards, though the
    // batch went to state lane 0 only: both lanes drain the old epoch at the same watermark, the earlier final time,
    // 7,195, `x` and `y` with their open [7190, 7200). The tick between the openings fires in neither lane: lane 0's
    // window is open, and the reset mark keeps lane 1, whose window is not, from firing an old-epoch tick past its
    // cutoff, whose zeros of `y` and flush time no single core has.
    let (x, y) = (counter_on_lane("x", 0, 2, 1.0), counter_on_lane("y", 1, 2, 1.0));
    let (xn, yn) = (name_of(&x), name_of(&y));
    let mut trace = Trace::new().install(kept("10s"));
    for second in 7_181..=7_190 {
        trace = if second == 7_181 {
            trace.admit_at_lane(src(), 0, t(second), vec![x.clone(), y.clone()])
        } else {
            trace.heartbeat_at_lane(src(), 0, t(second))
        };
        trace = trace.heartbeat_at_lane(src(), 1, t(second)).advance(t(second));
    }
    for second in 7_191..=7_195 {
        let batch = if second == 7_195 {
            vec![x.clone()]
        } else {
            vec![x.clone(), y.clone()]
        };
        trace = trace.admit_at_lane(src(), 0, t(second), batch);
    }
    let mut trace = trace
        .heartbeat_at_lane(src(), 1, t(7_196))
        .admit_at_lane(src(), 0, t(3_601), vec![x.clone()])
        .advance(t(3_602))
        .heartbeat_at_lane(src(), 0, t(3_603))
        .advance(t(3_603))
        .heartbeat_at_lane(src(), 1, t(3_604))
        .advance(t(3_604));
    for second in 3_605..=3_611 {
        trace = trace
            .heartbeat_at_lane(src(), 0, t(second))
            .heartbeat_at_lane(src(), 1, t(second))
            .advance(t(second));
    }
    let trace = trace.close();
    let lanes = LanePolicy {
        dogstatsd: 2,
        state: 2,
        post: 1,
    };
    let delayed = DeliverySchedule::DelayFrom {
        point: PRODUCER_LANE_0,
        lane: 1,
        epoch: 1,
        by: Duration::from_millis(1_500),
    };
    let options = HarnessOptions {
        monotonic_across_jumps: Some(Duration::from_secs(1)),
        ..HarnessOptions::default()
    };
    let (run, replay) = Replay::run_on_lanes(&trace, false, t(7_180), options, lanes, delayed);
    let records = [
        (1, 7_190_000, format!("{} rate/10s [7180:1]", xn)),
        (1, 7_190_000, format!("{} rate/10s [7180:1]", yn)),
        (1, 7_195_000, format!("{} rate/10s [7190:5]", xn)),
        (1, 7_195_000, format!("{} rate/10s [7190:4]", yn)),
        (1, 3_610_000, format!("{} rate/10s [3600:1]", xn)),
    ];
    let records = records
        .iter()
        .map(|(generation, at, record)| (*generation, *at, record.as_str()))
        .collect::<Vec<_>>();
    assert_delivered(&run, &replay, "a", &records);
    // No lane fired an old-epoch tick the other did not. (Lane 0 counts a tick resolved apart: inside its window it
    // skipped the tick at 3,602 holding `x` of the new epoch, which a lane whose window had closed would have fired.)
    let slot = replay
        .harness
        .frame()
        .slots()
        .iter()
        .find(|slot| slot.is_isolation())
        .map(|slot| slot.name.to_string())
        .expect("the frame has a state slot");
    let old_ticks = |lane: u16| {
        replay
            .harness
            .time_events(&slot, lane)
            .into_iter()
            .filter_map(|event| match event {
                LaneTimeEvent::Ticked {
                    epoch: 0,
                    ordinal,
                    flush,
                    ..
                } => Some((ordinal, flush)),
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(old_ticks(1), old_ticks(0), "the lanes' old-epoch ticks");
}

#[test]
fn trace_l3_a_reset_after_a_cuts_publication_retires_the_old_lineage_at_its_windows_close_in_either_order() {
    // The interpreter's T-L3 (interpret::lane_tests), whose expectation is hand-written there, on two producer lanes
    // and two state lanes, with `c` on state lane 1, and a tick at 7,201 that sends what the keyed exits hold and
    // gates, and that the reset drops. In trace order that lane receives producer lane 1's new-generation batch before
    // producer lane 0's new-epoch header; with what producer lane 1 sends it delayed by a second, the header first.
    // Whichever it receives first, the old lineage's reset drain is its final retirement, at 7,198.5.
    let c = |value| counter_on_lane("c", 1, 2, value);
    let name = name_of(&c(1.0));
    let trace = Trace::new()
        .install(kept("10s"))
        .admit_at_lane(src(), 0, t(7_195), vec![c(1.0)])
        .heartbeat_at_lane(src(), 1, t(7_196))
        .heartbeat_at_lane(src(), 0, ms(7_198_500))
        .install(kept("5s"))
        .admit_at_lane(src(), 1, ms(7_200_500), vec![c(5.0)])
        .advance(t(7_201))
        .heartbeat_at_lane(src(), 0, t(3_601))
        .admit_at_lane(src(), 0, t(3_602), vec![c(2.0)])
        .heartbeat_at_lane(src(), 1, t(3_603))
        .heartbeat_at_lane(src(), 0, t(3_606))
        .heartbeat_at_lane(src(), 1, t(3_606))
        .advance(t(3_606))
        .close();
    let records = [
        (1, 7_198_500, format!("{} rate/10s [7190:1]", name)),
        (2, 7_198_500, format!("{} rate/5s [7200:5]", name)),
        (2, 3_606_000, format!("{} rate/5s [3600:2]", name)),
    ];
    let records = records
        .iter()
        .map(|(generation, at, record)| (*generation, *at, record.as_str()))
        .collect::<Vec<_>>();
    let lanes = LanePolicy {
        dogstatsd: 2,
        state: 2,
        post: 1,
    };
    for (schedule, header_first) in [
        (DeliverySchedule::TraceOrder, false),
        (
            DeliverySchedule::Delay {
                point: PRODUCER_LANE_1,
                lane: 1,
                by: Duration::from_secs(1),
            },
            true,
        ),
    ] {
        let (run, replay) = Replay::run_on_lanes(&trace, false, t(7_190), HarnessOptions::default(), lanes, schedule);
        assert_delivered(&run, &replay, "a", &records);
        // The order in which `c`'s lane received producer lane 0's new-epoch header and producer lane 1's batch.
        let observations = replay.harness.replay();
        let observations = observations.observations(1);
        let position = |wanted: &dyn Fn(&LaneObservation) -> bool| {
            observations.iter().position(wanted).expect("the lane observed it")
        };
        let header = position(&|observation| {
            matches!(
                observation,
                LaneObservation::Progress { lane: 0, epoch: 1, .. }
                    | LaneObservation::Admitted { lane: 0, epoch: 1, .. }
            )
        });
        let batch = position(&|observation| {
            matches!(
                observation,
                LaneObservation::Admitted { lane: 1, admitted_at, .. } if *admitted_at == ms(7_200_500)
            )
        });
        assert_eq!(header < batch, header_first, "{:?}", schedule);
    }
}

#[test]
fn trace_l3_a_plan_waiting_for_a_cut_starts_once_every_lane_passed_it_though_a_window_holds_the_old_lineage() {
    // The interpreter's form (interpret::lane_tests) on two producer lanes and two state lanes, with `c` on state lane
    // 1: the cut to 5 seconds finalizes once both producer lanes passed it, at producer lane 0's reset at 3,601, though
    // the windows still hold the old lineage, and the plan waiting for it starts then, its cut at T = 3,610, which the
    // harness, finalizing from the points' own stamps as the installer does, arms at the same time.
    let c = |value| counter_on_lane("c", 1, 2, value);
    let name = name_of(&c(1.0));
    let trace = Trace::new()
        .install(kept("10s"))
        .admit_at_lane(src(), 0, t(7_195), vec![c(1.0)])
        .heartbeat_at_lane(src(), 1, t(7_196))
        .heartbeat_at_lane(src(), 0, ms(7_198_500))
        .install(kept("5s"))
        .install(kept("10s"))
        .admit_at_lane(src(), 1, ms(7_200_500), vec![c(5.0)])
        .heartbeat_at_lane(src(), 0, t(3_601))
        .admit_at_lane(src(), 0, t(3_602), vec![c(2.0)])
        .heartbeat_at_lane(src(), 1, t(3_615))
        .heartbeat_at_lane(src(), 0, t(3_621))
        .heartbeat_at_lane(src(), 1, t(3_621))
        .advance(t(3_621))
        .close();
    let lanes = LanePolicy {
        dogstatsd: 2,
        state: 2,
        post: 1,
    };
    let (run, replay) = Replay::run_on_lanes(
        &trace,
        false,
        t(7_190),
        HarnessOptions::default(),
        lanes,
        DeliverySchedule::TraceOrder,
    );
    let waiting = run
        .installs()
        .iter()
        .find(|record| record.event == 5)
        .map(|record| &record.outcome);
    assert!(
        matches!(waiting, Some(InstallOutcome::Cut { at, .. }) if *at == t(3_610)),
        "{:?}",
        waiting
    );
    let records = [
        (1, 7_198_500, format!("{} rate/10s [7190:1]", name)),
        (2, 7_198_500, format!("{} rate/5s [7200:5]", name)),
        (2, 3_610_000, format!("{} rate/5s [3600:2]", name)),
    ];
    let records = records
        .iter()
        .map(|(generation, at, record)| (*generation, *at, record.as_str()))
        .collect::<Vec<_>>();
    assert_delivered(&run, &replay, "a", &records);
}

#[test]
fn trace_l4_a_preparation_one_of_sixteen_state_lanes_refuses_is_aborted_in_every_lane_and_never_stamped() {
    // Sixteen state lanes; state lane 9 refuses the cut's preparation, its state owner's kernel built for another
    // lane. As the coordinator does with the first refusal, the candidate is aborted in every lane of every slot: no
    // lane holds its generation, nothing stamps it, and the boot generation keeps admitting and flushing. The next
    // candidate, prepared on every lane, takes its identifier.
    let boot = kept("10s");
    let lanes = LanePolicy {
        dogstatsd: 1,
        state: 16,
        post: 1,
    };
    let mut harness = LanedFrameHarness::new(
        &boot,
        &report(&boot),
        OperatorRegistry::native(),
        HarnessOptions::default(),
        t(90),
        lanes,
        DeliverySchedule::TraceOrder,
    )
    .unwrap();
    let slot = harness
        .frame()
        .slots()
        .iter()
        .find(|slot| slot.is_isolation())
        .map(|slot| slot.id)
        .expect("the frame has a state slot");
    harness.admit("src", t(95), vec![counter("c", 1.0)]);
    let five = kept("5s");
    let refusal = harness
        .prepare_refused_by(&five, &report(&five), slot, 9)
        .expect_err("state lane 9 refuses the candidate");
    assert!(refusal.contains("lane 9"), "{}", refusal);
    for driver in harness.drivers() {
        assert_eq!(
            driver.generations(),
            vec![GenerationId::new(1)],
            "slot {:?} lane {:?} holds only the boot generation",
            driver.slot(),
            driver.lane()
        );
    }
    assert_eq!(harness.active().id(), GenerationId::new(1));
    harness.heartbeat("src", t(101));
    harness.advance(t(101));
    assert_eq!(framed(&harness, "a"), expected(&[(1, 101_000, "c rate/10s [90:1]")]));
    assert!(harness.prepare(&five, &report(&five)));
    for driver in harness.drivers() {
        assert_eq!(
            driver.generations(),
            vec![GenerationId::new(1), GenerationId::new(2)],
            "the next candidate takes the refused one's identifier"
        );
    }
}

#[test]
fn trace_l3_queued_trace_11_on_lanes_the_fresh_old_lineage_instance_crosses_at_the_cut() {
    // Stage 6's queued trace 11 at D = 3, P = 2: the producer lanes stamp 7,201 and then reset to 3,601, mirrored at
    // every lane, while both batches wait before the state lanes; the cut is armed with the post-reset clock (T =
    // 3,610). Released, the old-epoch batch drains at 7,201 unclamped, the epoch-1 batch reaches a fresh instance of
    // the old lineage, which keeps the cut's mark and crosses at 3,610.5.
    let (ten, five) = (filtered("10s", false), filtered("5s", false));
    let mut harness = LanedFrameHarness::new(
        &ten,
        &report(&ten),
        OperatorRegistry::native(),
        HarnessOptions::default(),
        t(3_590),
        TRACE_LANES,
        DeliverySchedule::TraceOrder,
    )
    .unwrap();
    harness.hold_between_slots();
    let mirrored = |harness: &mut LanedFrameHarness, lane: u16, at: AdmissionTime, batch: Vec<Metric>| {
        harness.admit_at_lane("src", lane, at, batch);
        for other in (0..TRACE_LANES.dogstatsd).filter(|other| *other != lane) {
            harness.heartbeat_at_lane("src", other, at);
        }
    };
    mirrored(&mut harness, 0, t(7_201), vec![counter("c", 1.0)]);
    mirrored(&mut harness, 1, t(3_601), vec![counter("c", 1.0)]);
    assert!(harness.prepare(&five, &report(&five)));
    harness.set_wall(t(3_605));
    assert_eq!(
        harness.arm_cut(&[Duration::from_secs(10), Duration::from_secs(5)]),
        t(3_610)
    );
    harness.stop_holding();
    harness.release();
    for lane in 0..TRACE_LANES.dogstatsd {
        harness.heartbeat_at_lane("src", lane, ms(3_610_500));
    }
    assert!(harness.cut_passed());
    harness.finalize_cut();
    mirrored(&mut harness, 2, t(3_612), vec![counter("c", 1.0)]);
    harness.close(t(3_620));
    let records = expected(&[
        (1, 7_201_000, "c rate/10s [7200:1]"),
        (1, 3_610_000, "c rate/10s [3600:1]"),
        (2, 3_620_000, "c rate/5s [3610:1]"),
    ]);
    assert_eq!(framed(&harness, "a"), records);
    assert_eq!(framed(&harness, "b"), records);
}

#[test]
fn trace_l6_lanes_finishing_apart_across_a_bucket_boundary_share_one_close_time() {
    // The state lanes finish 200 ms apart, across the boundary at 7,210: the first to finish sets the lineage's close
    // time, the later of its epoch's latest stamp, 7,209.5, and its wall clock, 7,209.9, and the other closes at it
    // too. [7200, 7210) is open then, so both lanes discard their share of it, `x` and `y`, as shutdown loss: neither
    // flushes what the other discards, which lane 1's own time, 7,210.1, would have.
    let (x, y) = (counter_on_lane("x", 0, 2, 1.0), counter_on_lane("y", 1, 2, 2.0));
    let boot = kept("10s");
    let mut harness = LanedFrameHarness::new(
        &boot,
        &report(&boot),
        OperatorRegistry::native(),
        HarnessOptions::default(),
        t(7_200),
        LanePolicy {
            dogstatsd: 1,
            state: 2,
            post: 1,
        },
        DeliverySchedule::TraceOrder,
    )
    .unwrap();
    harness.admit("src", ms(7_209_500), vec![x, y]);
    harness.close_staggered(ms(7_209_900), Duration::from_millis(200));
    assert_eq!(framed(&harness, "a"), expected(&[]));
    harness.publish();
    assert_eq!(harness.telemetry().loss("agg", LossClass::Shutdown), 2);
}

#[test]
fn trace_l6_shutdown_inside_a_window_drains_an_epoch_a_lane_never_stamped_in_at_its_latest_stamp() {
    // Producer lane 1 never stamps; producer lane 0 admits `m` at 3,700.5 and resets at 100.5. The window stays open
    // until shutdown, which closes it first: epoch 0's watermark is unknown, since producer lane 1 never reached it, so
    // its instance drains everything it holds without a watermark flush, stamped at the epoch's latest admission time,
    // 3,700.5, as one core over the two points stamps it; never at the Unix epoch.
    let trace = Trace::new()
        .install(kept("10s"))
        .admit_at_lane(src(), 0, ms(3_700_500), vec![counter("m", 1.0)])
        .heartbeat_at_lane(src(), 0, ms(100_500))
        .close();
    let lanes = LanePolicy {
        dogstatsd: 2,
        state: 1,
        post: 1,
    };
    let (run, replay) = Replay::run_on_lanes(
        &trace,
        false,
        t(3_700),
        HarnessOptions::default(),
        lanes,
        DeliverySchedule::TraceOrder,
    );
    let stamps = replay
        .harness
        .delivered()
        .iter()
        .map(|record| record.admitted_at)
        .collect::<Vec<_>>();
    assert_eq!(stamps, vec![Some(ms(3_700_500))], "the drain's stamp");
    assert_delivered(&run, &replay, "a", &[(1, 3_700_500, "m rate/10s [3700:1]")]);
}

#[test]
fn trace_l7_a_revocation_while_armed_cancels_the_cut_over_every_producer_lane() {
    // D = 4: the cut is armed over the four lane points; no lane has stamped its generation, so the cancel restores
    // the old generation for every lane, and the default program is installed again, retaining its lineage. Once lane
    // 3 stamps the new generation, the cancel is refused.
    let lanes = LanePolicy {
        dogstatsd: 4,
        state: 2,
        post: 1,
    };
    let armed = || {
        let trace = Trace::new().install(kept("10s"));
        let trace = (0..4).fold(trace, |trace, lane| {
            trace.admit_at_lane(src(), lane, t(95 + u64::from(lane)), vec![counter("c", 1.0)])
        });
        trace.install(kept("30s"))
    };
    let (run, replay) = Replay::run_on_lanes(
        &armed().cancel().install(kept("10s")).advance(t(101)),
        true,
        t(90),
        HarnessOptions::default(),
        lanes,
        DeliverySchedule::TraceOrder,
    );
    assert_delivered(&run, &replay, "a", &[(3, 101_000, "c rate/10s [90:4]")]);

    let (run, replay) = Replay::run_on_lanes(
        &armed().heartbeat_at_lane(src(), 3, t(121)).cancel().advance(t(121)),
        true,
        t(90),
        HarnessOptions::default(),
        lanes,
        DeliverySchedule::TraceOrder,
    );
    assert_eq!(
        run.installs()
            .iter()
            .filter(|record| matches!(record.outcome, InstallOutcome::CancelRefused))
            .count(),
        1
    );
    assert_eq!(framed(&replay.harness, "a"), interpreted(&run, "a"));
}
