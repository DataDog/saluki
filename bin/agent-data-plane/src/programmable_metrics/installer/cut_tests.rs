//! Cut installations as the installer worker performs them: arming, polling, cancelling, and finalizing against running
//! admission points, and the installer's handling of the core's cut actions.
//!
//! The admission points are those of a small topology: two idle sources, each with one admission point on its
//! `metrics` output, registered in one registry, and each feeding a sink that reads nothing until it opens. A test
//! stamps a point by sending a heartbeat through it at a time it sets on the point's wall clock, and blocks a point by
//! filling its sink's channel, so that the point's dispatcher waits for room while it holds the point's lock.

use std::{num::NonZeroUsize, sync::Mutex as StdMutex, time::Duration};

use async_trait::async_trait;
use metrics::Key;
use saluki_core::{
    accounting::{ComponentRegistry, MemoryBounds, MemoryBoundsBuilder, MemoryLimiter},
    components::{
        destinations::{Destination, DestinationBuilder, DestinationContext},
        sources::{Source, SourceBuilder, SourceContext},
        BuildContext,
    },
    data_model::event::EventType,
    execution::{
        AdmissionMode, AdmissionPointConfig, AdmissionPointId, AdmissionPointStatus, AdmissionTime, CutBoundary,
        GenerationToken, HeartbeatOutcome, ManualWallClock,
    },
    health::HealthRegistry,
    runtime::{state::ResourceRegistry, Supervisor, SupervisorError},
    topology::{OutputDefinition, TopologyBlueprint},
};
use saluki_error::GenericError;
use saluki_metric_plan::{
    compile::{LineageKey, RewriteId, RewriteOutcome, RewriteRecord},
    install::{CutProgress, StateBinding},
};
use saluki_metrics::test::TestRecorder;
use tokio::{
    sync::{oneshot, Semaphore},
    task::JoinHandle,
};

use super::{
    cut::{arm, cancel, poll, ArmOutcome, CancelOutcome},
    *,
};
use crate::programmable_metrics::{
    telemetry::{PLAN_CUTS_TOTAL, PLAN_CUT_CANCELLATIONS_REFUSED_TOTAL, PLAN_REWRITES_APPLIED},
    test_support::{configuration, programmable_metrics, ConfigCell},
};

/// The admission point upstream of the state owners a cut changes.
const AFFECTED: AdmissionPointId = AdmissionPointId::new(1);

/// An admission point upstream of no state owner.
const OTHER: AdmissionPointId = AdmissionPointId::new(2);

/// The order a cancellation locks the points in: those that feed no state owner first.
const CANCEL_ORDER: [AdmissionPointId; 2] = [OTHER, AFFECTED];

/// A Unix time, in seconds, that is a multiple of every width the tests cut between, up to 300 seconds.
const BASE: u64 = 1_700_000_100;

/// The longest a test waits for the topology.
const PATIENCE: Duration = Duration::from_secs(30);

/// Returns the admission time `offset` seconds after [`BASE`], to the millisecond.
fn at(offset: f64) -> AdmissionTime {
    let millis = (offset * 1_000.0).round() as u64;
    AdmissionTime::from_unix_nanos((BASE * 1_000 + millis) * 1_000_000)
}

fn seconds(widths: &[u64]) -> Vec<Duration> {
    widths.iter().copied().map(Duration::from_secs).collect()
}

fn generation(id: u64) -> GenerationRef {
    Arc::new(GenerationToken::new(GenerationId::new(id)))
}

/// A cut from `old` to `new` over the affected point, between bucket widths `widths` (in seconds).
fn arm_cut(old: &GenerationRef, new: &GenerationRef, widths: &[u64]) -> ArmCut {
    ArmCut {
        generation: new.clone(),
        old: old.clone(),
        widths: seconds(widths),
        affected: vec![AFFECTED],
        margin: limits::values::CUT_MARGIN,
    }
}

/// A source with one metrics output, `metrics`, that sends nothing and runs until shutdown, so that its admission
/// point stays attached.
struct IdleSourceBuilder {
    outputs: [OutputDefinition<EventType>; 1],
}

struct IdleSource;

#[async_trait]
impl SourceBuilder for IdleSourceBuilder {
    fn outputs(&self) -> &[OutputDefinition<EventType>] {
        &self.outputs
    }

    async fn build(&self, _context: BuildContext) -> Result<Box<dyn Source + Send>, GenericError> {
        Ok(Box::new(IdleSource))
    }
}

impl MemoryBounds for IdleSourceBuilder {
    fn specify_bounds(&self, _builder: &mut MemoryBoundsBuilder) {}
}

#[async_trait]
impl Source for IdleSource {
    async fn run(self: Box<Self>, mut context: SourceContext) -> Result<(), GenericError> {
        // The dispatcher in the context holds the point's attachment for as long as this runs.
        context.take_shutdown_handle().await;
        drop(context);
        Ok(())
    }
}

/// A destination that reads one buffer per permit of its gate.
struct GatedSinkBuilder {
    gate: Arc<Semaphore>,
}

struct GatedSink {
    gate: Arc<Semaphore>,
}

#[async_trait]
impl DestinationBuilder for GatedSinkBuilder {
    fn input_event_type(&self) -> EventType {
        EventType::Metric
    }

    async fn build(&self, _context: BuildContext) -> Result<Box<dyn Destination + Send>, GenericError> {
        Ok(Box::new(GatedSink {
            gate: Arc::clone(&self.gate),
        }))
    }
}

impl MemoryBounds for GatedSinkBuilder {
    fn specify_bounds(&self, _builder: &mut MemoryBoundsBuilder) {}
}

#[async_trait]
impl Destination for GatedSink {
    async fn run(self: Box<Self>, mut context: DestinationContext) -> Result<(), GenericError> {
        loop {
            self.gate.acquire().await.expect("gates are never closed").forget();
            if context.events().next().await.is_none() {
                return Ok(());
            }
        }
    }
}

/// The two admission points of a running topology, their wall clocks, and the gates of their sinks.
struct Points {
    registry: AdmissionRegistry,
    affected_clock: ManualWallClock,
    other_clock: ManualWallClock,
    gates: BTreeMap<AdmissionPointId, Arc<Semaphore>>,
    blocked: StdMutex<Vec<JoinHandle<()>>>,
    shutdown: Option<oneshot::Sender<()>>,
    task: JoinHandle<Result<(), SupervisorError>>,
}

impl Points {
    /// Starts the topology, with points that stamp from `schedule` and register in `registry`, and both wall clocks at
    /// [`BASE`].
    async fn start(registry: AdmissionRegistry, schedule: &ScheduleHandle) -> Self {
        let affected_clock = ManualWallClock::new(at(0.0));
        let other_clock = ManualWallClock::new(at(0.0));
        let mut gates = BTreeMap::new();
        let component_registry = ComponentRegistry::default();
        let mut blueprint = TopologyBlueprint::new("cut-test", &component_registry);
        // One buffer fills a sink's channel, so the second heartbeat to a closed sink waits for room.
        blueprint.with_interconnect_capacity(NonZeroUsize::MIN);
        for (point, name, clock) in [(AFFECTED, "affected", &affected_clock), (OTHER, "other", &other_clock)] {
            let gate = Arc::new(Semaphore::new(0));
            let sink = format!("{}_sink", name);
            blueprint
                .add_source(
                    name,
                    IdleSourceBuilder {
                        outputs: [OutputDefinition::named_output("metrics", EventType::Metric)],
                    },
                )
                .unwrap()
                .add_destination(
                    sink.as_str(),
                    GatedSinkBuilder {
                        gate: Arc::clone(&gate),
                    },
                )
                .unwrap()
                .connect_components(format!("{}.metrics", name).as_str(), sink.as_str())
                .unwrap()
                .with_admission_point(
                    format!("{}.metrics", name).as_str(),
                    AdmissionPointConfig::new(
                        point,
                        AdmissionMode::Admit,
                        Arc::clone(schedule),
                        limits::values::CLOCK_RESET_THRESHOLD,
                    )
                    .with_registry(registry.clone())
                    .with_wall_clock(clock.clone()),
                )
                .unwrap();
            gates.insert(point, gate);
        }
        blueprint
            .with_health_registry(HealthRegistry::new())
            .with_memory_limiter(MemoryLimiter::noop())
            .with_resource_registry(ResourceRegistry::new())
            .with_ambient_worker_pool();

        let mut supervisor = Supervisor::new("cut-test").unwrap();
        supervisor.add_worker(blueprint);
        let (shutdown, shutdown_rx) = oneshot::channel::<()>();
        let task = tokio::spawn(async move { supervisor.run_with_shutdown(shutdown_rx).await });
        tokio::time::timeout(PATIENCE, async {
            while registry.ids() != [AFFECTED, OTHER] {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("the points register when the topology starts");

        Self {
            registry,
            affected_clock,
            other_clock,
            gates,
            blocked: StdMutex::new(Vec::new()),
            shutdown: Some(shutdown),
            task,
        }
    }

    fn clock(&self, point: AdmissionPointId) -> &ManualWallClock {
        if point == AFFECTED {
            &self.affected_clock
        } else {
            &self.other_clock
        }
    }

    /// Stamps a heartbeat at `point` at the admission time `time`, setting the point's wall clock to it.
    async fn stamp(&self, point: AdmissionPointId, time: AdmissionTime) {
        self.clock(point).set(time);
        let outcome = self
            .registry
            .get(point)
            .expect("the point is registered")
            .heartbeat()
            .await;
        assert_eq!(outcome, HeartbeatOutcome::Sent);
        // The sink reads the heartbeat, leaving room for the next one.
        self.gates[&point].add_permits(1);
    }

    /// Blocks `point`: fills its sink's channel and leaves a heartbeat waiting for room, holding the point's lock,
    /// until the topology stops.
    async fn block(&self, point: AdmissionPointId) {
        let target = self.registry.get(point).expect("the point is registered");
        let task = tokio::spawn(async move {
            for _ in 0..2 {
                let _ = target.heartbeat().await;
            }
        });
        self.blocked.lock().unwrap().push(task);
        // The point's lock is also taken for each send that completes: it is blocked once it stays taken.
        tokio::time::timeout(PATIENCE, async {
            let mut taken = 0;
            while taken < 10 {
                match self.registry.status_within(point, Duration::ZERO).await {
                    Some(_) => taken = 0,
                    None => taken += 1,
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("the point blocks");
    }

    async fn status(&self, point: AdmissionPointId) -> AdmissionPointStatus {
        self.registry
            .status_within(point, PATIENCE)
            .await
            .expect("the point is registered")
    }

    async fn stop(mut self) {
        // The sinks read everything from now on, and end with their input.
        for gate in self.gates.values() {
            gate.add_permits(Semaphore::MAX_PERMITS / 4);
        }
        let blocked = std::mem::take(&mut *self.blocked.lock().unwrap());
        for task in blocked {
            task.await.unwrap();
        }
        self.shutdown.take().unwrap().send(()).unwrap();
        let result = tokio::time::timeout(PATIENCE, self.task).await.unwrap().unwrap();
        assert!(result.is_ok(), "the topology stops cleanly: {:?}", result);
    }
}

/// Starts the two points stamping from a schedule of `Steady(old)`.
async fn points(old: &GenerationRef) -> (Points, ScheduleHandle) {
    let schedule = AdmissionSchedule::Steady(old.clone()).into_handle();
    (Points::start(AdmissionRegistry::new(), &schedule).await, schedule)
}

fn cut_of(schedule: &ScheduleHandle) -> Option<(GenerationId, GenerationId, AdmissionTime)> {
    match &**schedule.load() {
        AdmissionSchedule::Cut { old, new, at } => Some((old.id(), new.id(), *at)),
        AdmissionSchedule::Steady(_) => None,
    }
}

fn steady_of(schedule: &ScheduleHandle) -> Option<GenerationId> {
    match &**schedule.load() {
        AdmissionSchedule::Steady(steady) => Some(steady.id()),
        AdmissionSchedule::Cut { .. } => None,
    }
}

#[tokio::test]
async fn a_cut_is_armed_at_the_first_shared_boundary_a_margin_after_the_affected_points_latest_time() {
    // (affected point's latest stamp, its wall clock afterwards, the other point's latest stamp, widths, cut time);
    // offsets in seconds after a time every width divides.
    let cases = [
        // The design's examples: a backlog stamped up to 98.5 s cuts at the next 10 s boundary a second later.
        (Some(98.5), 98.5, None, vec![10], 100.0),
        (Some(95.0), 95.0, None, vec![10, 30], 120.0),
        (None, 105.0, None, vec![10], 110.0),
        // A boundary exactly a margin later is the cut.
        (Some(99.0), 99.0, None, vec![10], 100.0),
        // The wall clock, read under the lock, counts when it is past the latest stamp.
        (Some(90.0), 95.5, None, vec![10], 100.0),
        (Some(90.0), 99.5, None, vec![10], 110.0),
        // Widths line up at their least common multiple.
        (Some(95.0), 95.0, None, vec![10, 5], 100.0),
        (Some(95.0), 95.0, None, vec![60, 300], 300.0),
        // A point the cut is not armed over never moves it, however late its clock.
        (Some(98.5), 98.5, Some(250.0), vec![10], 100.0),
    ];
    for (latest, wall, other, widths, expected) in cases {
        let (old, new) = (generation(1), generation(2));
        let (points, schedule) = points(&old).await;
        if let Some(latest) = latest {
            points.stamp(AFFECTED, at(latest)).await;
        }
        points.affected_clock.set(at(wall));
        if let Some(other) = other {
            points.stamp(OTHER, at(other)).await;
        }
        let what = format!(
            "latest {:?}, wall {}, other {:?}, widths {:?}",
            latest, wall, other, widths
        );

        let outcome = arm(
            &points.registry,
            &schedule,
            &arm_cut(&old, &new, &widths),
            Duration::from_secs(1),
        )
        .await;
        assert_eq!(outcome, ArmOutcome::Armed { at: at(expected) }, "{}", what);
        assert_eq!(cut_of(&schedule), Some((old.id(), new.id(), at(expected))), "{}", what);
        let epoch = points.status(AFFECTED).await.epoch;
        assert_eq!(
            new.cut(),
            Some(&CutBoundary {
                at: at(expected),
                replaces: old.id(),
                epochs: [(AFFECTED, epoch)].into_iter().collect(),
            }),
            "{}",
            what
        );
        points.stop().await;
    }
}

#[tokio::test]
async fn an_armed_cut_assigns_the_old_generation_before_its_time_and_the_new_one_from_it() {
    let (old, new) = (generation(1), generation(2));
    let (points, schedule) = points(&old).await;
    points.stamp(AFFECTED, at(98.5)).await;
    let outcome = arm(
        &points.registry,
        &schedule,
        &arm_cut(&old, &new, &[10]),
        Duration::from_secs(1),
    )
    .await;
    assert_eq!(outcome, ArmOutcome::Armed { at: at(100.0) });

    // Stamps before the cut stay with the old generation: nothing was admitted to the new one, and nothing passed.
    points.stamp(AFFECTED, at(99.9)).await;
    let before = points.status(AFFECTED).await;
    assert!(!before.admitted_to_new && !before.passed_cut, "{:?}", before);

    // The first stamp at the cut time is admitted to the new generation, and every later one is.
    points.stamp(AFFECTED, at(100.0)).await;
    let after = points.status(AFFECTED).await;
    assert!(after.admitted_to_new && after.passed_cut, "{:?}", after);
    points.stop().await;
}

#[tokio::test]
async fn the_boundary_records_the_epoch_the_point_stamps_in_after_a_clock_reset() {
    let (old, new) = (generation(1), generation(2));
    let (points, schedule) = points(&old).await;
    points.stamp(AFFECTED, at(1_000.0)).await;
    let before = points.status(AFFECTED).await.epoch;
    // A backward jump beyond the reset threshold restarts the point's clock in a new epoch.
    points.stamp(AFFECTED, at(500.0)).await;
    let reset = points.status(AFFECTED).await;
    assert_ne!(reset.epoch, before);
    assert_eq!(reset.last_admitted_at, Some(at(500.0)));

    let outcome = arm(
        &points.registry,
        &schedule,
        &arm_cut(&old, &new, &[10]),
        Duration::from_secs(1),
    )
    .await;
    assert_eq!(outcome, ArmOutcome::Armed { at: at(510.0) });
    assert_eq!(new.cut().and_then(|cut| cut.epoch_of(AFFECTED)), Some(reset.epoch));
    points.stop().await;
}

#[tokio::test]
async fn arming_is_put_off_while_an_affected_clock_runs_ahead_of_its_wall_clock() {
    let (old, new) = (generation(1), generation(2));
    let (points, schedule) = points(&old).await;
    // The wall clock steps back 10 s, within the reset threshold: the point keeps stamping its latest time.
    points.stamp(AFFECTED, at(1_000.0)).await;
    points.affected_clock.set(at(990.0));
    let outcome = arm(
        &points.registry,
        &schedule,
        &arm_cut(&old, &new, &[10]),
        Duration::from_secs(1),
    )
    .await;
    assert_eq!(
        outcome,
        ArmOutcome::Deferred(CutDeferReason::ClockAhead {
            by: Duration::from_secs(10)
        })
    );
    assert_eq!(steady_of(&schedule), Some(old.id()));
    assert!(new.cut().is_none(), "nothing is recorded on the generation");

    // Ahead by no more than the margin, the cut is armed from the latest stamp.
    points.affected_clock.set(at(999.5));
    let outcome = arm(
        &points.registry,
        &schedule,
        &arm_cut(&old, &new, &[10]),
        Duration::from_secs(1),
    )
    .await;
    assert_eq!(outcome, ArmOutcome::Armed { at: at(1_010.0) });
    points.stop().await;
}

#[tokio::test(start_paused = true)]
async fn arming_waits_only_for_the_affected_points_and_is_put_off_when_one_stays_busy() {
    let (old, new) = (generation(1), generation(2));
    let (points, schedule) = points(&old).await;

    // A point the cut is not armed over can wait on its downstream for as long as it likes.
    points.block(OTHER).await;
    let outcome = arm(
        &points.registry,
        &schedule,
        &arm_cut(&old, &new, &[10]),
        Duration::from_secs(1),
    )
    .await;
    assert_eq!(outcome, ArmOutcome::Armed { at: at(10.0) });

    // An affected point that stays busy past the lock timeout puts arming off, changing nothing.
    let (other_new, newer) = (generation(3), generation(4));
    let schedule = AdmissionSchedule::Steady(other_new.clone()).into_handle();
    let points_two = Points::start(AdmissionRegistry::new(), &schedule).await;
    points_two.block(AFFECTED).await;
    let started = tokio::time::Instant::now();
    let outcome = arm(
        &points_two.registry,
        &schedule,
        &arm_cut(&other_new, &newer, &[10]),
        Duration::from_secs(1),
    )
    .await;
    assert_eq!(outcome, ArmOutcome::Deferred(CutDeferReason::Timeout));
    assert_eq!(started.elapsed(), Duration::from_secs(1));
    assert_eq!(steady_of(&schedule), Some(other_new.id()));
    assert!(newer.cut().is_none());

    points.stop().await;
    points_two.stop().await;
}

#[tokio::test]
async fn arming_again_reports_the_cut_already_armed_and_never_moves_its_time() {
    let (old, new) = (generation(1), generation(2));
    let (points, schedule) = points(&old).await;
    points.stamp(AFFECTED, at(98.5)).await;
    let cut = arm_cut(&old, &new, &[10]);
    assert_eq!(
        arm(&points.registry, &schedule, &cut, Duration::from_secs(1)).await,
        ArmOutcome::Armed { at: at(100.0) }
    );

    // Later, with the point past the first cut time, arming again reports the cut the schedule holds.
    points.stamp(AFFECTED, at(104.0)).await;
    assert_eq!(
        arm(&points.registry, &schedule, &cut, Duration::from_secs(1)).await,
        ArmOutcome::Armed { at: at(100.0) }
    );
    assert_eq!(new.cut().map(|cut| cut.at), Some(at(100.0)));
    points.stop().await;
}

#[tokio::test]
async fn a_boundary_recorded_but_never_published_is_published_only_while_it_is_still_ahead() {
    // (the affected point's latest stamp, the epoch the boundary records relative to the point's, outcome); the
    // boundary is at 100 s.
    let cases = [
        (98.5, 0, Some(100.0)),
        // Stamped past it since: arming at another time is never right, since regions may have read it.
        (100.5, 0, None),
        // Recorded in another epoch.
        (98.5, 1, None),
    ];
    for (latest, epoch_offset, expected) in cases {
        let (old, new) = (generation(1), generation(2));
        let (points, schedule) = points(&old).await;
        points.stamp(AFFECTED, at(latest)).await;
        let epoch = points.status(AFFECTED).await.epoch.wrapping_add(epoch_offset);
        new.set_cut(CutBoundary {
            at: at(100.0),
            replaces: old.id(),
            epochs: [(AFFECTED, epoch)].into_iter().collect(),
        })
        .unwrap();

        let outcome = arm(
            &points.registry,
            &schedule,
            &arm_cut(&old, &new, &[10]),
            Duration::from_secs(1),
        )
        .await;
        match expected {
            Some(expected) => {
                assert_eq!(outcome, ArmOutcome::Armed { at: at(expected) }, "latest {}", latest);
                assert_eq!(cut_of(&schedule), Some((old.id(), new.id(), at(expected))));
            }
            None => {
                assert!(
                    matches!(outcome, ArmOutcome::Stranded(_)),
                    "latest {}: {:?}",
                    latest,
                    outcome
                );
                assert_eq!(steady_of(&schedule), Some(old.id()), "latest {}", latest);
            }
        }
        points.stop().await;
    }
}

#[tokio::test]
async fn a_boundary_that_can_no_longer_be_published_is_reported_abandoned_and_its_change_prepared_again() {
    let mut installing = Installing::start().await;
    let old = installing.boot();
    // A default program whose aggregation window changes from 10 s to 30 s is a cut installation: once every region
    // acknowledged it, the core asks for its cut to be armed.
    let boot = installing
        .installer()
        .core
        .active_plan()
        .expect("the boot plan is active");
    let mut value = serde_json::to_value(&**boot).unwrap();
    let aggregation = value["nodes"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|node| node["id"] == "dsd_agg")
        .expect("the default program aggregates");
    aggregation["params"]["window"]["resolved"] = serde_json::json!("30s");
    let plan = Plan::from_value(value).unwrap();
    let catalog = &installing.installer().default_program.catalog;
    let report = Validator::new(&OperatorRegistry::native(), catalog).validate(&plan);
    let candidate = Candidate {
        plan: Arc::new(plan),
        report,
        origin: Origin::DefaultProgram { config_change: 1 },
    };
    let (_, actions) = installing.installer().core.propose(candidate);
    let (new, hosts) = actions
        .iter()
        .find_map(|action| match action {
            Action::Prepare(prepare) => Some((
                prepare.generation.clone(),
                prepare.hosts.iter().map(|part| part.host.clone()).collect::<Vec<_>>(),
            )),
            _ => None,
        })
        .expect("the change is prepared");
    let slots = installing.installer().core.frame().slots().len();
    let mut actions = Vec::new();
    for slot in 0..slots {
        let slot = SlotId::new(u16::try_from(slot).unwrap());
        actions.extend(installing.installer().core.prepare_acknowledged(new.id(), slot, Ok(())));
    }
    // Each delivery host whose recipients the generation requires acknowledges its part too.
    for host in hosts {
        actions.extend(installing.installer().core.prepare_acknowledged(
            new.id(),
            saluki_metric_plan::install::PrepareSite::Host(host),
            Ok(()),
        ));
    }
    let mut arming = actions
        .into_iter()
        .find_map(|action| match action {
            Action::ArmCut(arming) => Some(arming),
            _ => None,
        })
        .expect("the cut is to be armed");
    arming.affected = vec![AFFECTED];

    // An interrupted arm recorded the cut's boundary at 100 s, and the affected point stamped past it since.
    installing.points.stamp(AFFECTED, at(100.5)).await;
    let epoch = installing.points.status(AFFECTED).await.epoch;
    new.set_cut(CutBoundary {
        at: at(100.0),
        replaces: old.id(),
        epochs: [(AFFECTED, epoch)].into_iter().collect(),
    })
    .unwrap();
    installing.perform(Action::ArmCut(arming)).await;

    // The generation is never armed at another time: the core aborts it, and prepares the change again as a new
    // generation, while the active one keeps running.
    assert_eq!(steady_of(&installing.schedule()), Some(old.id()));
    let core = installing.installer().core.status();
    let record = core
        .generations
        .iter()
        .find(|record| record.id == new.id())
        .expect("the core remembers the generation");
    assert_eq!(record.state, GenerationState::Rejected);
    let reason = record.reason.as_deref().unwrap_or_default();
    assert!(
        reason.starts_with("its cut boundary was recorded without the cut being published"),
        "{}",
        reason
    );
    let preparing = core.preparing.expect("the change is prepared again");
    assert!(preparing.id > new.id(), "{:?}", preparing);
    let queued = &installing.installer().actions;
    assert!(
        queued.iter().any(|action| matches!(
            action,
            Action::Abort { generation, .. } if *generation == new.id()
        )),
        "{:?}",
        queued
    );
    installing.stop().await;
}

#[tokio::test]
async fn a_cut_without_affected_points_or_with_widths_that_never_line_up_soon_enough_cannot_be_armed() {
    let (old, new) = (generation(1), generation(2));
    let (points, schedule) = points(&old).await;
    let mut nowhere = arm_cut(&old, &new, &[10]);
    nowhere.affected.clear();
    let outcome = arm(&points.registry, &schedule, &nowhere, Duration::from_secs(1)).await;
    assert_eq!(
        outcome,
        ArmOutcome::Failed("the cut of generation 2 has no affected admission point to arm it over".to_string())
    );

    // 120 s and 180 s line up every 360 s, more than a cut may wait.
    let outcome = arm(
        &points.registry,
        &schedule,
        &arm_cut(&old, &new, &[120, 180]),
        Duration::from_secs(1),
    )
    .await;
    let ArmOutcome::Failed(error) = outcome else {
        panic!("the cut cannot be armed: {:?}", outcome)
    };
    assert!(error.contains("line up only every 360 seconds"), "{}", error);
    assert_eq!(steady_of(&schedule), Some(old.id()));
    assert!(new.cut().is_none());
    points.stop().await;
}

#[tokio::test(start_paused = true)]
async fn a_cut_is_cancelled_only_while_no_point_stamped_its_new_generation() {
    // (point stamping at the cut time, if any, blocked point, outcome)
    let cases = [
        (None, None, true),
        (Some(AFFECTED), None, false),
        // A point the cut is not armed over switches at the cut time too, so its stamp makes the cut irrevocable.
        (Some(OTHER), None, false),
        // A point whose lock is not free in time refuses the cancellation: the cut proceeds.
        (None, Some(OTHER), false),
    ];
    for (stamping, blocked, cancelled) in cases {
        let what = format!("stamping {:?}, blocked {:?}", stamping, blocked);
        let (old, new) = (generation(1), generation(2));
        let (points, schedule) = points(&old).await;
        points.stamp(AFFECTED, at(98.5)).await;
        let armed = arm(
            &points.registry,
            &schedule,
            &arm_cut(&old, &new, &[10]),
            Duration::from_secs(1),
        )
        .await;
        assert_eq!(armed, ArmOutcome::Armed { at: at(100.0) });
        if let Some(point) = stamping {
            points.stamp(point, at(100.0)).await;
        }
        if let Some(point) = blocked {
            points.block(point).await;
        }

        let outcome = cancel(
            &points.registry,
            &schedule,
            &CANCEL_ORDER,
            new.id(),
            &old,
            Duration::from_secs(1),
        )
        .await;
        if cancelled {
            assert_eq!(outcome, CancelOutcome::Cancelled, "{}", what);
            assert_eq!(steady_of(&schedule), Some(old.id()), "{}", what);
            // Cancelling again, as a restarted worker does, reports it cancelled before, and changes nothing.
            let again = cancel(
                &points.registry,
                &schedule,
                &CANCEL_ORDER,
                new.id(),
                &old,
                Duration::from_secs(1),
            )
            .await;
            assert_eq!(again, CancelOutcome::AlreadyCancelled, "{}", what);
            assert_eq!(steady_of(&schedule), Some(old.id()), "{}", what);
        } else {
            assert!(matches!(outcome, CancelOutcome::Refused(_)), "{}: {:?}", what, outcome);
            assert_eq!(cut_of(&schedule), Some((old.id(), new.id(), at(100.0))), "{}", what);
        }
        points.stop().await;
    }
}

#[tokio::test]
async fn a_finalized_cut_or_a_schedule_without_it_is_never_cancelled() {
    let (old, new, other) = (generation(1), generation(2), generation(3));
    let schedule = AdmissionSchedule::Steady(new.clone()).into_handle();
    let registry = AdmissionRegistry::new();
    let outcome = cancel(
        &registry,
        &schedule,
        &CANCEL_ORDER,
        new.id(),
        &old,
        Duration::from_secs(1),
    )
    .await;
    assert_eq!(
        outcome,
        CancelOutcome::Refused("the cut of generation 2 is complete".to_string())
    );
    schedule.store(Arc::new(AdmissionSchedule::Steady(other.clone())));
    let outcome = cancel(
        &registry,
        &schedule,
        &CANCEL_ORDER,
        new.id(),
        &old,
        Duration::from_secs(1),
    )
    .await;
    assert!(matches!(outcome, CancelOutcome::Refused(_)), "{:?}", outcome);
    assert_eq!(steady_of(&schedule), Some(other.id()));
}

#[tokio::test(start_paused = true)]
async fn a_poll_tells_whether_any_point_stamped_the_new_generation_and_which_affected_points_passed() {
    let (old, new) = (generation(1), generation(2));
    let (points, schedule) = points(&old).await;
    points.stamp(AFFECTED, at(98.5)).await;
    arm(
        &points.registry,
        &schedule,
        &arm_cut(&old, &new, &[10]),
        Duration::from_secs(1),
    )
    .await;
    let progress = |stamped_new, passed| CutProgress {
        stamped_new,
        passed,
        affected: 1,
    };
    let poll_now = || poll(&points.registry, &CANCEL_ORDER, &[AFFECTED], limits::values::CUT_POLL);

    let polled = poll_now().await;
    assert_eq!(polled.progress, progress(false, 0));
    assert_eq!(polled.wall, Some(at(98.5)));

    // Another point stamping the new generation makes the cut irrevocable, but only affected points pass it.
    points.stamp(OTHER, at(101.0)).await;
    assert_eq!(poll_now().await.progress, progress(true, 0));
    points.stamp(AFFECTED, at(100.5)).await;
    let polled = poll_now().await;
    assert_eq!(polled.progress, progress(true, 1));
    assert!(polled.progress.all_passed());
    assert_eq!(polled.wall, Some(at(101.0)));

    // A busy point is left out of this poll, after waiting at most the poll period for it.
    points.block(AFFECTED).await;
    let started = tokio::time::Instant::now();
    let polled = poll_now().await;
    assert_eq!(started.elapsed(), limits::values::CUT_POLL);
    assert_eq!(polled.progress, progress(true, 0));
    assert_eq!(polled.wall, Some(at(101.0)));
    points.stop().await;
}

/// An installer over the default program, whose admission registry and schedule the two points use, and whose cuts
/// lock and poll them; and the recorder of its telemetry.
struct Installing {
    metrics: ProgrammableMetrics,
    points: Points,
    recorder: TestRecorder,
}

impl Installing {
    async fn start() -> Self {
        let recorder = TestRecorder::default();
        let config = configuration("").await;
        let cell = ConfigCell::new(config.clone());
        // The installer registers its telemetry when it is created, and counts through the handles it registered.
        let mut path = metrics::with_local_recorder(&recorder, || {
            programmable_metrics(&config, cell.live(), &ComponentRegistry::default()).unwrap()
        });
        let boot = path.boot_topology();
        let points = Points::start(boot.admission.clone(), &boot.schedule).await;
        path.installer.cut_points = CutPoints {
            every: CANCEL_ORDER.to_vec(),
            affected: vec![AFFECTED],
        };
        Self {
            metrics: path,
            points,
            recorder,
        }
    }

    fn installer(&mut self) -> &mut Installer {
        &mut self.metrics.installer
    }

    fn boot(&self) -> GenerationRef {
        self.metrics.topology.generation.clone()
    }

    fn schedule(&self) -> ScheduleHandle {
        Arc::clone(&self.metrics.installer.schedule)
    }

    async fn perform(&mut self, action: Action) {
        self.metrics.installer.perform(action).await;
    }

    fn cuts(&self, outcome: &'static str) -> u64 {
        self.recorder
            .counter(Key::from_parts(PLAN_CUTS_TOTAL, &[("outcome", outcome)]))
            .unwrap_or_default()
    }

    fn refusals(&self) -> u64 {
        self.recorder
            .counter(Key::from_name(PLAN_CUT_CANCELLATIONS_REFUSED_TOTAL))
            .unwrap_or_default()
    }

    async fn stop(self) {
        self.points.stop().await;
    }
}

#[tokio::test]
async fn the_installer_arms_watches_and_finalizes_a_cut() {
    let mut installing = Installing::start().await;
    let (old, new) = (installing.boot(), generation(100));
    let schedule = installing.schedule();
    // Every cut outcome is reported from boot, at zero.
    for outcome in CutOutcome::ALL {
        assert_eq!(installing.cuts(outcome.as_str()), 0, "{:?}", outcome);
    }
    installing.points.stamp(AFFECTED, at(98.5)).await;
    let far = Instant::now() + Duration::from_secs(3_600);
    installing.installer().prepare_deadline = Some((new.id(), far));
    installing.installer().draining.insert(
        old.id(),
        Draining {
            since: Instant::now(),
            overdue: true,
        },
    );

    // Arming clears the preparation's deadline: the cut may wait longer than any preparation.
    installing.perform(Action::ArmCut(arm_cut(&old, &new, &[10]))).await;
    assert_eq!(installing.installer().prepare_deadline, None);
    assert_eq!(cut_of(&schedule), Some((old.id(), new.id(), at(100.0))));
    installing
        .perform(Action::WatchCut {
            generation: new.id(),
            at: at(100.0),
        })
        .await;

    // The installer polls the points while the cut is in flight.
    let polled = installing.installer().next_event().await;
    let Event::CutPolled(generation, polled) = polled else {
        panic!("the installer polls the points")
    };
    assert_eq!(generation, new.id());
    assert_eq!(
        polled.progress,
        CutProgress {
            stamped_new: false,
            passed: 0,
            affected: 1
        }
    );
    installing.installer().handle(Event::CutPolled(generation, polled));
    installing.points.stamp(AFFECTED, at(100.5)).await;
    let Event::CutPolled(_, polled) = installing.installer().next_event().await else {
        panic!("the installer polls the points again")
    };
    assert!(
        polled.progress.stamped_new && polled.progress.all_passed(),
        "{:?}",
        polled
    );

    // Finalizing stores the new generation alone and starts the replaced generation's drain deadline.
    installing
        .perform(Action::FinalizeCut {
            generation: new.clone(),
        })
        .await;
    assert_eq!(steady_of(&schedule), Some(new.id()));
    assert_eq!(installing.cuts("completed"), 1);
    assert!(installing.installer().cut.is_none(), "the installer stops polling");
    let draining = &installing.installer().draining[&old.id()];
    assert!(!draining.overdue, "the drain deadline starts again");

    // Finalizing again changes and counts nothing.
    installing
        .perform(Action::FinalizeCut {
            generation: new.clone(),
        })
        .await;
    assert_eq!(steady_of(&schedule), Some(new.id()));
    assert_eq!(installing.cuts("completed"), 1);
    installing.stop().await;
}

#[tokio::test]
async fn the_installer_never_finalizes_a_cut_the_schedule_does_not_hold() {
    let mut installing = Installing::start().await;
    let (old, new) = (installing.boot(), generation(100));
    let schedule = installing.schedule();
    installing
        .perform(Action::FinalizeCut {
            generation: new.clone(),
        })
        .await;
    assert_eq!(steady_of(&schedule), Some(old.id()));
    assert_eq!(installing.cuts("completed"), 0);
    installing.stop().await;
}

#[tokio::test]
async fn a_cut_whose_affected_points_have_not_passed_it_by_the_drain_deadline_is_overdue() {
    let mut installing = Installing::start().await;
    let (old, new) = (installing.boot(), generation(100));
    installing.points.stamp(AFFECTED, at(98.5)).await;
    installing.perform(Action::ArmCut(arm_cut(&old, &new, &[10]))).await;
    installing
        .perform(Action::WatchCut {
            generation: new.id(),
            at: at(100.0),
        })
        .await;

    // (the affected point's wall clock, whether the replaced generation is overdue); the drain deadline is 30 s.
    for (wall, overdue) in [(129.9, false), (130.0, true), (131.0, true)] {
        installing.points.affected_clock.set(at(wall));
        let Event::CutPolled(generation, polled) = installing.installer().next_event().await else {
            panic!("the installer polls the points")
        };
        installing.installer().handle(Event::CutPolled(generation, polled));
        let status = installing.installer().current_status();
        let expected = if overdue { vec![old.id()] } else { Vec::new() };
        assert_eq!(status.overdue, expected, "wall {}", wall);
    }

    // Once finalized, the cut is no longer overdue.
    installing.points.stamp(AFFECTED, at(131.0)).await;
    installing
        .perform(Action::FinalizeCut {
            generation: new.clone(),
        })
        .await;
    assert!(installing.installer().current_status().overdue.is_empty());
    installing.stop().await;
}

#[tokio::test]
async fn the_installer_cancels_a_cut_before_any_point_stamped_it_and_counts_each_cut_once_by_how_it_ended() {
    let mut installing = Installing::start().await;
    let (old, new) = (installing.boot(), generation(100));
    let schedule = installing.schedule();
    installing.points.stamp(AFFECTED, at(98.5)).await;
    installing.perform(Action::ArmCut(arm_cut(&old, &new, &[10]))).await;
    installing
        .perform(Action::WatchCut {
            generation: new.id(),
            at: at(100.0),
        })
        .await;
    let cancel = |generation: &GenerationRef| Action::CancelCut {
        generation: generation.id(),
        old: old.clone(),
    };
    installing.perform(cancel(&new)).await;
    assert_eq!(steady_of(&schedule), Some(old.id()));
    assert_eq!(installing.cuts("cancelled"), 1);
    assert!(installing.installer().cut.is_none(), "the installer stops polling");
    // The same cancellation performed again, as a restarted worker does, finds the cut cancelled and counts nothing.
    installing.perform(cancel(&new)).await;
    assert_eq!(steady_of(&schedule), Some(old.id()));
    assert_eq!(installing.cuts("cancelled"), 1);

    // A later cut that a point already stamped proceeds: the refusal is counted apart, and the cut ends once, when it
    // completes.
    let newer = generation(101);
    installing.perform(Action::ArmCut(arm_cut(&old, &newer, &[10]))).await;
    let (_, _, cut_at) = cut_of(&schedule).expect("the cut is armed");
    installing.points.stamp(OTHER, cut_at).await;
    installing.perform(cancel(&newer)).await;
    assert_eq!(cut_of(&schedule).map(|(_, new, _)| new), Some(newer.id()));
    assert_eq!(installing.refusals(), 1);
    installing.points.stamp(AFFECTED, cut_at).await;
    installing
        .perform(Action::FinalizeCut {
            generation: newer.clone(),
        })
        .await;
    assert_eq!(steady_of(&schedule), Some(newer.id()));
    // Two cuts ended: one cancelled, and one completed after its cancellation was refused.
    let ended = CutOutcome::ALL
        .into_iter()
        .map(|outcome| (outcome.as_str(), installing.cuts(outcome.as_str())))
        .collect::<Vec<_>>();
    assert_eq!(ended, [("completed", 1), ("cancelled", 1), ("not_armed", 0)]);
    assert_eq!(installing.refusals(), 1);
    installing.stop().await;
}

#[tokio::test]
async fn a_cut_generation_reported_active_sets_the_active_plans_gauges_before_or_after_its_cut_is_finalized() {
    // Whether the core reports the cut's generation active before it asks to finalize the cut, or after.
    for active_first in [true, false] {
        let mut installing = Installing::start().await;
        let (old, new) = (installing.boot(), generation(100));
        let candidate = installing
            .installer()
            .core
            .status()
            .active
            .expect("the boot generation is active")
            .candidate;
        let applied = |installing: &Installing| {
            RewriteId::ALL.map(|rewrite| {
                installing
                    .recorder
                    .gauge(Key::from_parts(PLAN_REWRITES_APPLIED, &[("rewrite", rewrite.as_str())]))
            })
        };
        // The core's active plan, which a commit would describe.
        let active_plan = applied(&installing);
        installing.points.stamp(AFFECTED, at(98.5)).await;
        installing.perform(Action::ArmCut(arm_cut(&old, &new, &[10]))).await;
        installing
            .perform(Action::WatchCut {
                generation: new.id(),
                at: at(100.0),
            })
            .await;
        installing.points.stamp(AFFECTED, at(100.5)).await;
        // Until the cut's generation is reported active, the gauges describe another plan: one compiled with R2 twice.
        let applied_r2 = RewriteRecord {
            id: RewriteId::R2,
            outcome: RewriteOutcome::Applied,
            nodes: Vec::new(),
            justification: String::new(),
            telemetry: Vec::new(),
            capacity_rule: false,
        };
        installing
            .installer()
            .telemetry
            .rewrites(Some(&[applied_r2.clone(), applied_r2]));
        assert_ne!(applied(&installing), active_plan);

        let active = Action::Report(Report {
            candidate,
            origin: Origin::DefaultProgram { config_change: 1 },
            generation: Some(new.id()),
            outcome: Outcome::Generation(GenerationState::Active),
        });
        let finalize = Action::FinalizeCut {
            generation: new.clone(),
        };
        let actions = if active_first {
            [active, finalize]
        } else {
            [finalize, active]
        };
        for action in actions {
            installing.perform(action).await;
        }
        assert_eq!(steady_of(&installing.schedule()), Some(new.id()));
        assert_eq!(
            applied(&installing),
            active_plan,
            "active reported first: {}",
            active_first
        );
        installing.stop().await;
    }
}

#[tokio::test]
async fn a_cut_that_can_never_be_armed_fails_its_generation_once_rather_than_waiting_for_busy_points() {
    let mut installing = Installing::start().await;
    let old = installing.boot();
    // The core prepares a generation for a configuration change, and its regions hold it.
    let changed = configuration("metric_filterlist: [blocked]").await;
    let actions = installing.installer().propose_default_program(&changed);
    let new = actions
        .iter()
        .find_map(|action| match action {
            Action::Prepare(prepare) => Some(prepare.generation.clone()),
            _ => None,
        })
        .expect("the configuration change is prepared");
    installing.installer().actions.extend(actions);
    installing.installer().perform_actions().await;

    // A cut over no admission point can never be armed: the generation fails at once, and is aborted in its regions.
    let mut cut = arm_cut(&old, &new, &[10]);
    cut.affected.clear();
    installing.perform(Action::ArmCut(cut)).await;
    let core = installing.installer().core.status();
    assert!(core.preparing.is_none(), "{:?}", core.preparing);
    let record = core
        .generations
        .iter()
        .find(|record| record.id == new.id())
        .expect("the core remembers the generation");
    assert_eq!(record.state, GenerationState::Failed);
    let reason = record.reason.as_deref().unwrap_or_default();
    assert!(reason.starts_with("its cut cannot be armed: "), "{}", reason);
    let queued = &installing.installer().actions;
    assert!(
        queued.iter().any(|action| matches!(
            action,
            Action::Abort { generation, .. } if *generation == new.id()
        )),
        "{:?}",
        queued
    );
    assert!(
        !queued.iter().any(|action| matches!(action, Action::RetryAfter(_))),
        "{:?}",
        queued
    );

    // Its cut is counted once, as never armed; the active generation keeps running, and the status says nothing of
    // busy admission points.
    installing.installer().perform_actions().await;
    assert_eq!(installing.cuts("not_armed"), 1);
    assert_eq!(steady_of(&installing.schedule()), Some(old.id()));
    let status = installing.installer().current_status().to_string();
    assert!(!status.contains("busy"), "{}", status);
    installing.stop().await;
}

#[tokio::test]
async fn a_preparation_hands_each_region_how_its_state_owners_bind_to_instances_of_their_state() {
    let config = configuration("").await;
    let cell = ConfigCell::new(config.clone());
    let metrics = programmable_metrics(&config, cell.live(), &ComponentRegistry::default()).unwrap();
    let (boot, new) = (metrics.topology.generation.clone(), generation(100));
    let program = Arc::clone(&metrics.topology.programs[0]);
    let node = "dsd_agg".parse::<NodeId>().unwrap();
    let lineage = |window: &str| LineageKey {
        node: node.clone(),
        contract: PlanHash::of_bytes(window.as_bytes()),
    };
    let preparation = |state: Vec<StateBinding>| SlotPrepare {
        slot: program.slot(),
        program: Arc::clone(&program),
        unchanged: true,
        reuse: Vec::new(),
        build: Vec::new(),
        state,
        leases: Default::default(),
    };
    let artifact_nodes = BTreeSet::new();

    // A cut from a 10-second aggregation window to a 30-second one: the old window's instance retires at the cut, and
    // the new window's starts empty; or a parameter change that keeps the instance; or a slot without state owners.
    let cases = [
        vec![
            StateBinding::Retire {
                node: node.clone(),
                key: lineage("10s"),
                from: boot.id(),
            },
            StateBinding::New {
                node: node.clone(),
                key: lineage("30s"),
            },
        ],
        vec![StateBinding::Retain {
            node: node.clone(),
            key: lineage("10s"),
            from: boot.id(),
        }],
        Vec::new(),
    ];
    for state in cases {
        let message = region_preparation(
            &metrics.installer.region_telemetry,
            &new,
            preparation(state.clone()),
            PanicBudget::ARTIFACT,
            &artifact_nodes,
            1,
        )
        .expect("the preparation builds no kernel");
        assert_eq!(message.generation().id(), new.id());
        assert_eq!(message.state_bindings(), state.as_slice());
    }
}

#[tokio::test(start_paused = true)]
async fn a_cut_given_up_before_it_was_ever_armed_is_counted_once() {
    let mut installing = Installing::start().await;
    let (old, new) = (installing.boot(), generation(100));
    let candidate = installing
        .installer()
        .core
        .status()
        .active
        .expect("the boot generation is active")
        .candidate;
    installing.points.block(AFFECTED).await;
    installing.perform(Action::ArmCut(arm_cut(&old, &new, &[10]))).await;
    assert_eq!(steady_of(&installing.schedule()), Some(old.id()));

    let rejected = || {
        Action::Report(Report {
            candidate,
            origin: Origin::DefaultProgram { config_change: 1 },
            generation: Some(new.id()),
            outcome: Outcome::Generation(GenerationState::Rejected),
        })
    };
    installing.perform(rejected()).await;
    installing.perform(rejected()).await;
    assert_eq!(installing.cuts("not_armed"), 1);
    installing.stop().await;
}
