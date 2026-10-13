//! A frame of region drivers wired together in process, for tests: [`FrameHarness`], and, on lanes,
//! [`LanedFrameHarness`].
//!
//! The harness runs every slot of a frame through a [`MetricRegionDriver`], one per lane of a slot on lanes, and every
//! fixed component other than a source through an opaque-host adapter, synchronously and without channels. Admission
//! points are modeled on the real ones: each stamps with an [`AdmissionClock`], which the lanes of one point share, the
//! clock-reset threshold of the limits table, and the generation the schedule of the topology assigns, recording, as a
//! real point does, whether it admitted to a cut's new generation. A batch admitted at a source runs through every
//! region and host it reaches, in the order the topology would deliver it, and what reaches an output is recorded with
//! its header. Buffers between slots can be held back, as a queue between two regions holds them, and released later;
//! on lanes, a [delivery schedule][DeliverySchedule] decides when what one lane sends another arrives.
//!
//! Time comes in two forms:
//!
//! - **Interpreter time.** [`admit`][FrameHarness::admit] stamps a batch at the admission time it is given, and
//!   [`advance`][FrameHarness::advance] moves to a wall-clock time, as the reference interpreter's trace events do:
//!   every host ticks, sources that feed state owners heartbeat first when [`HarnessOptions::heartbeats_at_ticks`]
//!   says so, and the regions' monotonic clock follows, running the ticks that fall due.
//! - **Running time.** [`set_wall`][FrameHarness::set_wall] and [`elapse`][FrameHarness::elapse] move the wall clock
//!   and the regions' monotonic clock as a running host's move, and run every deadline that passes, as a region host
//!   does.
//!
//! Installations follow the installer: [`prepare`][FrameHarness::prepare] prepares a candidate in every region, with
//! the kernels and state bindings the installer would give it, and then [`commit`][FrameHarness::commit] stores it in
//! the schedule, or, for a cut, [`arm_cut`][FrameHarness::arm_cut], [`finalize_cut`][FrameHarness::finalize_cut], and
//! [`cancel_cut`][FrameHarness::cancel_cut] move the schedule as the cut protocol does.

use std::{
    collections::{BTreeMap, VecDeque},
    panic::{catch_unwind, AssertUnwindSafe},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

use saluki_core::{
    data_model::event::{metric::Metric, Event},
    execution::{
        AdmissionClock, AdmissionMode, AdmissionPointId, AdmissionSchedule, AdmissionTime, BatchHeader, Budget,
        CutBoundary, DrainPolicy, EntryId, ExitSink, GenerationId, GenerationRef, GenerationToken, LaneContext,
        LaneSplit, LossClass, PanicOutcome, PanicSite, Progress, RegionDriver, RegionPanic, RegionTurn, ScheduleHandle,
        WallClock,
    },
    topology::EventsBuffer,
};
use saluki_metrics::{Counter, Gauge, Histogram};

use super::{
    build_kernels,
    kernels::panic_message,
    state::{lanes::SlotLanes, BudgetRecordings},
    telemetry::StateTelemetry,
    MetricRegionDriver, PanicBudget, PrepareRegion, RegionConfig, RegionControl, RegisterTelemetry, StateBudgets,
};
use crate::{
    compile::{
        compile, CompiledPlan, ExitRouting, Frame, FrameHosts, FrameOptions, FrameTarget, LanePolicy, LeaseTable,
        LineageKey, Placement, PointRouting, RegionProgram, Site, Slot, SlotId, SlotKind,
    },
    equivalence::{CapacityOutcome, CapacityTrace, OutputPoints},
    explain::loss_class_name,
    install::{kernel_reuse, StateBinding},
    interpret::{
        replay::{ArrivalReplay, LaneObservation},
        HostBatch, HostOutput, HostTick, OpaqueHostAdapter, TraceEvent,
    },
    ir::{NativeComponentId, NodeId, OutputId, Plan, PortName},
    limits::values::{CLOCK_RESET_THRESHOLD, CUT_MARGIN},
    operators::{names, KernelBuildContext, LaneBinding, LaneCell, OperatorRegistry},
    runtime::{
        state::rules::{cut_time, epoch_newer, lcm_secs, Observation, TickDecision, TickGate, WatermarkTracker},
        StateLaneTelemetry, PLAN_LANE_RESET_OVERFLOW_TOTAL, PLAN_LANE_TICK_DIVERGENCE_TOTAL,
        PLAN_LANE_TICK_STRAGGLER_TOTAL,
    },
    validate::ValidationReport,
};

/// Counters and gauges by component and tag.
type Recorded = BTreeMap<(String, String), Arc<AtomicU64>>;

/// Telemetry that records into counters and gauges a test can read, keyed by component and tag.
///
/// Counters are tagged `port:<port>` for events per port, `class:<class>` for loss, and, for an isolation slot's own
/// telemetry, by the metric's name. Gauges hold the bits of their `f64` value. Histograms record nothing: a test that
/// reads a state lane's flush times reads them from a metrics recorder.
#[derive(Clone, Default)]
pub struct RecordingTelemetry {
    counters: Arc<Mutex<Recorded>>,
}

impl RecordingTelemetry {
    fn cell(&self, component: String, tag: String) -> Arc<AtomicU64> {
        let mut counters = self.counters.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        Arc::clone(counters.entry((component, tag)).or_default())
    }

    fn counter(&self, component: String, tag: String) -> Counter {
        Counter::from_arc(self.cell(component, tag))
    }

    fn gauge(&self, component: String, tag: String) -> Gauge {
        Gauge::from_arc(self.cell(component, tag))
    }

    /// Returns the value of the counter of `component` with `tag` (`port:<port>`, `class:<class>`, or the name of an
    /// isolation slot's counter), or 0 if nothing registered it.
    pub fn value(&self, component: &str, tag: &str) -> u64 {
        self.counters
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&(component.to_string(), tag.to_string()))
            .map_or(0, |counter| counter.load(Ordering::Relaxed))
    }

    /// Returns the value of the gauge of `component` named `name`, or 0 if nothing registered it.
    pub fn gauge_value(&self, component: &str, name: &str) -> f64 {
        f64::from_bits(self.value(component, name))
    }

    /// Returns the points `component` lost in `class`.
    pub fn loss(&self, component: &str, class: LossClass) -> u64 {
        self.value(component, &format!("class:{}", loss_class_name(class)))
    }

    /// Returns the events `node` sent on `port`.
    pub fn port_events(&self, node: &str, port: &str) -> u64 {
        self.value(node, &format!("port:{}", port))
    }

    /// Returns the value of the counter named `name` that lane `lane` of the isolation slot `region` publishes to, or
    /// 0 if nothing registered it: tagged `<name>#<lane>`.
    pub fn lane_value(&self, region: &str, name: &str, lane: u16) -> u64 {
        self.value(region, &lane_tag(name, lane))
    }

    /// Returns the value of the gauge named `name` that lane `lane` of the isolation slot `region` publishes to, or 0
    /// if nothing registered it: tagged `<name>#<lane>`.
    pub fn lane_gauge(&self, region: &str, name: &str, lane: u16) -> f64 {
        self.gauge_value(region, &lane_tag(name, lane))
    }

    /// Returns the recording handles lane `lane` of the isolation slot `region` publishes its own telemetry to, each
    /// tagged `<name>#<lane>` ([`lane_value`][Self::lane_value]), a crossing counter's name followed by `:<kind>`.
    pub fn state_lane(&self, region: &str, lane: u16) -> StateLaneTelemetry {
        use super::telemetry::{
            PLAN_LANE_CONTEXTS, PLAN_LANE_CROSSINGS_TOTAL, PLAN_LANE_CROSSING_OVERLAP_CONTEXTS,
            PLAN_LANE_MISROUTED_TOTAL, PLAN_LANE_WATERMARK_LAG_SECONDS,
        };
        let counter = |name: &str| self.counter(region.to_string(), lane_tag(name, lane));
        let gauge = |name: &str| self.gauge(region.to_string(), lane_tag(name, lane));
        StateLaneTelemetry {
            tick_divergence: counter(PLAN_LANE_TICK_DIVERGENCE_TOTAL),
            tick_stragglers: counter(PLAN_LANE_TICK_STRAGGLER_TOTAL),
            reset_overflows: counter(PLAN_LANE_RESET_OVERFLOW_TOTAL),
            misrouted: counter(PLAN_LANE_MISROUTED_TOTAL),
            cut_crossings: counter(&format!("{}:cut", PLAN_LANE_CROSSINGS_TOTAL)),
            reset_crossings: counter(&format!("{}:reset", PLAN_LANE_CROSSINGS_TOTAL)),
            crossing_overlap: gauge(PLAN_LANE_CROSSING_OVERLAP_CONTEXTS),
            watermark_lag: gauge(PLAN_LANE_WATERMARK_LAG_SECONDS),
            contexts: gauge(PLAN_LANE_CONTEXTS),
            flush_seconds: Histogram::noop(),
            flush_blocked_seconds: Histogram::noop(),
        }
    }
}

/// The tag a [`RecordingTelemetry`] records the counter `name` of one state lane under.
fn lane_tag(name: &str, lane: u16) -> String {
    format!("{}#{}", name, lane)
}

/// The counters of the flush ticks a state lane resolved apart from its lineage's shared tick record (6.1): a grace
/// that expired, a tick dropped at a reset window's opening, a tick skipped while holding contexts, an idle-counter
/// back-fill that could not start where the lanes left it, and a straggler beyond the record's depth.
const RESOLVED_APART: [&str; 2] = [PLAN_LANE_TICK_DIVERGENCE_TOTAL, PLAN_LANE_TICK_STRAGGLER_TOTAL];

/// The counter of the clock epochs a state lane's reset window closed at once, past the most it keeps open: a later
/// batch of such an epoch has no instance and is lost.
const RESET_OVERFLOWS: [&str; 1] = [PLAN_LANE_RESET_OVERFLOW_TOTAL];

impl RegisterTelemetry for RecordingTelemetry {
    fn port_events(&self, node: &NodeId, _operator: &'static str, port: &PortName) -> Vec<Counter> {
        vec![self.counter(node.to_string(), format!("port:{}", port))]
    }

    fn loss(&self, component_id: &str, class: LossClass) -> Counter {
        self.counter(component_id.to_string(), format!("class:{}", loss_class_name(class)))
    }

    fn state(&self, region: &str) -> StateTelemetry {
        use super::telemetry::{
            PLAN_CLOCK_RESETS_TOTAL, PLAN_STATE_LINEAGES, PLAN_STATE_TICK_GRACE_EXPIRED_TOTAL,
            PLAN_STATE_WATERMARK_LAG_SECONDS,
        };
        StateTelemetry {
            watermark_lag: self.gauge(region.to_string(), PLAN_STATE_WATERMARK_LAG_SECONDS.to_string()),
            lineages: self.gauge(region.to_string(), PLAN_STATE_LINEAGES.to_string()),
            clock_resets: self.counter(region.to_string(), PLAN_CLOCK_RESETS_TOTAL.to_string()),
            tick_grace_expired: self.counter(region.to_string(), PLAN_STATE_TICK_GRACE_EXPIRED_TOTAL.to_string()),
        }
    }
}

/// A wall clock that moves only when the harness moves it; clones share the time.
#[derive(Clone, Debug, Default)]
pub struct HarnessClock {
    nanos: Arc<AtomicU64>,
}

impl HarnessClock {
    /// Creates a clock that reads `start`.
    pub fn new(start: AdmissionTime) -> Self {
        let clock = Self::default();
        clock.set(start);
        clock
    }

    /// Sets the time the clock reads, which may be earlier than the current reading.
    pub fn set(&self, time: AdmissionTime) {
        self.nanos.store(time.as_unix_nanos(), Ordering::SeqCst);
    }
}

impl WallClock for HarnessClock {
    fn now(&self) -> AdmissionTime {
        AdmissionTime::from_unix_nanos(self.nanos.load(Ordering::SeqCst))
    }
}

/// The cut an admission point first saw, by its new generation, as a real point records it.
#[derive(Debug)]
struct ObservedCut {
    new: GenerationId,
    epoch_at_publish: u32,
    admitted_to_new: bool,
}

/// The clock one fixed component's port stamps by, which every lane of the port shares, as the lane points of a source
/// share its source clock: one latest time, one epoch, and one reset decision.
struct SharedClock {
    /// What the clock reads: the harness sets it before each stamp.
    wall: HarnessClock,
    clock: AdmissionClock,
}

/// One admission point of the frame, stamping as a real one does.
struct ModelPoint {
    id: AdmissionPointId,
    mode: AdmissionMode,
    route: Option<FrameTarget>,
    feeds_state: bool,
    /// The point's lane of its port, and how the port's edge reaches the lanes of its target.
    lane: u16,
    routing: PointRouting,
    /// The clock the point stamps by, shared with the other lanes of its port.
    clock: Arc<Mutex<SharedClock>>,
    /// The point's own last stamp and its epoch, by which it passes a cut.
    own: Option<(AdmissionTime, u32)>,
    observed: Option<ObservedCut>,
    /// The rotation of a point that spreads its buffers over the lanes of its target.
    spread: u16,
}

impl ModelPoint {
    fn shared(&self) -> std::sync::MutexGuard<'_, SharedClock> {
        self.clock.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Returns the latest time of the clock the point stamps by.
    fn clock_latest(&self) -> Option<AdmissionTime> {
        self.shared().clock.latest()
    }

    /// Returns the epoch of the clock the point stamps by.
    fn clock_epoch(&self) -> u32 {
        self.shared().clock.epoch()
    }

    /// Records the cut to `new`, if the point has not seen it, and returns what it recorded: the epoch the cut's
    /// boundary recorded for the point, or, for a point it does not record, its clock's epoch now.
    fn observe(&mut self, new: &GenerationRef) -> &mut ObservedCut {
        if self.observed.as_ref().is_none_or(|observed| observed.new != new.id()) {
            let epoch = new
                .cut()
                .and_then(|cut| cut.epoch_of(self.id))
                .unwrap_or_else(|| self.clock_epoch());
            self.observed = Some(ObservedCut {
                new: new.id(),
                epoch_at_publish: epoch,
                admitted_to_new: false,
            });
        }
        self.observed.as_mut().expect("just recorded")
    }

    /// Stamps a header at `at` under `schedule`.
    fn stamp(&mut self, at: AdmissionTime, schedule: &AdmissionSchedule) -> BatchHeader {
        let epoch_at_publish = match schedule {
            AdmissionSchedule::Steady(_) => self.clock_epoch(),
            AdmissionSchedule::Cut { new, .. } => self.observe(new).epoch_at_publish,
        };
        let stamp = {
            let mut shared = self.shared();
            shared.wall.set(at);
            shared.clock.stamp()
        };
        self.own = Some((stamp.admitted_at, stamp.epoch));
        let generation = schedule.generation_for(stamp.admitted_at, stamp.epoch, epoch_at_publish);
        if let AdmissionSchedule::Cut { new, .. } = schedule {
            if Arc::ptr_eq(generation, new) {
                self.observe(new).admitted_to_new = true;
            }
        }
        BatchHeader {
            admitted_at: Some(stamp.admitted_at),
            admission_point: Some(self.id),
            epoch: stamp.epoch,
            generation: Some(Arc::clone(generation)),
            entry: None,
        }
    }

    /// Returns whether the point admitted to the new generation of the cut `schedule` holds.
    fn admitted_to_new(&self, schedule: &AdmissionSchedule) -> bool {
        match (schedule, &self.observed) {
            (AdmissionSchedule::Cut { new, .. }, Some(observed)) => {
                observed.new == new.id() && observed.admitted_to_new
            }
            _ => false,
        }
    }

    /// Returns whether every later stamp of the point gets the new generation of the cut `schedule` holds, by the
    /// point's own last stamp.
    fn passed_cut(&self, schedule: &AdmissionSchedule) -> bool {
        match (schedule, &self.observed, self.own) {
            (AdmissionSchedule::Cut { new, at, .. }, Some(observed), Some((latest, epoch)))
                if observed.new == new.id() =>
            {
                latest >= *at || epoch != observed.epoch_at_publish
            }
            _ => false,
        }
    }

    /// Returns the lane of the point's target its next buffer goes to, among `lanes`.
    fn target_lane(&mut self, lanes: u16) -> u16 {
        match self.routing {
            PointRouting::One => 0,
            PointRouting::Fixed => self.lane.min(lanes.saturating_sub(1)),
            PointRouting::Spread => {
                let lane = self.spread % lanes.max(1);
                self.spread = self.spread.wrapping_add(1);
                lane
            }
        }
    }
}

/// How a [`FrameHarness`] compiles and runs its programs.
#[derive(Clone, Copy, Debug)]
pub struct HarnessOptions {
    /// The entries of each memoized decision's memo; zero memoizes nothing.
    pub memo_entries: usize,

    /// The most records each output accepts; it refuses the rest, as an output under capacity pressure does.
    pub output_capacity: Option<usize>,

    /// The budget each call into a region gets, which bounds its drains; a call that runs out yields, and the harness
    /// resumes it at once.
    pub turn_budget: Budget,

    /// Whether every source that feeds a state owner heartbeats at each [`advance`][FrameHarness::advance]'s time
    /// first, unless it stamped later already, as the interpreter's
    /// [`with_heartbeats_at_ticks`][crate::interpret::Interpreter::with_heartbeats_at_ticks] does.
    pub heartbeats_at_ticks: bool,

    /// The capacity of the host's global state budget, or `None` for an unlimited one.
    pub state_capacity: Option<u64>,

    /// The panic budget of every generation [`prepare`][FrameHarness::prepare] prepares: the default program's by
    /// default, and an artifact's to exercise recovery. The boot generation always has the default program's.
    pub candidate_budget: PanicBudget,

    /// Whether the host's state budgets record every operation on them, for the capacity check
    /// ([`FrameHarness::budget_traces`]); `state_capacity` is then the global budget's capacity, unlimited if
    /// `None`.
    pub record_budgets: bool,

    /// How the regions' monotonic clock moves at an [`advance`][FrameHarness::advance]. With `None`, it follows the
    /// wall clock from the harness's start and never moves back, so that after the wall clock jumps back no tick falls
    /// due until it passes its latest time again. With `Some(step)`, it moves by each forward step of the wall clock
    /// from the previous advance, and by `step` when the wall clock jumped back, or forward by more than the
    /// clock-reset threshold, between two advances: as a running host's monotonic clock goes on while its wall clock
    /// steps, so that regions tick in every timeline of a trace whose clock resets.
    pub monotonic_across_jumps: Option<Duration>,

    /// Whether each lane of a region runs every deadline that passed before the next lane runs any, as lane tasks of
    /// which one runs ahead of the others do. By default the lanes run one passed deadline each in turn, round after
    /// round, as tasks that run together do.
    pub deadlines_lane_by_lane: bool,

    /// Whether programs are compiled with the decision-time rewrites (R2, R3's folding of bound constants, R4), as
    /// production compiles them.
    #[cfg(test)]
    pub decision_rewrites: bool,

    /// Whether programs are compiled with R5b, which pushes filters into the state owners above them, as production
    /// compiles them.
    #[cfg(test)]
    pub pushed_filters: bool,
}

impl Default for HarnessOptions {
    fn default() -> Self {
        Self {
            memo_entries: crate::limits::values::DECISION_MEMO_ENTRIES as usize,
            output_capacity: None,
            turn_budget: Budget::unlimited(),
            heartbeats_at_ticks: false,
            state_capacity: None,
            candidate_budget: PanicBudget::DEFAULT_PROGRAM,
            record_budgets: false,
            monotonic_across_jumps: None,
            deadlines_lane_by_lane: false,
            #[cfg(test)]
            decision_rewrites: true,
            #[cfg(test)]
            pushed_filters: true,
        }
    }
}

/// One record an output received, with the header its buffer carried.
#[derive(Clone, Debug)]
pub struct DeliveredRecord {
    /// The output.
    pub output: OutputId,

    /// The admission time the buffer was stamped with: for time-driven output, its flush time.
    pub admitted_at: Option<AdmissionTime>,

    /// The admission point that stamped the buffer: an isolation slot's output point for time-driven output.
    pub admission_point: Option<AdmissionPointId>,

    /// The epoch of that point's clock.
    pub epoch: u32,

    /// The generation the buffer carried.
    pub generation: Option<GenerationId>,

    /// The record.
    pub metric: Metric,
}

/// A candidate prepared in every region and not committed yet.
struct Prepared {
    generation: GenerationRef,
    compiled: CompiledPlan,
}

/// A frame whose slots run through region drivers, wired together in process; see the [module documentation](self).
pub struct FrameHarness {
    options: HarnessOptions,
    registry: OperatorRegistry,
    frame: Frame,
    placement: Placement,
    programs: Vec<Arc<RegionProgram>>,
    /// The drivers of every lane of every slot, slot by slot, lane by lane.
    drivers: Vec<MetricRegionDriver>,
    /// The index in `drivers` of each slot's first lane.
    first_lane: Vec<usize>,
    /// What each slot's lanes share: their lineage records and panic ledger.
    slot_lanes: Vec<Arc<SlotLanes>>,
    /// The cell of each lineage of a slot on several lanes, which every lane's instance is built against.
    cells: BTreeMap<(SlotId, LineageKey), Arc<LaneCell>>,
    /// The rotation of each lane's exit that spreads its buffers over its target's lanes.
    spread: BTreeMap<(usize, usize), u16>,
    hosts: BTreeMap<NodeId, (Box<dyn OpaqueHostAdapter>, usize)>,
    points: BTreeMap<AdmissionPointId, ModelPoint>,
    schedule: ScheduleHandle,
    active: GenerationRef,
    /// The generation a cut in flight replaces, with its programs and placement, which a cancelled cut restores.
    replaced: Option<(GenerationRef, Vec<Arc<RegionProgram>>, Placement)>,
    prepared: Option<Prepared>,
    next_generation: u64,
    wall: HarnessClock,
    /// The wall-clock time the harness started at, when the regions' monotonic clock read `start`.
    start_wall: AdmissionTime,
    start: Instant,
    elapsed: Duration,
    /// The wall-clock time of the latest `advance`, at which its stamps arrive.
    last_tick: Option<AdmissionTime>,
    outputs: OutputPoints,
    delivered: Vec<DeliveredRecord>,
    accepted: BTreeMap<OutputId, usize>,
    refused: OutputPoints,
    telemetry: RecordingTelemetry,
    budgets: StateBudgets,
    recordings: Option<BudgetRecordings>,
    holding: bool,
    held: VecDeque<(FrameTarget, u16, EventsBuffer, AdmissionTime)>,
    /// The generation hosts re-admit with while held buffers are delivered: the held batch's own.
    readmit_as: Option<GenerationRef>,
    /// The slots in an order in which every slot comes after those that feed it.
    order: Vec<SlotId>,
    /// The regions that failed, with why.
    failed: BTreeMap<SlotId, String>,
    /// The generations regions reported failed, with why.
    failures: Arc<Mutex<Vec<(GenerationId, String)>>>,
    /// The number of times a region was resumed after a call yielded.
    resumes: usize,
    /// The buffers each lane of each slot received: those with events, and header-only ones.
    arrivals: BTreeMap<(SlotId, u16), (usize, usize)>,
    /// The lease of every output a generation's plan declared.
    leases: HarnessLeases,
    /// How buffers between lanes are delivered, and what is in flight on each path.
    delivery: Delivery,
    /// What each state lane observed, when the harness records it for an arrival replay.
    recorder: Option<LaneRecorder>,
    /// What every state lane reported of its time, by driver index, in the order the harness took it.
    time_events: Vec<(usize, LaneTimeEvent)>,
}

/// The leases the harness gives outputs, standing in for the installer and the encode hosts: one per output, the first
/// time a plan declares it, never reused.
#[derive(Default)]
struct HarnessLeases {
    leases: BTreeMap<OutputId, EntryId>,
    outputs: BTreeMap<EntryId, OutputId>,
}

impl HarnessLeases {
    /// Returns the lease table of a generation whose plan is `plan`, giving each output it declares for the first time
    /// a lease.
    fn table(&mut self, plan: &Plan) -> Arc<LeaseTable> {
        for output in &plan.outputs {
            if !self.leases.contains_key(&output.id) {
                let lease = EntryId::new(u16::try_from(self.leases.len()).expect("a test plan declares few outputs"));
                self.leases.insert(output.id.clone(), lease);
                self.outputs.insert(lease, output.id.clone());
            }
        }
        Arc::new(
            plan.outputs
                .iter()
                .map(|output| (output.id.clone(), self.leases[&output.id]))
                .collect(),
        )
    }

    /// Returns the output a lease belongs to.
    fn output(&self, lease: EntryId) -> Option<&OutputId> {
        self.outputs.get(&lease)
    }
}

/// Names slots as the default program's host does, and every other slot `region_<first node>`.
fn namer(nodes: &[NodeId]) -> Result<NativeComponentId, String> {
    crate::testing::default_program_slot_name(nodes)
}

/// The most deadlines one region runs for one move of time before the harness gives up on it.
const MAX_DEADLINES: usize = 100_000;

impl FrameHarness {
    /// Computes the frame of a valid boot plan, and creates a driver for each slot, prepared for generation 1 and
    /// started at the wall-clock time `B` the default program's fixtures use.
    pub fn new(plan: &Plan, report: &ValidationReport, registry: OperatorRegistry) -> Self {
        Self::with_options(plan, report, registry, HarnessOptions::default())
    }

    /// Computes the frame of a valid boot plan, and creates a driver for each slot, prepared for generation 1, compiled
    /// and run as `options` say, and started at the wall-clock time `B` the default program's fixtures use.
    pub fn with_options(
        plan: &Plan, report: &ValidationReport, registry: OperatorRegistry, options: HarnessOptions,
    ) -> Self {
        Self::starting_at(
            plan,
            report,
            registry,
            options,
            AdmissionTime::from_unix_secs(1_700_000_000),
        )
    }

    /// Computes the frame of a valid boot plan, and creates a driver for each slot, prepared for generation 1, compiled
    /// and run as `options` say, and started at the wall-clock time `start`.
    ///
    /// # Panics
    ///
    /// Panics if the plan has no frame or does not compile, or a region cannot be created: a test bug.
    pub fn starting_at(
        plan: &Plan, report: &ValidationReport, registry: OperatorRegistry, options: HarnessOptions,
        start: AdmissionTime,
    ) -> Self {
        Self::with_hosts(plan, report, registry, options, start, &FrameHosts::new())
    }

    /// Computes the frame of a valid boot plan with the given delivery hosts, and creates a driver for each slot,
    /// prepared for generation 1, compiled and run as `options` say, and started at the wall-clock time `start`.
    ///
    /// The harness stands in for the encode hosts: it gives every output a lease the first time a generation's plan
    /// declares it, never reused, prepares each generation with the lease table of its plan, and delivers a batch that
    /// reaches an encode host to the output its header's lease names.
    ///
    /// # Panics
    ///
    /// Panics if the plan has no frame with these hosts or does not compile, or a region cannot be created: a test bug.
    pub fn with_hosts(
        plan: &Plan, report: &ValidationReport, registry: OperatorRegistry, options: HarnessOptions,
        start: AdmissionTime, hosts: &FrameHosts,
    ) -> Self {
        let frame = Frame::compute_with_hosts(plan, &registry, &namer, hosts).expect("the boot plan has a frame");
        Self::on_frame(frame, plan, report, registry, options, start).unwrap_or_else(|error| panic!("{}", error))
    }

    /// Creates a driver for each lane of each slot of `frame`, prepared for generation 1 with the boot plan's programs,
    /// compiled and run as `options` say, and started at the wall-clock time `start`.
    fn on_frame(
        frame: Frame, plan: &Plan, report: &ValidationReport, registry: OperatorRegistry, options: HarnessOptions,
        start: AdmissionTime,
    ) -> Result<Self, String> {
        let compiled = compiled_with(plan, report, &registry, &frame, options);
        let mut leases = HarnessLeases::default();
        let boot_leases = leases.table(plan);
        let active: GenerationRef = Arc::new(GenerationToken::new(GenerationId::new(1)));
        let schedule = AdmissionSchedule::Steady(Arc::clone(&active)).into_handle();
        let telemetry = RecordingTelemetry::default();
        let (budgets, recordings) = match (options.state_capacity, options.record_budgets) {
            (capacity, true) => {
                let (budgets, recordings) = StateBudgets::recorded(capacity.unwrap_or(u64::MAX));
                (budgets, Some(recordings))
            }
            (Some(capacity), false) => (StateBudgets::new(capacity), None),
            (None, false) => (StateBudgets::unlimited(), None),
        };
        let wall = HarnessClock::new(start);
        let started = Instant::now();
        let failures = Arc::new(Mutex::new(Vec::new()));
        let reporter: crate::runtime::GenerationFailureReporter = {
            let failures = Arc::clone(&failures);
            Arc::new(move |generation, reason| {
                failures
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push((generation, reason.to_string()));
            })
        };
        let mut cells = BTreeMap::new();
        let mut drivers = Vec::new();
        let mut first_lane = Vec::new();
        let mut slot_lanes = Vec::new();
        for (slot, program) in frame.slots().iter().zip(&compiled.programs) {
            first_lane.push(drivers.len());
            let shared = SlotLanes::new(slot.lanes);
            let slot_cells = lane_cells(&mut cells, slot, program);
            for lane in 0..slot.lanes {
                let keys = program.kernels().iter().map(|kernel| &kernel.key);
                let kernels = build_kernels(program, keys, |node| {
                    lane_context(&budgets, &slot_cells, slot, node, lane)
                })
                .map_err(|error| format!("the kernels of `{}` lane {} do not build: {}", slot.name, lane, error))?;
                let boot = PrepareRegion::new(
                    Arc::clone(&active),
                    Arc::clone(program),
                    kernels,
                    PanicBudget::DEFAULT_PROGRAM,
                    &telemetry,
                )
                .with_leases(Arc::clone(&boot_leases));
                let mut config = RegionConfig::for_slot(slot, &telemetry)
                    .with_memo_entries(options.memo_entries)
                    .with_schedule(Arc::clone(&schedule))
                    .with_wall_clock(Arc::new(wall.clone()))
                    .with_state_budgets(budgets.clone())
                    .with_failure_reporter(Arc::clone(&reporter));
                // Every lane records what it counts, a slot's only lane too: its reset window overflows alike.
                config = config.with_state_lane_telemetry(telemetry.state_lane(slot.name.as_str(), lane));
                if slot.lanes > 1 {
                    config = config.with_lane(
                        LaneContext {
                            index: lane,
                            count: slot.lanes,
                        },
                        Arc::clone(&shared),
                    );
                }
                let mut driver = MetricRegionDriver::new(config, boot).map_err(|error| {
                    format!("the region of `{}` lane {} is not created: {}", slot.name, lane, error)
                })?;
                let mut budget = options.turn_budget;
                driver.start(&mut RegionTurn::new(started, &mut budget));
                drivers.push(driver);
            }
            slot_lanes.push(shared);
        }
        let hosts = frame
            .fixed()
            .iter()
            .filter(|fixed| fixed.operator.as_str() != names::SOURCE)
            .map(|fixed| {
                let operator = registry
                    .configure(fixed.operator.as_str(), &fixed.params)
                    .expect("a fixed component configures");
                let ports = operator.ports().outputs.len();
                // Every opaque host echoes its input: the harness runs plans, not native components.
                (
                    fixed.node.clone(),
                    (Box::new(EchoHost) as Box<dyn OpaqueHostAdapter>, ports),
                )
            })
            .collect();
        let mut points = BTreeMap::new();
        for point in frame.admission_points() {
            // The lanes of a point stamp by one clock, as the lane points of a source share its clock.
            let clock_wall = HarnessClock::new(start);
            let clock = Arc::new(Mutex::new(SharedClock {
                clock: AdmissionClock::with_wall_clock(CLOCK_RESET_THRESHOLD, Arc::new(clock_wall.clone())),
                wall: clock_wall,
            }));
            for (lane, id) in point.lane_ids().enumerate() {
                points.insert(
                    id,
                    ModelPoint {
                        id,
                        mode: point.mode,
                        route: point.route.clone(),
                        feeds_state: point.feeds_state,
                        lane: lane as u16,
                        routing: point.routing,
                        clock: Arc::clone(&clock),
                        own: None,
                        observed: None,
                        spread: 0,
                    },
                );
            }
        }
        let order = topological_slots(&frame);
        Ok(Self {
            options,
            registry,
            frame,
            placement: compiled.placement,
            programs: compiled.programs,
            drivers,
            first_lane,
            slot_lanes,
            cells,
            spread: BTreeMap::new(),
            hosts,
            points,
            schedule,
            active,
            replaced: None,
            prepared: None,
            next_generation: 2,
            wall,
            start_wall: start,
            start: started,
            elapsed: Duration::ZERO,
            last_tick: None,
            outputs: OutputPoints::new(),
            delivered: Vec::new(),
            accepted: BTreeMap::new(),
            refused: OutputPoints::new(),
            telemetry,
            budgets,
            recordings,
            holding: false,
            held: VecDeque::new(),
            readmit_as: None,
            order,
            failed: BTreeMap::new(),
            failures,
            resumes: 0,
            arrivals: BTreeMap::new(),
            leases,
            delivery: Delivery::new(DeliverySchedule::TraceOrder),
            recorder: None,
            time_events: Vec::new(),
        })
    }

    /// Returns the index in `drivers` of lane `lane` of `slot`.
    fn driver_index(&self, slot: SlotId, lane: u16) -> usize {
        self.first_lane[slot.get()] + usize::from(lane)
    }

    /// Returns the lanes `slot` runs on.
    fn lanes_of(&self, slot: SlotId) -> u16 {
        self.frame.slot(slot).map_or(1, |slot| slot.lanes)
    }

    /// Returns the regions' monotonic clock reading.
    pub fn now(&self) -> Instant {
        self.start + self.elapsed
    }

    /// Returns the wall clock the regions and sources read.
    pub fn wall(&self) -> AdmissionTime {
        self.wall.now()
    }

    /// Returns the frame.
    pub fn frame(&self) -> &Frame {
        &self.frame
    }

    /// Returns the registry the frame was computed with.
    pub fn registry(&self) -> &OperatorRegistry {
        &self.registry
    }

    /// Returns where the active plan's nodes run.
    pub fn placement(&self) -> &Placement {
        &self.placement
    }

    /// Returns the generation the schedule gives new batches, or the new generation of a cut in flight.
    pub fn active(&self) -> &GenerationRef {
        &self.active
    }

    /// Returns the host's state budgets, which every state owner's kernel draws on.
    pub fn budgets(&self) -> &StateBudgets {
        &self.budgets
    }

    /// Holds every buffer one slot sends another from now on, until [`stop_holding`][Self::stop_holding]: what is in
    /// flight between two regions when a generation commits.
    pub fn hold_between_slots(&mut self) {
        self.holding = true;
    }

    /// Stops holding buffers between slots; what is held stays held until [`release`][Self::release].
    pub fn stop_holding(&mut self) {
        self.holding = false;
    }

    /// Delivers the buffers held between slots, in the order they were sent.
    ///
    /// A held batch runs whole in its own generation, as the reference interpreter runs it: a host it reaches
    /// re-admits it with that generation, as if the host had drained its queue before the commit. What the hold
    /// changes is when the regions below run it: after a later generation committed, and after later batches.
    pub fn release(&mut self) {
        while let Some((target, lane, buffer, arrival)) = self.held.pop_front() {
            self.readmit_as = buffer.header().generation.clone();
            self.deliver(Some(target), lane, buffer, arrival, None);
            self.readmit_as = None;
        }
    }

    /// Returns the number of buffers held between slots.
    pub fn held(&self) -> usize {
        self.held.len()
    }

    /// Prepares a candidate plan in every region, as the installer does before its barrier: each lane of each region
    /// gets the kernels it does not hold, built with the host's state budgets (a new lineage's on every lane against
    /// one cell), and its state owners' bindings. A state owner of a lineage the active program has in its slot
    /// retains that instance; any other is new, and an active state owner whose lineage the candidate does not keep
    /// retires, which makes the installation a cut. Admission points keep stamping the active generation until
    /// [`commit`][Self::commit] or [`arm_cut`][Self::arm_cut].
    ///
    /// Returns whether the installation is a cut.
    ///
    /// # Panics
    ///
    /// Panics if the candidate does not compile into the frame, or a region refuses the preparation: a test bug.
    pub fn prepare(&mut self, plan: &Plan, report: &ValidationReport) -> bool {
        self.try_prepare(plan, report, None)
            .unwrap_or_else(|rejection| panic!("every region prepares the candidate: {}", rejection))
    }

    /// Prepares a candidate plan as [`prepare`][Self::prepare] does, except that lane `lane` of `slot` gets its state
    /// owners' kernels built for another lane of the slot, which its region refuses. As the coordinator does with the
    /// first refusal of a preparation, the candidate is then aborted in every lane of every region, and its generation
    /// is never published, so nothing stamps it: the next candidate takes its identifier. Returns why it was refused.
    ///
    /// # Errors
    ///
    /// Returns the refusal, or `Ok` with whether the installation is a cut if no region refused it, as for a slot on
    /// one lane, whose lane every kernel is built for.
    ///
    /// # Panics
    ///
    /// Panics if the candidate does not compile into the frame: a test bug.
    pub fn prepare_refused_by(
        &mut self, plan: &Plan, report: &ValidationReport, slot: SlotId, lane: u16,
    ) -> Result<bool, String> {
        self.try_prepare(plan, report, Some((slot, lane)))
    }

    /// Prepares a candidate in every region, its state owners' kernels on lane `faulty` built for another lane; see
    /// [`prepare_refused_by`][Self::prepare_refused_by].
    fn try_prepare(
        &mut self, plan: &Plan, report: &ValidationReport, faulty: Option<(SlotId, u16)>,
    ) -> Result<bool, String> {
        let compiled = compiled_with(plan, report, &self.registry, &self.frame, self.options);
        let reuse = kernel_reuse(Some(&self.programs), &compiled.programs);
        let generation: GenerationRef = Arc::new(GenerationToken::new(GenerationId::new(self.next_generation)));
        self.next_generation += 1;
        let active = self.active.id();
        let bindings = compiled
            .programs
            .iter()
            .zip(&self.programs)
            .map(|(candidate, current)| state_bindings(current, candidate, active))
            .collect::<Vec<_>>();
        let cut = bindings
            .iter()
            .flatten()
            .any(|binding| !matches!(binding, StateBinding::Retain { .. }));
        let leases = self.leases.table(plan);
        let slots = self.frame.slots().to_vec();
        let mut rejection = None;
        for (((slot, program), kernels), bindings) in slots.iter().zip(&compiled.programs).zip(&reuse).zip(bindings) {
            // State owners that start a new instance are built even when an instance of their lineage is retiring.
            let mut keys = kernels.build.clone();
            let mut new = Vec::new();
            for binding in &bindings {
                if let StateBinding::New { node, key } = binding {
                    new.push(key.clone());
                    if let Some(kernel) = program.kernels().iter().find(|kernel| &kernel.key.node == node) {
                        if !keys.contains(&kernel.key) {
                            keys.push(kernel.key.clone());
                        }
                    }
                }
            }
            // A new lineage's instances on every lane share one new cell.
            for key in new {
                self.cells.remove(&(slot.id, key));
            }
            let slot_cells = lane_cells(&mut self.cells, slot, program);
            for lane in 0..slot.lanes {
                let built_for = match faulty {
                    Some((faulty_slot, faulty_lane)) if faulty_slot == slot.id && faulty_lane == lane => {
                        (lane + 1) % slot.lanes
                    }
                    _ => lane,
                };
                let built = build_kernels(program, &keys, |node| {
                    lane_context(&self.budgets, &slot_cells, slot, node, built_for)
                })
                .expect("the kernels build");
                let prepare = PrepareRegion::new(
                    Arc::clone(&generation),
                    Arc::clone(program),
                    built,
                    self.options.candidate_budget,
                    &self.telemetry,
                )
                .with_state_bindings(bindings.clone())
                .with_leases(Arc::clone(&leases));
                let mut budget = self.options.turn_budget;
                let mut sink = ExitSink::new(0);
                let now = self.start + self.elapsed;
                let index = self.driver_index(slot.id, lane);
                if let Err(refusal) = self.drivers[index].control(
                    &mut RegionTurn::new(now, &mut budget),
                    RegionControl::Prepare(Box::new(prepare)),
                    &mut sink,
                ) {
                    rejection.get_or_insert_with(|| {
                        format!(
                            "the region of `{}` lane {} refused the candidate: {}",
                            slot.name, lane, refusal
                        )
                    });
                }
            }
        }
        if let Some(rejection) = rejection {
            for driver in &mut self.drivers {
                let mut budget = self.options.turn_budget;
                let mut sink = ExitSink::new(0);
                driver
                    .control(
                        &mut RegionTurn::new(self.start + self.elapsed, &mut budget),
                        RegionControl::Abort {
                            generation: generation.id(),
                        },
                        &mut sink,
                    )
                    .expect("aborting never fails");
            }
            self.next_generation -= 1;
            return Err(rejection);
        }
        self.prepared = Some(Prepared { generation, compiled });
        Ok(cut)
    }

    /// Commits the prepared generation: admission points stamp it from now on, and nothing in the harness holds the
    /// one it replaces any more.
    ///
    /// # Panics
    ///
    /// Panics if no generation is prepared.
    pub fn commit(&mut self) {
        let Prepared { generation, compiled } = self.prepared.take().expect("a generation is prepared");
        self.schedule
            .store(Arc::new(AdmissionSchedule::Steady(Arc::clone(&generation))));
        self.install(generation, compiled);
    }

    fn install(&mut self, generation: GenerationRef, compiled: CompiledPlan) {
        self.active = generation;
        self.programs = compiled.programs;
        self.placement = compiled.placement;
    }

    /// Arms a cut to the prepared generation, as the installer does in one transaction over the admission points that
    /// feed state owners: the cut takes effect at the first multiple of the least common multiple of `widths` at least
    /// [`CUT_MARGIN`] after the latest of those points' clocks' latest stamps and the wall clock, recorded on the
    /// generation with each point's clock's epoch before the schedule publishes it. Returns the cut's time.
    ///
    /// # Panics
    ///
    /// Panics if no generation is prepared, or the widths do not line up within the transition bound.
    pub fn arm_cut(&mut self, widths: &[Duration]) -> AdmissionTime {
        let Prepared { generation, compiled } = self.prepared.take().expect("a generation is prepared");
        let wall = self.wall.now();
        let affected = self
            .points
            .values()
            .filter(|point| point.feeds_state)
            .map(|point| point.id)
            .collect::<Vec<_>>();
        let now = affected
            .iter()
            .filter_map(|id| self.points[id].clock_latest())
            .fold(wall, AdmissionTime::max);
        let at = cut_time(now, lcm_secs(widths).expect("the widths line up"), CUT_MARGIN);
        let old = Arc::clone(&self.active);
        let epochs = affected.iter().map(|id| (*id, self.points[id].clock_epoch())).collect();
        generation
            .set_cut(CutBoundary {
                at,
                replaces: old.id(),
                epochs,
            })
            .expect("a generation is armed once");
        self.schedule.store(Arc::new(AdmissionSchedule::Cut {
            old: Arc::clone(&old),
            new: Arc::clone(&generation),
            at,
        }));
        self.replaced = Some((old, self.programs.clone(), self.placement.clone()));
        self.install(generation, compiled);
        at
    }

    /// Returns whether some admission point admitted to the new generation of the cut in flight, which makes it
    /// irrevocable.
    pub fn cut_irrevocable(&self) -> bool {
        let schedule = self.schedule.load();
        self.points.values().any(|point| point.admitted_to_new(&schedule))
    }

    /// Returns whether every admission point that feeds state passed the cut in flight: every later stamp gets its new
    /// generation.
    pub fn cut_passed(&self) -> bool {
        let schedule = self.schedule.load();
        self.points
            .values()
            .filter(|point| point.feeds_state)
            .all(|point| point.passed_cut(&schedule))
    }

    /// Finalizes the cut in flight: the schedule holds its new generation alone.
    pub fn finalize_cut(&mut self) {
        self.schedule
            .store(Arc::new(AdmissionSchedule::Steady(Arc::clone(&self.active))));
        self.replaced = None;
    }

    /// Cancels the cut in flight if no admission point admitted to its new generation, restoring the replaced
    /// generation and aborting the new one in every region, and returns whether it did.
    pub fn cancel_cut(&mut self) -> bool {
        if self.cut_irrevocable() {
            return false;
        }
        let Some((old, programs, placement)) = self.replaced.take() else {
            return false;
        };
        self.schedule
            .store(Arc::new(AdmissionSchedule::Steady(Arc::clone(&old))));
        let new = std::mem::replace(&mut self.active, old);
        // Later candidates are prepared against the generation the cut replaced, as the installer prepares them.
        self.programs = programs;
        self.placement = placement;
        for driver in &mut self.drivers {
            let mut budget = self.options.turn_budget;
            let mut sink = ExitSink::new(0);
            driver
                .control(
                    &mut RegionTurn::new(self.start + self.elapsed, &mut budget),
                    RegionControl::Abort { generation: new.id() },
                    &mut sink,
                )
                .expect("aborting never fails");
        }
        true
    }

    /// Admits a batch at a source's `metrics` output, stamped at `at` by its admission point's clock: at its first
    /// lane.
    pub fn admit(&mut self, source: &str, at: AdmissionTime, metrics: Vec<Metric>) {
        self.admit_at_lane(source, 0, at, metrics);
    }

    /// Admits a batch at lane `lane` of a source's `metrics` output, stamped at `at` by the clock its lanes share.
    ///
    /// # Panics
    ///
    /// Panics if the source has no such lane.
    pub fn admit_at_lane(&mut self, source: &str, lane: u16, at: AdmissionTime, metrics: Vec<Metric>) {
        let point = self.source_point(source, lane);
        let header = self.stamp(point, at);
        let arrival = self.last_tick.map_or(at, |tick| tick.max(at));
        self.deliver_from_point(point, buffer_of(header, metrics), arrival);
        self.pump();
    }

    /// Admits a batch at a source's `metrics` output at the wall clock's time.
    pub fn admit_now(&mut self, source: &str, metrics: Vec<Metric>) {
        let at = self.wall.now();
        self.admit(source, at, metrics);
    }

    /// Sends a heartbeat from a source's `metrics` output, stamped at `at`, toward the state owners it feeds: from its
    /// first lane.
    pub fn heartbeat(&mut self, source: &str, at: AdmissionTime) {
        self.heartbeat_at_lane(source, 0, at);
    }

    /// Sends a heartbeat from lane `lane` of a source's `metrics` output, stamped at `at`.
    ///
    /// # Panics
    ///
    /// Panics if the source has no such lane.
    pub fn heartbeat_at_lane(&mut self, source: &str, lane: u16, at: AdmissionTime) {
        let point = self.source_point(source, lane);
        self.heartbeat_point(point, at);
        self.pump();
    }

    fn heartbeat_point(&mut self, point: AdmissionPointId, at: AdmissionTime) {
        let header = self.stamp(point, at);
        self.deliver_from_point(point, EventsBuffer::with_capacity(0).with_header(header), at);
    }

    /// Delivers what an admission point stamped to the lane of its target its routing gives.
    fn deliver_from_point(&mut self, point: AdmissionPointId, buffer: EventsBuffer, arrival: AdmissionTime) {
        let route = self.points[&point].route.clone();
        let lanes = match &route {
            Some(FrameTarget::Slot(slot)) => self.lanes_of(*slot),
            _ => 1,
        };
        let lane = self
            .points
            .get_mut(&point)
            .expect("the frame's admission point")
            .target_lane(lanes);
        self.deliver(route, lane, buffer, arrival, None);
    }

    fn source_point(&self, source: &str, lane: u16) -> AdmissionPointId {
        let node: NodeId = source.parse().expect("a source's ID");
        let point = self
            .frame
            .admission_point_at(&node, &PortName::from_static(crate::operators::METRICS_PORT))
            .expect("the source has an admission point");
        point
            .lane_id(lane)
            .unwrap_or_else(|| panic!("source `{}` has {} lane(s), not lane {}", source, point.lanes, lane))
    }

    fn stamp(&mut self, point: AdmissionPointId, at: AdmissionTime) -> BatchHeader {
        let schedule = self.schedule.load();
        let point = self.points.get_mut(&point).expect("the frame's admission point");
        let header = point.stamp(at, &schedule);
        self.delivery.stamped(&header);
        header
    }

    /// Moves time to the wall-clock time `now`, as the reference interpreter's `advance` does: every host ticks, every
    /// lane of every source that feeds a state owner heartbeats first if [`HarnessOptions::heartbeats_at_ticks`] says
    /// so, every lane's open routed buffers are sent, and the regions' monotonic clock moves as
    /// [`HarnessOptions::monotonic_across_jumps`] says, running every deadline that passes: by default, to as long
    /// after their start as `now` is after the harness's start.
    ///
    /// An isolation slot ticks on its own schedule, every flush interval from its start, not at every `advance`: a test
    /// that compares it with the interpreter advances at the times its ticks are due.
    pub fn advance(&mut self, now: AdmissionTime) {
        // A seeded schedule holds nothing across more than one tick: what waits arrives before the next tick's
        // heartbeats, so that no lane falls further behind than the tick its heartbeat still waits for.
        if matches!(self.delivery.schedule, DeliverySchedule::Seeded(_)) {
            self.delivery.advanced();
            self.flush_waited();
        }
        let previous = self.last_tick.replace(now);
        self.wall.set(now);
        let nodes = self.hosts.keys().cloned().collect::<Vec<_>>();
        for node in nodes {
            let (host, ports) = self.hosts.get_mut(&node).expect("a host");
            let mut output = HostOutput::new(*ports);
            host.tick(HostTick { now, watermark: None }, &mut output);
            self.readmit(&node, output, now);
        }
        if self.options.heartbeats_at_ticks {
            let stale = self
                .points
                .values()
                .filter(|point| point.feeds_state && point.mode == AdmissionMode::Admit)
                .filter(|point| {
                    point
                        .own
                        .is_none_or(|(latest, epoch)| latest < now || epoch != point.clock_epoch())
                })
                .map(|point| point.id)
                .collect::<Vec<_>>();
            for point in stale {
                self.heartbeat_point(point, now);
            }
        }
        self.pump();
        self.elapsed = self.elapsed_at(previous, now);
        if let Some(recorder) = &mut self.recorder {
            recorder.begin_tick();
        }
        for slot in self.order.clone() {
            // What a lane holds open goes out once its hold expired, before the regions downstream tick.
            self.end_turns(slot, now);
            if let Some(recorder) = &mut self.recorder {
                for lane in 0..self.frame.slot(slot).map_or(1, |slot| slot.lanes) {
                    recorder.tick(self.first_lane[slot.get()] + usize::from(lane), now);
                }
            }
            self.run_deadlines(slot, now);
        }
        self.pump();
    }

    /// Returns the regions' monotonic clock, as time since their start, once an advance moved the wall clock from the
    /// previous advance's time, `previous`, to `now`; see [`HarnessOptions::monotonic_across_jumps`].
    fn elapsed_at(&self, previous: Option<AdmissionTime>, now: AdmissionTime) -> Duration {
        let since =
            |earlier: AdmissionTime| Duration::from_nanos(now.as_unix_nanos().saturating_sub(earlier.as_unix_nanos()));
        match self.options.monotonic_across_jumps {
            // The monotonic clock follows the wall clock from the start, and never moves back.
            None => self.elapsed.max(since(self.start_wall)),
            Some(step) => {
                let from = previous.unwrap_or(self.start_wall);
                let forward = now >= from && since(from) <= CLOCK_RESET_THRESHOLD;
                self.elapsed + if forward { since(from) } else { step }
            }
        }
    }

    /// Sets the wall clock the regions read, without moving their monotonic clock.
    pub fn set_wall(&mut self, now: AdmissionTime) {
        self.wall.set(now);
    }

    /// Moves the wall clock and the regions' monotonic clock forward by `duration`, and runs every deadline that
    /// passes, as running region hosts do.
    pub fn elapse(&mut self, duration: Duration) {
        let wall = self.wall.now().saturating_add(duration);
        self.wall.set(wall);
        self.elapsed += duration;
        for slot in self.order.clone() {
            self.end_turns(slot, wall);
            self.run_deadlines(slot, wall);
        }
        self.pump();
    }

    /// Ends the turn of every lane of a region whose input is idle, as its host does after a turn: what a lane holds
    /// open past its hold goes out.
    fn end_turns(&mut self, slot: SlotId, arrival: AdmissionTime) {
        if self.failed.contains_key(&slot) {
            return;
        }
        let exits = self.frame.slot(slot).map_or(0, |slot| slot.exits.len());
        for lane in 0..self.lanes_of(slot) {
            let index = self.driver_index(slot, lane);
            let mut budget = self.options.turn_budget;
            let mut sink = ExitSink::new(exits);
            let now = self.now();
            self.drivers[index].end_turn(&mut RegionTurn::new(now, &mut budget), true, &mut sink);
            let buffers = sink.drain_routed().collect::<Vec<_>>();
            self.route_exits(slot, lane, buffers, arrival);
        }
    }

    /// Runs every deadline of every lane of a region that has passed on its monotonic clock, as its host would.
    ///
    /// Lanes run their deadlines together, as their tasks would: one passed deadline of each lane in turn, round after
    /// round, so that when time jumps over several ticks no lane resolves all of them before another resolved the
    /// first, which would make the others stragglers behind the ticks their lineage's records keep. With
    /// [`HarnessOptions::deadlines_lane_by_lane`], each lane runs all of its passed deadlines before the next.
    fn run_deadlines(&mut self, slot: SlotId, arrival: AdmissionTime) {
        let now = self.now();
        if self.options.deadlines_lane_by_lane {
            for lane in 0..self.lanes_of(slot) {
                for _ in 0..MAX_DEADLINES {
                    let index = self.driver_index(slot, lane);
                    if self.failed.contains_key(&slot)
                        || self.drivers[index]
                            .next_deadline()
                            .is_none_or(|deadline| deadline > now)
                    {
                        break;
                    }
                    self.call(slot, lane, Call::Advance, arrival);
                }
            }
            return;
        }
        for _ in 0..MAX_DEADLINES {
            let mut ran = false;
            for lane in 0..self.lanes_of(slot) {
                if self.failed.contains_key(&slot) {
                    return;
                }
                let index = self.driver_index(slot, lane);
                if self.drivers[index]
                    .next_deadline()
                    .is_some_and(|deadline| deadline <= now)
                {
                    ran = true;
                    self.call(slot, lane, Call::Advance, arrival);
                }
            }
            if !ran {
                return;
            }
        }
        panic!(
            "region `{}` kept a passed deadline for {} rounds",
            self.frame.slot(slot).map_or("?", |slot| slot.name.as_str()),
            MAX_DEADLINES
        );
    }

    /// Ends every region's input at the wall-clock time `now`, in an order in which every region finishes after the
    /// regions that feed it, as a topology shuts down: everything in flight toward a region arrives before it finishes.
    pub fn close(&mut self, now: AdmissionTime) {
        self.release();
        self.wall.set(now);
        self.flush_paths();
        for slot in self.order.clone() {
            self.flush_paths();
            for lane in 0..self.lanes_of(slot) {
                self.call(slot, lane, Call::Finish, now);
            }
        }
        self.flush_paths();
        if let Some(recorder) = &mut self.recorder {
            recorder.close();
        }
    }

    /// Delivers a control message to a region, as its coordinator does between calls: to its one lane, or split into
    /// one per lane ([`LaneSplit`]), a kernel message to the lane it names.
    pub fn control(
        &mut self, slot: SlotId, message: RegionControl,
    ) -> Result<(), saluki_core::execution::RegionFailure> {
        let lanes = self.lanes_of(slot);
        let messages = match message {
            RegionControl::Kernel(control) if lanes > 1 => {
                let lane = control.lane.unwrap_or(0);
                vec![(lane, RegionControl::Kernel(control))]
            }
            message if lanes > 1 => match message.split(lanes) {
                Ok(parts) => parts
                    .into_iter()
                    .enumerate()
                    .map(|(lane, part)| (lane as u16, part))
                    .collect(),
                Err(message) => vec![(0, message)],
            },
            message => vec![(0, message)],
        };
        let mut result = Ok(());
        for (lane, message) in messages {
            let mut budget = self.options.turn_budget;
            let mut sink = ExitSink::new(self.frame.slot(slot).map_or(0, |slot| slot.exits.len()));
            let now = self.now();
            let index = self.driver_index(slot, lane);
            let outcome = self.drivers[index].control(&mut RegionTurn::new(now, &mut budget), message, &mut sink);
            let buffers = sink.drain_routed().collect::<Vec<_>>();
            let arrival = self.wall.now();
            self.route_exits(slot, lane, buffers, arrival);
            if result.is_ok() {
                result = outcome;
            }
        }
        self.pump();
        result
    }

    /// Publishes every region's telemetry.
    pub fn publish(&mut self) {
        for driver in &mut self.drivers {
            driver.publish_telemetry();
        }
    }

    /// Re-admits what a host emitted, at its output's admission point, with the host's clock, which never moves back.
    fn readmit(&mut self, node: &NodeId, mut output: HostOutput, now: AdmissionTime) {
        let (ports, _) = output.take();
        let fixed = self.frame.fixed_component(node).expect("a fixed component").clone();
        for (port, metrics) in ports.into_iter().enumerate() {
            if metrics.is_empty() {
                continue;
            }
            let point = fixed.admission_points[port];
            let mut header = self.stamp(point, now);
            if let Some(generation) = &self.readmit_as {
                header.generation = Some(Arc::clone(generation));
            }
            self.deliver_from_point(point, buffer_of(header, metrics), now);
        }
    }

    /// Runs one call of one lane of a region, resuming it while it yields, and delivers what it writes.
    ///
    /// Each call runs under supervision, as a region host runs it: after a panic, what the call wrote is discarded,
    /// the driver recovers, and a yielded call is over; a finish the panic ended is called again until one completes.
    /// A region that fails stops: nothing reaches it any more.
    fn call(&mut self, slot: SlotId, lane: u16, call: Call, arrival: AdmissionTime) {
        if self.failed.contains_key(&slot) {
            return;
        }
        let exits = self.frame.slot(slot).map_or(0, |slot| slot.exits.len());
        let index = self.driver_index(slot, lane);
        let finishing = matches!(call, Call::Finish);
        let mut call = Some(call);
        for _ in 0..MAX_DEADLINES {
            let mut budget = self.options.turn_budget;
            let mut sink = ExitSink::new(exits);
            let now = self.now();
            let driver = &mut self.drivers[index];
            let (site, lost) = match &call {
                Some(Call::Process(buffer)) => (PanicSite::Process, buffer.len() as u64),
                Some(Call::Advance) => (PanicSite::AdvanceTime, 0),
                Some(Call::Finish) => (PanicSite::Finish, 0),
                None => (PanicSite::Resume, 0),
            };
            let current = call.take();
            let result = catch_unwind(AssertUnwindSafe(|| {
                let mut turn = RegionTurn::new(now, &mut budget);
                match current {
                    Some(Call::Process(buffer)) => driver.process(&mut turn, buffer, &mut sink),
                    Some(Call::Advance) => driver.advance_time(&mut turn, &mut sink),
                    Some(Call::Finish) => driver.finish(&mut turn, &mut sink, DrainPolicy::Shutdown),
                    None => driver.resume(&mut turn, &mut sink),
                }
            }));
            if site == PanicSite::Resume {
                self.resumes += 1;
            }
            let events = take_time_events(&mut self.drivers[index]);
            if !events.is_empty() {
                if let Some(recorder) = &mut self.recorder {
                    recorder.report(index, &events);
                }
                self.time_events.extend(events.into_iter().map(|event| (index, event)));
            }
            let progress = match result {
                Ok(progress) => progress,
                Err(payload) => {
                    let lost = lost + sink.event_count() as u64;
                    let panic = RegionPanic::new(site, panic_message(&*payload), lost);
                    match self.drivers[index].reset_after_panic(&panic) {
                        PanicOutcome::Continue if finishing => {
                            call = Some(Call::Finish);
                            continue;
                        }
                        PanicOutcome::Continue => {
                            // A drain the panic ended is due at once, as the host's next turn finds it.
                            self.run_deadlines(slot, arrival);
                            return;
                        }
                        PanicOutcome::Fail(error) => {
                            self.failed.insert(slot, format!("{:#}", error));
                            return;
                        }
                    }
                }
            };
            let buffers = sink.drain_routed().collect::<Vec<_>>();
            self.route_exits(slot, lane, buffers, arrival);
            if progress != Progress::Yielded {
                return;
            }
        }
        panic!("a region yielded {} times in one call", MAX_DEADLINES);
    }

    /// Returns the buffers the slot named `name` received so far: those that carried events, and header-only ones,
    /// such as heartbeats and progress.
    pub fn arrivals(&self, name: &str) -> (usize, usize) {
        let slot = self.slot_named(name);
        (0..self.lanes_of(slot))
            .filter_map(|lane| self.arrivals.get(&(slot, lane)))
            .fold((0, 0), |(data, progress), lane| (data + lane.0, progress + lane.1))
    }

    /// Returns the buffers lane `lane` of the slot named `name` received so far, as [`arrivals`][Self::arrivals] counts
    /// them.
    pub fn lane_arrivals(&self, name: &str, lane: u16) -> (usize, usize) {
        self.arrivals
            .get(&(self.slot_named(name), lane))
            .copied()
            .unwrap_or_default()
    }

    /// Returns the number of times a region was resumed after a call yielded, because its turn's budget ran out.
    pub fn resumes(&self) -> usize {
        self.resumes
    }

    /// Returns why the region of `slot` failed, if it did: a panic it could not recover from.
    pub fn failure(&self, slot: SlotId) -> Option<&str> {
        self.failed.get(&slot).map(String::as_str)
    }

    /// Returns every generation a region reported failed, and why, in the order they were reported.
    pub fn failures(&self) -> Vec<(GenerationId, String)> {
        self.failures
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Hands what one lane of a region wrote to its exits to their targets' lanes, as each exit's routing gives them,
    /// holding buffers between slots when asked to.
    fn route_exits(
        &mut self, slot: SlotId, lane: u16, buffers: Vec<(saluki_core::execution::ExitId, Option<u16>, EventsBuffer)>,
        arrival: AdmissionTime,
    ) {
        let exits = self.frame.slot(slot).expect("the frame's slot").exits.clone();
        let sender = self.driver_index(slot, lane);
        for (exit, routed, buffer) in buffers {
            let definition = &exits[exit.get()];
            let target = definition.target.clone();
            // Every buffer leaves stamped, with an entry exactly when it carries events to another slot or to an encode
            // host, where the entry is its output's lease.
            assert!(buffer.header().generation.is_some(), "exit buffers keep the generation");
            if !buffer.is_empty() {
                assert_eq!(
                    buffer.header().entry.is_some(),
                    matches!(target, FrameTarget::Slot(_) | FrameTarget::EncodeHost(_)),
                    "exit to {:?} has entry {:?}",
                    target,
                    buffer.header().entry
                );
            }
            let target_lanes = match &target {
                FrameTarget::Slot(next) => self.lanes_of(*next),
                _ => 1,
            };
            let target_lane = match (&definition.routing, routed) {
                (_, Some(routed)) => routed,
                (ExitRouting::One, None) | (ExitRouting::Keyed(_), None) => 0,
                (ExitRouting::Affinity, None) => lane % target_lanes.max(1),
                (ExitRouting::Spread, None) => {
                    // Each lane's rotation starts at its own index, so that one tick's burst divides over the lanes.
                    let counter = self.spread.entry((sender, exit.get())).or_insert(lane);
                    let chosen = *counter % target_lanes.max(1);
                    *counter = counter.wrapping_add(1);
                    chosen
                }
            };
            if self.holding && matches!(target, FrameTarget::Slot(_)) {
                self.held.push_back((target, target_lane, buffer, arrival));
            } else {
                self.deliver(Some(target), target_lane, buffer, arrival, Some(sender));
            }
        }
    }

    /// Delivers a buffer to lane `lane` of its target, or, between lanes, puts it on its path from `sender` when the
    /// delivery schedule holds it there.
    fn deliver(
        &mut self, target: Option<FrameTarget>, lane: u16, buffer: EventsBuffer, arrival: AdmissionTime,
        sender: Option<usize>,
    ) {
        // An encode host delivers each buffer to the output its lease names, as the encode host's front task does.
        let target = match target {
            Some(FrameTarget::EncodeHost(host)) if !buffer.is_empty() => {
                let lease = buffer
                    .header()
                    .entry
                    .expect("a batch to an encode host names its lease");
                let output = self.leases.output(lease).unwrap_or_else(|| {
                    panic!("a batch to the encode host of {} names unknown lease {:?}", host, lease)
                });
                Some(FrameTarget::Destination(output.clone()))
            }
            // Header-only progress carries nothing to deliver.
            Some(FrameTarget::EncodeHost(_)) => None,
            target => target,
        };
        match target {
            None | Some(FrameTarget::EncodeHost(_)) => {}
            Some(FrameTarget::Destination(output)) => {
                let header = buffer.header().clone();
                for event in buffer {
                    let metric = event.try_into_metric().expect("outputs receive metrics");
                    let accepted = self.accepted.entry(output.clone()).or_default();
                    if self
                        .options
                        .output_capacity
                        .is_some_and(|capacity| *accepted >= capacity)
                    {
                        self.refused.insert(&output, &metric);
                        continue;
                    }
                    *accepted += 1;
                    self.outputs.insert(&output, &metric);
                    self.delivered.push(DeliveredRecord {
                        output: output.clone(),
                        admitted_at: header.admitted_at,
                        admission_point: header.admission_point,
                        epoch: header.epoch,
                        generation: header.generation_id(),
                        metric,
                    });
                }
            }
            Some(FrameTarget::Slot(slot)) => {
                if let Some(sender) = sender {
                    let path = Path { sender, slot, lane };
                    if let Some(buffer) = self.delivery.hold(path, buffer, arrival) {
                        self.arrive(slot, lane, buffer, arrival);
                    }
                } else {
                    self.arrive(slot, lane, buffer, arrival);
                }
            }
            Some(FrameTarget::Fixed(node)) => {
                let admitted_at = buffer.header().admitted_at;
                let events = buffer.into_iter().filter_map(Event::try_into_metric).collect();
                let (host, ports) = self.hosts.get_mut(&node).expect("every fixed target has a host");
                let mut output = HostOutput::new(*ports);
                host.admit(
                    HostBatch {
                        admitted_at,
                        arrived_at: arrival,
                        events,
                    },
                    &mut output,
                );
                self.readmit(&node, output, arrival);
            }
        }
    }

    /// Runs a buffer that arrived at lane `lane` of `slot`: recorded, if the slot is a state slot whose lanes the
    /// harness records, and processed.
    fn arrive(&mut self, slot: SlotId, lane: u16, buffer: EventsBuffer, arrival: AdmissionTime) {
        let arrivals = self.arrivals.entry((slot, lane)).or_default();
        if buffer.is_empty() {
            arrivals.1 += 1;
        } else {
            arrivals.0 += 1;
        }
        let index = self.driver_index(slot, lane);
        let observed = self
            .recorder
            .as_mut()
            .and_then(|recorder| recorder.observe(index, &self.frame, &buffer));
        self.call(slot, lane, Call::Process(buffer), arrival);
        if let Some(recorder) = &mut self.recorder {
            recorder.processed(observed);
        }
    }

    /// Delivers what the schedule lets through of what is in flight between lanes: in trace order nothing waits; a
    /// seeded schedule delivers a random part of it, the paths interleaved at random; a delay delivers what the time
    /// reached.
    fn pump(&mut self) {
        if matches!(self.delivery.schedule, DeliverySchedule::Seeded(_)) {
            let mut budgets = self.delivery.seeded_budgets();
            while let Some((path, buffer, arrival)) = self.delivery.take_budgeted(&mut budgets) {
                self.arrive(path.slot, path.lane, buffer, arrival);
            }
            return;
        }
        loop {
            let Some((path, buffer, arrival)) = self.delivery.next() else {
                return;
            };
            self.arrive(path.slot, path.lane, buffer, arrival);
        }
    }

    /// Delivers everything in flight between lanes, path by path in the schedule's order, as everything sent before a
    /// region's input closes arrives before it finishes.
    fn flush_paths(&mut self) {
        while let Some((path, buffer, arrival)) = self.delivery.take_any() {
            self.arrive(path.slot, path.lane, buffer, arrival);
        }
    }

    /// Delivers what a seeded schedule held across [`SEEDED_TICKS_HELD`] advances, in a random order of its paths.
    fn flush_waited(&mut self) {
        let before = self.delivery.advances.saturating_sub(SEEDED_TICKS_HELD);
        while let Some((path, buffer, arrival)) = self.delivery.take_sent_before(before) {
            self.arrive(path.slot, path.lane, buffer, arrival);
        }
    }

    /// Returns what every output received.
    pub fn outputs(&self) -> &OutputPoints {
        &self.outputs
    }

    /// Returns every record the outputs received, in delivery order, with the headers of their buffers.
    pub fn delivered(&self) -> &[DeliveredRecord] {
        &self.delivered
    }

    /// Returns what every output received and refused.
    pub fn capacity_outcome(&self) -> CapacityOutcome {
        CapacityOutcome {
            delivered: self.outputs.clone(),
            refused: self.refused.clone(),
        }
    }

    /// Returns the recorded telemetry.
    pub fn telemetry(&self) -> &RecordingTelemetry {
        &self.telemetry
    }

    /// Returns the `drop` nodes that run in a slot, whose loss a region counts.
    pub fn drops_in_slots(&self, plan: &Plan) -> Vec<NodeId> {
        plan.nodes
            .iter()
            .filter(|node| node.op.as_str() == names::DROP)
            .filter(|node| matches!(self.placement.site(&node.id), Some(Site::Slot(_))))
            .map(|node| node.id.clone())
            .collect()
    }

    /// Returns the drivers of every lane of every slot, slot by slot, lane by lane: one per slot without lanes.
    pub fn drivers(&self) -> &[MetricRegionDriver] {
        &self.drivers
    }

    /// Returns the driver of the slot named `name`: of its first lane.
    ///
    /// # Panics
    ///
    /// Panics if the frame has no such slot.
    pub fn driver(&self, name: &str) -> &MetricRegionDriver {
        self.lane_driver(name, 0)
    }

    /// Returns the driver of lane `lane` of the slot named `name`.
    ///
    /// # Panics
    ///
    /// Panics if the frame has no such slot, or the slot no such lane.
    pub fn lane_driver(&self, name: &str, lane: u16) -> &MetricRegionDriver {
        let slot = self.slot_named(name);
        assert!(lane < self.lanes_of(slot), "slot `{}` has no lane {}", name, lane);
        &self.drivers[self.driver_index(slot, lane)]
    }

    /// Returns the slot named `name`.
    ///
    /// # Panics
    ///
    /// Panics if the frame has no such slot.
    pub fn slot_named(&self, name: &str) -> SlotId {
        self.frame
            .slots()
            .iter()
            .find(|slot| slot.name.as_str() == name)
            .unwrap_or_else(|| panic!("the frame has no slot `{}`", name))
            .id
    }

    /// Returns what the shared parts of a slot's lanes hold: their lineage records and panic ledger.
    pub fn slot_lanes(&self, slot: SlotId) -> &Arc<SlotLanes> {
        &self.slot_lanes[slot.get()]
    }
}

/// A call the harness makes into a region.
enum Call {
    Process(EventsBuffer),
    Advance,
    Finish,
}

/// Returns the context a harness builds a kernel in: detached, with the host's state budgets.
fn context(budgets: &StateBudgets, node: &NodeId) -> KernelBuildContext {
    KernelBuildContext::detached(node).with_state_budget(budgets.context_budget(node))
}

/// Returns the bindings of the state owners of a slot's candidate program, against the active one: a state owner of a
/// lineage the active program has retains its instance, any other is new, and an active state owner whose lineage the
/// candidate does not keep retires.
fn state_bindings(active: &RegionProgram, candidate: &RegionProgram, from: GenerationId) -> Vec<StateBinding> {
    let lineages = |program: &RegionProgram| {
        program
            .kernels()
            .iter()
            .filter_map(|kernel| kernel.lineage.as_ref().map(|lineage| lineage.key.clone()))
            .collect::<Vec<_>>()
    };
    let (old, new) = (lineages(active), lineages(candidate));
    let mut bindings = new
        .iter()
        .map(|key| {
            if old.contains(key) {
                StateBinding::Retain {
                    node: key.node.clone(),
                    key: key.clone(),
                    from,
                }
            } else {
                StateBinding::New {
                    node: key.node.clone(),
                    key: key.clone(),
                }
            }
        })
        .collect::<Vec<_>>();
    let cut = bindings
        .iter()
        .any(|binding| matches!(binding, StateBinding::New { .. }))
        || old.iter().any(|key| !new.contains(key));
    if cut {
        bindings.extend(
            old.iter()
                .filter(|key| !new.contains(key))
                .map(|key| StateBinding::Retire {
                    node: key.node.clone(),
                    key: key.clone(),
                    from,
                }),
        );
    }
    bindings
}

/// Returns the slots of a frame in an order in which every slot comes after the slots whose exits lead to it.
fn topological_slots(frame: &Frame) -> Vec<SlotId> {
    let slots = frame.slots();
    let mut incoming = vec![0usize; slots.len()];
    for slot in slots {
        for exit in &slot.exits {
            if let FrameTarget::Slot(next) = exit.target {
                incoming[next.get()] += 1;
            }
        }
    }
    let mut ready = slots
        .iter()
        .filter(|slot| incoming[slot.id.get()] == 0)
        .map(|slot| slot.id)
        .collect::<VecDeque<_>>();
    let mut order = Vec::with_capacity(slots.len());
    while let Some(slot) = ready.pop_front() {
        order.push(slot);
        for exit in &slots[slot.get()].exits {
            if let FrameTarget::Slot(next) = exit.target {
                incoming[next.get()] -= 1;
                if incoming[next.get()] == 0 {
                    ready.push_back(next);
                }
            }
        }
    }
    order
}

fn buffer_of(header: BatchHeader, metrics: Vec<Metric>) -> EventsBuffer {
    let mut buffer = EventsBuffer::with_capacity(metrics.len()).with_header(header);
    for metric in metrics {
        assert!(
            buffer.try_push(Event::Metric(metric)).is_none(),
            "harness batches fit one buffer"
        );
    }
    buffer
}

/// An opaque host that forwards everything it admits unchanged, as an opaque native filter that keeps everything
/// would.
struct EchoHost;

impl OpaqueHostAdapter for EchoHost {
    fn admit(&mut self, batch: HostBatch, output: &mut HostOutput) {
        for metric in batch.events {
            output.push(saluki_core::execution::PortIndex::PRIMARY, metric);
        }
    }
}

impl HarnessOptions {
    /// Returns whether programs are compiled with the decision-time rewrites: always, outside this crate's own tests.
    fn rewrites_decisions(&self) -> bool {
        #[cfg(test)]
        return self.decision_rewrites;
        #[cfg(not(test))]
        return true;
    }

    /// Returns whether programs are compiled with R5b: always, outside this crate's own tests.
    fn pushes_filters(&self) -> bool {
        #[cfg(test)]
        return self.pushed_filters;
        #[cfg(not(test))]
        return true;
    }
}

/// Compiles a plan into a frame, with or without the decision-time rewrites.
fn compiled_with(
    plan: &Plan, report: &ValidationReport, registry: &OperatorRegistry, frame: &Frame, options: HarnessOptions,
) -> CompiledPlan {
    let compiled = if !options.pushes_filters() {
        without_pushed_filters(plan, report, registry, frame)
    } else if options.rewrites_decisions() {
        compile(plan, report, registry, frame)
    } else {
        without_decision_rewrites(plan, report, registry, frame)
    };
    compiled.expect("the plan compiles into the frame")
}

#[cfg(test)]
fn without_pushed_filters(
    plan: &Plan, report: &ValidationReport, registry: &OperatorRegistry, frame: &Frame,
) -> Result<CompiledPlan, crate::compile::CompileError> {
    crate::compile::compile_without_pushed_filters(plan, report, registry, frame)
}

/// Outside this crate's own tests, programs are always compiled with R5b.
#[cfg(not(test))]
fn without_pushed_filters(
    plan: &Plan, report: &ValidationReport, registry: &OperatorRegistry, frame: &Frame,
) -> Result<CompiledPlan, crate::compile::CompileError> {
    compile(plan, report, registry, frame)
}

#[cfg(test)]
fn without_decision_rewrites(
    plan: &Plan, report: &ValidationReport, registry: &OperatorRegistry, frame: &Frame,
) -> Result<CompiledPlan, crate::compile::CompileError> {
    crate::compile::compile_without_decision_rewrites(plan, report, registry, frame)
}

/// Outside this crate's own tests, programs are always compiled with the decision-time rewrites.
#[cfg(not(test))]
fn without_decision_rewrites(
    plan: &Plan, report: &ValidationReport, registry: &OperatorRegistry, frame: &Frame,
) -> Result<CompiledPlan, crate::compile::CompileError> {
    compile(plan, report, registry, frame)
}

/// How a [`LanedFrameHarness`] delivers what each lane sends to the lanes of the slots downstream.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeliverySchedule {
    /// In trace order: everything a call sends is delivered before the next trace event, the open routed buffers of
    /// every keyed exit flushed first at every tick.
    TraceOrder,

    /// Interleaved by a schedule seeded with this value. Every buffer one lane sends another waits on its path, one
    /// path per sending lane and receiving lane, which stays first in, first out; after each trace event each path
    /// delivers a random number of what waits on it, each next buffer with probability 3/4, the paths' deliveries
    /// interleaved in a random order, so that buffers wait across later events and ticks, as a busy channel holds
    /// them. A buffer waits across at most [`SEEDED_TICKS_HELD`] advances: what still waits from before that arrives,
    /// in a random order of the paths, before the advance's heartbeats, so that a lane falls at most that many ticks
    /// behind the others. The keyed exits' holds and progress throttle run on the regions' own clock, as they would.
    /// Everything still waiting arrives before a region finishes.
    Seeded(u64),

    /// In trace order, except that what lane `lane` of a slot receives from the admission point `point` waits until
    /// the point has stamped `by` after its admission time, or in a later epoch, as a slow channel holds it, with
    /// everything sent after it on its path behind it. What still waits arrives before the region finishes.
    Delay {
        /// The admission point whose batches are delayed.
        point: AdmissionPointId,

        /// The receiving lane.
        lane: u16,

        /// How much later, in admission time.
        by: Duration,
    },

    /// As [`Delay`][Self::Delay], but only for what the point stamps in `epoch` or a newer epoch of its clock: the
    /// channel slows from the point's reset on, so that the state lane learns of the reset after the others.
    DelayFrom {
        /// The admission point whose batches are delayed.
        point: AdmissionPointId,

        /// The receiving lane.
        lane: u16,

        /// The oldest epoch of the point's clock whose batches are delayed.
        epoch: u32,

        /// How much later, in admission time.
        by: Duration,
    },
}

impl DeliverySchedule {
    /// Returns the point, lane, delay, and oldest epoch delayed of a schedule that delays one point's path to one lane.
    fn delayed(self) -> Option<(AdmissionPointId, u16, Duration, Option<u32>)> {
        match self {
            Self::Delay { point, lane, by } => Some((point, lane, by, None)),
            Self::DelayFrom { point, lane, epoch, by } => Some((point, lane, by, Some(epoch))),
            Self::TraceOrder | Self::Seeded(_) => None,
        }
    }
}

/// The most advances a buffer waits across on its path under a [seeded schedule][DeliverySchedule::Seeded].
pub const SEEDED_TICKS_HELD: u64 = 1;

/// One path between lanes: from the driver of a sending lane, to lane `lane` of `slot`.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct Path {
    sender: usize,
    slot: SlotId,
    lane: u16,
}

/// A buffer waiting on its path, and, if it is delayed, the stamp of the delayed point that lets it through: an
/// admission time in an epoch.
struct InFlight {
    buffer: EventsBuffer,
    arrival: AdmissionTime,
    ready_at: Option<(AdmissionTime, u32)>,
    /// The advances that had happened when the buffer was sent.
    sent: u64,
}

/// How buffers between lanes are delivered: the schedule, and what waits on each path.
struct Delivery {
    schedule: DeliverySchedule,
    paths: BTreeMap<Path, VecDeque<InFlight>>,
    /// The state of the seeded schedule's `xorshift64*` generator.
    rng: u64,
    /// The delayed point's latest stamp, in its epoch.
    delayed: Option<(AdmissionTime, u32)>,
    /// The advances so far.
    advances: u64,
}

impl Delivery {
    fn new(schedule: DeliverySchedule) -> Self {
        let seed = match schedule {
            DeliverySchedule::Seeded(seed) => seed,
            _ => 0,
        };
        Self {
            schedule,
            paths: BTreeMap::new(),
            // A zero state never leaves zero; any other seed gives the same sequence for the same seed.
            rng: seed ^ 0x9E37_79B9_7F4A_7C15,
            delayed: None,
            advances: 0,
        }
    }

    /// Counts an advance of time.
    fn advanced(&mut self) {
        self.advances += 1;
    }

    /// Records a stamp of an admission point: the delayed point's moves what its delay lets through.
    fn stamped(&mut self, header: &BatchHeader) {
        if let (Some((point, ..)), Some(at)) = (self.schedule.delayed(), header.admitted_at) {
            if header.admission_point == Some(point) {
                self.delayed = Some(match self.delayed {
                    Some((latest, epoch)) if epoch == header.epoch => (latest.max(at), epoch),
                    _ => (at, header.epoch),
                });
            }
        }
    }

    /// Returns `true` if the delayed point's latest stamp lets a buffer that waits until `ready_at` through.
    fn ready(&self, ready_at: Option<(AdmissionTime, u32)>) -> bool {
        match (ready_at, self.delayed) {
            (None, _) => true,
            (Some(_), None) => false,
            (Some((at, epoch)), Some((latest, current))) => {
                (current == epoch && latest >= at) || crate::runtime::state::rules::epoch_newer(current, epoch)
            }
        }
    }

    fn draw(&mut self) -> u64 {
        let mut x = self.rng;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.rng = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Returns `buffer` if it is delivered now, or puts it on its path, behind what waits there.
    fn hold(&mut self, path: Path, buffer: EventsBuffer, arrival: AdmissionTime) -> Option<EventsBuffer> {
        let ready_at = match self.schedule {
            DeliverySchedule::TraceOrder => None,
            DeliverySchedule::Seeded(_) => None,
            DeliverySchedule::Delay { .. } | DeliverySchedule::DelayFrom { .. } => {
                let (point, lane, by, from) = self.schedule.delayed().expect("a delay");
                let header = buffer.header();
                let in_epoch = from.is_none_or(|from| header.epoch == from || epoch_newer(header.epoch, from));
                (path.lane == lane && header.admission_point == Some(point) && in_epoch).then(|| {
                    (
                        header
                            .admitted_at
                            .unwrap_or(AdmissionTime::UNIX_EPOCH)
                            .saturating_add(by),
                        header.epoch,
                    )
                })
            }
        };
        let queue = self.paths.entry(path).or_default();
        let waits = match self.schedule {
            DeliverySchedule::TraceOrder => !queue.is_empty(),
            DeliverySchedule::Seeded(_) => true,
            DeliverySchedule::Delay { .. } | DeliverySchedule::DelayFrom { .. } => {
                ready_at.is_some() || !queue.is_empty()
            }
        };
        if !waits {
            return Some(buffer);
        }
        queue.push_back(InFlight {
            buffer,
            arrival,
            ready_at,
            sent: self.advances,
        });
        None
    }

    /// Returns the next buffer the schedule lets through now, with its path.
    fn next(&mut self) -> Option<(Path, EventsBuffer, AdmissionTime)> {
        match self.schedule {
            DeliverySchedule::TraceOrder => self.take_any(),
            // A seeded schedule delivers by the budgets it draws for each pump.
            DeliverySchedule::Seeded(_) => None,
            DeliverySchedule::Delay { .. } | DeliverySchedule::DelayFrom { .. } => {
                let path = self
                    .paths
                    .iter()
                    .find(|(_, queue)| queue.front().is_some_and(|front| self.ready(front.ready_at)))
                    .map(|(path, _)| *path)?;
                let item = self.paths.get_mut(&path)?.pop_front()?;
                Some((path, item.buffer, item.arrival))
            }
        }
    }

    /// Draws, for every path with buffers waiting, how many of them a seeded schedule delivers now: each next one with
    /// probability 3/4, so that a buffer sometimes waits across the next events and ticks.
    fn seeded_budgets(&mut self) -> BTreeMap<Path, usize> {
        let waiting = self
            .paths
            .iter()
            .filter(|(_, queue)| !queue.is_empty())
            .map(|(path, queue)| (*path, queue.len()))
            .collect::<Vec<_>>();
        let mut budgets = BTreeMap::new();
        for (path, waits) in waiting {
            let mut budget = 0;
            while budget < waits && !self.draw().is_multiple_of(4) {
                budget += 1;
            }
            if budget > 0 {
                budgets.insert(path, budget);
            }
        }
        budgets
    }

    /// Takes the front buffer of a random path with budget left, spending one of its budget.
    fn take_budgeted(&mut self, budgets: &mut BTreeMap<Path, usize>) -> Option<(Path, EventsBuffer, AdmissionTime)> {
        budgets.retain(|_, budget| *budget > 0);
        if budgets.is_empty() {
            return None;
        }
        let index = (self.draw() % budgets.len() as u64) as usize;
        let (path, budget) = budgets.iter_mut().nth(index)?;
        *budget -= 1;
        let path = *path;
        let item = self.paths.get_mut(&path)?.pop_front()?;
        Some((path, item.buffer, item.arrival))
    }

    /// Takes a buffer that waits, whatever its delay: from a random path for a seeded schedule, else from the first.
    fn take_any(&mut self) -> Option<(Path, EventsBuffer, AdmissionTime)> {
        self.take_sent_before(u64::MAX)
    }

    /// Takes a buffer that waits since before `advances` advances had happened, as [`take_any`][Self::take_any]
    /// takes one: on its path, everything before it waited longer still.
    fn take_sent_before(&mut self, advances: u64) -> Option<(Path, EventsBuffer, AdmissionTime)> {
        let waiting = self
            .paths
            .iter()
            .filter(|(_, queue)| queue.front().is_some_and(|front| front.sent < advances))
            .map(|(path, _)| *path)
            .collect::<Vec<_>>();
        let path = match self.schedule {
            DeliverySchedule::Seeded(_) if !waiting.is_empty() => {
                let index = (self.draw() % waiting.len() as u64) as usize;
                waiting[index]
            }
            _ => *waiting.first()?,
        };
        let item = self.paths.get_mut(&path)?.pop_front()?;
        Some((path, item.buffer, item.arrival))
    }
}

/// What a recorder knows of one state lane: its lane, and its model of the shared time rules, from the headers it
/// received, which places its ticks among what it received.
struct StateLaneModel {
    lane: u16,
    tracker: WatermarkTracker,
    /// The epoch every upstream point has switched to at the lane.
    epoch: u32,
    /// The newest epoch of a header the lane received: newer than `epoch` while the lane's reset window is open.
    newest: u32,
    /// The tick waiting at the gate: its ordinal, its wall-clock second, and when its grace ends.
    gated: Option<(u64, u64, AdmissionTime)>,
    last: Option<AdmissionTime>,
    /// The newest ordinal the lane resolved, with a flush or without.
    resolved: u64,
}

impl StateLaneModel {
    /// Returns `true` while the lane's reset window is open.
    fn in_window(&self) -> bool {
        self.newest != self.epoch
    }
}

/// What a state lane's driver reports of its time, as it happens: each tick that took effect, each reset drain, each
/// crossing at a cut, and each continuation of a lineage past a reset window, for the arrival replay and the
/// properties on lanes ([`FrameHarness::time_events`]).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LaneTimeEvent {
    /// The lane's instance of the lineage created by `created_by`, holding data of `epoch`, fired the tick of
    /// `ordinal`, due at `scheduled`, flushing at `flush`. A slot on one lane has no ordinals.
    Ticked {
        /// The generation that created the lineage's instance.
        created_by: GenerationId,
        /// The epoch of the data the instance holds.
        epoch: u32,
        /// The tick's ordinal in the lineage's record shared by the lanes.
        ordinal: Option<u64>,
        /// When the tick was due on the regions' monotonic clock.
        scheduled: Instant,
        /// The flush time.
        flush: AdmissionTime,
    },

    /// The lane's instance ran its reset drain, at `pre`, the watermark of `epoch` the closing window gave it.
    ResetDrained {
        /// The generation that created the lineage's instance.
        created_by: GenerationId,
        /// The epoch the window closed.
        epoch: u32,
        /// The flush time of the drain: the epoch's final watermark, if any point stamped in it.
        pre: Option<AdmissionTime>,
    },

    /// The lane's instance made its final flush at its cut, at `at`.
    Crossed {
        /// The generation that created the lineage's instance.
        created_by: GenerationId,
        /// The epoch of the data the instance held.
        epoch: u32,
        /// The cut's time.
        at: AdmissionTime,
    },

    /// The lane's drained instance of epoch `from` continued its lineage in `epoch`, joining the epoch's record: its
    /// ticks resume at `resume_at`, the old record's next instant, and the record gave it `next_at` as its next
    /// deadline.
    Continued {
        /// The generation that created the lineage's instance.
        created_by: GenerationId,
        /// The epoch the drained instance held.
        from: u32,
        /// The epoch the lineage continues in.
        epoch: u32,
        /// The instant the lineage's ticks resume at: the old record's next instant.
        resume_at: Option<Instant>,
        /// The next deadline the successor record gave the lane.
        next_at: Option<Instant>,
    },
}

/// Takes what a region's driver reported of its state lane's time since it was last asked.
fn take_time_events(driver: &mut MetricRegionDriver) -> Vec<LaneTimeEvent> {
    driver.take_time_events()
}

/// The recorder of what every lane of a frame's state slot observes, for an [arrival replay][ArrivalReplay].
///
/// It records each buffer a state lane receives, with its header, as it receives it, and each tick the lane's driver
/// reports it fired ([`LaneTimeEvent`]), before the buffer whose processing fired it: a tick a header releases fires
/// before its batch is admitted.
///
/// It also keeps a model of the lanes' ticks, from the shared [time rules][crate::runtime::state::rules], in a replay
/// of its own: one tick per [`advance`][FrameHarness::advance], at the advance's second, fired once the lane's
/// watermark reached it, before the first header that raised it there if it gated. That is the lanes' own schedule when
/// their lineage flushes every second and the advances come a second apart, as the traces that compare with replays
/// arrange. A lane fires no tick while its reset window is open, and the first window to open over an epoch marks it as
/// the lanes' shared reset mark does: no ordinal of that epoch after the newest one any lane had resolved then fires at
/// any lane. A tick whose grace expired, or that a gated lane dropped at a reset, excludes the model's replay. The
/// model is a cross-check of the lanes ([`FrameHarness::replay_disagreement`]), and the replay of a driver that reports
/// nothing. The recorder records the lineage the boot plan starts; a tick of another one, after a cut, excludes the
/// lanes' replay, which runs one lineage.
struct LaneRecorder {
    /// The replay with the ticks the lanes reported.
    reported: ArrivalReplay,
    /// The replay with the ticks the model placed.
    model: ArrivalReplay,
    /// Whether any lane reported a tick.
    reports: bool,
    lanes: BTreeMap<usize, StateLaneModel>,
    gate: TickGate,
    advances: u64,
    /// The reset mark: the epoch the first window to open over it marked, and the cutoff, the newest ordinal any lane
    /// had resolved then.
    mark: Option<(u32, u64)>,
    /// Per lane and epoch, the ticks the lane reported, for a slot on one lane, which has no ordinals.
    counted: BTreeMap<(u16, u32), u64>,
}

impl LaneRecorder {
    /// Records the lanes of the frame's first state slot on lanes, starting the replay with the boot plan's
    /// installation.
    fn new(harness: &FrameHarness, boot: &Plan) -> Option<Self> {
        let slot = harness.frame.slots().iter().find(|slot| slot.is_isolation())?;
        let SlotKind::Isolation { upstream, .. } = &slot.kind else {
            return None;
        };
        let lanes = (0..slot.lanes)
            .map(|lane| {
                (
                    harness.driver_index(slot.id, lane),
                    StateLaneModel {
                        lane,
                        tracker: WatermarkTracker::new(upstream.iter().copied()),
                        epoch: 0,
                        newest: 0,
                        gated: None,
                        last: None,
                        resolved: 0,
                    },
                )
            })
            .collect();
        let prelude = TraceEvent::Install {
            plan: Arc::new(boot.clone()),
        };
        let replay = ArrivalReplay::new(slot.lanes).with_prelude([prelude]);
        Some(Self {
            reported: replay.clone(),
            model: replay,
            reports: false,
            lanes,
            gate: TickGate::default(),
            advances: 0,
            mark: None,
            counted: BTreeMap::new(),
        })
    }

    /// Records what the driver at index `index` reported it did with its time, if it is a lane the recorder records:
    /// each tick of the boot plan's lineage, in the lanes' replay.
    fn report(&mut self, index: usize, events: &[LaneTimeEvent]) {
        let Some(lane) = self.lanes.get(&index).map(|model| model.lane) else {
            return;
        };
        for event in events {
            let LaneTimeEvent::Ticked {
                created_by,
                epoch,
                ordinal,
                flush,
                ..
            } = *event
            else {
                continue;
            };
            self.reports = true;
            if created_by != GenerationId::new(1) {
                self.reported.exclude(format!(
                    "state lane {} ticked a lineage generation {} created, and a replay runs one lineage",
                    lane, created_by
                ));
                continue;
            }
            let ordinal = ordinal.unwrap_or_else(|| {
                let counted = self.counted.entry((lane, epoch)).or_default();
                *counted += 1;
                *counted
            });
            self.reported
                .record(lane, LaneObservation::Ticked { epoch, ordinal, flush });
        }
    }

    /// Records, in the lanes' replay, what a lane observed of a buffer it received, as [`observe`][Self::observe]
    /// returned it, once the buffer's processing ran and the ticks it fired were reported: they go before it.
    fn processed(&mut self, observed: Option<(u16, LaneObservation)>) {
        if let Some((lane, observation)) = observed {
            self.reported.record(lane, observation);
        }
    }

    /// Returns the replay of the ticks the lanes reported, or of the model's if no lane reported any.
    fn replay(&self) -> &ArrivalReplay {
        if self.reports {
            &self.reported
        } else {
            &self.model
        }
    }

    /// Records that the execution's input ended.
    fn close(&mut self) {
        self.reported.close();
        self.model.close();
    }

    /// Returns `true` if the reset mark keeps the tick of `ordinal` in `epoch` from firing.
    fn marked(&self, epoch: u32, ordinal: u64) -> bool {
        self.mark
            .is_some_and(|(marked, cutoff)| marked == epoch && ordinal > cutoff)
    }

    /// Records a buffer the lane at driver index `index` receives in the model's replay, after the gated tick the model
    /// says it releases, and returns the lane and the observation, for the lanes' replay once its processing ran.
    fn observe(&mut self, index: usize, frame: &Frame, buffer: &EventsBuffer) -> Option<(u16, LaneObservation)> {
        let resolved = self.lanes.values().map(|model| model.resolved).max().unwrap_or(0);
        let mark = self.mark;
        let model = self.lanes.get_mut(&index)?;
        let header = buffer.header();
        let point = header
            .admission_point
            .and_then(|point| frame.admission_point(point).map(|info| (point, info)))?;
        if epoch_newer(header.epoch, model.newest) {
            if !model.in_window() && mark.is_none_or(|(marked, _)| epoch_newer(model.epoch, marked)) {
                self.mark = Some((model.epoch, resolved));
            }
            model.newest = header.epoch;
        }
        match model.tracker.observe(header) {
            Observation::EpochSwitched { .. } => {
                if model.gated.take().is_some() {
                    self.model.exclude(format!(
                        "state lane {} dropped a tick waiting at its gate when epoch {} opened a reset window",
                        model.lane, header.epoch
                    ));
                }
                if model.tracker.switched_epoch(header.epoch) {
                    model.epoch = header.epoch;
                    model.last = None;
                }
            }
            Observation::Fault => {
                self.model.exclude(format!(
                    "state lane {} received a header its tracker rejects",
                    model.lane
                ));
                return None;
            }
            Observation::Ok => {
                if model.epoch != header.epoch && model.tracker.switched_epoch(header.epoch) {
                    model.epoch = header.epoch;
                    model.last = None;
                }
            }
        }
        if let Some((ordinal, second, _)) = model.gated {
            if let Some(flush) = self
                .gate
                .on_watermark(second, model.tracker.watermark_in(model.epoch), model.last)
            {
                model.gated = None;
                model.resolved = model.resolved.max(ordinal);
                // A flush after the reset mark's cutoff does not take effect: the lane resolves it as a skip.
                let marked = self
                    .mark
                    .is_some_and(|(marked, cutoff)| marked == model.epoch && ordinal > cutoff);
                if !marked {
                    model.last = Some(flush);
                    self.model.record(
                        model.lane,
                        LaneObservation::Ticked {
                            epoch: model.epoch,
                            ordinal,
                            flush,
                        },
                    );
                }
            }
        }
        let (point, info) = point;
        let source = info.node.clone();
        let lane = info.lane_of(point).unwrap_or(0);
        let (admitted_at, epoch) = (header.admitted_at.unwrap_or(AdmissionTime::UNIX_EPOCH), header.epoch);
        let observation = if buffer.is_empty() {
            LaneObservation::Progress {
                source,
                lane,
                admitted_at,
                epoch,
            }
        } else {
            LaneObservation::Admitted {
                source,
                lane,
                admitted_at,
                epoch,
                batch: buffer
                    .into_iter()
                    .filter_map(|event| event.try_as_metric().cloned())
                    .collect(),
            }
        };
        self.model.record(model.lane, observation.clone());
        Some((model.lane, observation))
    }

    /// Starts the tick of one `advance`, which every state lane resolves.
    fn begin_tick(&mut self) {
        self.advances += 1;
    }

    /// Resolves the current tick at the lane at driver index `index`, at the wall-clock time `now`, as its deadline
    /// does: fired once its watermark reached `now`'s second, gated otherwise, and not at all inside a reset window.
    fn tick(&mut self, index: usize, now: AdmissionTime) {
        let ordinal = self.advances;
        let marked = self
            .lanes
            .get(&index)
            .is_some_and(|model| self.marked(model.epoch, ordinal));
        let Some(model) = self.lanes.get_mut(&index) else {
            return;
        };
        if model.in_window() {
            return;
        }
        if marked || !model.tracker.switched_epoch(model.epoch) || model.tracker.watermark_in(model.epoch).is_none() {
            model.resolved = ordinal;
            return;
        }
        if let Some((_, _, until)) = model.gated {
            if now >= until {
                model.gated = None;
                self.model.exclude(format!(
                    "the grace of a tick expired at state lane {}, which flushed at its own watermark",
                    model.lane
                ));
            }
            return;
        }
        match self.gate.due_at_second(
            true,
            model.tracker.watermark_in(model.epoch),
            now.as_unix_secs(),
            model.last,
        ) {
            TickDecision::Skip => model.resolved = ordinal,
            TickDecision::Gate { second } => {
                model.gated = Some((ordinal, second, now.saturating_add(self.gate.grace())));
            }
            TickDecision::Fire { flush } => {
                model.last = Some(flush);
                model.resolved = ordinal;
                self.model.record(
                    model.lane,
                    LaneObservation::Ticked {
                        epoch: model.epoch,
                        ordinal,
                        flush,
                    },
                );
            }
        }
    }
}

/// Returns, for each state owner of a slot's program, the cell its lineage's instances on every lane are built
/// against, creating the cell of a lineage the slot has none for: none for a slot on one lane.
fn lane_cells(
    cells: &mut BTreeMap<(SlotId, LineageKey), Arc<LaneCell>>, slot: &Slot, program: &RegionProgram,
) -> BTreeMap<NodeId, Arc<LaneCell>> {
    if slot.lanes <= 1 {
        return BTreeMap::new();
    }
    program
        .kernels()
        .iter()
        .filter_map(|kernel| {
            let lineage = kernel.lineage.as_ref()?.key.clone();
            let cell = cells.entry((slot.id, lineage)).or_default();
            Some((kernel.key.node.clone(), Arc::clone(cell)))
        })
        .collect()
}

/// Returns the context a harness builds a kernel of lane `lane` of `slot` in: detached, with the host's state budgets,
/// and, for a state owner on several lanes, against its lineage's cell.
fn lane_context(
    budgets: &StateBudgets, cells: &BTreeMap<NodeId, Arc<LaneCell>>, slot: &Slot, node: &NodeId, lane: u16,
) -> KernelBuildContext {
    let context = context(budgets, node);
    match cells.get(node) {
        Some(cell) => context.with_lanes(LaneBinding {
            cell: Arc::clone(cell),
            lane,
            lanes: slot.lanes,
        }),
        None => context,
    }
}

/// A harness that runs a whole frame of region drivers whose slots run on lanes, as a [`FrameHarness`] runs one
/// without lanes, delivering between lanes as its [schedule][DeliverySchedule] says.
///
/// The frame is laid out on the lane policy without pipelining, as lanes need. Every lane of a slot runs a driver of
/// its own, built for its lane (`RegionConfig::with_lane`), its state owners' kernels against one cell per lineage; the
/// lanes of a point stamp by one clock; points and exits reach their targets' lanes by their routing (fixed, keyed,
/// affinity, or spread). A harness dereferences to its [`FrameHarness`], whose operations it runs on lanes:
/// [`admit_at_lane`][FrameHarness::admit_at_lane] and [`heartbeat_at_lane`][FrameHarness::heartbeat_at_lane] stamp at a
/// source's lane. It records what every lane of the frame's state slot observes, for an [arrival
/// replay][FrameHarness::replay], and, with [`HarnessOptions::record_budgets`], every operation on the state budgets,
/// for the [capacity check][FrameHarness::budget_traces].
pub struct LanedFrameHarness {
    inner: FrameHarness,
    lanes: LanePolicy,
    schedule: DeliverySchedule,
}

impl LanedFrameHarness {
    /// Computes the frame of a valid boot plan on `lanes`, without pipelining, and creates a driver for each lane of
    /// each slot, prepared for generation 1, compiled and run as `options` say, started at the wall-clock time `start`,
    /// delivering as `schedule` says.
    ///
    /// # Errors
    ///
    /// Returns an error if the plan has no frame on `lanes`, if the frame lays a slot out on fewer lanes than `lanes`
    /// asks for, or if a lane's region cannot be created, saying why.
    ///
    /// # Panics
    ///
    /// Panics if the plan does not compile into its frame: a test bug.
    pub fn new(
        plan: &Plan, report: &ValidationReport, registry: OperatorRegistry, options: HarnessOptions,
        start: AdmissionTime, lanes: LanePolicy, schedule: DeliverySchedule,
    ) -> Result<Self, String> {
        let frame = Frame::compute_with(
            plan,
            &registry,
            &namer,
            &FrameHosts::new(),
            &lanes,
            FrameOptions { pipelining: false },
        )
        .map_err(|error| format!("the boot plan has no frame on {:?}: {}", lanes, error))?;
        check_realized_lanes(plan, &registry, &frame, &lanes)?;
        let mut inner = FrameHarness::on_frame(frame, plan, report, registry, options, start)?;
        inner.delivery = Delivery::new(schedule);
        inner.recorder = LaneRecorder::new(&inner, plan);
        Ok(Self { inner, lanes, schedule })
    }

    /// Returns the lanes the harness's frame runs on.
    pub fn lane_policy(&self) -> &LanePolicy {
        &self.lanes
    }

    /// Returns how the harness delivers between lanes.
    pub fn schedule(&self) -> DeliverySchedule {
        self.schedule
    }

    /// Returns the harness that runs the frame's lanes.
    pub fn one_lane(&mut self) -> &mut FrameHarness {
        &mut self.inner
    }

    /// Returns the harness that runs the frame's lanes, for a test that drives it as a [`FrameHarness`] does.
    pub fn into_harness(self) -> FrameHarness {
        self.inner
    }
}

/// Checks that a frame runs the lanes a harness asked for, so that a test on lanes never runs on fewer without saying
/// so: every isolation slot that only slots' exits feed, and whose state owners all partition their contexts, runs
/// `lanes.state` lanes, at least one isolation slot does when more than one state lane is asked for, and every slot
/// fed by a point with lanes runs that point's lanes.
///
/// A state owner without a partition key, or fed straight by an admission point, keeps one lane by the frame's rules:
/// a plan whose every state owner does has no state lanes, which a test that asks for them must not run silently.
fn check_realized_lanes(
    plan: &Plan, registry: &OperatorRegistry, frame: &Frame, lanes: &LanePolicy,
) -> Result<(), String> {
    let partitioned = |node: &NodeId| {
        plan.node(node)
            .and_then(|node| registry.get(node.op.as_str()))
            .is_some_and(|operator| operator.partition_key().is_some())
    };
    for slot in frame.slots() {
        let fed_by_exits = slot
            .inputs
            .iter()
            .all(|input| matches!(input, crate::compile::FrameSource::Exit { .. }));
        let owners_partition = slot.nodes.iter().all(partitioned);
        if slot.is_isolation() && fed_by_exits && owners_partition && slot.lanes != lanes.state {
            return Err(format!(
                "`{}` runs {} lanes, not the {} state lanes asked for",
                slot.name, slot.lanes, lanes.state
            ));
        }
        for input in &slot.inputs {
            if let crate::compile::FrameSource::Admission(id) = input {
                let point_lanes = frame.admission_point(*id).map_or(1, |point| point.lanes);
                if point_lanes > 1 && slot.lanes != point_lanes {
                    return Err(format!(
                        "`{}` runs {} lanes, where the point feeding it has {}",
                        slot.name, slot.lanes, point_lanes
                    ));
                }
            }
        }
    }
    let isolation = frame
        .slots()
        .iter()
        .filter(|slot| slot.is_isolation())
        .collect::<Vec<_>>();
    if lanes.state > 1 && !isolation.is_empty() && isolation.iter().all(|slot| slot.lanes == 1) {
        return Err(format!(
            "no isolation slot runs the {} state lanes asked for: `{}` runs one, its state owners fed straight by an \
             admission point or keeping no partition key",
            lanes.state, isolation[0].name
        ));
    }
    Ok(())
}

impl FrameHarness {
    /// Returns how many flush ticks the lanes of the frame's isolation slots resolved apart from their lineages'
    /// shared tick record so far, publishing first: the sum of every state lane's `plan_lane_tick_divergence_total`
    /// and `plan_lane_tick_straggler_total`, as its region publishes them to the recording handles it was given. The
    /// equivalences the lanes keep exclude such ticks (6.1, 6.2), whose only effect on what outputs receive is which
    /// idle buckets of a counter get zeros. A region given no lane handles records nothing, and this reads 0.
    pub fn ticks_resolved_apart(&mut self) -> u64 {
        self.lane_counters(&RESOLVED_APART)
    }

    /// Returns how many clock epochs the lanes of the frame's isolation slots closed at once so far, past the most a
    /// reset window keeps open, publishing first, as [`ticks_resolved_apart`][Self::ticks_resolved_apart] reads its
    /// counters: a later batch of such an epoch is lost, so neither trace-order exactness nor an arrival replay holds.
    pub fn reset_overflows(&mut self) -> u64 {
        self.lane_counters(&RESET_OVERFLOWS)
    }

    /// Returns the sum of the counters named `names` over every lane of the frame's isolation slots, publishing first.
    fn lane_counters(&mut self, names: &[&str]) -> u64 {
        self.publish();
        self.frame
            .slots()
            .iter()
            .filter(|slot| slot.is_isolation())
            .flat_map(|slot| (0..slot.lanes).map(move |lane| (slot.name.as_str(), lane)))
            .flat_map(|(region, lane)| names.iter().map(move |name| (region, *name, lane)))
            .map(|(region, name, lane)| self.telemetry.lane_value(region, name, lane))
            .sum()
    }

    /// Returns what every lane of the frame's state slot observed so far, as an [arrival replay][ArrivalReplay] that
    /// starts with the boot plan's installation, with the ticks the lanes reported, or, if they reported none, those
    /// the recorder's model placed: empty for a harness that does not record lanes, such as one created without lanes.
    pub fn replay(&self) -> ArrivalReplay {
        self.recorder
            .as_ref()
            .map_or_else(|| ArrivalReplay::new(0), |recorder| recorder.replay().clone())
    }

    /// Returns every operation on the host's state budgets so far, budget by budget, for
    /// [`check_capacity`][crate::equivalence::check_capacity]: the global budget, then each node's. Empty unless
    /// [`HarnessOptions::record_budgets`] is set.
    pub fn budget_traces(&self) -> Vec<(String, CapacityTrace)> {
        self.recordings
            .as_ref()
            .map(BudgetRecordings::traces)
            .unwrap_or_default()
    }

    /// Ends every region's input as [`close`][Self::close] does, but lets lane `i` of every slot finish at `now`
    /// plus `i` times `stagger`, the wall clock moving between them, as lanes of one component finish at their own
    /// times.
    pub fn close_staggered(&mut self, now: AdmissionTime, stagger: Duration) {
        self.release();
        self.wall.set(now);
        self.flush_paths();
        for slot in self.order.clone() {
            self.flush_paths();
            for lane in 0..self.lanes_of(slot) {
                let at = now.saturating_add(stagger * u32::from(lane));
                self.wall.set(at);
                self.call(slot, lane, Call::Finish, at);
            }
        }
        self.flush_paths();
        if let Some(recorder) = &mut self.recorder {
            recorder.close();
        }
    }

    /// Returns what lane `lane` of the slot named `name` reported of its time so far, in order: nothing for a driver
    /// that reports nothing.
    ///
    /// # Panics
    ///
    /// Panics if the frame has no such slot.
    pub fn time_events(&self, name: &str, lane: u16) -> Vec<LaneTimeEvent> {
        let index = self.driver_index(self.slot_named(name), lane);
        self.time_events
            .iter()
            .filter(|(driver, _)| *driver == index)
            .map(|(_, event)| *event)
            .collect()
    }

    /// Returns what every lane of the slot named `name` reported of its time so far, in the order the harness took it,
    /// each with its lane.
    ///
    /// # Panics
    ///
    /// Panics if the frame has no such slot.
    pub fn slot_time_events(&self, name: &str) -> Vec<(u16, LaneTimeEvent)> {
        let slot = self.slot_named(name);
        let first = self.driver_index(slot, 0);
        let lanes = usize::from(self.lanes_of(slot));
        self.time_events
            .iter()
            .filter(|(driver, _)| (first..first + lanes).contains(driver))
            .map(|(driver, event)| ((driver - first) as u16, *event))
            .collect()
    }

    /// Returns how the recorder's model of the lanes' ticks differs from the ticks the lanes reported, lane by lane, if
    /// it does and they reported any: each lane's ticks, by epoch and flush time, in order. A tick at the flush time of
    /// the lane's previous one closes nothing and adds no zeros, as a deadline a lane catches up on after a reset
    /// window held its instance does, so repeats of one flush time count once.
    ///
    /// The model keeps the shared time rules for one lineage that ticks every second, without catch-up, stragglers, or
    /// graces that expire; where the lanes count a tick they resolved apart, the model may differ from them.
    pub fn replay_disagreement(&self) -> Option<String> {
        let recorder = self.recorder.as_ref().filter(|recorder| recorder.reports)?;
        let ticks = |replay: &ArrivalReplay, lane: u16| {
            let mut ticks = replay
                .observations(lane)
                .iter()
                .filter_map(|observation| match observation {
                    LaneObservation::Ticked { epoch, flush, .. } => Some((*epoch, *flush)),
                    _ => None,
                })
                .collect::<Vec<_>>();
            ticks.dedup();
            ticks
        };
        recorder.lanes.values().find_map(|model| {
            let (modeled, reported) = (
                ticks(&recorder.model, model.lane),
                ticks(&recorder.reported, model.lane),
            );
            (modeled != reported).then(|| {
                format!(
                    "state lane {}: the model fires {:?}, the lane fired {:?}",
                    model.lane, modeled, reported
                )
            })
        })
    }
}

impl std::ops::Deref for LanedFrameHarness {
    type Target = FrameHarness;

    fn deref(&self) -> &FrameHarness {
        &self.inner
    }
}

impl std::ops::DerefMut for LanedFrameHarness {
    fn deref_mut(&mut self) -> &mut FrameHarness {
        &mut self.inner
    }
}
