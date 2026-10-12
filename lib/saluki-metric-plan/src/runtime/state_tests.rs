//! Isolation slots: aggregation as a stateful kernel in a region of its own, driven through a [`FrameHarness`] and
//! directly.
//!
//! Plans run under the native registry, whose `aggregate` is a state owner behind an isolation fence. Expected outputs
//! are written by hand from the stage 6 time rules: a point's bucket is the one containing its admission time; a tick
//! fires at its wall-clock second once the watermark has reached it, and otherwise waits for it at most the grace;
//! time-driven output is stamped with the flush time at the slot's output point. Times are relative to `B`, aligned to
//! every width used, and the harness starts at `B`, so a flush interval of 15 seconds ticks at `B + 15`, `B + 30`, and
//! so on.

use std::{
    any::Any,
    panic::{catch_unwind, AssertUnwindSafe},
    sync::{Arc, OnceLock},
    time::{Duration, Instant},
};

use saluki_core::{
    data_model::{
        event::metric::{context::Context, Metric, MetricValues},
        tags::TagSet,
    },
    execution::{
        AdmissionPointId, AdmissionSchedule, AdmissionTime, BatchHeader, Budget, DrainPolicy, EntryId, ExitSink,
        GenerationId, GenerationRef, GenerationToken, Kernel, LossClass, LossCount, Progress, RegionDriver, RegionTurn,
        Turn, WorkKind,
    },
    topology::EventsBuffer,
};
use saluki_metric_kernels::aggregate::ContextSnapshot;
use serde_json::{json, Value};

use super::{
    build_kernels,
    harness::{DeliveredRecord, FrameHarness, HarnessClock, HarnessOptions, RecordingTelemetry},
    KernelControl, KernelTarget, MetricRegionDriver, NoTelemetry, PanicBudget, PrepareRegion, RegionConfig,
    RegionControl,
};
use crate::{
    canonical::PlanHash,
    compile::Frame,
    equivalence::{compare, OutputPoints},
    install::StateBinding,
    interpret::{Interpreter, Trace},
    ir::{DecodedParams, NodeId, OutputId, ParamSpec, Plan, RecipientId},
    model::PortType,
    operators::{
        Aggregate, Boundary, EffectContract, KernelBuildContext, KernelSpec, OperatorDef, OperatorRegistry, Ports,
    },
    validate::{CatalogOutput, MemoryCatalog, ValidationReport, Validator},
};

/// The base time: a multiple of every allowed width, so that `B + k` lines up with every width as `k` does.
const B: u64 = 1_699_999_200;

const OUTPUT: &str = "aggregated";

/// Returns `B` plus `relative_millis` milliseconds.
fn t(relative_millis: u64) -> AdmissionTime {
    AdmissionTime::from_unix_nanos((B * 1_000 + relative_millis) * 1_000_000)
}

fn src() -> NodeId {
    "src".parse().unwrap()
}

fn catalog() -> MemoryCatalog {
    MemoryCatalog::new().with_output(
        OutputId::new(OUTPUT).unwrap(),
        CatalogOutput {
            accepts: PortType::ANY,
            fingerprint: PlanHash::of_bytes(OUTPUT.as_bytes()),
            recipients: vec![RecipientId::new(format!("out/{}", OUTPUT)).unwrap()],
            legacy_fingerprint: None,
        },
    )
}

/// The parameters of an aggregation with a `window` width, a 15-second flush interval, and native defaults otherwise.
fn aggregate_params(window: &str) -> Value {
    json!({
        "window": window, "flush_interval": "15s", "context_limit": 1_000_000, "counter_expiry": "5m",
        "flush_open_windows": false, "histogram_aggregates": ["max", "median", "avg", "count"],
        "histogram_percentiles": [95], "copy_to_distribution": false, "copy_to_distribution_prefix": ""
    })
}

/// The plan `source -> aggregate -> emit`, with the aggregation's parameters; the aggregation's slot is `dsd_agg`.
fn plan_with(params: Value) -> Plan {
    Plan::from_value(json!({
        "ir_version": 1,
        "nodes": [
            {"id": "src", "op": "source", "params": {"binding": "dogstatsd", "timestamps": "untimestamped"},
             "provenance": {"native": "dsd_in"}},
            {"id": "dsd_agg", "op": "aggregate", "provenance": {"native": "dsd_agg"}, "params": params},
            {"id": "out", "op": "emit", "params": {"output": OUTPUT}, "provenance": {"native": "dd_metrics_encode"}},
        ],
        "edges": [{"from": "src.metrics", "to": "dsd_agg.in"}, {"from": "dsd_agg.out", "to": "out.in"}],
        "outputs": [{"id": OUTPUT, "accepts": {"kinds": "all", "timestamps": "mixed"},
                     "fingerprint": PlanHash::of_bytes(OUTPUT.as_bytes()).to_string(),
                     "recipients": [format!("out/{}", OUTPUT)]}],
    }))
    .unwrap()
}

fn plan() -> Plan {
    plan_with(aggregate_params("10s"))
}

fn report(plan: &Plan) -> ValidationReport {
    let report = Validator::new(&OperatorRegistry::native(), &catalog()).validate(plan);
    assert!(report.is_valid(), "the fixture plan is invalid:\n{}", report);
    report
}

fn harness(plan: &Plan, options: HarnessOptions) -> FrameHarness {
    FrameHarness::starting_at(plan, &report(plan), OperatorRegistry::native(), options, t(0))
}

fn counter(name: &'static str, value: f64) -> Metric {
    Metric::counter(Context::from_static_parts(name, &[]), value)
}

fn gauge(name: &'static str, value: f64) -> Metric {
    Metric::gauge(Context::from_static_parts(name, &[]), value)
}

/// Describes a record as `name kind [timestamp:value, ...]`, timestamps relative to `B`.
fn describe(metric: &Metric) -> String {
    let (kind, points) = match metric.values() {
        MetricValues::Rate(points, interval) => (format!("rate/{}s", interval.as_secs()), points),
        MetricValues::Gauge(points) => ("gauge".to_string(), points),
        other => panic!("unexpected values {:?}", other),
    };
    let mut points = points
        .into_iter()
        .map(|(ts, value)| format!("{}:{}", ts.map_or(0, |ts| ts.get() - B), value))
        .collect::<Vec<_>>();
    points.sort();
    format!("{} {} [{}]", metric.context().name(), kind, points.join(", "))
}

/// Returns the records the output received, grouped by the admission time of their buffer in milliseconds relative to
/// `B`, which is the flush time of the tick that emitted them, with the generation they carried.
fn flushes(records: &[DeliveredRecord]) -> Vec<(u64, u64, Vec<String>)> {
    let mut by_flush = std::collections::BTreeMap::<(u64, u64), Vec<String>>::new();
    for record in records {
        let at = record.admitted_at.expect("flushed records are stamped").as_unix_nanos() / 1_000_000 - B * 1_000;
        let generation = record.generation.expect("flushed records carry a generation").get();
        by_flush
            .entry((at, generation))
            .or_default()
            .push(describe(&record.metric));
    }
    by_flush
        .into_iter()
        .map(|((at, generation), mut records)| {
            records.sort();
            (at, generation, records)
        })
        .collect()
}

/// One expected flush: its time in milliseconds relative to `B`, its generation, and its records.
type ExpectedFlush<'a> = (u64, u64, &'a [&'a str]);

fn expected(flushes: &[ExpectedFlush<'_>]) -> Vec<(u64, u64, Vec<String>)> {
    flushes
        .iter()
        .map(|(at, generation, records)| {
            let mut records = records.iter().map(|record| record.to_string()).collect::<Vec<_>>();
            records.sort();
            (*at, *generation, records)
        })
        .collect()
}

fn with_heartbeats() -> HarnessOptions {
    HarnessOptions {
        heartbeats_at_ticks: true,
        ..HarnessOptions::default()
    }
}

#[test]
fn every_tick_flushes_at_its_wall_clock_second_what_was_admitted_before_it() {
    // Heartbeats at each tick's time raise the watermark to it, so every tick fires at once, at its whole second: 15
    // closes [0, 10); 30 closes [10, 20) and [20, 30), where the idle counter gains zeros. Ticks are due every 15
    // seconds from the start; the advance to 22 runs none.
    let plan = plan();
    let mut harness = harness(&plan, with_heartbeats());
    harness.admit("src", t(3_000), vec![counter("c", 1.0), gauge("g", 5.0)]);
    harness.admit("src", t(8_500), vec![counter("c", 4.0)]);
    harness.advance(t(15_200));
    harness.advance(t(22_000));
    harness.advance(t(30_700));
    assert_eq!(
        flushes(harness.delivered()),
        expected(&[
            (15_000, 1, &["c rate/10s [0:5]", "g gauge [0:5]"]),
            (30_000, 1, &["c rate/10s [10:0, 20:0]"]),
        ])
    );
    let output_point = match &harness.frame().slots()[0].kind {
        crate::compile::SlotKind::Isolation { output_point, .. } => *output_point,
        crate::compile::SlotKind::Fused => panic!("aggregation runs in an isolation slot"),
    };
    assert!(harness
        .delivered()
        .iter()
        .all(|record| record.admission_point == Some(output_point) && record.epoch == 0));

    // The interpreter, ticking at the same times with heartbeats, delivers the same.
    let run = Interpreter::new(&OperatorRegistry::native(), &catalog())
        .with_heartbeats_at_ticks()
        .run(
            &Trace::new()
                .install(plan)
                .admit(src(), t(3_000), vec![counter("c", 1.0), gauge("g", 5.0)])
                .admit(src(), t(8_500), vec![counter("c", 4.0)])
                .advance(t(15_200))
                .advance(t(30_700)),
        )
        .unwrap();
    if let Err(mismatch) = compare(&OutputPoints::from_run(&run), harness.outputs()) {
        panic!("the isolation slot differs from the interpreter:\n{}", mismatch);
    }
}

#[test]
fn a_state_owner_publishes_the_telemetry_it_accumulated_when_its_region_publishes() {
    // The aggregation kernel counts its contexts in integers of its own, and writes them to its native series when
    // the region publishes, as a region does once a second: never per metric.
    let recorder = saluki_metrics::test::TestRecorder::default();
    let active = |recorder: &saluki_metrics::test::TestRecorder| {
        recorder
            .gauges()
            .into_iter()
            .find(|(key, _)| key.name() == "aggregate_active_contexts")
            .map(|(_, value)| value)
    };
    let mut harness = {
        let _local = metrics::set_default_local_recorder(&recorder);
        harness(&plan(), HarnessOptions::default())
    };
    harness.admit("src", t(3_000), vec![gauge("g", 1.0), gauge("h", 2.0)]);
    assert_eq!(active(&recorder), Some(0.0), "nothing is written per metric");
    harness.publish();
    assert_eq!(active(&recorder), Some(2.0));
}

#[test]
fn a_tick_whose_watermark_lags_waits_at_the_gate_for_a_later_stamp_or_the_end_of_its_grace() {
    // Without heartbeats the watermark is the latest admission time. The tick due at 15, run at 15.2, finds it at 3,
    // so it waits; the batch at 16 raises it past 15, which fires the tick at 15 before the batch is admitted, so the
    // 16 gauge stays for later. The tick due at 30, run at 31, waits again; once its 2-second grace ends, at 33, it
    // fires at the watermark's second, 16, which closes nothing. The tick due at 45 fires at 45 once the batch at 46
    // arrives.
    let plan = plan();
    let mut harness = harness(&plan, HarnessOptions::default());
    harness.admit("src", t(3_000), vec![gauge("g", 1.0)]);
    harness.advance(t(15_200));
    assert!(harness.delivered().is_empty(), "the tick waits for the watermark");
    harness.admit("src", t(16_000), vec![gauge("g", 2.0)]);
    assert_eq!(
        flushes(harness.delivered()),
        expected(&[(15_000, 1, &["g gauge [0:1]"])])
    );
    harness.advance(t(31_000));
    harness.advance(t(32_500));
    harness.publish();
    assert_eq!(
        harness
            .telemetry()
            .value("dsd_agg", crate::runtime::PLAN_STATE_TICK_GRACE_EXPIRED_TOTAL),
        0,
        "the grace has not ended"
    );
    harness.advance(t(33_500));
    assert_eq!(harness.delivered().len(), 1, "the grace ends, and nothing closes by 16");
    harness.publish();
    assert_eq!(
        harness
            .telemetry()
            .value("dsd_agg", crate::runtime::PLAN_STATE_TICK_GRACE_EXPIRED_TOTAL),
        1
    );
    harness.advance(t(45_100));
    harness.admit("src", t(46_000), vec![gauge("g", 3.0)]);
    assert_eq!(
        flushes(harness.delivered()),
        expected(&[(15_000, 1, &["g gauge [0:1]"]), (45_000, 1, &["g gauge [10:2]"])])
    );
}

/// Returns a harness of `plan` started at `start`.
fn harness_at(plan: &Plan, options: HarnessOptions, start: AdmissionTime) -> FrameHarness {
    FrameHarness::starting_at(plan, &report(plan), OperatorRegistry::native(), options, start)
}

#[test]
fn a_clock_reset_closes_everything_held_at_the_watermark_before_its_batch_and_starts_a_new_output_epoch() {
    // The batch at 3,603 comes after one at 7,201: a jump back of 3,598 seconds, more than the reset threshold, so the
    // point's clock restarts in a new epoch. Before the batch is admitted, the reset drain flushes at the watermark
    // before the reset, 7,201, emitting everything held, [7200], stamped in output epoch 0. The batch then starts the
    // new timeline: the flush at shutdown, at the wall clock's 3,615, emits [3600] in output epoch 1, with no zeros,
    // since the instance forgot its flushes.
    let plan = plan();
    let mut harness = harness_at(&plan, HarnessOptions::default(), t(7_200_000));
    harness.admit("src", t(7_201_000), vec![counter("c", 1.0)]);
    harness.admit("src", t(3_603_000), vec![counter("c", 2.0)]);
    let epochs = |harness: &FrameHarness| {
        harness
            .delivered()
            .iter()
            .map(|record| record.epoch)
            .collect::<Vec<_>>()
    };
    assert_eq!(
        flushes(harness.delivered()),
        expected(&[(7_201_000, 1, &["c rate/10s [7200:1]"])])
    );
    assert_eq!(epochs(&harness), vec![0]);

    harness.set_wall(t(3_615_000));
    harness.close(t(3_615_000));
    assert_eq!(
        flushes(harness.delivered()),
        expected(&[
            (3_615_000, 1, &["c rate/10s [3600:2]"]),
            (7_201_000, 1, &["c rate/10s [7200:1]"]),
        ])
    );
    assert_eq!(epochs(&harness), vec![0, 1]);
    harness.publish();
    assert_eq!(
        harness
            .telemetry()
            .value("dsd_agg", crate::runtime::PLAN_CLOCK_RESETS_TOTAL),
        1
    );
}

#[test]
fn at_shutdown_the_final_flush_closes_at_the_later_of_the_latest_stamp_and_the_wall_clock_and_discards_the_rest() {
    // (close at, delivered, shutdown loss): the bucket [0, 10) closes only if the final flush runs at or after 10.
    let cases: [(u64, &[ExpectedFlush<'_>], u64); 2] = [
        (7_000, &[], 2),
        (12_000, &[(12_000, 1, &["c rate/10s [0:3]", "g gauge [0:1]"])], 0),
    ];
    for (close, delivered, lost) in cases {
        let plan = plan();
        let mut harness = harness(&plan, HarnessOptions::default());
        harness.admit("src", t(3_000), vec![counter("c", 3.0), gauge("g", 1.0)]);
        harness.close(t(close));
        assert_eq!(flushes(harness.delivered()), expected(delivered), "closed at {}", close);
        harness.publish();
        assert_eq!(
            harness.telemetry().loss("dsd_agg", LossClass::Shutdown),
            lost,
            "closed at {}",
            close
        );
    }
}

#[test]
fn at_a_cut_the_old_lineage_makes_its_final_flush_at_the_cut_before_the_first_batch_of_the_new_one() {
    // Trace 3 of the stage 6 design: the width changes from 10 to 30 seconds with counters at 95, 105, 115, and 121.
    // Armed at 95, the cut takes effect at 120, the first multiple of lcm(10, 30) = 30 at least a second later. The old
    // lineage flushes [90] at the tick at 105 and makes its final flush at 120, under its own generation, when the
    // heartbeat at 120, the first stamp of the new generation, crosses: [100] and [110], with no zero at or after 120.
    // The new lineage then admits 121 into [120, 150) and emits it at the tick at 150, with no zeros on its first
    // flush.
    let old = plan();
    let new = plan_with(aggregate_params("30s"));
    let mut harness = harness_at(&old, with_heartbeats(), t(90_000));
    harness.admit("src", t(95_000), vec![counter("c", 1.0)]);
    assert!(harness.prepare(&new, &report(&new)), "a change of width is a cut");
    harness.set_wall(t(95_000));
    let at = harness.arm_cut(&[Duration::from_secs(10), Duration::from_secs(30)]);
    assert_eq!(at, t(120_000));
    assert_eq!(
        harness.driver("dsd_agg").lineages().len(),
        2,
        "the new lineage is prepared beside the retiring one"
    );

    harness.advance(t(105_000));
    harness.admit("src", t(105_000), vec![counter("c", 1.0)]);
    harness.admit("src", t(115_000), vec![counter("c", 1.0)]);
    assert!(!harness.cut_irrevocable(), "every stamp so far is before the cut");
    harness.advance(t(120_000));
    assert!(harness.cut_irrevocable() && harness.cut_passed());
    harness.finalize_cut();
    harness.admit("src", t(121_000), vec![counter("c", 1.0)]);
    harness.advance(t(135_000));
    harness.advance(t(150_000));
    assert_eq!(
        flushes(harness.delivered()),
        expected(&[
            (105_000, 1, &["c rate/10s [90:1]"]),
            (120_000, 1, &["c rate/10s [100:1, 110:1]"]),
            (150_000, 2, &["c rate/30s [120:1]"]),
        ])
    );
    let lineages = harness.driver("dsd_agg").lineages();
    assert_eq!(
        lineages
            .iter()
            .map(|(_, created_by, retired)| (created_by.get(), *retired))
            .collect::<Vec<_>>(),
        vec![(1, true), (2, false)],
        "the old lineage retired and stays until its generation's program is released"
    );
}

#[test]
fn a_reset_after_the_cuts_publication_is_the_crossing_and_the_empty_final_retirement_stamps_nothing() {
    // Trace 11 of the stage 6 design, a post-publication reset as the crossing. The width changes from 10 to 30
    // seconds; armed with the wall clock and the latest stamp at 7,201, the cut takes effect at 7,230. The next batch
    // is stamped at 3,603, a jump back of 3,598 seconds: the point's clock restarts in a new epoch, after the cut's
    // publication, so the batch carries the new generation and is also the crossing. Before it is admitted, the reset
    // drain flushes the old lineage at 7,201, emitting [7200] in output epoch 0; the old lineage's final retirement
    // then finds it empty and emits nothing, so it stamps nothing either. The new lineage admits the batch into
    // [3600, 3630) and emits it at shutdown, at the wall clock's 3,635, stamped 3,635 in output epoch 1, not at the
    // cut's 7,230.
    let old = plan();
    let new = plan_with(aggregate_params("30s"));
    let mut harness = harness_at(&old, HarnessOptions::default(), t(7_200_000));
    harness.admit("src", t(7_201_000), vec![counter("c", 1.0)]);
    assert!(harness.prepare(&new, &report(&new)));
    harness.set_wall(t(7_201_000));
    assert_eq!(
        harness.arm_cut(&[Duration::from_secs(10), Duration::from_secs(30)]),
        t(7_230_000)
    );
    harness.admit("src", t(3_603_000), vec![counter("c", 2.0)]);
    assert!(
        harness.cut_irrevocable() && harness.cut_passed(),
        "the reset moved the point to the new generation"
    );
    let stamps = |harness: &FrameHarness| {
        harness
            .delivered()
            .iter()
            .map(|record| (record.epoch, record.generation.map(GenerationId::get)))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        flushes(harness.delivered()),
        expected(&[(7_201_000, 1, &["c rate/10s [7200:1]"])])
    );
    assert_eq!(
        harness
            .driver("dsd_agg")
            .lineages()
            .iter()
            .map(|(_, created_by, retired)| (created_by.get(), *retired))
            .collect::<Vec<_>>(),
        vec![(1, true), (2, false)],
        "the old lineage retired at the crossing"
    );
    harness.finalize_cut();

    harness.set_wall(t(3_635_000));
    harness.close(t(3_635_000));
    assert_eq!(
        flushes(harness.delivered()),
        expected(&[
            (3_635_000, 2, &["c rate/30s [3600:2]"]),
            (7_201_000, 1, &["c rate/10s [7200:1]"]),
        ])
    );
    assert_eq!(stamps(&harness), vec![(0, Some(1)), (1, Some(2))]);
}

// Supervision: a state owner whose kernel panics on demand.

/// The name of the test operator: aggregation behind an isolation fence whose kernel panics on demand.
const PANICKING: &str = "test_panicking_aggregate";

/// Aggregation behind an isolation fence, whose kernel panics when it admits a metric whose name ends in
/// `panic.consume`, and, once it admitted one ending in `panic.tick`, `panic.reset`, or `panic.snapshot`, at its next
/// flush tick, clock reset, or context snapshot. Everything else is the native operator's.
struct PanickingAggregate;

impl OperatorDef for PanickingAggregate {
    fn name(&self) -> &'static str {
        PANICKING
    }

    fn summary(&self) -> &'static str {
        "Aggregates natively, and panics on demand."
    }

    fn params(&self) -> &'static [ParamSpec] {
        Aggregate.params()
    }

    fn check_params(&self, params: &DecodedParams) -> Vec<crate::ir::ParamError> {
        Aggregate.check_params(params)
    }

    fn ports(&self, params: &DecodedParams) -> Ports {
        Aggregate.ports(params)
    }

    fn contract(&self, params: &DecodedParams) -> EffectContract {
        EffectContract {
            boundary: Boundary::Isolated,
            ..Aggregate.contract(params)
        }
    }

    fn output_types(&self, params: &DecodedParams, ports: &Ports, inputs: &[PortType]) -> Vec<PortType> {
        Aggregate.output_types(params, ports, inputs)
    }

    fn kernel(&self, params: &DecodedParams) -> Option<KernelSpec> {
        let inner = Aggregate.kernel(params)?;
        Some(KernelSpec::new(move |context| {
            Ok(Box::new(PanickingKernel {
                inner: inner.build(context)?,
                armed: None,
            }))
        }))
    }

    fn lineage_params(&self, params: &DecodedParams) -> crate::ir::Params {
        Aggregate.lineage_params(params)
    }

    fn step_params(
        &self, params: &DecodedParams, filter: Option<Arc<dyn saluki_metric_kernels::aggregate::EmissionFilter>>,
    ) -> Option<Arc<dyn Any + Send + Sync>> {
        Aggregate.step_params(params, filter)
    }

    fn flush_interval(&self, params: &DecodedParams) -> Option<Duration> {
        Aggregate.flush_interval(params)
    }
}

/// The kernel of [`PanickingAggregate`].
struct PanickingKernel {
    inner: Box<dyn Kernel>,
    /// The call the kernel panics at next: `panic.tick`, `panic.reset`, or `panic.snapshot`.
    armed: Option<&'static str>,
}

impl Kernel for PanickingKernel {
    fn consume(&mut self, turn: &mut Turn<'_>, input: &mut EventsBuffer) -> Progress {
        for event in &*input {
            let Some(name) = event.try_as_metric().map(|metric| metric.context().name().as_ref()) else {
                continue;
            };
            if name.ends_with("panic.consume") {
                panic!("the state owner panicked admitting a batch");
            }
            for trigger in ["panic.tick", "panic.reset", "panic.snapshot"] {
                if name.ends_with(trigger) {
                    self.armed = Some(trigger);
                }
            }
        }
        self.inner.consume(turn, input)
    }

    fn advance_time(&mut self, turn: &mut Turn<'_>) -> Progress {
        if self.armed == Some("panic.tick") {
            panic!("the state owner panicked at a flush tick");
        }
        self.inner.advance_time(turn)
    }

    fn control(&mut self, turn: &mut Turn<'_>, control: &mut dyn Any) -> Progress {
        if control.is::<saluki_core::execution::ClockReset>() && self.armed == Some("panic.reset") {
            panic!("the state owner panicked at a clock reset");
        }
        if control.is::<ContextSnapshot>() && self.armed == Some("panic.snapshot") {
            panic!("the state owner panicked reading a snapshot");
        }
        self.inner.control(turn, control)
    }

    fn finish(&mut self, turn: &mut Turn<'_>, policy: DrainPolicy) -> Progress {
        self.inner.finish(turn, policy)
    }

    fn held(&self) -> LossCount {
        self.inner.held()
    }

    fn publish_telemetry(&mut self) {
        self.inner.publish_telemetry();
    }
}

static PANICKING_AGGREGATE: PanickingAggregate = PanickingAggregate;

/// The isolated registry, with [`PanickingAggregate`].
fn panicking_registry() -> OperatorRegistry {
    static OPERATORS: OnceLock<Vec<&'static dyn OperatorDef>> = OnceLock::new();
    let operators = OPERATORS.get_or_init(|| {
        let mut operators = OperatorRegistry::native().iter().collect::<Vec<_>>();
        operators.push(&PANICKING_AGGREGATE);
        operators
    });
    OperatorRegistry::with_operators(operators)
}

/// The plan `source -> test_panicking_aggregate -> emit`, with a 10-second window.
fn panicking_plan() -> Plan {
    let mut value = serde_json::to_value(plan()).unwrap();
    value["nodes"][1]["op"] = json!(PANICKING);
    Plan::from_value(value).unwrap()
}

fn panicking_harness(plan: &Plan, options: HarnessOptions) -> FrameHarness {
    let registry = panicking_registry();
    let report = Validator::new(&registry, &catalog()).validate(plan);
    assert!(report.is_valid(), "the fixture plan is invalid:\n{}", report);
    FrameHarness::starting_at(plan, &report, registry, options, t(0))
}

fn panicking_report(plan: &Plan) -> ValidationReport {
    let report = Validator::new(&panicking_registry(), &catalog()).validate(plan);
    assert!(report.is_valid(), "the fixture plan is invalid:\n{}", report);
    report
}

#[test]
fn a_state_owners_panic_under_the_default_program_fails_the_region() {
    let plan = panicking_plan();
    let mut harness = panicking_harness(&plan, with_heartbeats());
    harness.admit("src", t(3_000), vec![counter("c", 1.0)]);
    harness.admit("src", t(4_000), vec![counter("panic.consume", 1.0)]);
    let failure = harness
        .failure(harness.slot_named("dsd_agg"))
        .expect("the region fails");
    assert!(
        failure.contains("node `dsd_agg` of generation 1, whose panic budget is zero"),
        "{}",
        failure
    );
}

#[test]
fn a_state_owners_panic_under_an_artifact_restarts_its_state_empty_and_asks_for_a_rollback_while_it_keeps_running() {
    // Generation 2 runs the same program under an artifact's budget, retaining the instance. Its kernel panics at the
    // tick at 15, while it holds the counter admitted at 3: the instance restarts empty, its point counted as invalid
    // loss, and the generation asks to be rolled back, but keeps processing its input: the counter at 21 is flushed
    // at 30, under generation 2, with no zero for the bucket the panic lost.
    let plan = panicking_plan();
    let mut harness = panicking_harness(
        &plan,
        HarnessOptions {
            candidate_budget: crate::runtime::PanicBudget::ARTIFACT,
            ..with_heartbeats()
        },
    );
    assert!(
        !harness.prepare(&plan, &panicking_report(&plan)),
        "the same plan retains its state"
    );
    harness.commit();
    harness.admit("src", t(3_000), vec![counter("c", 1.0), counter("panic.tick", 1.0)]);
    harness.advance(t(15_000));
    assert!(
        harness.failure(harness.slot_named("dsd_agg")).is_none(),
        "the region recovers"
    );
    let failures = harness.failures();
    assert_eq!(failures.len(), 1, "{:?}", failures);
    assert_eq!(failures[0].0.get(), 2);
    assert!(failures[0].1.contains("panicked at a flush tick"), "{}", failures[0].1);

    harness.admit("src", t(21_000), vec![counter("c", 4.0)]);
    harness.advance(t(30_000));
    assert_eq!(
        flushes(harness.delivered()),
        expected(&[(30_000, 2, &["c rate/10s [20:4]"])])
    );
    harness.publish();
    assert_eq!(
        harness.telemetry().loss("dsd_agg", LossClass::Invalid),
        2,
        "the two points the instance held"
    );
    assert_eq!(harness.failures().len(), 1, "the generation asks for its rollback once");
}

// Drains: bounded, resumable, and equal to whole ones.

/// Runs a trace of ticks, a clock reset, and shutdown over 40 contexts, with `budget` for every region call.
fn drained(budget: Budget) -> FrameHarness {
    let plan = plan();
    let mut harness = harness_at(
        &plan,
        HarnessOptions {
            turn_budget: budget,
            ..with_heartbeats()
        },
        t(1_000_000),
    );
    let batch = |base: &str, value: f64| {
        (0..20)
            .flat_map(|index| {
                let name = format!("{}.{}", base, index);
                [
                    Metric::counter(Context::from_parts(format!("{}.c", name), TagSet::default()), value),
                    Metric::gauge(Context::from_parts(format!("{}.g", name), TagSet::default()), value),
                ]
            })
            .collect::<Vec<_>>()
    };
    harness.admit("src", t(1_003_000), batch("a", 1.0));
    harness.advance(t(1_015_000));
    harness.admit("src", t(1_021_000), batch("b", 2.0));
    harness.advance(t(1_030_000));
    harness.admit("src", t(1_031_000), batch("a", 3.0));
    // A jump back of 431 seconds resets the clock: the reset drain emits everything held, [1030] included.
    harness.admit("src", t(600_000), batch("c", 4.0));
    harness.set_wall(t(612_000));
    harness.close(t(612_000));
    harness
}

#[test]
fn drains_that_yield_at_every_context_deliver_what_whole_drains_deliver() {
    let whole = drained(Budget::unlimited());
    let stepped = drained(
        Budget::unlimited()
            .with_limit(WorkKind::StateWork, 1)
            .with_limit(WorkKind::Events, 1),
    );
    assert_eq!(whole.resumes(), 0);
    assert!(
        stepped.resumes() > 100,
        "every context is a step of its own: {} resumes",
        stepped.resumes()
    );
    assert_eq!(flushes(stepped.delivered()), flushes(whole.delivered()));
    let flush_times = flushes(whole.delivered())
        .into_iter()
        .map(|(at, _, records)| (at, records.len()))
        .collect::<Vec<_>>();
    // The tick at 1,015 flushes [1000] (20 counters, 20 gauges), the one at 1,030 [1020] (40) with zeros for the 20
    // idle counters at [1010] in the same records; the reset at 1,031 emits [1030] (40) and the counters' zeros; the
    // shutdown flush at 612 emits [600] (40), with no zeros, since the reset forgot every flush.
    assert_eq!(
        flush_times,
        vec![(612_000, 40), (1_015_000, 40), (1_030_000, 60), (1_031_000, 40)]
    );
}

#[test]
fn a_tick_of_a_lineage_no_published_generation_binds_moves_on_without_firing() {
    // A cut to a 30-second width is prepared and not armed: its new lineage exists, but no published generation binds
    // it, so it holds nothing and its ticks are skipped, each moving its deadline on by its interval; the old lineage
    // ticks as before. A kept deadline would make the harness spin, which it reports as a failure. Between the ticks
    // the region takes every batch, which the old lineage admits and flushes at its next tick.
    let old = plan();
    let new = plan_with(aggregate_params("30s"));
    let mut harness = harness(&old, with_heartbeats());
    harness.admit("src", t(3_000), vec![counter("c", 1.0)]);
    assert!(harness.prepare(&new, &report(&new)));
    assert_eq!(harness.driver("dsd_agg").lineages().len(), 2);
    for (tick, admitted, value) in [(15_000, 16_000, 2.0), (30_000, 31_000, 4.0), (45_000, 46_000, 8.0)] {
        harness.advance(t(tick));
        let driver = harness.driver("dsd_agg");
        assert!(
            driver.accepts_input(),
            "the region takes input after the tick at {}",
            tick
        );
        let deadline = driver.next_deadline().expect("both lineages tick");
        assert!(
            deadline > harness.now(),
            "every deadline that fired at {} moved on",
            tick
        );
        harness.admit("src", t(admitted), vec![counter("c", value)]);
    }
    harness.advance(t(60_000));
    assert_eq!(
        flushes(harness.delivered()),
        expected(&[
            (15_000, 1, &["c rate/10s [0:1]"]),
            (30_000, 1, &["c rate/10s [10:2, 20:0]"]),
            (45_000, 1, &["c rate/10s [30:4]"]),
            (60_000, 1, &["c rate/10s [40:8, 50:0]"]),
        ])
    );
}

/// The plan `source -> split -> (aggregate | test_panicking_aggregate) -> merge -> emit`, the split sending metrics
/// named `a.` to the boot aggregation and the rest to a second state owner of width `second` when `two` is set, and
/// dropping them otherwise.
fn branched_plan(two: bool, second: &str) -> Plan {
    let mut nodes = vec![
        json!({"id": "src", "op": "source", "params": {"binding": "dogstatsd", "timestamps": "untimestamped"},
               "provenance": {"native": "dsd_in"}}),
        json!({"id": "pick", "op": "split", "provenance": {"native": "pick"},
               "params": {"predicate": {"name": {"prefix_in": ["a."]}}}}),
        json!({"id": "dsd_agg", "op": PANICKING, "provenance": {"native": "dsd_agg"},
               "params": aggregate_params("10s")}),
        json!({"id": "out", "op": "emit", "params": {"output": OUTPUT}, "provenance": {"native": "dd_metrics_encode"}}),
    ];
    let mut edges = vec![
        json!({"from": "src.metrics", "to": "pick.in"}),
        json!({"from": "pick.true", "to": "dsd_agg.in"}),
    ];
    if two {
        nodes.push(
            json!({"id": "second", "op": "aggregate", "provenance": {"native": "second"},
                          "params": aggregate_params(second)}),
        );
        nodes.push(json!({"id": "join", "op": "merge", "provenance": {"native": "join"}, "params": {"inputs": 2}}));
        edges.extend([
            json!({"from": "pick.false", "to": "second.in"}),
            json!({"from": "dsd_agg.out", "to": "join.0"}),
            json!({"from": "second.out", "to": "join.1"}),
            json!({"from": "join.out", "to": "out.in"}),
        ]);
    } else {
        nodes.push(json!({"id": "pick_false", "op": "drop", "provenance": {"native": "pick"}}));
        edges.extend([
            json!({"from": "pick.false", "to": "pick_false.in"}),
            json!({"from": "dsd_agg.out", "to": "out.in"}),
        ]);
    }
    Plan::from_value(json!({
        "ir_version": 1,
        "nodes": nodes,
        "edges": edges,
        "outputs": [{"id": OUTPUT, "accepts": {"kinds": "all", "timestamps": "mixed"},
                     "fingerprint": PlanHash::of_bytes(OUTPUT.as_bytes()).to_string(),
                     "recipients": [format!("out/{}", OUTPUT)]}],
    }))
    .unwrap()
}

#[test]
fn a_candidate_aggregates_each_branch_with_its_own_width_in_the_one_isolation_slot() {
    // The second state owner runs in the boot plan's isolation slot, which the split's slot has an exit to, as a second
    // entry over the same edge; the merge after both runs in the slot their exits lead to. Each flushes on its own
    // width: `a.` metrics in 10-second buckets, the rest in 20-second ones.
    let boot = branched_plan(false, "20s");
    let candidate = branched_plan(true, "20s");
    let mut harness = panicking_harness(&boot, with_heartbeats());
    assert!(
        harness.prepare(&candidate, &panicking_report(&candidate)),
        "adding a state owner is a cut"
    );
    harness.commit();
    let slot = harness.slot_named("dsd_agg");
    let placed = harness.placement().slot(slot).expect("the isolation slot").clone();
    assert_eq!(
        placed.nodes,
        vec!["dsd_agg".parse::<NodeId>().unwrap(), "second".parse().unwrap()]
    );
    assert_eq!(placed.entries.len(), 2, "one entry per state owner over the one edge");
    assert_eq!(harness.driver("dsd_agg").lineages().len(), 2);

    harness.admit("src", t(3_000), vec![counter("a.c", 1.0), counter("b.c", 2.0)]);
    harness.admit("src", t(12_000), vec![counter("a.c", 3.0), counter("b.c", 4.0)]);
    harness.advance(t(15_000));
    harness.advance(t(30_000));
    assert_eq!(
        flushes(harness.delivered()),
        expected(&[
            (15_000, 2, &["a.c rate/10s [0:1]"]),
            (30_000, 2, &["a.c rate/10s [10:3, 20:0]", "b.c rate/20s [0:6]"]),
        ])
    );
}

#[test]
fn a_panic_in_one_instance_of_a_reset_drain_completes_the_reset_of_the_others_and_drops_only_its_batch() {
    // Both state owners hold points when the clock resets. The boot aggregation's kernel panics at the reset, under
    // the artifact's budget: it restarts empty, its points lost as invalid, and the drain the panic ended resumes at
    // once with the second state owner, whose reset emits what it held, stamped at the watermark before the reset,
    // in output epoch 0. The buffer that started the reset is lost with the call; the output's epoch moves on once.
    let boot = branched_plan(false, "20s");
    let candidate = branched_plan(true, "20s");
    let mut harness = panicking_harness(
        &boot,
        HarnessOptions {
            candidate_budget: crate::runtime::PanicBudget::ARTIFACT,
            ..with_heartbeats()
        },
    );
    assert!(harness.prepare(&candidate, &panicking_report(&candidate)));
    harness.commit();
    harness.admit(
        "src",
        t(7_201_000),
        vec![counter("a.c", 1.0), counter("a.panic.reset", 1.0), counter("b.c", 2.0)],
    );
    harness.admit("src", t(3_603_000), vec![counter("a.c", 5.0), counter("b.c", 6.0)]);
    assert!(harness.failure(harness.slot_named("dsd_agg")).is_none());
    assert_eq!(
        flushes(harness.delivered()),
        expected(&[(7_201_000, 2, &["b.c rate/20s [7200:2]"])])
    );
    let failures = harness.failures();
    assert_eq!(failures.len(), 1, "{:?}", failures);
    assert!(failures[0].1.contains("panicked at a clock reset"), "{}", failures[0].1);

    // The next timeline starts empty in both, in output epoch 1. Of the batch at 3,603, the split sent each state
    // owner a buffer of its own: the first, which started the reset, was lost with the panic, and the second reached
    // the second state owner after the reset, in the new timeline.
    harness.admit("src", t(3_604_000), vec![counter("a.c", 7.0), counter("b.c", 8.0)]);
    harness.set_wall(t(3_625_000));
    harness.close(t(3_625_000));
    let delivered = harness.delivered();
    assert_eq!(
        flushes(delivered),
        expected(&[
            (3_625_000, 2, &["a.c rate/10s [3600:7]", "b.c rate/20s [3600:14]"]),
            (7_201_000, 2, &["b.c rate/20s [7200:2]"]),
        ])
    );
    assert_eq!(
        delivered.iter().map(|record| record.epoch).collect::<Vec<_>>(),
        vec![0, 1, 1]
    );
    harness.publish();
    assert_eq!(
        harness.telemetry().loss("dsd_agg", LossClass::Invalid),
        2,
        "the panicking instance held the points of `a.c` and `a.panic.reset`"
    );
}

// The driver of an isolation slot, called directly, as its host calls it.

/// One isolation slot's driver, with the frame, schedule, and boot generation it was created with.
struct Direct {
    driver: MetricRegionDriver,
    generation: GenerationRef,
    point: AdmissionPointId,
    exits: usize,
    start: Instant,
    wall: HarnessClock,
    registry: OperatorRegistry,
    frame: Frame,
    /// The isolation slot.
    slot: crate::compile::SlotId,
    /// The boot generation's program.
    program: Arc<crate::compile::RegionProgram>,
    schedule: saluki_core::execution::ScheduleHandle,
    telemetry: RecordingTelemetry,
    /// The generations the region reported failed, with why.
    failures: Arc<std::sync::Mutex<Vec<(GenerationId, String)>>>,
}

impl Direct {
    /// Creates the driver of the isolation slot of `plan`, `source -> aggregate -> emit`, started at `start`.
    fn new(plan: &Plan) -> Self {
        Self::with(plan, OperatorRegistry::native())
    }

    /// Creates the driver of the one isolation slot of `plan` under `registry`, for generation 1 with the default
    /// program's panic budget.
    fn with(plan: &Plan, registry: OperatorRegistry) -> Self {
        let frame = Frame::compute(plan, &registry, &crate::testing::default_program_slot_name).unwrap();
        let compiled = Self::compiled(&registry, &frame, plan);
        let slot = frame
            .slots()
            .iter()
            .find(|slot| slot.is_isolation())
            .expect("the plan has an isolation slot")
            .clone();
        let generation: GenerationRef = Arc::new(GenerationToken::new(GenerationId::new(1)));
        let schedule = AdmissionSchedule::Steady(Arc::clone(&generation)).into_handle();
        let program = Arc::clone(&compiled.programs[slot.id.get()]);
        let keys = program.kernels().iter().map(|kernel| &kernel.key);
        let kernels = build_kernels(&program, keys, KernelBuildContext::detached).unwrap();
        let telemetry = RecordingTelemetry::default();
        let boot = PrepareRegion::new(
            Arc::clone(&generation),
            Arc::clone(&program),
            kernels,
            PanicBudget::DEFAULT_PROGRAM,
            &telemetry,
        );
        let wall = HarnessClock::new(t(0));
        let failures = Arc::new(std::sync::Mutex::new(Vec::new()));
        let reporter: crate::runtime::GenerationFailureReporter = {
            let failures = Arc::clone(&failures);
            Arc::new(move |generation, reason| failures.lock().unwrap().push((generation, reason.to_string())))
        };
        let config = RegionConfig::for_slot(&slot, &telemetry)
            .with_schedule(Arc::clone(&schedule))
            .with_wall_clock(Arc::new(wall.clone()))
            .with_failure_reporter(reporter);
        let mut driver = MetricRegionDriver::new(config, boot).unwrap();
        let start = Instant::now();
        driver.start(&mut RegionTurn::new(start, &mut Budget::unlimited()));
        Self {
            driver,
            generation,
            point: frame.admission_points()[0].id,
            exits: slot.exits.len(),
            start,
            wall,
            registry,
            frame,
            slot: slot.id,
            program,
            schedule,
            telemetry,
            failures,
        }
    }

    /// Compiles a valid plan into the frame.
    fn compiled(registry: &OperatorRegistry, frame: &Frame, plan: &Plan) -> crate::compile::CompiledPlan {
        let report = Validator::new(registry, &catalog()).validate(plan);
        assert!(report.is_valid(), "the fixture plan is invalid:\n{}", report);
        crate::compile::compile(plan, &report, registry, frame).unwrap()
    }

    fn header(&self, at: AdmissionTime) -> BatchHeader {
        BatchHeader {
            admitted_at: Some(at),
            admission_point: Some(self.point),
            epoch: 0,
            generation: Some(Arc::clone(&self.generation)),
            entry: None,
        }
    }

    /// Returns the header of a batch admitted at `at` in epoch `epoch` under `generation`, reaching the isolation slot
    /// at `entry`, if it comes from another slot.
    fn header_of(
        &self, at: AdmissionTime, epoch: u32, generation: &GenerationRef,
        entry: Option<saluki_core::execution::EntryId>,
    ) -> BatchHeader {
        BatchHeader {
            admitted_at: Some(at),
            admission_point: Some(self.point),
            epoch,
            generation: Some(Arc::clone(generation)),
            entry,
        }
    }

    /// Processes a batch with `header`, with `budget`, returning its progress and the events written.
    fn send(&mut self, header: BatchHeader, metrics: Vec<Metric>, budget: Budget) -> (Progress, usize) {
        let mut buffer = EventsBuffer::with_capacity(metrics.len().max(1)).with_header(header);
        for metric in metrics {
            assert!(buffer
                .try_push(saluki_core::data_model::event::Event::Metric(metric))
                .is_none());
        }
        let mut sink = ExitSink::new(self.exits);
        let mut budget = budget;
        let progress = self
            .driver
            .process(&mut RegionTurn::new(self.start, &mut budget), buffer, &mut sink);
        (progress, sink.event_count())
    }

    /// Processes a batch admitted at `at`.
    fn process(&mut self, at: AdmissionTime, metrics: Vec<Metric>) -> Progress {
        let header = self.header(at);
        self.send(header, metrics, Budget::unlimited()).0
    }

    /// Stores `generation` in the schedule alone, as a commit does.
    fn publish(&self, generation: &GenerationRef) {
        self.schedule
            .store(Arc::new(AdmissionSchedule::Steady(Arc::clone(generation))));
    }

    /// Runs time-driven work `elapsed` after the start, with the wall clock as long after `B`, and `budget`, returning
    /// its progress and the events written.
    fn advance(&mut self, elapsed: Duration, budget: Budget) -> (Progress, usize) {
        self.wall.set(t(elapsed.as_millis() as u64));
        let mut sink = ExitSink::new(self.exits);
        let mut budget = budget;
        let progress = self
            .driver
            .advance_time(&mut RegionTurn::new(self.start + elapsed, &mut budget), &mut sink);
        (progress, sink.event_count())
    }

    /// Resumes a yielded call with `budget`, returning its progress and the events written.
    fn resume(&mut self, elapsed: Duration, budget: Budget) -> (Progress, usize) {
        let mut sink = ExitSink::new(self.exits);
        let mut budget = budget;
        let progress = self
            .driver
            .resume(&mut RegionTurn::new(self.start + elapsed, &mut budget), &mut sink);
        (progress, sink.event_count())
    }

    /// Asks every state owner for a snapshot of its contexts, returning the contexts' names.
    fn snapshot(&mut self) -> Vec<String> {
        let collected = Arc::new(std::sync::Mutex::new(Vec::new()));
        let reply = {
            let collected = Arc::clone(&collected);
            Box::new(move |message: Box<dyn Any + Send>| {
                let snapshot = message.downcast::<ContextSnapshot>().expect("the snapshot comes back");
                let mut names = snapshot
                    .entries
                    .iter()
                    .map(|entry| entry.context().name().to_string())
                    .collect::<Vec<_>>();
                names.sort();
                *collected.lock().unwrap() = names;
            })
        };
        let mut sink = ExitSink::new(self.exits);
        self.driver
            .control(
                &mut RegionTurn::new(self.start, &mut Budget::unlimited()),
                RegionControl::Kernel(KernelControl {
                    target: KernelTarget::Stateful,
                    message: Box::new(ContextSnapshot::default()),
                    reply,
                }),
                &mut sink,
            )
            .unwrap();
        assert!(sink.is_empty(), "a snapshot writes nothing");
        let names = collected.lock().unwrap().clone();
        names
    }
}

fn one_context_per_call() -> Budget {
    Budget::unlimited().with_limit(WorkKind::StateWork, 1)
}

#[test]
fn a_snapshot_lists_every_context_held_and_is_answered_between_the_steps_of_a_yielded_drain() {
    // Three gauges held; the tick at 15 flushes [0, 10) one context per call. Between the calls of the yielded flush,
    // a snapshot lists every context still held: a flush removes the contexts it leaves empty only once it completes,
    // so all three, and the drain resumes where it stopped, emitting each context once. Input waits while it is
    // pending.
    let mut direct = Direct::new(&plan());
    let names = ["a", "b", "c"];
    direct.process(t(3_000), names.iter().map(|name| gauge(name, 1.0)).collect());
    assert_eq!(direct.snapshot(), strings(&names));
    // A heartbeat raises the watermark to the tick's time.
    assert_eq!(direct.process(t(15_000), Vec::new()), Progress::Done);

    let (progress, mut emitted) = direct.advance(Duration::from_secs(15), one_context_per_call());
    assert_eq!(progress, Progress::Yielded);
    assert!(!direct.driver.accepts_input(), "input waits while the drain is pending");
    assert_eq!(
        direct.driver.next_deadline(),
        None,
        "no deadline while a drain is pending"
    );
    assert_eq!(direct.snapshot(), strings(&names));
    let mut calls = 1;
    loop {
        let (progress, written) = direct.resume(Duration::from_secs(15), one_context_per_call());
        emitted += written;
        calls += 1;
        if progress != Progress::Yielded {
            break;
        }
        assert_eq!(direct.snapshot(), strings(&names));
    }
    assert_eq!(emitted, 3, "each context's gauge once");
    assert!(calls >= 3, "one context per call: {} calls", calls);
    assert!(direct.driver.accepts_input());
    assert_eq!(
        direct.snapshot(),
        strings(&[]),
        "a flushed gauge leaves the state, unlike an idle counter"
    );
}

fn strings(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| value.to_string()).collect()
}

impl Direct {
    /// Prepares `plan` as generation `id`, with every kernel built, the region binding its state owners itself.
    fn prepare(&mut self, plan: &Plan, id: u64) -> GenerationRef {
        self.prepare_with(plan, id, PanicBudget::DEFAULT_PROGRAM, |_| Vec::new())
            .0
    }

    /// Prepares `plan` as generation `id` with `budget`, with every kernel built and the state bindings `bindings`
    /// returns for its program in the isolation slot; returns the generation and the compiled candidate.
    fn prepare_with(
        &mut self, plan: &Plan, id: u64, budget: PanicBudget,
        bindings: impl FnOnce(&crate::compile::RegionProgram) -> Vec<StateBinding>,
    ) -> (GenerationRef, crate::compile::CompiledPlan) {
        let compiled = Self::compiled(&self.registry, &self.frame, plan);
        let program = &compiled.programs[self.slot.get()];
        let keys = program.kernels().iter().map(|kernel| &kernel.key);
        let kernels = build_kernels(program, keys, KernelBuildContext::detached).unwrap();
        let generation: GenerationRef = Arc::new(GenerationToken::new(GenerationId::new(id)));
        let prepare = PrepareRegion::new(
            Arc::clone(&generation),
            Arc::clone(program),
            kernels,
            budget,
            &self.telemetry,
        )
        .with_state_bindings(bindings(program));
        let mut sink = ExitSink::new(self.exits);
        self.driver
            .control(
                &mut RegionTurn::new(self.start, &mut Budget::unlimited()),
                RegionControl::Prepare(Box::new(prepare)),
                &mut sink,
            )
            .unwrap();
        (generation, compiled)
    }

    /// Processes a batch admitted at `at` in epoch `epoch`, with `budget`.
    fn process_with(
        &mut self, at: AdmissionTime, epoch: u32, metrics: Vec<Metric>, budget: Budget,
    ) -> (Progress, usize) {
        let mut header = self.header(at);
        header.epoch = epoch;
        self.send(header, metrics, budget)
    }

    /// Returns the lineage of the boot program's state owner.
    fn boot_lineage(&self) -> crate::compile::LineageKey {
        lineage_of(&self.program)
    }
}

/// Returns the lineage of the first state owner of a program.
fn lineage_of(program: &crate::compile::RegionProgram) -> crate::compile::LineageKey {
    program
        .kernels()
        .iter()
        .find_map(|kernel| kernel.lineage.as_ref())
        .expect("the program has a state owner")
        .key
        .clone()
}

/// Returns the entry, in the isolation slot `slot` of a compiled candidate, of the edge into `node`.
fn entry_into(compiled: &crate::compile::CompiledPlan, slot: crate::compile::SlotId, node: &str) -> EntryId {
    let placed = compiled.placement.slot(slot).expect("the frame's slot");
    match placed
        .entries
        .iter()
        .find(|entry| entry.input.node.to_string() == node)
        .unwrap_or_else(|| panic!("no entry into `{}`", node))
        .key
    {
        crate::compile::EntryKey::Region(entry) => entry,
        other => panic!("the entry into `{}` is {}", node, other),
    }
}

#[test]
fn an_abort_during_a_yielded_drain_removes_the_aborted_lineage_once_the_drain_ends() {
    // A cut's new lineage is prepared beside the old one, which holds five gauges. A clock reset queues a reset drain
    // of both; while it yields one context per call, the cut's generation is aborted: its lineage, which the drain
    // still refers to, is marked removed, skipped, and freed once the drain ends. The old lineage's reset emits all
    // five.
    let mut direct = Direct::new(&plan());
    let new = direct.prepare(&plan_with(aggregate_params("30s")), 2);
    assert_eq!(direct.driver.lineages().len(), 2);
    let names = ["a", "b", "c", "d", "e"];
    direct.process(t(7_201_000), names.iter().map(|name| gauge(name, 1.0)).collect());

    // A stamp of another epoch is a reset of the point's clock.
    let (mut progress, mut emitted) =
        direct.process_with(t(3_603_000), 1, vec![gauge("f", 2.0)], one_context_per_call());
    assert_eq!(progress, Progress::Yielded);
    let mut sink = ExitSink::new(direct.exits);
    direct
        .driver
        .control(
            &mut RegionTurn::new(direct.start, &mut Budget::unlimited()),
            RegionControl::Abort { generation: new.id() },
            &mut sink,
        )
        .unwrap();
    drop(new);
    assert_eq!(
        direct.driver.lineages().len(),
        2,
        "the drain still refers to the aborted lineage"
    );
    while progress == Progress::Yielded {
        let (next, written) = direct.resume(Duration::ZERO, one_context_per_call());
        progress = next;
        emitted += written;
    }
    assert_eq!(emitted, 5, "the old lineage's reset emits every gauge it held");
    assert_eq!(
        direct
            .driver
            .lineages()
            .into_iter()
            .map(|(node, created_by, retired)| (node.to_string(), created_by.get(), retired))
            .collect::<Vec<_>>(),
        vec![("dsd_agg".to_string(), 1, false)],
        "the aborted lineage is freed once the drain ends"
    );
    assert_eq!(
        direct.snapshot(),
        strings(&["f"]),
        "the batch that started the reset is admitted after it"
    );
}

#[test]
fn an_instance_a_drain_refers_to_keeps_its_place_until_the_drain_ends_and_a_later_instance_takes_it_then() {
    // As in the test above, generation 2's new lineage is aborted while a reset drain yields one context per call, so
    // it is marked removed and keeps its place. Generation 3, of the same width, is prepared before the drain ends:
    // its instance takes a place of its own, and the drain, whose queue was decided before, runs neither: the old
    // lineage's reset emits its five gauges, and nothing else does. Once the drain ends, the aborted instance is freed,
    // and generation 4's new instance of the 30-second lineage takes its place, which the slot's order of lineages
    // shows. Published, generation 4 ticks its own instance only: it flushes what it admitted, under its own
    // generation.
    let mut direct = Direct::new(&plan());
    let thirty = plan_with(aggregate_params("30s"));
    let aborted = direct.prepare(&thirty, 2);
    let names = ["a", "b", "c", "d", "e"];
    direct.process(t(7_201_000), names.iter().map(|name| gauge(name, 1.0)).collect());
    let (mut progress, mut emitted) =
        direct.process_with(t(3_603_000), 1, vec![gauge("f", 2.0)], one_context_per_call());
    assert_eq!(progress, Progress::Yielded);
    let mut sink = ExitSink::new(direct.exits);
    direct
        .driver
        .control(
            &mut RegionTurn::new(direct.start, &mut Budget::unlimited()),
            RegionControl::Abort {
                generation: aborted.id(),
            },
            &mut sink,
        )
        .unwrap();
    drop(aborted);
    let _third = direct.prepare(&thirty, 3);
    let lineages = |direct: &Direct| {
        direct
            .driver
            .lineages()
            .into_iter()
            .map(|(_, created_by, _)| created_by.get())
            .collect::<Vec<_>>()
    };
    assert_eq!(lineages(&direct), vec![1, 2, 3], "the aborted instance keeps its place");
    while progress == Progress::Yielded {
        let (next, written) = direct.resume(Duration::ZERO, one_context_per_call());
        progress = next;
        emitted += written;
    }
    assert_eq!(emitted, 5, "only the old lineage's reset emits");
    assert_eq!(
        lineages(&direct),
        vec![1, 3],
        "the aborted instance is freed once the drain ends"
    );

    let (fourth, compiled) = direct.prepare_with(&thirty, 4, PanicBudget::DEFAULT_PROGRAM, |program| {
        vec![StateBinding::New {
            node: "dsd_agg".parse().unwrap(),
            key: lineage_of(program),
        }]
    });
    assert_eq!(
        lineages(&direct),
        vec![1, 4, 3],
        "generation 4's instance takes the freed place"
    );
    direct.publish(&fourth);
    assert_eq!(
        compiled.placement.slot(direct.slot).unwrap().entries[0].key,
        crate::compile::EntryKey::Admission(direct.point)
    );
    let header = direct.header_of(t(3_604_000), 1, &fourth, None);
    direct.send(header, vec![gauge("g", 7.0)], Budget::unlimited());
    let heartbeat = direct.header_of(t(3_641_000), 1, &fourth, None);
    direct.send(heartbeat, Vec::new(), Budget::unlimited());
    // Every instance's tick is long past: generation 4's fires at the wall clock's second, and the others, which no
    // published generation binds, move on.
    let (progress, emitted) = direct.advance(Duration::from_secs(3_640), Budget::unlimited());
    assert_eq!(
        (progress, emitted),
        (Progress::Done, 1),
        "the gauge generation 4 admitted"
    );
    assert_eq!(
        direct.snapshot(),
        strings(&["f"]),
        "the old lineage still holds the batch that started its reset"
    );
}

#[test]
fn a_snapshot_that_panics_in_one_instance_during_a_yielded_tick_drain_restarts_it_and_the_drain_resumes() {
    // Generation 2, under an artifact's budget, aggregates `a.` metrics in the boot state owner and the rest in a
    // second one, whose kernel panics when asked for a snapshot once it admitted a metric named `panic.snapshot`. The
    // tick at 15 drains both, one context per call, the boot state owner first; while its flush yields, a snapshot
    // panics in the second. The panic is charged to generation 2, whose time-driven output the second's state goes
    // to: its instance restarts empty, its two points lost as invalid, and the generation asks once to be rolled back.
    // The host ends the yielded call; the drain is due at once, and input waits. The next `advance_time` resumes the
    // first state owner's flush where it stopped, emitting each of its gauges once, and the second has nothing left.
    let swapped = |plan: Plan| {
        let mut value = serde_json::to_value(plan).unwrap();
        for node in value["nodes"].as_array_mut().unwrap() {
            match node["id"].as_str() {
                Some("dsd_agg") => node["op"] = json!("aggregate"),
                Some("second") => node["op"] = json!(PANICKING),
                _ => {}
            }
        }
        Plan::from_value(value).unwrap()
    };
    let mut direct = Direct::with(&swapped(branched_plan(false, "20s")), panicking_registry());
    let (second, compiled) =
        direct.prepare_with(&swapped(branched_plan(true, "20s")), 2, PanicBudget::ARTIFACT, |_| {
            Vec::new()
        });
    direct.publish(&second);
    let names = ["a.x", "a.y", "a.z"];
    let into_first = direct.header_of(
        t(3_000),
        0,
        &second,
        Some(entry_into(&compiled, direct.slot, "dsd_agg")),
    );
    direct.send(
        into_first,
        names.iter().map(|name| gauge(name, 1.0)).collect(),
        Budget::unlimited(),
    );
    let into_second = direct.header_of(t(4_000), 0, &second, Some(entry_into(&compiled, direct.slot, "second")));
    direct.send(
        into_second,
        vec![gauge("b.c", 1.0), gauge("b.panic.snapshot", 1.0)],
        Budget::unlimited(),
    );
    let heartbeat = direct.header_of(t(15_000), 0, &second, None);
    direct.send(heartbeat, Vec::new(), Budget::unlimited());

    let (progress, mut emitted) = direct.advance(Duration::from_secs(15), one_context_per_call());
    assert_eq!(progress, Progress::Yielded);
    let snapshot = catch_unwind(AssertUnwindSafe(|| direct.snapshot()));
    let payload = snapshot.expect_err("the second state owner's snapshot panics");
    let message = crate::runtime::kernels::panic_message(&*payload);
    let panic = saluki_core::execution::RegionPanic::new(saluki_core::execution::PanicSite::Control, message, 0);
    assert!(matches!(
        direct.driver.reset_after_panic(&panic),
        saluki_core::execution::PanicOutcome::Continue
    ));
    let failures = direct.failures.lock().unwrap().clone();
    assert_eq!(failures.len(), 1, "{:?}", failures);
    assert_eq!(failures[0].0, second.id());
    assert!(
        failures[0].1.contains("panicked reading a snapshot"),
        "{}",
        failures[0].1
    );
    assert!(!direct.driver.accepts_input(), "the drain is left to complete");
    assert!(
        direct
            .driver
            .next_deadline()
            .is_some_and(|deadline| deadline <= direct.start + Duration::from_secs(15)),
        "the drain the panic ended is due at once"
    );

    let (mut progress, written) = direct.advance(Duration::from_secs(15), one_context_per_call());
    emitted += written;
    while progress == Progress::Yielded {
        let (next, written) = direct.resume(Duration::from_secs(15), one_context_per_call());
        progress = next;
        emitted += written;
    }
    assert_eq!(emitted, 3, "each of the first state owner's gauges once");
    assert!(direct.driver.accepts_input());
    assert_eq!(direct.snapshot(), strings(&[]), "both state owners hold nothing");
    direct.driver.publish_telemetry();
    assert_eq!(direct.telemetry.loss("second", LossClass::Invalid), 2);
    assert_eq!(direct.telemetry.loss("dsd_agg", LossClass::Invalid), 0);
}

#[test]
fn a_flush_of_a_hundred_thousand_contexts_emits_at_most_one_drain_step_per_call() {
    // A tick over 90,000 gauges and 10,000 histograms of five statistics each, run with the drain step's budget of
    // the limits table, as the region host runs it: each call emits at most the step's events, plus what is left of
    // the one context whose expansion it never splits, and the calls together emit every record once.
    use crate::{
        limits::values::DRAIN_STEP_CONTEXTS, limits::values::DRAIN_STEP_EVENTS, operators::MAX_HISTOGRAM_SERIES,
    };

    let mut direct = Direct::new(&plan());
    for chunk in 0..100 {
        let metrics = (0..1_000)
            .map(|index| {
                let context = Context::from_parts(format!("held.{}.{}", chunk, index), TagSet::default());
                if index % 10 == 0 {
                    Metric::histogram(context, [1.0, 2.0, 3.0])
                } else {
                    Metric::gauge(context, 1.0)
                }
            })
            .collect();
        direct.process(t(3_000), metrics);
    }
    direct.process(t(15_000), Vec::new());
    let step = || {
        Budget::unlimited()
            .with_limit(WorkKind::Events, DRAIN_STEP_EVENTS)
            .with_limit(WorkKind::StateWork, DRAIN_STEP_CONTEXTS)
    };
    let bound = (DRAIN_STEP_EVENTS + u64::from(MAX_HISTOGRAM_SERIES)) as usize;
    let (mut progress, written) = direct.advance(Duration::from_secs(15), step());
    let mut calls = vec![written];
    while progress == Progress::Yielded {
        let (next, written) = direct.resume(Duration::from_secs(15), step());
        progress = next;
        calls.push(written);
    }
    let total = calls.iter().sum::<usize>();
    assert_eq!(
        total,
        90_000 + 10_000 * 5,
        "every gauge once, and every histogram's five statistics"
    );
    assert!(
        calls.iter().all(|&written| written <= bound),
        "a call emitted more than {}: {:?}",
        bound,
        calls.iter().max()
    );
    assert!(
        calls.len() >= total.div_ceil(bound),
        "{} calls for {} records",
        calls.len(),
        total
    );
}

#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "a retiring instance's tick never passes its cut in the cut's publication epoch")]
fn a_retiring_instances_tick_past_its_cut_in_the_publication_epoch_trips_the_guard() {
    // The old lineage retires at a cut at 10. A batch of the replaced generation stamped at 16, in the cut's
    // publication epoch, is a stamping fault: every stamp of that epoch after the cut carries the new generation, and
    // would have been the crossing. The tick at 15 would flush the old lineage past its cut; the flush time is clamped
    // to the cut, and the guard on the time the gate gave fails in debug builds.
    let mut direct = Direct::new(&plan());
    let old = direct.boot_lineage();
    let boot = direct.generation.id();
    let (new, _) = direct.prepare_with(
        &plan_with(aggregate_params("30s")),
        2,
        PanicBudget::DEFAULT_PROGRAM,
        |program| {
            vec![
                StateBinding::New {
                    node: "dsd_agg".parse().unwrap(),
                    key: lineage_of(program),
                },
                StateBinding::Retire {
                    node: "dsd_agg".parse().unwrap(),
                    key: old,
                    from: boot,
                },
            ]
        },
    );
    new.set_cut(saluki_core::execution::CutBoundary {
        at: t(10_000),
        replaces: boot,
        epochs: std::iter::once((direct.point, 0)).collect(),
    })
    .unwrap();
    direct.schedule.store(Arc::new(AdmissionSchedule::Cut {
        old: Arc::clone(&direct.generation),
        new: Arc::clone(&new),
        at: t(10_000),
    }));
    direct.process(t(3_000), vec![gauge("a", 1.0)]);
    direct.process(t(16_000), vec![gauge("b", 1.0)]);
    direct.advance(Duration::from_secs(15), Budget::unlimited());
}

#[test]
fn a_retained_lineage_takes_each_batchs_context_limit_from_the_batchs_generation() {
    // Generation 2 changes only the context limit, from 1 to 2: its state owner retains generation 1's instance, and
    // its program's fingerprint differs, since the state parameters do. Batches of generation 1 are admitted with a
    // limit of 1, and of generation 2 with a limit of 2.
    let limited = |limit: u64| {
        let mut params = aggregate_params("10s");
        params["context_limit"] = json!(limit);
        plan_with(params)
    };
    let mut direct = Direct::new(&limited(1));
    let second = direct.prepare(&limited(2), 2);
    assert_eq!(direct.driver.lineages().len(), 1, "the lineage is retained");
    assert_eq!(direct.driver.generations().len(), 2);

    direct.process(t(3_000), vec![gauge("a", 1.0), gauge("b", 1.0)]);
    assert_eq!(direct.snapshot(), strings(&["a"]), "generation 1 admits one context");
    let mut header = direct.header(t(4_000));
    header.generation = Some(Arc::clone(&second));
    let mut buffer = EventsBuffer::with_capacity(1).with_header(header);
    assert!(buffer
        .try_push(saluki_core::data_model::event::Event::Metric(gauge("c", 1.0)))
        .is_none());
    let mut sink = ExitSink::new(direct.exits);
    direct.driver.process(
        &mut RegionTurn::new(direct.start, &mut Budget::unlimited()),
        buffer,
        &mut sink,
    );
    assert_eq!(
        direct.snapshot(),
        strings(&["a", "c"]),
        "generation 2 admits a second one"
    );
}

// Progress: what a slot forwards toward a state owner when a batch leaves it without output.

/// The plan `source -> split -> aggregate -> emit`, whose split keeps metrics named `keep.` and drops the rest.
fn filtered_plan() -> Plan {
    Plan::from_value(json!({
        "ir_version": 1,
        "nodes": [
            {"id": "src", "op": "source", "params": {"binding": "dogstatsd", "timestamps": "untimestamped"},
             "provenance": {"native": "dsd_in"}},
            {"id": "pick", "op": "split", "provenance": {"native": "pick"},
             "params": {"predicate": {"name": {"prefix_in": ["keep."]}}}},
            {"id": "pick_false", "op": "drop", "provenance": {"native": "pick"}},
            {"id": "dsd_agg", "op": "aggregate", "provenance": {"native": "dsd_agg"},
             "params": aggregate_params("10s")},
            {"id": "out", "op": "emit", "params": {"output": OUTPUT}, "provenance": {"native": "dd_metrics_encode"}},
        ],
        "edges": [
            {"from": "src.metrics", "to": "pick.in"}, {"from": "pick.true", "to": "dsd_agg.in"},
            {"from": "pick.false", "to": "pick_false.in"}, {"from": "dsd_agg.out", "to": "out.in"},
        ],
        "outputs": [{"id": OUTPUT, "accepts": {"kinds": "all", "timestamps": "mixed"},
                     "fingerprint": PlanHash::of_bytes(OUTPUT.as_bytes()).to_string(),
                     "recipients": [format!("out/{}", OUTPUT)]}],
    }))
    .unwrap()
}

#[test]
fn a_slot_forwards_progress_toward_a_state_owner_at_most_every_progress_period_when_it_writes_nothing_there() {
    // A thousand fully filtered batches, a millisecond apart, reach the isolation slot as progress only: the first,
    // then one every 100 milliseconds of admission time, ten in all. A batch that writes to the exit carries its own
    // progress; a filtered one 50 milliseconds later forwards nothing, and one 100 milliseconds later does. A heartbeat
    // is always forwarded.
    let plan = filtered_plan();
    let mut harness = harness(&plan, HarnessOptions::default());
    for millis in 0..1_000 {
        harness.admit("src", t(millis), vec![counter("drop.c", 1.0)]);
    }
    assert_eq!(harness.arrivals("dsd_agg"), (0, 10));
    harness.admit("src", t(1_000), vec![counter("keep.c", 1.0)]);
    harness.admit("src", t(1_050), vec![counter("drop.c", 1.0)]);
    assert_eq!(harness.arrivals("dsd_agg"), (1, 10));
    harness.admit("src", t(1_100), vec![counter("drop.c", 1.0)]);
    assert_eq!(harness.arrivals("dsd_agg"), (1, 11));
    harness.heartbeat("src", t(1_101));
    assert_eq!(harness.arrivals("dsd_agg"), (1, 12));

    // The watermark follows the progress: the tick due at 15 fires once a filtered batch is admitted at 15.
    harness.advance(t(15_000));
    assert!(harness.delivered().is_empty(), "the watermark is at 1.101");
    harness.admit("src", t(15_000), vec![counter("drop.c", 1.0)]);
    assert_eq!(
        flushes(harness.delivered()),
        expected(&[(15_000, 1, &["keep.c rate/10s [0:1]"])])
    );
}

#[test]
fn a_region_passes_an_admission_points_batches_through_to_a_state_owner_it_feeds_straight() {
    // The candidate drops the split: aggregation takes the source's batches straight. The split's region passes them
    // through to the isolation slot, the kept counter and the one the split dropped alike, and heartbeats reach it as
    // progress; the lineage is retained, so the counter admitted before the change is flushed with the later ones.
    let boot = filtered_plan();
    let straight = {
        let mut value = serde_json::to_value(&boot).unwrap();
        value["nodes"]
            .as_array_mut()
            .unwrap()
            .retain(|node| node["id"] != "pick" && node["id"] != "pick_false");
        value["edges"] = json!([{"from": "src.metrics", "to": "dsd_agg.in"}, {"from": "dsd_agg.out", "to": "out.in"}]);
        Plan::from_value(value).unwrap()
    };
    let mut harness = harness(&boot, with_heartbeats());
    harness.admit("src", t(3_000), vec![counter("keep.c", 1.0)]);
    assert!(
        !harness.prepare(&straight, &report(&straight)),
        "the lineage is retained"
    );
    harness.commit();
    harness.admit("src", t(4_000), vec![counter("keep.c", 2.0), counter("drop.c", 4.0)]);
    assert_eq!(harness.arrivals("dsd_agg"), (2, 0));
    harness.advance(t(15_000));
    assert_eq!(harness.arrivals("dsd_agg"), (2, 1), "the heartbeat at 15 is forwarded");
    assert_eq!(
        flushes(harness.delivered()),
        expected(&[(15_000, 2, &["drop.c rate/10s [0:4]", "keep.c rate/10s [0:3]"])])
    );
}

// The default program, with aggregation in its isolation slot.

#[test]
fn the_default_program_runs_through_its_isolation_slot_as_the_interpreter_runs_it() {
    use crate::testing::{default_program_catalog, DEFAULT_PROGRAM_JSON, NO_AGGREGATION_PIPELINE_OFF_JSON};

    for (json, timestamped) in [(DEFAULT_PROGRAM_JSON, true), (NO_AGGREGATION_PIPELINE_OFF_JSON, false)] {
        let plan = Plan::from_json(json).unwrap();
        let registry = OperatorRegistry::native();
        let catalog = default_program_catalog();
        let report = Validator::new(&registry, &catalog).validate(&plan);
        assert!(report.is_valid(), "{}", report);
        let base = 1_700_000_000;
        let at = |relative: u64| AdmissionTime::from_unix_secs(base + relative);
        let gauge_at = |name: &'static str, relative: u64, value: f64| {
            Metric::gauge(Context::from_static_name(name), (base + relative, value))
        };

        let mut dogstatsd = vec![
            Metric::counter(Context::from_static_parts("raw.requests.login", &["env:prod"]), 1.0),
            Metric::counter(Context::from_static_name("raw.blocked"), 1.0),
            Metric::distribution(
                Context::from_static_parts("checkout.latency", &["user_id:42", "env:dev"]),
                10.0,
            ),
            Metric::gauge(Context::from_static_name("internal.queue"), 7.0),
            Metric::histogram(
                Context::from_static_parts("checkout.latency", &["env:prod"]),
                [1.0, 2.0, 3.0, 4.0, 5.0],
            ),
        ];
        if timestamped {
            dogstatsd.push(Metric::counter(
                Context::from_static_name("checkout.requests"),
                (base + 1, 2.0),
            ));
        }
        let otlp = vec![gauge_at("catalog.items", 4, 3.0)];
        let later = vec![Metric::counter(Context::from_static_name("raw.requests.login"), 5.0)];
        let trace = Trace::new()
            .install(plan.clone())
            .admit(src_of("dsd_in"), at(3), dogstatsd.clone())
            .admit(src_of("otlp_in"), at(4), otlp.clone())
            .advance(at(15))
            .admit(src_of("dsd_in"), at(21), later.clone())
            .advance(at(30))
            .advance(at(315));
        let run = Interpreter::new(&registry, &catalog)
            .with_heartbeats_at_ticks()
            .run(&trace)
            .unwrap();

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        let _guard = runtime.enter();
        let mut harness = FrameHarness::with_options(
            &plan,
            &report,
            registry,
            HarnessOptions {
                heartbeats_at_ticks: true,
                ..HarnessOptions::default()
            },
        );
        assert!(
            harness
                .frame()
                .slots()
                .iter()
                .any(|slot| slot.name.as_str() == "dsd_agg" && slot.is_isolation()),
            "aggregation runs in the isolation slot `dsd_agg`"
        );
        harness.admit("dsd_in", at(3), dogstatsd);
        harness.admit("otlp_in", at(4), otlp);
        harness.advance(at(15));
        harness.admit("dsd_in", at(21), later);
        harness.advance(at(30));
        harness.advance(at(315));

        let outputs = OutputPoints::from_run(&run);
        assert!(
            outputs.count(&OutputId::new("dd_metrics_encode").unwrap()) > 10,
            "the trace delivers"
        );
        if let Err(mismatch) = compare(&outputs, harness.outputs()) {
            panic!("the frame differs from the interpreter:\n{}", mismatch);
        }
        // Every flush tick's output leaves the isolation slot stamped at the tick's whole second.
        let flush_times = harness
            .delivered()
            .iter()
            .filter(|record| record.output.as_ref() == "dd_metrics_encode")
            .filter_map(|record| record.admitted_at)
            .map(|admitted_at| admitted_at.as_unix_secs() - base)
            .collect::<std::collections::BTreeSet<_>>();
        assert!(
            flush_times.contains(&15) && flush_times.contains(&30),
            "{:?}",
            flush_times
        );
    }
}

fn src_of(id: &str) -> NodeId {
    id.parse().unwrap()
}

// Bounds and accounting.

#[test]
fn an_isolation_slots_bounds_hold_its_state_owners_state_at_its_top_level_and_one_drain_step() {
    use saluki_core::{
        accounting::{ComponentRegistry, MemoryBounds},
        data_model::event::Event,
        support::SubsystemIdentifier,
    };

    let plan = plan();
    let registry = OperatorRegistry::native();
    let frame = Frame::compute(&plan, &registry, &crate::testing::default_program_slot_name).unwrap();
    let compiled = crate::compile::compile(&plan, &report(&plan), &registry, &frame).unwrap();
    let slot = &frame.slots()[0];
    assert_eq!(slot.name.as_str(), "dsd_agg");
    let program = &compiled.programs[0];
    let node: NodeId = "dsd_agg".parse().unwrap();
    assert_eq!(
        super::kernel_accounting_in(slot, &node),
        None,
        "the region stands for the native `dsd_agg`"
    );
    assert_eq!(
        super::kernel_accounting_in(&frame.slots()[1], &"out".parse().unwrap()),
        Some("out".to_string()),
        "a fused slot's kernels are accounted under their nodes"
    );

    let totals = |bounds: super::RegionBounds| {
        let components = ComponentRegistry::default();
        bounds.specify_bounds(&mut components.bounds_builder(&SubsystemIdentifier::from_segments(["region"])));
        components.as_bounds()
    };
    let isolated = totals(super::RegionBounds::for_slot(&plan, &registry, program, slot).unwrap());
    let plain = totals(super::RegionBounds::new(&plan, &registry, program).unwrap());
    // One drain step: 1,024 events and one histogram's whole expansion of 108 series.
    let drain_step = (1_024 + 108) * size_of::<Event>();
    assert_eq!(
        isolated.total_firm_limit_bytes(),
        plain.total_firm_limit_bytes() + drain_step
    );
    // The state map sits at the region's top level, as the native component's `aggregate_context_limit` does, rather
    // than under a subcomponent named after the node.
    let region = isolated.subcomponents().into_iter().next().unwrap().1;
    assert!(
        region
            .subcomponents()
            .into_iter()
            .all(|(name, _)| name.as_str() != "dsd_agg"),
        "no subcomponent for the state owner"
    );
    let plain_region = plain.subcomponents().into_iter().next().unwrap().1;
    assert!(plain_region
        .subcomponents()
        .into_iter()
        .any(|(name, _)| name.as_str() == "dsd_agg"));
    let state = 1_000_000
        * (saluki_metric_kernels::aggregate::STATE_ENTRY_BYTES
            + size_of::<saluki_metric_kernels::aggregate::AggregateContextSnapshotEntry>());
    assert!(
        isolated.total_firm_limit_bytes() > state,
        "the state map is in the bounds"
    );
}

#[test]
fn an_artifacts_state_owner_at_the_native_context_limit_is_within_the_artifact_memory_limit() {
    use crate::validate::Rule;

    // An artifact's aggregation at the native limit of a million contexts, and at ten million. Its contexts come from
    // the host's state budget, which the boot program's bounds already hold at the budget's capacity, so only the
    // kernel itself counts against the artifact's memory limit, whatever its context limit.
    for limit in [1_000_000u64, 10_000_000] {
        let mut params = aggregate_params("30s");
        params["context_limit"] = json!(limit);
        let mut value = serde_json::to_value(plan_with(params)).unwrap();
        value["provenance"] =
            json!({"artifacts": [{"revision": 7, "hash": PlanHash::of_bytes(b"artifact").to_string()}]});
        value["nodes"][1]["provenance"] =
            json!({"artifact": {"revision": 7, "hash": PlanHash::of_bytes(b"artifact").to_string()}});
        let plan = Plan::from_value(value).unwrap();
        let report = Validator::new(&OperatorRegistry::native(), &catalog()).validate(&plan);
        assert!(
            report
                .findings()
                .iter()
                .all(|finding| finding.rule != Rule::ArtifactMemory),
            "context limit {}:\n{}",
            limit,
            report
        );
        assert!(report.is_valid(), "context limit {}:\n{}", limit, report);
    }

    // The region still holds the state at the boot program's context limit: its state map and a snapshot of it.
    let plan = plan();
    let registry = OperatorRegistry::native();
    let frame = Frame::compute(&plan, &registry, &crate::testing::default_program_slot_name).unwrap();
    let compiled = crate::compile::compile(&plan, &report(&plan), &registry, &frame).unwrap();
    let bounds = super::RegionBounds::for_slot(&plan, &registry, &compiled.programs[0], &frame.slots()[0]).unwrap();
    let components = saluki_core::accounting::ComponentRegistry::default();
    saluki_core::accounting::MemoryBounds::specify_bounds(
        &bounds,
        &mut components.bounds_builder(&saluki_core::support::SubsystemIdentifier::from_segments(["region"])),
    );
    let state = 1_000_000
        * (saluki_metric_kernels::aggregate::STATE_ENTRY_BYTES
            + size_of::<saluki_metric_kernels::aggregate::AggregateContextSnapshotEntry>());
    assert!(components.as_bounds().total_firm_limit_bytes() > state);
}

#[test]
fn a_region_refuses_a_program_whose_kernels_do_not_fit_its_kind_of_slot_and_an_isolation_slot_needs_a_schedule() {
    // Slot 0 is the isolation slot of `source -> aggregate -> emit`, and a fused slot of the filtered plan: each
    // region refuses the other's program, before changing anything.
    let compile_slot0 = |plan: &Plan| {
        let registry = OperatorRegistry::native();
        let frame = Frame::compute(plan, &registry, &crate::testing::default_program_slot_name).unwrap();
        let compiled = crate::compile::compile(plan, &report(plan), &registry, &frame).unwrap();
        (frame.slots()[0].clone(), Arc::clone(&compiled.programs[0]))
    };
    let (isolation, state_program) = compile_slot0(&plan());
    let (fused, fused_program) = compile_slot0(&filtered_plan());
    assert!(isolation.is_isolation() && !fused.is_isolation());
    let generation: GenerationRef = Arc::new(GenerationToken::new(GenerationId::new(1)));
    let boot = |program: &Arc<crate::compile::RegionProgram>| {
        let keys = program.kernels().iter().map(|kernel| &kernel.key);
        let kernels = build_kernels(program, keys, KernelBuildContext::detached).unwrap();
        PrepareRegion::new(
            Arc::clone(&generation),
            Arc::clone(program),
            kernels,
            PanicBudget::DEFAULT_PROGRAM,
            &NoTelemetry,
        )
    };
    let schedule = AdmissionSchedule::Steady(Arc::clone(&generation)).into_handle();

    let error = MetricRegionDriver::new(RegionConfig::for_slot(&fused, &NoTelemetry), boot(&state_program))
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("a step feeds the state owner `dsd_agg`, which runs only in an isolation slot"),
        "{}",
        error
    );
    let error = MetricRegionDriver::new(
        RegionConfig::for_slot(&isolation, &NoTelemetry).with_schedule(Arc::clone(&schedule)),
        boot(&fused_program),
    )
    .unwrap_err()
    .to_string();
    assert!(
        error.contains("a stateless step runs in an isolation slot, which runs only state owners"),
        "{}",
        error
    );
    let error = MetricRegionDriver::new(RegionConfig::for_slot(&isolation, &NoTelemetry), boot(&state_program))
        .unwrap_err()
        .to_string();
    assert!(error.contains("needs the topology's schedule"), "{}", error);

    // A region given the host's state budgets refuses a state owner's kernel built without them.
    let error = MetricRegionDriver::new(
        RegionConfig::for_slot(&isolation, &NoTelemetry)
            .with_schedule(schedule)
            .with_state_budgets(crate::runtime::StateBudgets::new(10)),
        boot(&state_program),
    )
    .unwrap_err()
    .to_string();
    assert!(
        error.contains("was not built with the host's state budget"),
        "{}",
        error
    );
}

#[test]
fn the_first_batch_of_the_new_generation_retires_the_old_lineage_with_its_future_buckets_before_it_is_admitted() {
    // Traces 1 and 9 of the stage 6 design. The cut to a 30-second width takes effect at 120. The old lineage holds a
    // counter at 95 and a gauge timestamped 130, admitted at 98, in a future bucket. No tick runs: the first batch of
    // the new generation, at 121, is the crossing, and the old lineage's final retirement before it flushes at 120,
    // its first flush, so with no zeros, and emits everything it holds, the future bucket included, under its own
    // generation. The new lineage then holds only the batch at 121.
    let timestamped = |window: &str| {
        let mut value = serde_json::to_value(plan_with(aggregate_params(window))).unwrap();
        value["nodes"][0]["params"]["timestamps"] = json!("mixed");
        Plan::from_value(value).unwrap()
    };
    let old = timestamped("10s");
    let new = timestamped("30s");
    let mut harness = harness_at(&old, HarnessOptions::default(), t(90_000));
    harness.admit("src", t(95_000), vec![counter("c", 1.0)]);
    harness.admit(
        "src",
        t(98_000),
        vec![Metric::gauge(Context::from_static_name("g"), (B + 130, 5.0))],
    );
    assert!(harness.prepare(&new, &report(&new)));
    harness.set_wall(t(98_000));
    assert_eq!(
        harness.arm_cut(&[Duration::from_secs(10), Duration::from_secs(30)]),
        t(120_000)
    );
    harness.admit("src", t(121_000), vec![counter("c", 2.0)]);
    assert_eq!(
        flushes(harness.delivered()),
        expected(&[(120_000, 1, &["c rate/10s [90:1]", "g gauge [130:5]"])])
    );
    harness.finalize_cut();
    harness.close(t(160_000));
    assert_eq!(
        flushes(harness.delivered()),
        expected(&[
            (120_000, 1, &["c rate/10s [90:1]", "g gauge [130:5]"]),
            (160_000, 2, &["c rate/30s [120:2]"]),
        ])
    );
}

#[test]
fn a_preparation_that_overtakes_queued_batches_creates_a_second_instance_of_a_lineage_that_is_still_retiring() {
    // Trace 7 of the stage 6 design, a width round trip 10 -> 5 -> 10. Generation 2 (5 seconds) is armed with its cut
    // at 100; the batch at 101 passes the point and is held between the split's slot and the isolation slot, so the
    // cut finalizes while the old lineage has not crossed. Generation 3 (10 seconds again) is prepared then: its new
    // lineage has the same key as the old one, which is still retiring, and the two coexist. Once released, the batch
    // at 101 crosses for the old lineage, which retires at 100 before the batch is admitted to generation 2's; the
    // batch at 111 crosses for generation 2's lineage, which retires at 110; generation 3's holds what came after.
    let width = |window: &str| {
        let mut value = serde_json::to_value(filtered_plan()).unwrap();
        value["nodes"][3]["params"]["window"] = json!(window);
        Plan::from_value(value).unwrap()
    };
    let (ten, five) = (width("10s"), width("5s"));
    let mut harness = harness_at(&ten, HarnessOptions::default(), t(90_000));
    harness.admit("src", t(95_000), vec![counter("keep.c", 1.0)]);
    assert!(harness.prepare(&five, &report(&five)));
    harness.set_wall(t(98_500));
    assert_eq!(
        harness.arm_cut(&[Duration::from_secs(10), Duration::from_secs(5)]),
        t(100_000)
    );

    harness.hold_between_slots();
    harness.admit("src", t(101_000), vec![counter("keep.c", 1.0)]);
    assert!(harness.held() > 0);
    assert!(harness.cut_passed());
    harness.finalize_cut();
    assert!(harness.prepare(&ten, &report(&ten)));
    let lineages = harness
        .driver("dsd_agg")
        .lineages()
        .into_iter()
        .map(|(_, created_by, retired)| (created_by.get(), retired))
        .collect::<Vec<_>>();
    assert_eq!(
        lineages,
        vec![(1, false), (2, false), (3, false)],
        "two instances of the 10-second lineage"
    );
    harness.set_wall(t(105_000));
    assert_eq!(
        harness.arm_cut(&[Duration::from_secs(5), Duration::from_secs(10)]),
        t(110_000)
    );

    harness.stop_holding();
    harness.release();
    harness.admit("src", t(106_000), vec![counter("keep.c", 1.0)]);
    harness.admit("src", t(111_000), vec![counter("keep.c", 1.0)]);
    harness.finalize_cut();
    harness.close(t(130_000));
    assert_eq!(
        flushes(harness.delivered()),
        expected(&[
            (100_000, 1, &["keep.c rate/10s [90:1]"]),
            (110_000, 2, &["keep.c rate/5s [100:1, 105:1]"]),
            (130_000, 3, &["keep.c rate/10s [110:1]"]),
        ])
    );
}
