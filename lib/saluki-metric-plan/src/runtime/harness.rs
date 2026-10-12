//! A frame of region drivers wired together in process, for tests: [`FrameHarness`].
//!
//! The harness runs every slot of a frame through a [`MetricRegionDriver`] and every fixed component other than a
//! source through an opaque-host adapter, synchronously and without channels. Admission points are modeled on the real
//! ones: each stamps with an [`AdmissionClock`], the clock-reset threshold of the limits table, and the generation the
//! schedule of the topology assigns, recording, as a real point does, whether it admitted to a cut's new generation. A
//! batch admitted at a source runs through every region and host it reaches, in the order the topology would deliver
//! it, and what reaches an output is recorded with its header. Buffers between slots can be held back, as a queue
//! between two regions holds them, and released later.
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
        CutBoundary, DrainPolicy, EntryId, ExitSink, GenerationId, GenerationRef, GenerationToken, LossClass,
        PanicOutcome, PanicSite, Progress, RegionDriver, RegionPanic, RegionTurn, ScheduleHandle, WallClock,
    },
    topology::EventsBuffer,
};
use saluki_metrics::{Counter, Gauge};

use super::{
    build_kernels, kernels::panic_message, telemetry::StateTelemetry, MetricRegionDriver, PanicBudget, PrepareRegion,
    RegionConfig, RegionControl, RegisterTelemetry, StateBudgets,
};
use crate::{
    compile::{
        compile, CompiledPlan, Frame, FrameHosts, FrameTarget, LeaseTable, Placement, RegionProgram, Site, SlotId,
    },
    equivalence::{CapacityOutcome, OutputPoints},
    explain::loss_class_name,
    install::{kernel_reuse, StateBinding},
    interpret::{HostBatch, HostOutput, HostTick, OpaqueHostAdapter},
    ir::{NativeComponentId, NodeId, OutputId, Plan, PortName},
    limits::values::{CLOCK_RESET_THRESHOLD, CUT_MARGIN},
    operators::{names, KernelBuildContext, OperatorRegistry},
    runtime::state::rules::{cut_time, lcm_secs},
    validate::ValidationReport,
};

/// Counters and gauges by component and tag.
type Recorded = BTreeMap<(String, String), Arc<AtomicU64>>;

/// Telemetry that records into counters and gauges a test can read, keyed by component and tag.
///
/// Counters are tagged `port:<port>` for events per port, `class:<class>` for loss, and, for an isolation slot's own
/// telemetry, by the metric's name. Gauges hold the bits of their `f64` value.
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
}

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

/// One admission point of the frame, stamping as a real one does.
struct ModelPoint {
    id: AdmissionPointId,
    mode: AdmissionMode,
    route: Option<FrameTarget>,
    feeds_state: bool,
    /// What the point's clock reads: the harness sets it before each stamp.
    wall: HarnessClock,
    clock: AdmissionClock,
    observed: Option<ObservedCut>,
}

impl ModelPoint {
    /// Records the cut to `new`, if the point has not seen it, and returns what it recorded.
    fn observe(&mut self, new: GenerationId) -> &mut ObservedCut {
        let epoch = self.clock.epoch();
        if self.observed.as_ref().is_none_or(|observed| observed.new != new) {
            self.observed = Some(ObservedCut {
                new,
                epoch_at_publish: epoch,
                admitted_to_new: false,
            });
        }
        self.observed.as_mut().expect("just recorded")
    }

    /// Stamps a header at `at` under `schedule`.
    fn stamp(&mut self, at: AdmissionTime, schedule: &AdmissionSchedule) -> BatchHeader {
        self.wall.set(at);
        let epoch_at_publish = match schedule {
            AdmissionSchedule::Steady(_) => self.clock.epoch(),
            AdmissionSchedule::Cut { new, .. } => self.observe(new.id()).epoch_at_publish,
        };
        let stamp = self.clock.stamp();
        let generation = schedule.generation_for(stamp.admitted_at, stamp.epoch, epoch_at_publish);
        if let AdmissionSchedule::Cut { new, .. } = schedule {
            if Arc::ptr_eq(generation, new) {
                self.observe(new.id()).admitted_to_new = true;
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

    /// Returns whether every later stamp of the point gets the new generation of the cut `schedule` holds.
    fn passed_cut(&self, schedule: &AdmissionSchedule) -> bool {
        match (schedule, &self.observed) {
            (AdmissionSchedule::Cut { new, at, .. }, Some(observed)) if observed.new == new.id() => {
                self.clock.latest().is_some_and(|latest| latest >= *at)
                    || self.clock.epoch() != observed.epoch_at_publish
            }
            _ => false,
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
    drivers: Vec<MetricRegionDriver>,
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
    last_tick: Option<AdmissionTime>,
    outputs: OutputPoints,
    delivered: Vec<DeliveredRecord>,
    accepted: BTreeMap<OutputId, usize>,
    refused: OutputPoints,
    telemetry: RecordingTelemetry,
    budgets: StateBudgets,
    holding: bool,
    held: VecDeque<(FrameTarget, EventsBuffer, AdmissionTime)>,
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
    /// The buffers each slot received: those with events, and header-only ones.
    arrivals: BTreeMap<SlotId, (usize, usize)>,
    /// The lease of every output a generation's plan declared.
    leases: HarnessLeases,
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
        let compiled = compiled_with(plan, report, &registry, &frame, options);
        let mut leases = HarnessLeases::default();
        let boot_leases = leases.table(plan);
        let active: GenerationRef = Arc::new(GenerationToken::new(GenerationId::new(1)));
        let schedule = AdmissionSchedule::Steady(Arc::clone(&active)).into_handle();
        let telemetry = RecordingTelemetry::default();
        let budgets = options
            .state_capacity
            .map_or_else(StateBudgets::unlimited, StateBudgets::new);
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
        let drivers = frame
            .slots()
            .iter()
            .zip(&compiled.programs)
            .map(|(slot, program)| {
                let keys = program.kernels().iter().map(|kernel| &kernel.key);
                let kernels = build_kernels(program, keys, |node| context(&budgets, node)).expect("the kernels build");
                let boot = PrepareRegion::new(
                    Arc::clone(&active),
                    Arc::clone(program),
                    kernels,
                    PanicBudget::DEFAULT_PROGRAM,
                    &telemetry,
                )
                .with_leases(Arc::clone(&boot_leases));
                let config = RegionConfig::for_slot(slot, &telemetry)
                    .with_memo_entries(options.memo_entries)
                    .with_schedule(Arc::clone(&schedule))
                    .with_wall_clock(Arc::new(wall.clone()))
                    .with_state_budgets(budgets.clone())
                    .with_failure_reporter(Arc::clone(&reporter));
                let mut driver = MetricRegionDriver::new(config, boot).expect("the region is created");
                let mut budget = options.turn_budget;
                driver.start(&mut RegionTurn::new(started, &mut budget));
                driver
            })
            .collect();
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
        let points = frame
            .admission_points()
            .iter()
            .map(|point| {
                let clock_wall = HarnessClock::new(start);
                let clock = AdmissionClock::with_wall_clock(CLOCK_RESET_THRESHOLD, Arc::new(clock_wall.clone()));
                (
                    point.id,
                    ModelPoint {
                        id: point.id,
                        mode: point.mode,
                        route: point.route.clone(),
                        feeds_state: point.feeds_state,
                        wall: clock_wall,
                        clock,
                        observed: None,
                    },
                )
            })
            .collect();
        let order = topological_slots(&frame);
        Self {
            options,
            registry,
            frame,
            placement: compiled.placement,
            programs: compiled.programs,
            drivers,
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
            holding: false,
            held: VecDeque::new(),
            readmit_as: None,
            order,
            failed: BTreeMap::new(),
            failures,
            resumes: 0,
            arrivals: BTreeMap::new(),
            leases,
        }
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
        while let Some((target, buffer, arrival)) = self.held.pop_front() {
            self.readmit_as = buffer.header().generation.clone();
            self.deliver(Some(target), buffer, arrival);
            self.readmit_as = None;
        }
    }

    /// Returns the number of buffers held between slots.
    pub fn held(&self) -> usize {
        self.held.len()
    }

    /// Prepares a candidate plan in every region, as the installer does before its barrier: each region gets the
    /// kernels it does not hold, built with the host's state budgets, and its state owners' bindings. A state owner of
    /// a lineage the active program has in its slot retains that instance; any other is new, and an active state owner
    /// whose lineage the candidate does not keep retires, which makes the installation a cut. Admission points keep
    /// stamping the active generation until [`commit`][Self::commit] or [`arm_cut`][Self::arm_cut].
    ///
    /// Returns whether the installation is a cut.
    ///
    /// # Panics
    ///
    /// Panics if the candidate does not compile into the frame, or a region refuses the preparation: a test bug.
    pub fn prepare(&mut self, plan: &Plan, report: &ValidationReport) -> bool {
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
        for (((driver, program), kernels), bindings) in self
            .drivers
            .iter_mut()
            .zip(&compiled.programs)
            .zip(&reuse)
            .zip(bindings)
        {
            // State owners that start a new instance are built even when an instance of their lineage is retiring.
            let mut keys = kernels.build.clone();
            for binding in &bindings {
                if let StateBinding::New { node, .. } = binding {
                    if let Some(kernel) = program.kernels().iter().find(|kernel| &kernel.key.node == node) {
                        if !keys.contains(&kernel.key) {
                            keys.push(kernel.key.clone());
                        }
                    }
                }
            }
            let built = build_kernels(program, &keys, |node| context(&self.budgets, node)).expect("the kernels build");
            let prepare = PrepareRegion::new(
                Arc::clone(&generation),
                Arc::clone(program),
                built,
                self.options.candidate_budget,
                &self.telemetry,
            )
            .with_state_bindings(bindings)
            .with_leases(Arc::clone(&leases));
            let mut budget = self.options.turn_budget;
            let mut sink = ExitSink::new(0);
            driver
                .control(
                    &mut RegionTurn::new(self.start + self.elapsed, &mut budget),
                    RegionControl::Prepare(Box::new(prepare)),
                    &mut sink,
                )
                .expect("every region prepares the candidate");
        }
        self.prepared = Some(Prepared { generation, compiled });
        cut
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
    /// [`CUT_MARGIN`] after the latest of those points' latest stamps and the wall clock, recorded on the generation
    /// with each point's epoch before the schedule publishes it. Returns the cut's time.
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
            .filter_map(|id| self.points[id].clock.latest())
            .fold(wall, AdmissionTime::max);
        let at = cut_time(now, lcm_secs(widths).expect("the widths line up"), CUT_MARGIN);
        let old = Arc::clone(&self.active);
        let epochs = affected.iter().map(|id| (*id, self.points[id].clock.epoch())).collect();
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

    /// Admits a batch at a source's `metrics` output, stamped at `at` by its admission point's clock.
    pub fn admit(&mut self, source: &str, at: AdmissionTime, metrics: Vec<Metric>) {
        let point = self.source_point(source);
        let header = self.stamp(point, at);
        let route = self.points[&point].route.clone();
        let arrival = self.last_tick.map_or(at, |tick| tick.max(at));
        self.deliver(route, buffer_of(header, metrics), arrival);
    }

    /// Admits a batch at a source's `metrics` output at the wall clock's time.
    pub fn admit_now(&mut self, source: &str, metrics: Vec<Metric>) {
        let at = self.wall.now();
        self.admit(source, at, metrics);
    }

    /// Sends a heartbeat from a source's `metrics` output, stamped at `at`, toward the state owners it feeds.
    pub fn heartbeat(&mut self, source: &str, at: AdmissionTime) {
        let point = self.source_point(source);
        self.heartbeat_point(point, at);
    }

    fn heartbeat_point(&mut self, point: AdmissionPointId, at: AdmissionTime) {
        let header = self.stamp(point, at);
        let route = self.points[&point].route.clone();
        self.deliver(route, EventsBuffer::with_capacity(0).with_header(header), at);
    }

    fn source_point(&self, source: &str) -> AdmissionPointId {
        let node: NodeId = source.parse().expect("a source's ID");
        self.frame
            .admission_point_at(&node, &PortName::from_static(crate::operators::METRICS_PORT))
            .expect("the source has an admission point")
            .id
    }

    fn stamp(&mut self, point: AdmissionPointId, at: AdmissionTime) -> BatchHeader {
        let schedule = self.schedule.load();
        let point = self.points.get_mut(&point).expect("the frame's admission point");
        point.stamp(at, &schedule)
    }

    /// Moves time to the wall-clock time `now`, as the reference interpreter's `advance` does: every host ticks, every
    /// source that feeds a state owner heartbeats first if [`HarnessOptions::heartbeats_at_ticks`] says so, and the
    /// regions' monotonic clock moves to as long after their start as `now` is after the harness's start, running every
    /// deadline that passes.
    ///
    /// An isolation slot ticks on its own schedule, every flush interval from its start, not at every `advance`: a test
    /// that compares it with the interpreter advances at the times its ticks are due.
    pub fn advance(&mut self, now: AdmissionTime) {
        self.last_tick = Some(now);
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
                .filter(|point| point.clock.latest().is_none_or(|latest| latest < now))
                .map(|point| point.id)
                .collect::<Vec<_>>();
            for point in stale {
                self.heartbeat_point(point, now);
            }
        }
        // The regions' monotonic clock follows the wall clock from the start, and never moves back.
        let since_start = Duration::from_nanos(now.as_unix_nanos().saturating_sub(self.start_wall.as_unix_nanos()));
        self.elapsed = self.elapsed.max(since_start);
        for slot in self.order.clone() {
            self.run_deadlines(slot, now);
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
            self.run_deadlines(slot, wall);
        }
    }

    /// Runs every deadline of a region that has passed on its monotonic clock, as its host would.
    fn run_deadlines(&mut self, slot: SlotId, arrival: AdmissionTime) {
        let now = self.now();
        for _ in 0..MAX_DEADLINES {
            if self.failed.contains_key(&slot) {
                return;
            }
            let Some(deadline) = self.drivers[slot.get()].next_deadline() else {
                return;
            };
            if deadline > now {
                return;
            }
            self.call(slot, Call::Advance, arrival);
        }
        panic!(
            "region `{}` kept a passed deadline for {} calls",
            self.frame.slot(slot).map_or("?", |slot| slot.name.as_str()),
            MAX_DEADLINES
        );
    }

    /// Ends every region's input at the wall-clock time `now`, in an order in which every region finishes after the
    /// regions that feed it, as a topology shuts down.
    pub fn close(&mut self, now: AdmissionTime) {
        self.release();
        self.wall.set(now);
        for slot in self.order.clone() {
            self.call(slot, Call::Finish, now);
        }
    }

    /// Delivers a control message to a region, as its host does between calls.
    pub fn control(
        &mut self, slot: SlotId, message: RegionControl,
    ) -> Result<(), saluki_core::execution::RegionFailure> {
        let mut budget = self.options.turn_budget;
        let mut sink = ExitSink::new(self.frame.slot(slot).map_or(0, |slot| slot.exits.len()));
        let now = self.now();
        let result = self.drivers[slot.get()].control(&mut RegionTurn::new(now, &mut budget), message, &mut sink);
        let buffers = sink.drain().collect::<Vec<_>>();
        let arrival = self.wall.now();
        self.route_exits(slot, buffers, arrival);
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
            let route = self.points[&point].route.clone();
            self.deliver(route, buffer_of(header, metrics), now);
        }
    }

    /// Runs one call of a region, resuming it while it yields, and delivers what it writes.
    ///
    /// Each call runs under supervision, as a region host runs it: after a panic, what the call wrote is discarded,
    /// the driver recovers, and a yielded call is over; a finish the panic ended is called again until one completes.
    /// A region that fails stops: nothing reaches it any more.
    fn call(&mut self, slot: SlotId, call: Call, arrival: AdmissionTime) {
        if self.failed.contains_key(&slot) {
            return;
        }
        let exits = self.frame.slot(slot).map_or(0, |slot| slot.exits.len());
        let finishing = matches!(call, Call::Finish);
        let mut call = Some(call);
        for _ in 0..MAX_DEADLINES {
            let mut budget = self.options.turn_budget;
            let mut sink = ExitSink::new(exits);
            let now = self.now();
            let driver = &mut self.drivers[slot.get()];
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
            let progress = match result {
                Ok(progress) => progress,
                Err(payload) => {
                    let lost = lost + sink.event_count() as u64;
                    let panic = RegionPanic::new(site, panic_message(&*payload), lost);
                    match self.drivers[slot.get()].reset_after_panic(&panic) {
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
            let buffers = sink.drain().collect::<Vec<_>>();
            self.route_exits(slot, buffers, arrival);
            if progress != Progress::Yielded {
                return;
            }
        }
        panic!("a region yielded {} times in one call", MAX_DEADLINES);
    }

    /// Returns the buffers the slot named `name` received so far: those that carried events, and header-only ones,
    /// such as heartbeats and progress.
    pub fn arrivals(&self, name: &str) -> (usize, usize) {
        self.arrivals.get(&self.slot_named(name)).copied().unwrap_or_default()
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

    /// Hands what a region wrote to its exits to their targets, holding buffers between slots when asked to.
    fn route_exits(
        &mut self, slot: SlotId, buffers: Vec<(saluki_core::execution::ExitId, EventsBuffer)>, arrival: AdmissionTime,
    ) {
        let exits = self.frame.slot(slot).expect("the frame's slot").exits.clone();
        for (exit, buffer) in buffers {
            let target = exits[exit.get()].target.clone();
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
            if self.holding && matches!(target, FrameTarget::Slot(_)) {
                self.held.push_back((target, buffer, arrival));
            } else {
                self.deliver(Some(target), buffer, arrival);
            }
        }
    }

    fn deliver(&mut self, target: Option<FrameTarget>, buffer: EventsBuffer, arrival: AdmissionTime) {
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
                let arrivals = self.arrivals.entry(slot).or_default();
                if buffer.is_empty() {
                    arrivals.1 += 1;
                } else {
                    arrivals.0 += 1;
                }
                self.call(slot, Call::Process(buffer), arrival)
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

    /// Returns the drivers, in slot order.
    pub fn drivers(&self) -> &[MetricRegionDriver] {
        &self.drivers
    }

    /// Returns the driver of the slot named `name`.
    ///
    /// # Panics
    ///
    /// Panics if the frame has no such slot.
    pub fn driver(&self, name: &str) -> &MetricRegionDriver {
        &self.drivers[self.slot_named(name).get()]
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
