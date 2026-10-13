//! State lanes: an isolation slot on several lanes, each lane's driver called directly, as its host calls it, against
//! the same slot on one lane fed the same input.
//!
//! The plan is `source -> namespace prefix -> aggregate -> emit`, laid out without pipelining, so that the prefix runs
//! in a fused slot with one lane per DogStatsD point and the aggregation in `dsd_agg`, on its state lanes. The tests
//! feed `dsd_agg`'s lanes as the prefix slot's keyed exit does: each event to the lane owning its context key, and
//! every lane that receives no event of a batch the batch's header alone, so that every lane sees every point's
//! progress. Expected outputs are written by hand from the time rules, and every test also checks that the lanes
//! together deliver what the slot on one lane delivers.

use std::sync::Mutex;

use saluki_core::execution::{CutBoundary, LaneContext, PanicOutcome, PanicSite, RegionPanic, ScheduleHandle};
use saluki_metrics::{test::TestRecorder, MetricsBuilder};

use super::*;
use crate::{
    compile::{FrameHosts, FrameOptions, KeyGroups, LanePolicy, Slot, SlotKind},
    operators::{LaneBinding, LaneCell},
    runtime::{state::lanes::SlotLanes, PlanTelemetry, PreparedKernel, StateBudgets},
};

/// The plan `source -> namespace prefix -> aggregate -> emit`, aggregating with a `window` width under the operator
/// `op`.
fn lane_plan(window: &str, op: &str) -> Plan {
    Plan::from_value(json!({
        "ir_version": 1,
        "nodes": [
            {"id": "src", "op": "source", "params": {"binding": "dogstatsd", "timestamps": "untimestamped"},
             "provenance": {"native": "dsd_in"}},
            {"id": "dsd_enrich", "op": "namespace_prefix", "params": {"namespace": "t", "exclusions": []},
             "provenance": {"native": "dsd_prefix_filter"}},
            {"id": "dsd_agg", "op": op, "provenance": {"native": "dsd_agg"}, "params": aggregate_params(window)},
            {"id": "out", "op": "emit", "params": {"output": OUTPUT}, "provenance": {"native": "dd_metrics_encode"}},
        ],
        "edges": [
            {"from": "src.metrics", "to": "dsd_enrich.in"},
            {"from": "dsd_enrich.out", "to": "dsd_agg.in"},
            {"from": "dsd_agg.out", "to": "out.in"},
        ],
        "outputs": [{"id": OUTPUT, "accepts": {"kinds": "all", "timestamps": "mixed"},
                     "fingerprint": PlanHash::of_bytes(OUTPUT.as_bytes()).to_string(),
                     "recipients": [format!("out/{}", OUTPUT)]}],
    }))
    .unwrap()
}

/// `B` minus an hour plus `relative_millis` milliseconds: the timeline after the clock stepped back an hour.
fn back(relative_millis: u64) -> AdmissionTime {
    AdmissionTime::from_unix_nanos(((B - 3_600) * 1_000 + relative_millis) * 1_000_000)
}

/// Milliseconds relative to `B`, which are negative on the timeline after a step back.
fn relative(at: AdmissionTime) -> i64 {
    at.as_unix_nanos() as i64 / 1_000_000 - (B * 1_000) as i64
}

/// Describes a record as `name kind [timestamp:value, ...]`, timestamps in seconds relative to `B`.
fn described(metric: &Metric) -> String {
    let (kind, points) = match metric.values() {
        MetricValues::Rate(points, interval) => (format!("rate/{}s", interval.as_secs()), points),
        MetricValues::Gauge(points) => ("gauge".to_string(), points),
        other => panic!("unexpected values {:?}", other),
    };
    let mut points = points
        .into_iter()
        .map(|(ts, value)| format!("{}:{}", ts.map_or(0, |ts| ts.get() as i64 - B as i64), value))
        .collect::<Vec<_>>();
    points.sort();
    format!("{} {} [{}]", metric.context().name(), kind, points.join(", "))
}

/// One record a lane delivered, with the header of its buffer.
#[derive(Clone)]
struct Delivered {
    lane: u16,
    admitted_at: AdmissionTime,
    epoch: u32,
    generation: u64,
    metric: Metric,
}

/// The records delivered, grouped by flush time in milliseconds relative to `B` and generation, each group sorted.
fn grouped(delivered: &[Delivered]) -> Vec<(i64, u64, Vec<String>)> {
    let mut by_flush = std::collections::BTreeMap::<(i64, u64), Vec<String>>::new();
    for record in delivered {
        by_flush
            .entry((relative(record.admitted_at), record.generation))
            .or_default()
            .push(described(&record.metric));
    }
    by_flush
        .into_iter()
        .map(|((at, generation), mut records)| {
            records.sort();
            (at, generation, records)
        })
        .collect()
}

fn group(at: i64, generation: u64, records: &[&str]) -> (i64, u64, Vec<String>) {
    let mut records = records.iter().map(|record| record.to_string()).collect::<Vec<_>>();
    records.sort();
    (at, generation, records)
}

/// `dsd_agg` on `p` state lanes fed by `d` DogStatsD points, each lane's driver called directly.
struct Lanes {
    drivers: Vec<MetricRegionDriver>,
    /// The upstream points, in lane order.
    points: Vec<AdmissionPointId>,
    /// The region entry into `dsd_agg`, which the prefix slot's exit writes.
    entry: EntryId,
    groups: KeyGroups,
    generation: GenerationRef,
    schedule: ScheduleHandle,
    start: Instant,
    wall: HarnessClock,
    exits: usize,
    shared: Option<Arc<SlotLanes>>,
    registry: OperatorRegistry,
    /// The plan the lanes booted with.
    plan: Plan,
    frame: Frame,
    slot: Slot,
    budgets: StateBudgets,
    /// What every lane's kernels and telemetry record to.
    recorder: TestRecorder,
    delivered: Vec<Delivered>,
    /// The generations the lanes reported failed, with why.
    failures: Arc<Mutex<Vec<(GenerationId, String)>>>,
}

impl Lanes {
    /// Creates `dsd_agg` of `lane_plan(window, "aggregate")` on `p` lanes fed by `d` points, started at `B`.
    fn new(d: u16, p: u16) -> Self {
        Self::with(
            &lane_plan("10s", "aggregate"),
            OperatorRegistry::native(),
            d,
            p,
            PanicBudget::DEFAULT_PROGRAM,
        )
    }

    fn with(plan: &Plan, registry: OperatorRegistry, d: u16, p: u16, budget: PanicBudget) -> Self {
        Self::started_apart(plan, registry, d, p, budget, Duration::ZERO)
    }

    /// Creates `dsd_agg` as [`with`][Self::with] does, lane `i` starting `i * stagger` after lane 0.
    fn started_apart(
        plan: &Plan, registry: OperatorRegistry, d: u16, p: u16, budget: PanicBudget, stagger: Duration,
    ) -> Self {
        let frame = Frame::compute_with(
            plan,
            &registry,
            &crate::testing::default_program_slot_name,
            &FrameHosts::new(),
            &LanePolicy {
                dogstatsd: d,
                state: p,
                post: 1,
            },
            FrameOptions { pipelining: false },
        )
        .unwrap();
        let compiled = Direct::compiled(&registry, &frame, plan);
        let slot = frame
            .slots()
            .iter()
            .find(|slot| slot.is_isolation())
            .expect("the plan has an isolation slot")
            .clone();
        assert_eq!(slot.lanes, p);
        let SlotKind::Isolation { upstream, .. } = &slot.kind else {
            unreachable!("an isolation slot")
        };
        let points = upstream.clone();
        assert_eq!(points.len(), usize::from(d), "every point of the source is upstream");
        let entry = entry_into(&compiled, slot.id, "dsd_agg");
        let program = Arc::clone(&compiled.programs[slot.id.get()]);
        let generation: GenerationRef = Arc::new(GenerationToken::new(GenerationId::new(1)));
        let schedule = AdmissionSchedule::Steady(Arc::clone(&generation)).into_handle();
        let mut lanes = Self {
            drivers: Vec::new(),
            points,
            entry,
            groups: KeyGroups::new(p),
            generation: Arc::clone(&generation),
            schedule,
            start: Instant::now(),
            wall: HarnessClock::new(t(0)),
            exits: slot.exits.len(),
            shared: (p > 1).then(|| SlotLanes::new(p)),
            registry,
            plan: plan.clone(),
            frame,
            slot,
            budgets: StateBudgets::new(1_000_000),
            recorder: TestRecorder::default(),
            delivered: Vec::new(),
            failures: Arc::new(Mutex::new(Vec::new())),
        };
        let cell = LaneCell::new();
        for lane in 0..p {
            let kernels = lanes.built(&program, &cell, lane);
            let boot = PrepareRegion::new(
                Arc::clone(&generation),
                Arc::clone(&program),
                kernels,
                budget,
                &NoTelemetry,
            );
            let driver = lanes.driver(lane, boot);
            lanes.drivers.push(driver);
        }
        let start = lanes.start;
        for (lane, driver) in lanes.drivers.iter_mut().enumerate() {
            driver.start(&mut RegionTurn::new(
                start + stagger * lane as u32,
                &mut Budget::unlimited(),
            ));
        }
        lanes
    }

    /// Builds every kernel of `program` for `lane`, against `cell` with lanes.
    fn built(
        &self, program: &Arc<crate::compile::RegionProgram>, cell: &Arc<LaneCell>, lane: u16,
    ) -> Vec<PreparedKernel> {
        let count = self.slot.lanes;
        let budgets = self.budgets.clone();
        let keys = program.kernels().iter().map(|kernel| &kernel.key);
        let _local = metrics::set_default_local_recorder(&self.recorder);
        build_kernels(program, keys, |node| {
            let context = KernelBuildContext::detached(node).with_state_budget(budgets.context_budget(node));
            if count > 1 {
                context.with_lanes(LaneBinding {
                    cell: Arc::clone(cell),
                    lane,
                    lanes: count,
                })
            } else {
                context
            }
        })
        .unwrap()
    }

    fn driver(&self, lane: u16, boot: PrepareRegion) -> MetricRegionDriver {
        let failures = Arc::clone(&self.failures);
        let reporter: crate::runtime::GenerationFailureReporter =
            Arc::new(move |generation, reason| failures.lock().unwrap().push((generation, reason.to_string())));
        // The slot's own telemetry, such as its clock resets, records with the kernels'.
        let config = {
            let _local = metrics::set_default_local_recorder(&self.recorder);
            RegionConfig::for_slot(&self.slot, &PlanTelemetry::new(MetricsBuilder::default()))
        };
        let mut config = config
            .with_schedule(Arc::clone(&self.schedule))
            .with_wall_clock(Arc::new(self.wall.clone()))
            .with_state_budgets(self.budgets.clone())
            .with_failure_reporter(reporter);
        if let Some(shared) = &self.shared {
            let telemetry = {
                let _local = metrics::set_default_local_recorder(&self.recorder);
                PlanTelemetry::new(MetricsBuilder::default()).state_lane("dsd_agg", lane)
            };
            config = config
                .with_lane(
                    LaneContext {
                        index: lane,
                        count: self.slot.lanes,
                    },
                    Arc::clone(shared),
                )
                .with_state_lane_telemetry(telemetry);
        }
        MetricRegionDriver::new(config, boot).unwrap()
    }

    fn lanes(&self) -> u16 {
        self.drivers.len() as u16
    }

    /// Returns the lane that owns a metric's context.
    fn lane_of(&self, metric: &Metric) -> u16 {
        self.groups.lane_of(metric.context().key().as_u64())
    }

    fn header(&self, point: usize, epoch: u32, at: AdmissionTime, generation: &GenerationRef) -> BatchHeader {
        BatchHeader {
            admitted_at: Some(at),
            admission_point: Some(self.points[point]),
            epoch,
            generation: Some(Arc::clone(generation)),
            entry: Some(self.entry),
        }
    }

    /// Runs `call` on lane `lane`'s driver at `elapsed` after the start, as the host runs it: catching a panic and
    /// recovering from it, and collecting what the lane wrote.
    fn call(
        &mut self, lane: u16, elapsed: Duration, site: PanicSite,
        call: impl FnOnce(&mut MetricRegionDriver, &mut RegionTurn<'_>, &mut ExitSink) -> Progress,
    ) -> Progress {
        let mut sink = ExitSink::new(self.exits);
        let mut budget = Budget::unlimited();
        let start = self.start;
        let driver = &mut self.drivers[usize::from(lane)];
        let outcome = catch_unwind(AssertUnwindSafe(|| {
            call(driver, &mut RegionTurn::new(start + elapsed, &mut budget), &mut sink)
        }));
        let progress = match outcome {
            Ok(progress) => progress,
            Err(payload) => {
                sink = ExitSink::new(self.exits);
                let message = crate::runtime::kernels::panic_message(&*payload);
                let outcome = driver.reset_after_panic(&RegionPanic::new(site, message, 0));
                assert!(matches!(outcome, PanicOutcome::Continue), "lane {} failed", lane);
                // As its host does, the lane resumes at once what the panic ended.
                if driver.next_deadline().is_some_and(|at| at <= start + elapsed) {
                    let mut budget = Budget::unlimited();
                    driver.advance_time(&mut RegionTurn::new(start + elapsed, &mut budget), &mut sink);
                }
                Progress::Done
            }
        };
        for (_, mut buffer) in sink.drain() {
            let header = buffer.take_header();
            for event in buffer {
                let metric = event.try_into_metric().expect("aggregation emits metrics");
                self.delivered.push(Delivered {
                    lane,
                    admitted_at: header.admitted_at.expect("time-driven output is stamped"),
                    epoch: header.epoch,
                    generation: header
                        .generation_id()
                        .expect("time-driven output has a generation")
                        .get(),
                    metric,
                });
            }
        }
        progress
    }

    /// Sends lane `lane` a batch of `metrics` with `header`.
    fn deliver(&mut self, lane: u16, header: BatchHeader, metrics: Vec<Metric>) {
        let mut buffer = EventsBuffer::with_capacity(metrics.len().max(1)).with_header(header);
        for metric in metrics {
            assert!(buffer
                .try_push(saluki_core::data_model::event::Event::Metric(metric))
                .is_none());
        }
        let progress = self.call(lane, Duration::ZERO, PanicSite::Process, |driver, turn, exits| {
            driver.process(turn, buffer, exits)
        });
        assert_eq!(progress, Progress::Done, "lane {} took the batch whole", lane);
    }

    /// Admits `metrics` at point `point` in `epoch` at `at`, under the boot generation: each to the lane owning its
    /// context, and the header alone to every other lane, as the prefix slot's keyed exit forwards progress.
    fn admit(&mut self, point: usize, epoch: u32, at: AdmissionTime, metrics: Vec<Metric>) {
        let generation = Arc::clone(&self.generation);
        self.admit_under(point, epoch, at, &generation, metrics);
    }

    fn admit_under(
        &mut self, point: usize, epoch: u32, at: AdmissionTime, generation: &GenerationRef, metrics: Vec<Metric>,
    ) {
        let mut routed = vec![Vec::new(); usize::from(self.lanes())];
        for metric in metrics {
            routed[usize::from(self.lane_of(&metric))].push(metric);
        }
        for (lane, metrics) in routed.into_iter().enumerate() {
            let header = self.header(point, epoch, at, generation);
            self.deliver(lane as u16, header, metrics);
        }
    }

    /// Sends every lane a header-only batch of point `point` in `epoch` at `at`: a heartbeat.
    fn heartbeat(&mut self, point: usize, epoch: u32, at: AdmissionTime) {
        self.admit(point, epoch, at, Vec::new());
    }

    /// Runs time-driven work in every lane `elapsed` after the start, with the wall clock as long after `B`.
    fn advance(&mut self, elapsed: Duration) {
        for lane in 0..self.lanes() {
            self.advance_lane(lane, elapsed, t(elapsed.as_millis() as u64));
        }
    }

    /// Runs time-driven work in lane `lane` `elapsed` after the start, with the wall clock reading `wall`.
    fn advance_lane(&mut self, lane: u16, elapsed: Duration, wall: AdmissionTime) {
        self.wall.set(wall);
        let progress = self.call(lane, elapsed, PanicSite::AdvanceTime, |driver, turn, exits| {
            driver.advance_time(turn, exits)
        });
        assert_eq!(progress, Progress::Done);
    }

    /// Finishes lane `lane` after its input ended, with the wall clock reading `wall`.
    fn finish_lane(&mut self, lane: u16, wall: AdmissionTime) {
        self.wall.set(wall);
        self.call(lane, Duration::ZERO, PanicSite::Finish, |driver, turn, exits| {
            driver.finish(turn, exits, DrainPolicy::Shutdown)
        });
    }

    /// Finishes every lane, with the wall clock reading `wall`.
    fn finish(&mut self, wall: AdmissionTime) {
        for lane in 0..self.lanes() {
            self.finish_lane(lane, wall);
        }
    }

    /// Publishes every lane's telemetry.
    fn publish(&mut self) {
        let _local = metrics::set_default_local_recorder(&self.recorder);
        for driver in &mut self.drivers {
            driver.publish_telemetry();
        }
    }

    /// Returns the total of every counter named `name`, over its labels.
    fn counter(&self, name: &str) -> u64 {
        self.recorder
            .counters()
            .into_iter()
            .filter(|(key, _)| key.name() == name)
            .map(|(_, value)| value)
            .sum()
    }

    fn records(&self) -> Vec<(i64, u64, Vec<String>)> {
        grouped(&self.delivered)
    }

    /// Returns the lanes that delivered a record of `name`.
    fn lanes_of(&self, name: &str) -> Vec<u16> {
        let mut lanes = self
            .delivered
            .iter()
            .filter(|record| record.metric.context().name().as_ref() == name)
            .map(|record| record.lane)
            .collect::<Vec<_>>();
        lanes.dedup();
        lanes
    }
}

/// Returns names, from `prefix0` on, until there are `count` on each of `lanes` lanes, in the lanes' order.
fn names_on_lanes(prefix: &str, lanes: u16, count: usize) -> Vec<Vec<String>> {
    let groups = KeyGroups::new(lanes);
    let mut on = vec![Vec::new(); usize::from(lanes)];
    for index in 0.. {
        if on.iter().all(|names: &Vec<String>| names.len() >= count) {
            break;
        }
        let name = format!("{}{}", prefix, index);
        let lane = groups.lane_of(Context::from_parts(name.clone(), TagSet::default()).key().as_u64());
        if on[usize::from(lane)].len() < count {
            on[usize::from(lane)].push(name);
        }
    }
    on
}

fn named_counter(name: &str, value: f64) -> Metric {
    Metric::counter(Context::from_parts(name.to_string(), TagSet::default()), value)
}

fn named_gauge(name: &str, value: f64) -> Metric {
    Metric::gauge(Context::from_parts(name.to_string(), TagSet::default()), value)
}

/// Runs `script` on `dsd_agg` on two lanes and on one lane, both fed by `d` points, and returns both.
fn on_two_lanes_and_one(d: u16, script: impl Fn(&mut Lanes)) -> (Lanes, Lanes) {
    let (mut two, mut one) = (Lanes::new(d, 2), Lanes::new(d, 1));
    script(&mut two);
    script(&mut one);
    (two, one)
}

#[test]
fn every_lane_flushes_a_tick_at_the_second_the_first_lane_at_it_read_and_the_lanes_deliver_what_one_lane_does() {
    // Five counters and two gauges on each lane. The tick due at 15 runs at 15.2 in lane 0, whose wall clock reads
    // 15.2, and in lane 1 at a wall clock of 16.4: both flush at 15, the second lane 0 read. At 30, the counters gain
    // zeros for [10, 20) and [20, 30). The aggregation counts one flush per tick, as one lane does.
    let names = names_on_lanes("c", 2, 5);
    let gauges = names_on_lanes("g", 2, 2);
    let script = |lanes: &mut Lanes| {
        let mut metrics = names
            .iter()
            .flatten()
            .map(|name| named_counter(name, 1.0))
            .collect::<Vec<_>>();
        metrics.extend(gauges.iter().flatten().map(|name| named_gauge(name, 5.0)));
        lanes.admit(0, 0, t(3_000), metrics);
        lanes.admit(
            0,
            0,
            t(8_500),
            names.iter().flatten().map(|name| named_counter(name, 2.0)).collect(),
        );
        lanes.heartbeat(0, 0, t(15_000));
        let lanes_count = lanes.lanes();
        lanes.advance_lane(0, Duration::from_millis(15_200), t(15_200));
        if lanes_count > 1 {
            lanes.advance_lane(1, Duration::from_millis(15_200), t(16_400));
        }
        lanes.heartbeat(0, 0, t(30_000));
        lanes.advance(Duration::from_millis(30_700));
        lanes.publish();
    };
    let (two, one) = on_two_lanes_and_one(1, script);
    let records = two.records();
    assert_eq!(records, one.records());
    let first = &names[0][0];
    let second = &names[1][0];
    assert!(
        records[0].2.contains(&format!("{} rate/10s [0:3]", first)),
        "{:?}",
        records
    );
    assert!(
        records[0].2.contains(&format!("{} rate/10s [0:3]", second)),
        "{:?}",
        records
    );
    assert!(
        records[1].2.contains(&format!("{} rate/10s [10:0, 20:0]", second)),
        "{:?}",
        records
    );
    assert_eq!(
        records
            .iter()
            .map(|(at, generation, _)| (*at, *generation))
            .collect::<Vec<_>>(),
        vec![(15_000, 1), (30_000, 1)],
        "lane 1 flushed at the second lane 0 read"
    );
    assert_eq!(two.lanes_of(first), vec![0]);
    assert_eq!(two.lanes_of(second), vec![1]);
    assert_eq!(
        two.counter("aggregate_flushes_total"),
        2,
        "one flush per tick, by the first lane"
    );
    assert_eq!(one.counter("aggregate_flushes_total"), 2);
    assert_eq!(two.counter("plan_lane_misrouted_total"), 0);
    assert_eq!(two.counter("plan_lane_tick_divergence_total"), 0);
}

#[test]
fn a_lane_that_was_empty_starts_its_idle_counters_where_the_lanes_last_flush_left_them() {
    // Fixture X1 with 5-second buckets: `a` (lane 0) at 3 flushes at 15 and 30; lane 1 is empty at both ticks. `b`
    // arrives in lane 1 at 31: at the tick at 45 it gains zeros from `align(30)`, as one lane's would, at 35 and 40,
    // not none from lane 1's own never.
    let a = names_on_lanes("a", 2, 1)[0][0].clone();
    let b = names_on_lanes("b", 2, 1)[1][0].clone();
    let script = |lanes: &mut Lanes| {
        lanes.admit(0, 0, t(3_000), vec![named_counter(&a, 1.0)]);
        for tick in [15_000, 30_000] {
            lanes.heartbeat(0, 0, t(tick));
            lanes.advance(Duration::from_millis(tick + 100));
        }
        lanes.admit(0, 0, t(31_000), vec![named_counter(&b, 2.0)]);
        lanes.heartbeat(0, 0, t(45_000));
        lanes.advance(Duration::from_millis(45_100));
    };
    let plan = lane_plan("5s", "aggregate");
    let mut two = Lanes::with(&plan, OperatorRegistry::native(), 1, 2, PanicBudget::DEFAULT_PROGRAM);
    let mut one = Lanes::with(&plan, OperatorRegistry::native(), 1, 1, PanicBudget::DEFAULT_PROGRAM);
    script(&mut two);
    script(&mut one);
    assert_eq!(two.records(), one.records());
    let at_45 = two
        .records()
        .into_iter()
        .find(|(at, _, _)| *at == 45_000)
        .expect("a flush at 45");
    assert!(
        at_45.2.contains(&format!("{} rate/5s [30:2, 35:0, 40:0]", b)),
        "{:?}",
        at_45
    );
    assert_eq!(two.lanes_of(&b), vec![1]);
}

#[test]
fn a_reset_closes_every_lanes_old_epoch_at_its_exact_watermark_once_every_point_switched() {
    // Two points. Point 1 steps back an hour while point 0 still sends old-epoch batches, the last at 9.8. Every lane
    // opens its window at point 1's first new-epoch header: new data goes to fresh instances, old data to the old
    // ones, and nothing ticks: the tick due at 15 finds the old epoch's instances reset-retiring and the new epoch's
    // watermark unknown. Once point 0 switches, every lane drains the old epoch at 9.5, the earlier of the two points'
    // final latest times there (point 1's), stamped there in output epoch 0, the gauge holding point 0's late 7; the
    // new timeline flushes at its own seconds afterwards, in output epoch 1, its instances continuing the lineage: one
    // record per context and bucket, the new timeline's batches before and after the close together.
    let x = names_on_lanes("x", 2, 1);
    let y = names_on_lanes("y", 2, 1);
    let z = names_on_lanes("z", 2, 1);
    let script = |lanes: &mut Lanes| {
        let old = x.iter().flatten().map(|name| named_counter(name, 1.0)).collect();
        lanes.admit(0, 0, t(3_000), old);
        lanes.admit(
            1,
            0,
            t(4_000),
            y.iter().flatten().map(|name| named_gauge(name, 2.0)).collect(),
        );
        lanes.heartbeat(0, 0, t(9_000));
        lanes.heartbeat(1, 0, t(9_500));
        lanes.admit(
            1,
            1,
            back(1_000),
            z.iter().flatten().map(|name| named_counter(name, 4.0)).collect(),
        );
        lanes.admit(
            0,
            0,
            t(9_800),
            y.iter().flatten().map(|name| named_gauge(name, 7.0)).collect(),
        );
        lanes.advance_all_at(Duration::from_millis(15_200), back(15_200));
        lanes.heartbeat(0, 1, back(2_000));
        // After the close, the new timeline's batches reach the instance that held its earlier data.
        lanes.admit(
            1,
            1,
            back(5_000),
            z.iter().flatten().map(|name| named_counter(name, 1.0)).collect(),
        );
        lanes.heartbeat(0, 1, back(30_000));
        lanes.heartbeat(1, 1, back(30_000));
        lanes.advance_all_at(Duration::from_millis(30_200), back(30_200));
    };
    let (two, one) = on_two_lanes_and_one(2, script);
    assert_eq!(two.records(), one.records());
    let mut expected = vec![group(
        9_500,
        1,
        &[
            &format!("{} rate/10s [0:1]", x[0][0]),
            &format!("{} rate/10s [0:1]", x[1][0]),
            &format!("{} gauge [0:7]", y[0][0]),
            &format!("{} gauge [0:7]", y[1][0]),
        ],
    )];
    expected.insert(
        0,
        group(
            relative(back(30_000)),
            1,
            &[
                &format!("{} rate/10s [-3600:5]", z[0][0]),
                &format!("{} rate/10s [-3600:5]", z[1][0]),
            ],
        ),
    );
    assert_eq!(two.records(), expected);
    let epochs = |lanes: &Lanes| {
        let mut epochs = lanes
            .delivered
            .iter()
            .map(|record| (relative(record.admitted_at), record.epoch))
            .collect::<Vec<_>>();
        epochs.sort();
        epochs.dedup();
        epochs
    };
    assert_eq!(epochs(&two), vec![(relative(back(30_000)), 1), (9_500, 0)]);
    assert_eq!(epochs(&two), epochs(&one));
}

impl Lanes {
    /// Runs time-driven work in every lane `elapsed` after the start, with the wall clock reading `wall`.
    fn advance_all_at(&mut self, elapsed: Duration, wall: AdmissionTime) {
        for lane in 0..self.lanes() {
            self.advance_lane(lane, elapsed, wall);
        }
    }
}

#[test]
fn the_first_lane_to_open_a_reset_window_stops_every_lanes_old_epoch_ticks() {
    // Both points' watermarks are at 15 when point 0 steps back. Lane 0 receives point 0's new-epoch header first and
    // opens its window, marking the lineage's record with the newest tick any lane resolved: none. Lane 1 runs the
    // tick due at 15 before it receives that header: its watermark has reached 15, but the mark forbids the tick, so
    // it fires in neither lane, and the reset drains both lanes' old epoch at 15 once point 1 switches, as one lane,
    // whose window opened before the tick, does.
    let names = names_on_lanes("c", 2, 1);
    let mut two = Lanes::new(2, 2);
    let mut one = Lanes::new(2, 1);
    for lanes in [&mut two, &mut one] {
        lanes.admit(
            0,
            0,
            t(3_000),
            names.iter().flatten().map(|name| named_counter(name, 1.0)).collect(),
        );
        lanes.heartbeat(0, 0, t(15_000));
        lanes.heartbeat(1, 0, t(15_000));
        let header = lanes.header(0, 1, back(1_000), &Arc::clone(&lanes.generation));
        lanes.deliver(0, header, Vec::new());
    }
    two.advance_lane(1, Duration::from_millis(15_200), t(15_200));
    assert!(two.delivered.is_empty(), "lane 1 fired no tick after lane 0's opening");
    one.advance_lane(0, Duration::from_millis(15_200), t(15_200));
    let header = two.header(0, 1, back(1_000), &Arc::clone(&two.generation));
    two.deliver(1, header, Vec::new());
    for lanes in [&mut two, &mut one] {
        lanes.heartbeat(1, 1, back(2_000));
    }
    let expected = vec![group(
        15_000,
        1,
        &[
            &format!("{} rate/10s [0:1]", names[0][0]),
            &format!("{} rate/10s [0:1]", names[1][0]),
        ],
    )];
    assert_eq!(two.records(), expected);
    assert_eq!(one.records(), expected);
}

#[test]
fn a_lane_whose_input_ends_inside_a_reset_window_closes_it_first_and_each_timeline_at_its_own_time() {
    // After a jump back from 7,201 to 3,600 at point 0, point 1 never switches. At shutdown each lane closes its window
    // first: the old epoch drains at its watermark so far, 7,200.5, point 0's final latest time, emitting `a`; then the
    // new timeline's instance closes at the later of its own latest stamp, 3,600.5, and the wall clock, 3,601, never at
    // 7,201: `b`'s bucket is still open there, and is discarded.
    let a = names_on_lanes("a", 2, 1);
    let b = names_on_lanes("b", 2, 1);
    let script = |lanes: &mut Lanes| {
        lanes.admit(
            0,
            0,
            t(7_200_500),
            a.iter().flatten().map(|name| named_counter(name, 1.0)).collect(),
        );
        lanes.heartbeat(1, 0, t(7_201_000));
        lanes.admit(
            0,
            1,
            t(3_600_500),
            b.iter().flatten().map(|name| named_counter(name, 2.0)).collect(),
        );
        lanes.finish(t(3_601_000));
        lanes.publish();
    };
    let (two, one) = on_two_lanes_and_one(2, script);
    let expected = vec![group(
        7_200_500,
        1,
        &[
            &format!("{} rate/10s [7200:1]", a[0][0]),
            &format!("{} rate/10s [7200:1]", a[1][0]),
        ],
    )];
    assert_eq!(two.records(), expected);
    assert_eq!(one.records(), expected);
}

#[test]
fn lanes_finishing_apart_close_at_the_time_the_first_one_set() {
    // Fixture X7: lane 0 finishes with the wall clock at 9.95 and lane 1 at 10.15, across the boundary of the bucket
    // [0, 10). Both close at 9.95, the time the first lane to finish set, so the bucket is discarded in both lanes, as
    // one lane finishing at 9.95 discards it, never flushed in one and discarded in the other.
    let names = names_on_lanes("c", 2, 1);
    let mut two = Lanes::new(1, 2);
    two.admit(
        0,
        0,
        t(8_000),
        names.iter().flatten().map(|name| named_counter(name, 1.0)).collect(),
    );
    two.heartbeat(0, 0, t(9_900));
    two.finish_lane(0, t(9_950));
    two.finish_lane(1, t(10_150));
    assert!(two.delivered.is_empty(), "{:?}", two.records());

    let mut one = Lanes::new(1, 1);
    one.admit(
        0,
        0,
        t(8_000),
        names.iter().flatten().map(|name| named_counter(name, 1.0)).collect(),
    );
    one.heartbeat(0, 0, t(9_900));
    one.finish(t(9_950));
    assert!(one.delivered.is_empty());
}

#[test]
fn a_cut_crosses_each_state_lane_on_its_own_and_counts_the_contexts_held_twice_meanwhile() {
    // A cut from 10-second to 30-second buckets at 120, over two points. Point 0 switches to the new generation first:
    // until point 1 does, lane 0 admits point 0's new batches into the new lineage while point 1's old ones still feed
    // the retiring one, so a context both points send is held by both, and counted. Lane 0 crosses at point 1's first
    // new header, making the old lineage's final flush at 120; lane 1, whose share holds nothing both points send,
    // crosses at the same header. The values are those of one lane.
    let shared_name = names_on_lanes("s", 2, 1)[0][0].clone();
    let other = names_on_lanes("o", 2, 1)[1][0].clone();
    let old_plan = lane_plan("10s", "aggregate");
    let new_plan = lane_plan("30s", "aggregate");
    let mut results = Vec::new();
    for p in [2, 1] {
        let mut lanes = Lanes::with(
            &old_plan,
            OperatorRegistry::native(),
            2,
            p,
            PanicBudget::DEFAULT_PROGRAM,
        );
        lanes.admit(
            0,
            0,
            t(95_000),
            vec![named_counter(&shared_name, 1.0), named_counter(&other, 1.0)],
        );
        lanes.admit(1, 0, t(96_000), vec![named_counter(&shared_name, 1.0)]);
        let new = lanes.prepare_cut(&new_plan, 2, t(120_000));
        lanes.admit_under(0, 0, t(121_000), &new, vec![named_counter(&shared_name, 5.0)]);
        lanes.admit(1, 0, t(119_000), vec![named_counter(&shared_name, 2.0)]);
        if p > 1 {
            lanes.publish();
            assert_eq!(
                lanes.gauge("plan_lane_crossing_overlap_contexts", 0),
                Some(1.0),
                "lane 0 holds the shared context in both lineages"
            );
        }
        lanes.admit_under(1, 0, t(122_000), &new, vec![named_counter(&other, 3.0)]);
        lanes.finish(t(160_000));
        lanes.publish();
        if p > 1 {
            assert_eq!(lanes.gauge("plan_lane_crossing_overlap_contexts", 0), Some(0.0));
            assert_eq!(lanes.counter("plan_lane_crossings_total"), 2, "one crossing per lane");
        }
        results.push(lanes.records());
    }
    assert_eq!(results[0], results[1]);
    assert_eq!(
        results[0],
        vec![
            group(
                120_000,
                1,
                &[
                    &format!("{} rate/10s [110:2, 90:2]", shared_name),
                    &format!("{} rate/10s [90:1]", other),
                ],
            ),
            group(
                160_000,
                2,
                &[
                    &format!("{} rate/30s [120:5]", shared_name),
                    &format!("{} rate/30s [120:3]", other),
                ],
            ),
        ]
    );
}

impl Lanes {
    /// Prepares `plan` as generation `id` in every lane, retiring the boot lineage at a cut at `at` and binding a new
    /// lineage, built against one new cell, and publishes the cut; returns the new generation.
    fn prepare_cut(&mut self, plan: &Plan, id: u64, at: AdmissionTime) -> GenerationRef {
        let compiled = Direct::compiled(&self.registry, &self.frame, plan);
        assert_eq!(
            entry_into(&compiled, self.slot.id, "dsd_agg"),
            self.entry,
            "the candidate's batches reach the slot at the same entry"
        );
        let program = Arc::clone(&compiled.programs[self.slot.id.get()]);
        let new: GenerationRef = Arc::new(GenerationToken::new(GenerationId::new(id)));
        new.set_cut(CutBoundary {
            at,
            replaces: self.generation.id(),
            epochs: self.points.iter().map(|point| (*point, 0)).collect(),
        })
        .unwrap();
        let old_lineage = {
            let boot = Direct::compiled(&self.registry, &self.frame, &self.plan);
            lineage_of(&boot.programs[self.slot.id.get()])
        };
        let new_lineage = lineage_of(&program);
        let node: NodeId = "dsd_agg".parse().unwrap();
        let cell = LaneCell::new();
        for lane in 0..self.lanes() {
            let kernels = self.built(&program, &cell, lane);
            let prepare = PrepareRegion::new(
                Arc::clone(&new),
                Arc::clone(&program),
                kernels,
                PanicBudget::DEFAULT_PROGRAM,
                &NoTelemetry,
            )
            .with_state_bindings(vec![
                StateBinding::Retire {
                    node: node.clone(),
                    key: old_lineage.clone(),
                    from: self.generation.id(),
                },
                StateBinding::New {
                    node: node.clone(),
                    key: new_lineage.clone(),
                },
            ]);
            let mut sink = ExitSink::new(self.exits);
            self.drivers[usize::from(lane)]
                .control(
                    &mut RegionTurn::new(self.start, &mut Budget::unlimited()),
                    RegionControl::Prepare(Box::new(prepare)),
                    &mut sink,
                )
                .unwrap();
        }
        self.schedule.store(Arc::new(AdmissionSchedule::Cut {
            old: Arc::clone(&self.generation),
            new: Arc::clone(&new),
            at,
        }));
        new
    }

    /// Returns the value of lane `lane`'s gauge named `name`.
    fn gauge(&self, name: &str, lane: u16) -> Option<f64> {
        self.recorder
            .gauges()
            .into_iter()
            .find(|(key, _)| {
                key.name() == name
                    && key
                        .labels()
                        .any(|label| label.key() == "lane" && label.value() == lane.to_string())
            })
            .map(|(_, value)| value)
    }
}

/// `dsd_agg` of the panicking operator on two lanes, under an artifact's budget.
fn panicking_lanes() -> Lanes {
    Lanes::with(
        &lane_plan("10s", PANICKING),
        panicking_registry(),
        1,
        2,
        PanicBudget::ARTIFACT,
    )
}

#[test]
fn a_panic_in_one_state_lane_loses_only_that_lanes_contexts_and_asks_for_one_rollback() {
    // Trace T-L5: the kernel of lane 1 panics at the tick at 15, under an artifact's budget. Lane 1's instance restarts
    // empty, its points lost; lane 0 flushes its own contexts as usual. The generation asks to be rolled back once,
    // and keeps running in both lanes.
    let on_lane_0 = names_on_lanes("c", 2, 1)[0][0].clone();
    let on_lane_1 = names_on_lanes("o", 2, 1)[1][0].clone();
    let trigger = name_ending_on_lane("panic.tick", 1, 2);
    let mut lanes = panicking_lanes();
    lanes.admit(
        0,
        0,
        t(3_000),
        vec![named_counter(&on_lane_0, 1.0), named_counter(&trigger, 1.0)],
    );
    lanes.heartbeat(0, 0, t(15_000));
    lanes.advance(Duration::from_millis(15_200));
    assert_eq!(
        lanes.records(),
        vec![group(15_000, 1, &[&format!("{} rate/10s [0:1]", on_lane_0)])],
        "only the other lane's contexts are flushed"
    );
    let failures = lanes.failures.lock().unwrap().clone();
    assert_eq!(failures.len(), 1, "{:?}", failures);
    assert!(failures[0].1.contains("panicked at a flush tick"), "{}", failures[0].1);

    // Both lanes keep processing the generation's batches.
    lanes.admit(
        0,
        0,
        t(21_000),
        vec![named_counter(&on_lane_0, 2.0), named_counter(&on_lane_1, 3.0)],
    );
    lanes.heartbeat(0, 0, t(30_000));
    lanes.advance(Duration::from_millis(30_200));
    assert_eq!(
        lanes.records()[1],
        group(
            30_000,
            1,
            &[
                &format!("{} rate/10s [10:0, 20:2]", on_lane_0),
                &format!("{} rate/10s [20:3]", on_lane_1),
            ],
        )
    );
    assert_eq!(
        lanes.failures.lock().unwrap().len(),
        1,
        "the rollback is asked for once"
    );
}

/// Returns a name ending in `suffix` whose context lane `lane` of `lanes` owns.
fn name_ending_on_lane(suffix: &str, lane: u16, lanes: u16) -> String {
    let groups = KeyGroups::new(lanes);
    (0..)
        .map(|index| format!("p{}.{}", index, suffix))
        .find(|name| groups.lane_of(Context::from_parts(name.clone(), TagSet::default()).key().as_u64()) == lane)
        .expect("some name routes to every lane")
}

#[test]
fn a_generation_that_exhausts_its_panic_budget_in_one_lane_stops_in_every_lane() {
    // An artifact's budget survives three panics within a minute. Panics alternate between the lanes; the fourth, in
    // lane 1, fails the generation, whose batches both lanes drop from then on.
    let mut lanes = panicking_lanes();
    for (round, lane) in [0u16, 1, 0, 1].into_iter().enumerate() {
        let header = lanes.header(0, 0, t(3_000 + round as u64), &Arc::clone(&lanes.generation));
        let trigger = name_ending_on_lane(&format!("r{}.panic.consume", round), lane, 2);
        lanes.deliver(lane, header, vec![named_counter(&trigger, 1.0)]);
    }
    // The first panic asked for the generation's rollback, once across lanes; the failure itself is not reported
    // again.
    assert_eq!(lanes.failures.lock().unwrap().len(), 1);
    // Lane 0 drops the failed generation's batches too: nothing it admits now is flushed.
    let kept = names_on_lanes("k", 2, 1)[0][0].clone();
    lanes.admit(0, 0, t(4_000), vec![named_counter(&kept, 1.0)]);
    lanes.heartbeat(0, 0, t(15_000));
    lanes.advance(Duration::from_millis(15_200));
    assert!(lanes.delivered.is_empty(), "{:?}", lanes.records());
}

#[test]
fn with_one_point_a_reset_drains_every_lane_at_once_and_each_lane_continues_in_the_successor_record() {
    // Stage 6's reset on two lanes: with one point the window opens and closes on the reset's header, so every lane
    // drains at once at the watermark before it, 20, where `c` and the idle counter `d` close [10, 20), in output epoch
    // 0. Each lane's instance continues empty in the new epoch, joining the successor record, whose first tick is the
    // old record's next one, at 30: `d`'s new-timeline point is flushed there, at the new timeline's second.
    let c = names_on_lanes("c", 2, 1)[0][0].clone();
    let d = names_on_lanes("d", 2, 1)[1][0].clone();
    let script = |lanes: &mut Lanes| {
        lanes.admit(0, 0, t(3_000), vec![named_counter(&c, 1.0)]);
        lanes.admit(0, 0, t(8_000), vec![named_counter(&d, 1.0)]);
        lanes.heartbeat(0, 0, t(15_000));
        lanes.advance(Duration::from_millis(15_200));
        lanes.admit(0, 0, t(16_000), vec![named_counter(&c, 2.0)]);
        lanes.heartbeat(0, 0, t(20_000));
        lanes.admit(0, 1, back(1_000), vec![named_counter(&d, 4.0)]);
        lanes.heartbeat(0, 1, back(30_000));
        lanes.advance_all_at(Duration::from_millis(30_200), back(30_200));
    };
    let (two, one) = on_two_lanes_and_one(1, script);
    let expected = vec![
        group(relative(back(30_000)), 1, &[&format!("{} rate/10s [-3600:4]", d)]),
        group(
            15_000,
            1,
            &[&format!("{} rate/10s [0:1]", c), &format!("{} rate/10s [0:1]", d)],
        ),
        group(
            20_000,
            1,
            &[&format!("{} rate/10s [10:2]", c), &format!("{} rate/10s [10:0]", d)],
        ),
    ];
    assert_eq!(two.records(), expected);
    assert_eq!(one.records(), expected);
}

/// `B` minus two hours plus `relative_millis` milliseconds: the timeline after the clock stepped back twice.
fn back_twice(relative_millis: u64) -> AdmissionTime {
    AdmissionTime::from_unix_nanos(((B - 7_200) * 1_000 + relative_millis) * 1_000_000)
}

#[test]
fn a_reset_inside_a_window_closes_every_older_epoch_in_order_once_the_last_point_reached_the_newest() {
    // Point 0 steps back twice, sending data in each timeline, while point 1 is still in the first. Point 1 then jumps
    // straight to the third timeline: both older epochs close, oldest first, each at its own watermark: epoch 0 at
    // 9, the earlier final latest time, and epoch 1 at point 0's only stamp there, since point 1 never stamped in it;
    // each ends an output epoch. The third timeline's instances continue the lineage.
    let x = names_on_lanes("x", 2, 1);
    let u = names_on_lanes("u", 2, 1);
    let w = names_on_lanes("w", 2, 1);
    let all = |names: &[Vec<String>], value| names.iter().flatten().map(|name| named_counter(name, value)).collect();
    let script = |lanes: &mut Lanes| {
        lanes.admit(0, 0, t(3_000), all(&x, 1.0));
        lanes.heartbeat(0, 0, t(9_000));
        lanes.heartbeat(1, 0, t(9_500));
        lanes.admit(0, 1, back(1_000), all(&u, 2.0));
        lanes.admit(0, 2, back_twice(1_000), all(&w, 3.0));
        lanes.heartbeat(1, 2, back_twice(2_000));
        lanes.heartbeat(0, 2, back_twice(30_000));
        lanes.heartbeat(1, 2, back_twice(30_000));
        lanes.advance_all_at(Duration::from_millis(30_200), back_twice(30_200));
    };
    let (two, one) = on_two_lanes_and_one(2, script);
    let names = |names: &[Vec<String>], points: &str| {
        names
            .iter()
            .flatten()
            .map(|name| format!("{} rate/10s [{}]", name, points))
            .collect::<Vec<_>>()
    };
    let sorted = |at, mut records: Vec<String>| {
        records.sort();
        (at, 1, records)
    };
    let expected = vec![
        sorted(relative(back_twice(30_000)), names(&w, "-7200:3")),
        sorted(relative(back(1_000)), names(&u, "-3600:2")),
        sorted(9_000, names(&x, "0:1")),
    ];
    assert_eq!(two.records(), expected);
    assert_eq!(one.records(), expected);
    let mut epochs = two
        .delivered
        .iter()
        .map(|record| (relative(record.admitted_at), record.epoch))
        .collect::<Vec<_>>();
    epochs.sort();
    epochs.dedup();
    assert_eq!(
        epochs,
        vec![
            (relative(back_twice(30_000)), 2),
            (relative(back(1_000)), 1),
            (9_000, 0)
        ]
    );
}

#[test]
#[cfg(not(debug_assertions))]
fn an_event_another_lane_owns_is_admitted_and_counted_as_misrouted() {
    let name = names_on_lanes("c", 2, 1)[0][0].clone();
    let mut lanes = Lanes::new(1, 2);
    let header = lanes.header(0, 0, t(3_000), &Arc::clone(&lanes.generation));
    lanes.deliver(1, header, vec![named_counter(&name, 1.0)]);
    lanes.publish();
    assert_eq!(lanes.counter("plan_lane_misrouted_total"), 1);
}

#[test]
#[cfg(debug_assertions)]
#[should_panic(expected = "a state lane admitted events another lane owns")]
fn an_event_another_lane_owns_fails_a_debug_assertion() {
    let name = names_on_lanes("c", 2, 1)[0][0].clone();
    let mut lanes = Lanes::new(1, 2);
    let header = lanes.header(0, 0, t(3_000), &Arc::clone(&lanes.generation));
    let start = lanes.start;
    let exits = lanes.exits;
    let mut buffer = EventsBuffer::with_capacity(1).with_header(header);
    assert!(buffer
        .try_push(saluki_core::data_model::event::Event::Metric(named_counter(&name, 1.0)))
        .is_none());
    lanes.drivers[1].process(
        &mut RegionTurn::new(start, &mut Budget::unlimited()),
        buffer,
        &mut ExitSink::new(exits),
    );
}

#[test]
fn lanes_that_start_apart_tick_at_the_instants_the_first_lane_to_start_set() {
    // Lane 1 starts a second after lane 0: both take the first tick's instant from the lineage's record, set by lane 0
    // at its start plus the interval, so at 15.2 both lanes flush the tick due at 15, as one lane does.
    let names = names_on_lanes("c", 2, 1);
    let plan = lane_plan("10s", "aggregate");
    let mut two = Lanes::started_apart(
        &plan,
        OperatorRegistry::native(),
        1,
        2,
        PanicBudget::DEFAULT_PROGRAM,
        Duration::from_secs(1),
    );
    two.admit(
        0,
        0,
        t(3_000),
        names.iter().flatten().map(|name| named_counter(name, 1.0)).collect(),
    );
    two.heartbeat(0, 0, t(15_000));
    two.advance(Duration::from_millis(15_200));
    assert_eq!(
        two.records(),
        vec![group(
            15_000,
            1,
            &[
                &format!("{} rate/10s [0:1]", names[0][0]),
                &format!("{} rate/10s [0:1]", names[1][0]),
            ],
        )]
    );
}

#[test]
fn a_reset_successor_starts_at_the_old_records_deadline_whichever_lane_closes_its_window_first() {
    // Two points. Both lanes flush the tick at 15. Lane 0 opens its window at point 0's new-epoch header, marking the
    // record at ordinal 1; lane 1 reaches the tick due at 30 before that header, and skips it, as the mark forbids,
    // moving its own deadline to 45. Once point 1 switches, every lane drains the old epoch at 30, and the lineage
    // continues in the successor record, whose first tick is the old record's current deadline, 30, as on one lane,
    // whichever lane closes its window first: the new timeline's counters are flushed at the advance at 31.
    let c = names_on_lanes("c", 2, 1)[0][0].clone();
    let d = names_on_lanes("d", 2, 1)[1][0].clone();
    let run = |p: u16, lane_1_first: bool| {
        let mut lanes = Lanes::new(2, p);
        lanes.admit(0, 0, t(3_000), vec![named_counter(&c, 1.0), named_counter(&d, 1.0)]);
        lanes.heartbeat(0, 0, t(15_000));
        lanes.heartbeat(1, 0, t(15_000));
        lanes.advance(Duration::from_millis(15_200));
        lanes.heartbeat(0, 0, t(30_000));
        lanes.heartbeat(1, 0, t(30_000));
        let generation = Arc::clone(&lanes.generation);
        let header = lanes.header(0, 1, back(1_000), &generation);
        lanes.deliver(0, header, Vec::new());
        if p > 1 {
            lanes.advance_lane(1, Duration::from_millis(30_200), t(30_200));
            let header = lanes.header(0, 1, back(1_000), &generation);
            lanes.deliver(1, header, Vec::new());
        } else {
            lanes.advance_lane(0, Duration::from_millis(30_200), t(30_200));
        }
        let order = if lane_1_first { vec![1, 0] } else { (0..p).collect() };
        for lane in order {
            let header = lanes.header(1, 1, back(2_000), &generation);
            lanes.deliver(lane, header, Vec::new());
        }
        let start = lanes.start;
        let deadlines = lanes
            .drivers
            .iter()
            .map(|driver| driver.next_deadline().map(|at| at - start))
            .collect::<Vec<_>>();
        lanes.admit(0, 1, back(5_000), vec![named_counter(&c, 2.0), named_counter(&d, 2.0)]);
        lanes.heartbeat(0, 1, back(31_000));
        lanes.heartbeat(1, 1, back(31_000));
        lanes.advance_all_at(Duration::from_millis(31_000), back(31_000));
        (deadlines, lanes.records())
    };
    let expected = vec![
        group(
            relative(back(31_000)),
            1,
            &[
                &format!("{} rate/10s [-3600:2]", c),
                &format!("{} rate/10s [-3600:2]", d),
            ],
        ),
        group(
            15_000,
            1,
            &[&format!("{} rate/10s [0:1]", c), &format!("{} rate/10s [0:1]", d)],
        ),
        group(
            30_000,
            1,
            &[
                &format!("{} rate/10s [10:0, 20:0]", c),
                &format!("{} rate/10s [10:0, 20:0]", d),
            ],
        ),
    ];
    let thirty = Some(Duration::from_secs(30));
    assert_eq!(run(1, false), (vec![thirty], expected.clone()));
    assert_eq!(
        run(2, false),
        (vec![thirty; 2], expected.clone()),
        "lane 0 closes first"
    );
    assert_eq!(run(2, true), (vec![thirty; 2], expected), "lane 1 closes first");
}

#[test]
fn a_lane_closing_a_window_over_two_epochs_first_starts_the_newest_epochs_ticks_after_the_epoch_between() {
    // Two points, two lanes, `x` on lane 0 and `y` on lane 1; both lanes flush the tick at 15. Point 0 steps back into
    // epoch 1, and lane 1, which hears point 1 there too, closes its window and ticks the lineage's epoch 1 at 30 and
    // 45; lane 0 never hears point 1 in epoch 1, so its window over epochs 0 and 1 stays open, holding `x`'s epoch 1
    // data beside the bound instance. Point 0 steps back again, into epoch 2, and lane 0 hears both points there first:
    // it closes its window over both older epochs, and continues the lineage through its bound instance of epoch 0,
    // the first lane to reach the lineage's record of epoch 2. That record starts at the current deadline of its
    // record of epoch 1, the newest epoch before it: 60, after the ticks lane 1 fired there, as one core keeps its
    // schedule across a reset. At epoch 0's, 30, lane 1 would fire epoch 2's first ticks at the instants of epoch 1's.
    let x = names_on_lanes("x", 2, 1)[0][0].clone();
    let y = names_on_lanes("y", 2, 1)[1][0].clone();
    let mut lanes = Lanes::new(2, 2);
    lanes.admit(0, 0, t(3_000), vec![named_counter(&x, 1.0), named_counter(&y, 1.0)]);
    lanes.heartbeat(0, 0, t(15_000));
    lanes.heartbeat(1, 0, t(15_000));
    lanes.advance(Duration::from_millis(15_200));
    let generation = Arc::clone(&lanes.generation);

    // Epoch 1: point 0 at both lanes, point 1 at lane 1 only, which closes its window and ticks at 30 and 45.
    lanes.admit(0, 1, back(1_000), vec![named_counter(&x, 2.0), named_counter(&y, 2.0)]);
    let header = lanes.header(1, 1, back(2_000), &generation);
    lanes.deliver(1, header, Vec::new());
    for at in [30_000, 45_000] {
        lanes.heartbeat(0, 1, back(at));
        let header = lanes.header(1, 1, back(at), &generation);
        lanes.deliver(1, header, Vec::new());
        lanes.advance_lane(1, Duration::from_millis(at + 200), back(at + 200));
    }
    let start = lanes.start;
    assert_eq!(
        lanes.drivers[1].next_deadline().map(|at| at - start),
        Some(Duration::from_secs(60)),
        "lane 1 ticked epoch 1 at 30 and 45"
    );

    // Epoch 2: point 0 at both lanes, then point 1 at lane 0 first, which closes its window over epochs 0 and 1.
    lanes.heartbeat(0, 2, back_twice(1_000));
    let header = lanes.header(1, 2, back_twice(2_000), &generation);
    lanes.deliver(0, header, Vec::new());
    let header = lanes.header(1, 2, back_twice(2_000), &generation);
    lanes.deliver(1, header, Vec::new());
    let deadlines = lanes
        .drivers
        .iter()
        .map(|driver| driver.next_deadline().map(|at| at - start))
        .collect::<Vec<_>>();
    assert_eq!(deadlines, vec![Some(Duration::from_secs(60)); 2]);
}

#[test]
fn a_lane_passing_an_epoch_it_holds_nothing_of_stops_the_epochs_ticks_in_the_lanes_that_hold_it() {
    // As above, but lane 0 holds nothing of epoch 1, and lane 1 hears of epoch 2 only after its deadline at 60: lane 0,
    // closing its window into epoch 2 first, marks the lineage's record of epoch 1, which no lane had marked, as the
    // first lane to open a window past it, so that lane 1 fires no tick of epoch 1 after the ones every lane had
    // resolved, 30 and 45, holding its data for the epoch's reset drain, as one core stops an epoch's ticks once its
    // clock reset. The lineage's ticks go on in epoch 2 at 60, once.
    let x = names_on_lanes("x", 2, 1)[0][0].clone();
    let y = names_on_lanes("y", 2, 1)[1][0].clone();
    let mut lanes = Lanes::new(2, 2);
    lanes.admit(0, 0, t(3_000), vec![named_counter(&x, 1.0), named_counter(&y, 1.0)]);
    lanes.heartbeat(0, 0, t(15_000));
    lanes.heartbeat(1, 0, t(15_000));
    lanes.advance(Duration::from_millis(15_200));
    let generation = Arc::clone(&lanes.generation);
    lanes.admit(0, 1, back(1_000), vec![named_counter(&y, 2.0)]);
    let header = lanes.header(1, 1, back(2_000), &generation);
    lanes.deliver(1, header, Vec::new());
    for at in [30_000, 45_000] {
        lanes.heartbeat(0, 1, back(at));
        let header = lanes.header(1, 1, back(at), &generation);
        lanes.deliver(1, header, Vec::new());
        lanes.advance_lane(1, Duration::from_millis(at + 200), back(at + 200));
    }
    // Lane 0 hears both points in epoch 2; lane 1 reaches 60 in epoch 1 before it hears either.
    for point in [0, 1] {
        let header = lanes.header(point, 2, back_twice(1_000), &generation);
        lanes.deliver(0, header, Vec::new());
    }
    for point in [0, 1] {
        let header = lanes.header(point, 1, back(60_000), &generation);
        lanes.deliver(1, header, Vec::new());
    }
    lanes.advance_lane(1, Duration::from_millis(60_200), back(60_200));
    for point in [0, 1] {
        let header = lanes.header(point, 2, back_twice(60_000), &generation);
        lanes.deliver(1, header, Vec::new());
        let header = lanes.header(point, 2, back_twice(60_000), &generation);
        lanes.deliver(0, header, Vec::new());
    }
    lanes.advance_all_at(Duration::from_millis(60_300), back_twice(60_300));
    let start = lanes.start;
    let ticks = lanes.drivers[1]
        .take_time_events()
        .into_iter()
        .filter_map(|event| match event {
            crate::runtime::harness::LaneTimeEvent::Ticked {
                epoch,
                ordinal: Some(ordinal),
                scheduled,
                ..
            } => Some((epoch, ordinal, scheduled - start)),
            _ => None,
        })
        .collect::<Vec<_>>();
    let seconds = Duration::from_secs;
    assert_eq!(
        ticks,
        vec![
            (0, 1, seconds(15)),
            (1, 1, seconds(30)),
            (1, 2, seconds(45)),
            (2, 1, seconds(60))
        ]
    );
}

#[test]
fn with_one_point_a_reset_while_every_lane_is_gated_drops_the_gated_tick_and_continues_one_interval_later() {
    // One point. The tick due at 15 gates in every lane: the watermark is at 14. The reset then drains every lane at
    // once at 14, dropping the gated tick, as one lane does, so the lineage's next tick is the one due at 30, not one
    // at 15 in the new timeline: nothing more is flushed at 15.5, and one flush is counted, the reset's.
    let c = names_on_lanes("c", 2, 1)[0][0].clone();
    let d = names_on_lanes("d", 2, 1)[1][0].clone();
    let script = |lanes: &mut Lanes| {
        lanes.admit(0, 0, t(3_000), vec![named_counter(&c, 1.0), named_counter(&d, 1.0)]);
        lanes.heartbeat(0, 0, t(14_000));
        lanes.advance_all_at(Duration::from_millis(15_200), t(15_200));
        lanes.admit(0, 1, back(1_000), vec![named_counter(&c, 2.0), named_counter(&d, 2.0)]);
        lanes.heartbeat(0, 1, back(16_000));
        lanes.advance_all_at(Duration::from_millis(15_500), back(16_500));
        lanes.publish();
    };
    let (two, one) = on_two_lanes_and_one(1, script);
    let expected = vec![group(
        14_000,
        1,
        &[&format!("{} rate/10s [0:1]", c), &format!("{} rate/10s [0:1]", d)],
    )];
    assert_eq!(two.records(), expected);
    assert_eq!(one.records(), expected);
    for lanes in [&two, &one] {
        assert_eq!(lanes.counter("aggregate_flushes_total"), 1);
        for driver in &lanes.drivers {
            assert_eq!(driver.next_deadline(), Some(lanes.start + Duration::from_secs(30)));
        }
    }
}

#[test]
fn an_epoch_a_point_skipped_stays_open_for_the_other_points_batches_whichever_header_arrives_first() {
    // Point 1 skips epoch 1, two resets while it was blocked: its first header after epoch 0 is in epoch 2. Point 0's
    // batch of epoch 1 is aggregated and drained at epoch 1's watermark, point 0's only stamp there, whether point 1's
    // header reaches the lanes before it or after it; epoch 0 drains at 9, and both resets are counted, once per slot.
    let x = names_on_lanes("x", 2, 1);
    let u = names_on_lanes("u", 2, 1);
    let all = |names: &[Vec<String>], value| names.iter().flatten().map(|name| named_counter(name, value)).collect();
    let run = |p: u16, skipping_first: bool| {
        let mut lanes = Lanes::new(2, p);
        lanes.admit(0, 0, t(3_000), all(&x, 1.0));
        lanes.heartbeat(0, 0, t(9_000));
        lanes.heartbeat(1, 0, t(9_500));
        if skipping_first {
            lanes.heartbeat(1, 2, back_twice(1_000));
            lanes.admit(0, 1, back(1_000), all(&u, 2.0));
        } else {
            lanes.admit(0, 1, back(1_000), all(&u, 2.0));
            lanes.heartbeat(1, 2, back_twice(1_000));
        }
        lanes.heartbeat(0, 2, back_twice(2_000));
        lanes.heartbeat(0, 2, back_twice(30_000));
        lanes.heartbeat(1, 2, back_twice(30_000));
        lanes.advance_all_at(Duration::from_millis(30_200), back_twice(30_200));
        lanes.publish();
        (lanes.records(), lanes.counter("plan_clock_resets_total"))
    };
    let described = |names: &[Vec<String>], points: &str| {
        let mut records = names
            .iter()
            .flatten()
            .map(|name| format!("{} rate/10s [{}]", name, points))
            .collect::<Vec<_>>();
        records.sort();
        records
    };
    let expected = vec![
        (relative(back(1_000)), 1, described(&u, "-3600:2")),
        (9_000, 1, described(&x, "0:1")),
    ];
    for p in [1, 2] {
        for skipping_first in [false, true] {
            assert_eq!(
                run(p, skipping_first),
                (expected.clone(), 2),
                "{} lanes, the skipping point's header first: {}",
                p,
                skipping_first
            );
        }
    }
}

/// The operator `test_unbuildable_aggregate`: aggregates natively, and its kernel builds once per compiled plan, every
/// later build failing, as a reset window's instance for another epoch would if its factory failed.
struct UnbuildableAggregate;

const UNBUILDABLE: &str = "test_unbuildable_aggregate";

impl OperatorDef for UnbuildableAggregate {
    fn name(&self) -> &'static str {
        UNBUILDABLE
    }

    fn summary(&self) -> &'static str {
        "Aggregates natively, and builds its kernel only once."
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
        let built = std::sync::atomic::AtomicBool::new(false);
        Some(KernelSpec::new(move |context| {
            if built.swap(true, std::sync::atomic::Ordering::Relaxed) {
                return Err(crate::operators::KernelBuildError {
                    message: "the test operator builds its kernel once".to_string(),
                });
            }
            inner.build(context)
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

    fn partition_key(&self) -> Option<crate::operators::PartitionKey> {
        Aggregate.partition_key()
    }
}

static UNBUILDABLE_AGGREGATE: UnbuildableAggregate = UnbuildableAggregate;

#[test]
fn a_batch_whose_epochs_instance_cannot_be_built_is_lost_and_the_lineage_carries_on() {
    // Two points, one lane. Point 1 steps back while point 0 is still in the old epoch, and sends new-epoch data: its
    // lineage needs an instance for the new epoch, whose kernel fails to build, so that batch is lost as invalid,
    // without a debug assertion. The old epoch drains at its watermark, 9, once point 0 switches, and the lineage
    // carries on in the drained instance: the new timeline's later batch is flushed at the tick due at 15.
    static OPERATORS: OnceLock<Vec<&'static dyn OperatorDef>> = OnceLock::new();
    let operators = OPERATORS.get_or_init(|| {
        let mut operators = OperatorRegistry::native().iter().collect::<Vec<_>>();
        operators.push(&UNBUILDABLE_AGGREGATE);
        operators
    });
    let mut lanes = Lanes::with(
        &lane_plan("10s", UNBUILDABLE),
        OperatorRegistry::with_operators(operators),
        2,
        1,
        PanicBudget::DEFAULT_PROGRAM,
    );
    let x = names_on_lanes("x", 1, 1)[0][0].clone();
    let lost = names_on_lanes("lost", 1, 1)[0][0].clone();
    let kept = names_on_lanes("kept", 1, 1)[0][0].clone();
    lanes.admit(0, 0, t(3_000), vec![named_counter(&x, 1.0)]);
    lanes.heartbeat(0, 0, t(9_000));
    lanes.heartbeat(1, 0, t(9_500));
    // Called directly rather than as the host would, so that a panic, such as a debug assertion, fails the test.
    let header = lanes.header(1, 1, back(1_000), &Arc::clone(&lanes.generation));
    let mut buffer = EventsBuffer::with_capacity(1).with_header(header);
    assert!(buffer
        .try_push(saluki_core::data_model::event::Event::Metric(named_counter(&lost, 4.0)))
        .is_none());
    let (start, exits) = (lanes.start, lanes.exits);
    let progress = lanes.drivers[0].process(
        &mut RegionTurn::new(start, &mut Budget::unlimited()),
        buffer,
        &mut ExitSink::new(exits),
    );
    assert_eq!(progress, Progress::Done);
    lanes.heartbeat(0, 1, back(2_000));
    lanes.admit(1, 1, back(3_000), vec![named_counter(&kept, 2.0)]);
    lanes.heartbeat(0, 1, back(15_000));
    lanes.heartbeat(1, 1, back(15_000));
    lanes.advance_all_at(Duration::from_millis(15_200), back(15_200));
    assert_eq!(
        lanes.records(),
        vec![
            group(relative(back(15_000)), 1, &[&format!("{} rate/10s [-3600:2]", kept)]),
            group(9_000, 1, &[&format!("{} rate/10s [0:1]", x)]),
        ]
    );
}

#[test]
fn a_tick_is_divergent_when_a_lane_flushing_contexts_cannot_start_its_zeros_where_the_lanes_left_them() {
    // One point; only lane 1 holds a context. Lane 0 runs the ticks due at 15 to 90 alone, holding nothing, so it
    // never needs to know where idle-counter zeros start. Lane 1 reaches its tick due at 15 only at 90.2, older than
    // the lanes' records keep: a straggler, it flushes at its own second, its zeros starting from its own last flush,
    // and it counts one tick resolved apart from the other lanes, the one flush that could not know.
    let d = names_on_lanes("d", 2, 1)[1][0].clone();
    let mut lanes = Lanes::new(1, 2);
    lanes.admit(0, 0, t(3_000), vec![named_counter(&d, 1.0)]);
    for second in [15, 30, 45, 60, 75, 90] {
        lanes.heartbeat(0, 0, t(second * 1_000));
        lanes.advance_lane(0, Duration::from_millis(second * 1_000 + 200), t(second * 1_000 + 200));
    }
    lanes.advance_lane(1, Duration::from_millis(90_200), t(90_200));
    lanes.publish();
    assert_eq!(
        lanes.records(),
        vec![group(90_000, 1, &[&format!("{} rate/10s [0:1]", d)])]
    );
    assert_eq!(lanes.counter("plan_lane_tick_straggler_total"), 1);
    assert_eq!(lanes.counter("plan_lane_tick_divergence_total"), 1);
}

#[test]
fn with_one_point_a_tick_every_lane_drops_at_a_reset_is_not_divergent() {
    // One point; only lane 0 holds a context. The tick due at 15 gates in both lanes, the watermark being at 14. Lane 1
    // receives the reset first, dropping the tick, and lane 0 then drops it too: with one point every lane gates a tick
    // alike, so no lane fired it, and lane 0's reset drain knows where its zeros start. Nothing is divergent.
    let c = names_on_lanes("c", 2, 1)[0][0].clone();
    let mut lanes = Lanes::new(1, 2);
    lanes.admit(0, 0, t(3_000), vec![named_counter(&c, 1.0)]);
    lanes.heartbeat(0, 0, t(14_000));
    lanes.advance_all_at(Duration::from_millis(15_200), t(15_200));
    let generation = Arc::clone(&lanes.generation);
    for lane in [1, 0] {
        let header = lanes.header(0, 1, back(1_000), &generation);
        lanes.deliver(lane, header, Vec::new());
    }
    lanes.publish();
    assert_eq!(
        lanes.records(),
        vec![group(14_000, 1, &[&format!("{} rate/10s [0:1]", c)])]
    );
    assert_eq!(lanes.counter("plan_lane_tick_divergence_total"), 0);
}

#[test]
fn a_tick_every_lane_skips_at_a_windows_opening_is_not_divergent() {
    // Two points, and point 1 has sent nothing: the lineage has no watermark, and every lane skips its ticks. Lane 1
    // skips the tick due at 15; lane 0 reaches it only at point 0's reset, where the mark's cutoff makes it resolve
    // it, and skips it too. No lane fired it, so nothing is divergent.
    let c = names_on_lanes("c", 2, 1)[0][0].clone();
    let d = names_on_lanes("d", 2, 1)[1][0].clone();
    let mut lanes = Lanes::new(2, 2);
    lanes.admit(0, 0, t(3_000), vec![named_counter(&c, 1.0), named_counter(&d, 1.0)]);
    lanes.heartbeat(0, 0, t(15_000));
    lanes.advance_lane(1, Duration::from_millis(15_200), t(15_200));
    lanes.heartbeat(0, 1, back(1_000));
    lanes.publish();
    assert!(lanes.delivered.is_empty(), "{:?}", lanes.records());
    assert_eq!(lanes.counter("plan_lane_tick_divergence_total"), 0);
}

#[test]
fn the_overlap_gauge_looks_the_new_lineages_contexts_up_in_the_retiring_one_and_on_one_lane_reads_neither() {
    // A cut over two points, in its switch spread: point 0's new batch put `s` in the new lineage, which the retiring
    // one holds too, and each lineage holds a context armed to panic at a snapshot, the retiring one's on lane 0 and
    // the new one's on lane 1. On two lanes, publication lists the new lineage's contexts and looks each up in the
    // retiring one, which it never snapshots, and only while both hold contexts: lane 0 counts `s`, and lane 1, whose
    // retiring instance holds nothing, asks neither. On one lane, where the gauge records nothing, publication asks
    // neither instance either.
    let s = names_on_lanes("s", 2, 1)[0][0].clone();
    let armed_old = name_ending_on_lane("old.panic.snapshot", 0, 2);
    let armed_new = name_ending_on_lane("new.panic.snapshot", 1, 2);
    for p in [2, 1] {
        let mut lanes = Lanes::with(
            &lane_plan("10s", PANICKING),
            panicking_registry(),
            2,
            p,
            PanicBudget::DEFAULT_PROGRAM,
        );
        lanes.admit(
            0,
            0,
            t(95_000),
            vec![named_counter(&s, 1.0), named_counter(&armed_old, 1.0)],
        );
        let new = lanes.prepare_cut(&lane_plan("30s", PANICKING), 2, t(120_000));
        lanes.publish();
        lanes.admit_under(
            0,
            0,
            t(121_000),
            &new,
            vec![named_counter(&s, 5.0), named_counter(&armed_new, 1.0)],
        );
        lanes.publish();
        if p > 1 {
            assert_eq!(lanes.gauge("plan_lane_crossing_overlap_contexts", 0), Some(1.0));
            assert_eq!(lanes.gauge("plan_lane_crossing_overlap_contexts", 1), Some(0.0));
        }
    }
}

#[test]
fn a_slot_on_lanes_reports_its_most_lagging_lanes_watermark_lag_and_each_lane_its_contexts() {
    // Two points, two state lanes: three counters on lane 0 and one on lane 1, admitted at 1. At 5, both points
    // reach lane 0 only, as if lane 1's input were behind. At a wall clock of 6, lane 0's watermark lags by a
    // second and lane 1's by five: the slot's lag is lane 1's, whichever lane publishes last.
    let names = names_on_lanes("c", 2, 3);
    let mut metrics = names[0].iter().map(|name| named_counter(name, 1.0)).collect::<Vec<_>>();
    metrics.push(named_counter(&names[1][0], 1.0));
    let lag_of = |lanes: &Lanes| {
        lanes
            .recorder
            .gauges()
            .into_iter()
            .find(|(key, _)| {
                key.name() == crate::runtime::PLAN_STATE_WATERMARK_LAG_SECONDS
                    && key
                        .labels()
                        .any(|label| label.key() == "component_id" && label.value() == "dsd_agg")
                    && !key.labels().any(|label| label.key() == "lane")
            })
            .map(|(_, value)| value)
    };
    for order in [[0u16, 1], [1, 0]] {
        let mut lanes = Lanes::new(2, 2);
        lanes.admit(0, 0, t(1_000), metrics.clone());
        lanes.heartbeat(1, 0, t(1_000));
        let header = lanes.header(0, 0, t(5_000), &Arc::clone(&lanes.generation));
        lanes.deliver(0, header, Vec::new());
        let header = lanes.header(1, 0, t(5_000), &Arc::clone(&lanes.generation));
        lanes.deliver(0, header, Vec::new());
        lanes.wall.set(t(6_000));
        {
            let _local = metrics::set_default_local_recorder(&lanes.recorder);
            for lane in order {
                lanes.drivers[usize::from(lane)].publish_telemetry();
            }
        }
        assert_eq!(
            lanes.gauge(crate::runtime::PLAN_LANE_WATERMARK_LAG_SECONDS, 0),
            Some(1.0)
        );
        assert_eq!(
            lanes.gauge(crate::runtime::PLAN_LANE_WATERMARK_LAG_SECONDS, 1),
            Some(5.0)
        );
        assert_eq!(lag_of(&lanes), Some(5.0), "publishing in the order {:?}", order);
        assert_eq!(lanes.gauge(crate::runtime::PLAN_LANE_CONTEXTS, 0), Some(3.0));
        assert_eq!(lanes.gauge(crate::runtime::PLAN_LANE_CONTEXTS, 1), Some(1.0));
    }
}

#[test]
fn a_lane_holds_its_ticks_while_its_reset_window_is_open_and_catches_them_up_once_it_closes() {
    // Two points. Point 1 steps back an hour with new counters `z`, which go to fresh instances, while point 0 stays in
    // the old epoch: the window stays open across the tick due at 15, which no instance of either epoch fires, and no
    // lane asks for a deadline meanwhile. Once point 0 switches, the lineage continues and the held tick is due at
    // once, whichever instance continues the lineage on a lane: the fresh one, holding `z`, or the drained one. It
    // fires at the next advance, flushing the new timeline's closed bucket, and every lane count alike.
    let x = names_on_lanes("x", 2, 1);
    let z = names_on_lanes("z", 2, 1);
    let script = |lanes: &mut Lanes| {
        lanes.admit(
            0,
            0,
            t(3_000),
            x.iter().flatten().map(|name| named_counter(name, 1.0)).collect(),
        );
        lanes.heartbeat(1, 0, t(9_500));
        lanes.admit(
            1,
            1,
            back(1_000),
            z.iter().flatten().map(|name| named_counter(name, 4.0)).collect(),
        );
        lanes.heartbeat(0, 0, t(14_000));
        lanes.advance_all_at(Duration::from_millis(15_200), back(15_200));
        let held = lanes
            .drivers
            .iter()
            .map(|driver| driver.next_deadline())
            .collect::<Vec<_>>();
        assert!(
            held.iter().all(Option::is_none),
            "no lane asks for a deadline: {:?}",
            held
        );
        lanes.heartbeat(0, 1, back(15_500));
        lanes.heartbeat(1, 1, back(15_500));
        let start = lanes.start;
        let due = lanes
            .drivers
            .iter()
            .map(|driver| driver.next_deadline().map(|at| at - start))
            .collect::<Vec<_>>();
        assert!(
            due.iter().all(|at| *at == Some(Duration::from_secs(15))),
            "the held tick is due at once on every lane: {:?}",
            due
        );
        lanes.advance_all_at(Duration::from_millis(15_600), back(15_600));
        lanes.publish();
    };
    let (two, one) = on_two_lanes_and_one(2, script);
    assert_eq!(two.records(), one.records());
    let expected = vec![
        group(
            relative(back(15_000)),
            1,
            &[
                &format!("{} rate/10s [-3600:4]", z[0][0]),
                &format!("{} rate/10s [-3600:4]", z[1][0]),
            ],
        ),
        // The old epoch drains at its final watermark, point 1's last old-epoch stamp.
        group(
            9_500,
            1,
            &[
                &format!("{} rate/10s [0:1]", x[0][0]),
                &format!("{} rate/10s [0:1]", x[1][0]),
            ],
        ),
    ];
    assert_eq!(two.records(), expected);
    assert_eq!(
        two.counter("aggregate_flushes_total"),
        one.counter("aggregate_flushes_total")
    );
}

impl Lanes {
    /// Returns the samples lane `lane`'s histogram named `name` recorded.
    fn samples(&self, name: &str, lane: u16) -> Vec<f64> {
        self.recorder
            .histogram(metrics::Key::from_parts(
                name.to_string(),
                vec![
                    metrics::Label::new("component_id", "dsd_agg"),
                    metrics::Label::new("lane", lane.to_string()),
                ],
            ))
            .unwrap_or_default()
    }
}

/// The operator `test_slow_aggregate`: aggregates natively, and every flush tick's call of its kernel takes at least
/// [`SLOW_STEP`] before it runs, so that a drain's time in the calls that ran its steps is known.
struct SlowAggregate;

const SLOW: &str = "test_slow_aggregate";

/// How long each flush tick's call of a [`SlowAggregate`] kernel takes, at least.
const SLOW_STEP: Duration = Duration::from_millis(5);

impl OperatorDef for SlowAggregate {
    fn name(&self) -> &'static str {
        SLOW
    }

    fn summary(&self) -> &'static str {
        "Aggregates natively, and takes a while at every flush tick."
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
            Ok(Box::new(SlowKernel {
                inner: inner.build(context)?,
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

    fn partition_key(&self) -> Option<crate::operators::PartitionKey> {
        Aggregate.partition_key()
    }
}

/// The kernel of [`SlowAggregate`].
struct SlowKernel {
    inner: Box<dyn Kernel>,
}

impl Kernel for SlowKernel {
    fn consume(&mut self, turn: &mut Turn<'_>, input: &mut EventsBuffer) -> Progress {
        self.inner.consume(turn, input)
    }

    fn advance_time(&mut self, turn: &mut Turn<'_>) -> Progress {
        std::thread::sleep(SLOW_STEP);
        self.inner.advance_time(turn)
    }

    fn control(&mut self, turn: &mut Turn<'_>, control: &mut dyn Any) -> Progress {
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

static SLOW_AGGREGATE: SlowAggregate = SlowAggregate;

#[test]
fn each_state_lane_times_its_flushes_and_the_part_of_them_blocked_on_its_output() {
    // Two points, two state lanes, 40 counters on each lane at 1. Both watermarks pass 15, and the tick at 15 fires
    // in both lanes, in drain calls of at most 8 events each, each call taking at least 5 ms in the kernel: the host
    // sends what a call emitted before it resumes the drain, here 20 ms later each time, as if it waited for room
    // downstream. Each lane records one flush, and the time between its calls as blocked, within the flush, never the
    // time its calls ran; a tick with nothing due records nothing.
    use crate::runtime::{PLAN_LANE_FLUSH_BLOCKED_SECONDS, PLAN_LANE_FLUSH_SECONDS};
    const PAUSE: Duration = Duration::from_millis(20);
    static OPERATORS: OnceLock<Vec<&'static dyn OperatorDef>> = OnceLock::new();
    let operators = OPERATORS.get_or_init(|| {
        let mut operators = OperatorRegistry::native().iter().collect::<Vec<_>>();
        operators.push(&SLOW_AGGREGATE);
        operators
    });
    let names = names_on_lanes("c", 2, 40);
    let mut lanes = Lanes::with(
        &lane_plan("10s", SLOW),
        OperatorRegistry::with_operators(operators),
        2,
        2,
        PanicBudget::DEFAULT_PROGRAM,
    );
    lanes.admit(
        0,
        0,
        t(1_000),
        names.iter().flatten().map(|name| named_counter(name, 1.0)).collect(),
    );
    lanes.heartbeat(0, 0, t(15_500));
    lanes.heartbeat(1, 0, t(15_500));
    lanes.wall.set(t(15_600));
    let at = lanes.start + Duration::from_millis(15_600);
    let mut calls = [0u32; 2];
    for (driver, calls) in lanes.drivers.iter_mut().zip(calls.iter_mut()) {
        let mut sink = ExitSink::new(lanes.exits);
        let mut budget = Budget::unlimited().with_limit(WorkKind::Events, 8);
        let mut progress = driver.advance_time(&mut RegionTurn::new(at, &mut budget), &mut sink);
        *calls = 1;
        while progress == Progress::Yielded {
            std::thread::sleep(PAUSE);
            let mut budget = Budget::unlimited().with_limit(WorkKind::Events, 8);
            progress = driver.resume(&mut RegionTurn::new(at, &mut budget), &mut sink);
            *calls += 1;
        }
    }
    for lane in 0..2u16 {
        let calls = calls[usize::from(lane)];
        assert!(calls > 2, "lane {}'s drain yielded ({} calls)", lane, calls);
        let flushes = lanes.samples(PLAN_LANE_FLUSH_SECONDS, lane);
        let blocked = lanes.samples(PLAN_LANE_FLUSH_BLOCKED_SECONDS, lane);
        assert_eq!(
            (flushes.len(), blocked.len()),
            (1, 1),
            "lane {} records one flush",
            lane
        );
        let waited = (PAUSE * (calls - 1)).as_secs_f64();
        let in_steps = (SLOW_STEP * calls).as_secs_f64();
        assert!(
            blocked[0] >= waited && blocked[0] <= flushes[0] - in_steps,
            "lane {}: blocked {} s of a {} s flush, after waiting {} s between its {} calls, which ran {} s at least",
            lane,
            blocked[0],
            flushes[0],
            waited,
            calls,
            in_steps
        );
    }
    lanes.advance(Duration::from_millis(15_700));
    assert_eq!(lanes.samples(PLAN_LANE_FLUSH_SECONDS, 0).len(), 1, "nothing is due");
}
