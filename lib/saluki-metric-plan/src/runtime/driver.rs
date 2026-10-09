//! The region driver of one slot.

use std::{
    collections::BTreeMap,
    fmt,
    panic::{catch_unwind, AssertUnwindSafe},
    sync::{Arc, Weak},
    time::Instant,
};

use saluki_core::{
    data_model::event::Event,
    execution::{
        BatchHeader, ExitSink, GenerationId, GenerationRef, GenerationToken, LossClass, LossCount, PanicOutcome,
        PortIndex, Progress, RegionDriver, RegionFailure, RegionPanic, RegionTurn, Turn, TurnCounters,
    },
    topology::EventsBuffer,
};
use saluki_error::{generic_error, GenericError};
use saluki_metrics::Counter;
use tracing::{debug, error, warn};

use super::{
    code::{EntryCode, Op, ProgramCode, Target},
    kernels::{KernelSlab, KernelSlot, PreparedKernel},
    supervision::{PanicBudget, PanicHistory},
    telemetry::{add_loss, Layout, ProgramCounts, ProgramTelemetry, RegisterTelemetry, LOSS_CLASSES},
    RegionError,
};
use crate::{
    compile::{DecisionIndex, KernelIndex, RegionProgram, Slot, SlotId},
    ir::NativeComponentId,
    model::EventView,
};

/// The most times a kernel may yield on one batch before the driver treats it as broken.
///
/// Stateless kernels complete every batch in one call, so a kernel that keeps yielding violates its contract.
const MAX_YIELDS: usize = 1024;

/// Called with a generation, and why, when a region fails it: the generation exceeded its panic budget, or its
/// kernels could not be rebuilt after a panic.
///
/// It runs on the region's task, so it must only hand the report on, for example by queueing it for the installer.
pub type GenerationFailureReporter = Arc<dyn Fn(GenerationId, &str) + Send + Sync>;

/// What a region driver needs to know about its slot.
pub struct RegionConfig {
    /// The slot.
    pub slot: SlotId,

    /// The slot's component name, for logs, errors, and loss outside any node.
    pub name: NativeComponentId,

    /// The number of the slot's exits.
    pub exits: usize,

    /// The counter of points lost to batches no program can process, such as one without a generation: tagged with
    /// the region's name and the `invalid` class.
    pub invalid_loss: Counter,

    /// Where the region reports the generations it fails, so that the installer can roll back from them. `None` by
    /// default: failures are then only logged.
    pub failures: Option<GenerationFailureReporter>,
}

impl RegionConfig {
    /// Returns the configuration of the region of a frame's slot, registering its counter with `telemetry`.
    pub fn for_slot(slot: &Slot, telemetry: &dyn RegisterTelemetry) -> Self {
        Self {
            slot: slot.id,
            name: slot.name.clone(),
            exits: slot.exits.len(),
            invalid_loss: telemetry.loss(slot.name.as_str(), LossClass::Invalid),
            failures: None,
        }
    }

    /// Reports the generations the region fails to `failures`.
    pub fn with_failure_reporter(mut self, failures: GenerationFailureReporter) -> Self {
        self.failures = Some(failures);
        self
    }
}

impl fmt::Debug for RegionConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RegionConfig")
            .field("slot", &self.slot)
            .field("name", &self.name)
            .field("exits", &self.exits)
            .field("failures", &self.failures.is_some())
            .finish_non_exhaustive()
    }
}

/// What a region prepares for one generation: its program, the kernels to build it with, its panic budget, and the
/// counters it publishes to.
pub struct PrepareRegion {
    generation: GenerationRef,
    program: Arc<RegionProgram>,
    kernels: Vec<PreparedKernel>,
    budget: PanicBudget,
    layout: Layout,
    telemetry: ProgramTelemetry,
}

impl PrepareRegion {
    /// Creates a preparation, registering the counters of the program's nodes with `telemetry`.
    ///
    /// `kernels` are the program's kernels the region does not hold yet, built off the data path: for the boot
    /// generation every kernel of the program, and for a later one those its preparation lists to build. A region
    /// reuses the instances it holds for every other kernel of the program.
    ///
    /// The region keeps `generation` only behind a weak reference, so that the generation retires once nothing else
    /// holds it, and releases the program then.
    pub fn new(
        generation: GenerationRef, program: Arc<RegionProgram>, kernels: Vec<PreparedKernel>, budget: PanicBudget,
        telemetry: &dyn RegisterTelemetry,
    ) -> Self {
        let layout = Layout::of(&program);
        let telemetry = ProgramTelemetry::register(&program, &layout, telemetry);
        Self {
            generation,
            program,
            kernels,
            budget,
            layout,
            telemetry,
        }
    }

    /// Returns the generation.
    pub fn generation(&self) -> &GenerationRef {
        &self.generation
    }

    /// Returns the program.
    pub fn program(&self) -> &Arc<RegionProgram> {
        &self.program
    }

    /// Returns the generation's panic budget.
    pub fn budget(&self) -> PanicBudget {
        self.budget
    }
}

impl fmt::Debug for PrepareRegion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PrepareRegion")
            .field("generation", &self.generation.id())
            .field("slot", &self.program.slot())
            .field("fingerprint", &self.program.fingerprint())
            .field("kernels", &self.kernels)
            .field("budget", &self.budget)
            .finish_non_exhaustive()
    }
}

/// A control message for a [`MetricRegionDriver`].
#[derive(Debug)]
pub enum RegionControl {
    /// Prepares a generation: the region can process its batches from when it acknowledges.
    Prepare(Box<PrepareRegion>),

    /// Drops a generation that was prepared and never committed, with its program and the kernels only it uses.
    ///
    /// Aborting a generation the region does not have does nothing.
    Abort {
        /// The generation.
        generation: GenerationId,
    },

    /// Releases every generation that retired: the program of each generation nothing references any more, with the
    /// kernels only it uses.
    ///
    /// A region also releases retired generations whenever it prepares or aborts one, and when it publishes telemetry,
    /// but never while it processes a batch. The installer sends this once a generation retires, so that a region
    /// that receives no batches releases it too.
    Release,
}

/// A generation the region can process batches of.
struct LiveGeneration {
    id: GenerationId,
    token: Weak<GenerationToken>,
    program: Arc<RegionProgram>,
    code: ProgramCode,
    layout: Layout,
    /// The slab index of each of the program's kernels, by kernel index.
    kernels: Vec<usize>,
    counts: ProgramCounts,
    telemetry: ProgramTelemetry,
    budget: PanicBudget,
    panics: PanicHistory,
    /// Whether the generation exceeded its panic budget: its batches are dropped.
    failed: bool,
}

/// The node a call is running, for charging a panic to it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum NodeRef {
    Kernel(usize),
    Decision(usize),
}

/// What the driver is running, recorded as it goes so that a panic can be charged to its generation and node.
#[derive(Clone, Copy, Debug, Default)]
struct Cursor {
    generation: Option<GenerationId>,
    node: Option<NodeRef>,
}

/// Storage reused by every batch.
#[derive(Default)]
struct Scratch {
    registers: Vec<Vec<EventsBuffer>>,
    pending: Vec<EventsBuffer>,
    matches: Vec<bool>,
    counters: TurnCounters,
}

impl Scratch {
    fn clear(&mut self) {
        for register in &mut self.registers {
            register.clear();
        }
        self.pending.clear();
        self.matches.clear();
        self.counters = TurnCounters::default();
    }
}

/// A wiring fault: a batch no program can process.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Fault {
    NoGeneration,
    UnknownGeneration,
    NoEntry,
    FailedGeneration,
}

impl Fault {
    const COUNT: usize = 4;

    const fn index(self) -> usize {
        match self {
            Self::NoGeneration => 0,
            Self::UnknownGeneration => 1,
            Self::NoEntry => 2,
            Self::FailedGeneration => 3,
        }
    }
}

/// Points lost to wiring faults, outside any node.
struct InvalidLoss {
    counter: Counter,
    pending: LossCount,
    warned: [bool; Fault::COUNT],
}

/// The [`RegionDriver`] of one slot of a frame: it runs the region program of every live generation over the batches
/// stamped with it.
///
/// # Generations
///
/// The driver keeps one program per live generation, each behind a [`Weak`] reference to its generation's token, and
/// releases a program, with every kernel only it uses, once its generation retires: once nothing, neither a batch nor
/// the schedule nor the installer, holds the token any more. It checks for retired generations off the data path,
/// which costs one atomic load per live generation: on every control message ([`RegionControl::Release`] asks for
/// nothing else) and every time the host publishes telemetry, never while it processes a batch. The boot generation
/// is prepared when the driver is created; later ones arrive as [`RegionControl::Prepare`], before the schedule
/// commits them, so the driver never assigns a generation: each batch runs the program of the generation in its
/// header. A batch whose header has no generation, a
/// generation the driver was never prepared for, or an entry its generation's program lacks is a wiring fault: the
/// driver drops it, counts its points as invalid loss of the region, and logs a warning the first time.
///
/// # Kernels
///
/// Kernel instances live in a slab keyed by [`KernelKey`][crate::compile::KernelKey]: generations whose programs
/// have a kernel with the same key share its instance, caches included, and an instance is dropped when the last
/// generation using it is released. A preparation brings the kernels the region does not hold yet, built off the
/// data path; the driver checks that it holds or receives every kernel of the program before it accepts it.
///
/// # Execution
///
/// A batch runs to completion in one call: the driver takes its header out, runs the steps of its entry's program
/// batch at a time over registers of buffers, and stamps every buffer it hands to an exit with a copy of the header,
/// with the consumer's entry set for an exit to another slot and cleared otherwise. Decisions filter buffers in place,
/// in order, and move a buffer whole where every event goes one way. Stateless kernels complete every batch, so the
/// driver never yields.
///
/// # Telemetry
///
/// The driver counts in plain integers as it runs: events per output port of every kernel and decision, policy loss
/// per `drop` node, the loss each kernel reports, and invalid loss of the region. It adds them to the counters
/// registered with each generation's preparation when the host calls
/// [`publish_telemetry`][RegionDriver::publish_telemetry].
///
/// # Supervision
///
/// After a panic, the driver discards every buffer the call that panicked held, and its kernels' unrecorded loss
/// counts, and charges the panic to the generation and node it was running. What the call counted before it panicked
/// stays counted: it is work the nodes did. Under the generation's [`PanicBudget::Zero`], the default program's, the
/// region fails, as a native component does. Under a limited budget, the driver rebuilds the kernels of the generation
/// that panicked from their specs and continues; a generation that exceeds its budget fails, its later batches are
/// dropped as invalid loss, and the failure goes to the [failure reporter][RegionConfig::failures]. A kernel that
/// cannot be rebuilt fails every generation that uses it, and the region if one of them has a zero budget. A panic
/// outside any program's steps fails the region.
///
/// # Accounting
///
/// The driver builds, calls, and rebuilds each kernel inside the resource group its build context names, if any, so
/// that each kernel's memory and CPU time are accounted under its own node, as a native component's are.
pub struct MetricRegionDriver {
    slot: SlotId,
    name: NativeComponentId,
    exit_count: usize,
    kernels: KernelSlab,
    generations: Vec<LiveGeneration>,
    scratch: Scratch,
    invalid: InvalidLoss,
    cursor: Cursor,
    now: Option<Instant>,
    failures: Option<GenerationFailureReporter>,
}

impl fmt::Debug for MetricRegionDriver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MetricRegionDriver")
            .field("slot", &self.slot)
            .field("name", &self.name)
            .field("exits", &self.exit_count)
            .field("generations", &self.generations())
            .field("kernels", &self.kernels.len())
            .finish_non_exhaustive()
    }
}

impl MetricRegionDriver {
    /// Creates the driver of a slot, prepared for the boot generation.
    ///
    /// `boot` must bring every kernel of the boot program, built with [`build_kernels`][super::build_kernels].
    ///
    /// # Errors
    ///
    /// Returns an error if the boot program is for another slot, cannot run in a region with the configured number of
    /// exits, or lacks a kernel.
    pub fn new(config: RegionConfig, boot: PrepareRegion) -> Result<Self, RegionError> {
        let RegionConfig {
            slot,
            name,
            exits,
            invalid_loss,
            failures,
        } = config;
        let mut driver = Self {
            slot,
            name,
            exit_count: exits,
            kernels: KernelSlab::default(),
            generations: Vec::new(),
            scratch: Scratch::default(),
            invalid: InvalidLoss {
                counter: invalid_loss,
                pending: LossCount::default(),
                warned: [false; Fault::COUNT],
            },
            cursor: Cursor::default(),
            now: None,
            failures,
        };
        driver.prepare(boot)?;
        Ok(driver)
    }

    /// Returns the slot the driver runs.
    pub fn slot(&self) -> SlotId {
        self.slot
    }

    /// Returns the generations whose programs the driver holds, in identifier order.
    pub fn generations(&self) -> Vec<GenerationId> {
        self.generations.iter().map(|generation| generation.id).collect()
    }

    /// Returns the number of kernel instances the driver holds.
    pub fn kernel_count(&self) -> usize {
        self.kernels.len()
    }

    /// Prepares a generation.
    fn prepare(&mut self, prepare: PrepareRegion) -> Result<(), RegionError> {
        self.prune();
        let PrepareRegion {
            generation,
            program,
            kernels,
            budget,
            layout,
            telemetry,
        } = prepare;
        let id = generation.id();
        if program.slot() != self.slot {
            return Err(RegionError::WrongSlot {
                region: self.slot,
                program: program.slot(),
            });
        }
        if self.generations.iter().any(|live| live.id == id) {
            return Err(RegionError::AlreadyPrepared {
                region: self.name.to_string(),
                generation: id,
            });
        }
        let code = ProgramCode::translate(&program, &layout, self.exit_count)?;

        // Resolve every kernel before changing anything, so that a rejected preparation leaves the region as it was.
        let mut supplied = kernels
            .into_iter()
            .map(|kernel| (kernel.key().clone(), kernel))
            .collect::<BTreeMap<_, _>>();
        let mut resolved = Vec::with_capacity(program.kernels().len());
        for kernel in program.kernels() {
            match self.kernels.find(&kernel.key) {
                Some(index) => resolved.push(Resolved::Reuse(index)),
                None => match supplied.remove(&kernel.key) {
                    Some(prepared) => resolved.push(Resolved::New(Box::new(prepared), kernel.ports.len())),
                    None => {
                        return Err(RegionError::MissingKernel {
                            region: self.name.to_string(),
                            generation: id,
                            key: kernel.key.clone(),
                        })
                    }
                },
            }
        }
        if !supplied.is_empty() {
            debug!(
                region = %self.name,
                generation = %id,
                kernels = supplied.len(),
                "Dropping prepared kernels the region already holds or the program does not use."
            );
        }

        let kernels = resolved
            .into_iter()
            .map(|resolved| match resolved {
                Resolved::Reuse(index) => {
                    self.kernels.acquire(index);
                    index
                }
                Resolved::New(prepared, ports) => self.kernels.insert(*prepared, ports),
            })
            .collect();
        let live = LiveGeneration {
            id,
            token: Arc::downgrade(&generation),
            counts: ProgramCounts::new(&layout),
            program,
            code,
            layout,
            kernels,
            telemetry,
            budget,
            panics: PanicHistory::default(),
            failed: false,
        };
        if self.scratch.registers.len() < live.code.registers {
            self.scratch.registers.resize_with(live.code.registers, Vec::new);
        }
        let position = self.generations.partition_point(|other| other.id < id);
        self.generations.insert(position, live);
        debug!(region = %self.name, generation = %id, "Prepared a generation.");
        Ok(())
    }

    /// Drops a generation that was never committed.
    fn abort(&mut self, id: GenerationId) {
        if let Some(position) = self.generations.iter().position(|live| live.id == id) {
            let generation = self.generations.remove(position);
            self.release(generation);
            debug!(region = %self.name, generation = %id, "Aborted a generation.");
        }
        self.prune();
    }

    /// Releases every generation whose token was dropped.
    fn prune(&mut self) {
        if self.generations.iter().all(|live| live.token.strong_count() > 0) {
            return;
        }
        let mut index = 0;
        while index < self.generations.len() {
            if self.generations[index].token.strong_count() == 0 {
                let generation = self.generations.remove(index);
                debug!(region = %self.name, generation = %generation.id, "Released a retired generation.");
                self.release(generation);
            } else {
                index += 1;
            }
        }
    }

    /// Publishes what a generation counted, and releases its kernels.
    fn release(&mut self, mut generation: LiveGeneration) {
        generation.telemetry.publish(&mut generation.counts);
        for index in generation.kernels {
            self.kernels.release(index);
        }
    }

    /// Drops a batch no program can process, counting it as invalid loss of the region.
    fn reject(&mut self, fault: Fault, header: &BatchHeader, batch: &EventsBuffer) {
        add_loss(&mut self.invalid.pending, batch.len() as u64, points_in(batch));
        if std::mem::replace(&mut self.invalid.warned[fault.index()], true) {
            return;
        }
        let generation = header
            .generation_id()
            .map_or_else(|| "none".to_string(), |id| id.to_string());
        match fault {
            Fault::NoGeneration => warn!(
                region = %self.name,
                "Region received a batch without a generation, which only a wiring fault produces: every batch is \
                 stamped with a generation where it enters the plan. Dropping it as invalid loss; later such batches \
                 are dropped without a warning."
            ),
            Fault::UnknownGeneration => warn!(
                region = %self.name,
                generation = %generation,
                "Region received a batch of a generation it was never prepared for, which only a wiring fault \
                 produces: every generation is prepared in every region before it is committed. Dropping it as \
                 invalid loss; later such batches are dropped without a warning."
            ),
            Fault::NoEntry => warn!(
                region = %self.name,
                generation = %generation,
                entry = ?header.entry,
                admission_point = ?header.admission_point,
                "Region received a batch its generation's program has no entry for, which only a wiring fault \
                 produces. Dropping it as invalid loss; later such batches are dropped without a warning."
            ),
            Fault::FailedGeneration => warn!(
                region = %self.name,
                generation = %generation,
                "Region received a batch of a generation that failed after exceeding its panic budget. Dropping it as \
                 invalid loss; later such batches are dropped without a warning."
            ),
        }
    }

    /// Rebuilds the kernels of the generation at `index` from their specs, after a panic in its program.
    ///
    /// The call that panicked ran only that generation's program, so only its kernels can have been left inconsistent.
    /// Generations that share a kernel with it share the rebuilt instance. Returns the slab index and the reason of the
    /// first kernel that could not be rebuilt, because its factory failed or panicked; that kernel keeps the instance
    /// that panicked.
    fn rebuild_kernels(&mut self, index: usize) -> Result<(), (usize, String)> {
        for &kernel in &self.generations[index].kernels {
            let slot = self.kernels.get_mut(kernel);
            let rebuilt = catch_unwind(AssertUnwindSafe(|| {
                slot.context.accounted(|| slot.spec.build(&slot.context))
            }));
            slot.kernel = match rebuilt {
                Ok(Ok(rebuilt)) => rebuilt,
                Ok(Err(error)) => return Err((kernel, error.to_string())),
                Err(payload) => return Err((kernel, format!("its factory panicked: {}", panic_message(&*payload)))),
            };
        }
        Ok(())
    }

    /// Fails the live generation at `index`: its later batches are dropped as invalid loss, and the failure is
    /// reported.
    fn fail_generation(&mut self, index: usize, reason: &str) {
        let generation = &mut self.generations[index];
        if std::mem::replace(&mut generation.failed, true) {
            return;
        }
        error!(
            region = %self.name,
            generation = %generation.id,
            reason,
            "Generation failed; its batches are dropped as invalid loss from now on."
        );
        if let Some(failures) = &self.failures {
            failures(generation.id, reason);
        }
    }
}

/// Returns the message a panic carried.
fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|message| (*message).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "<panic payload that is not a string>".to_owned())
}

/// How a preparation gets one of its program's kernels: an instance the region holds, by slab index, or a new one, with
/// its number of ports.
enum Resolved {
    Reuse(usize),
    New(Box<PreparedKernel>, usize),
}

impl RegionDriver for MetricRegionDriver {
    type Control = RegionControl;

    fn process(&mut self, turn: &mut RegionTurn<'_>, mut batch: EventsBuffer, exits: &mut ExitSink) -> Progress {
        // Retired generations are released off the data path: a batch holds its own generation, so the program it
        // needs is never released under it.
        self.now = Some(turn.now());
        // A header-only batch, such as a heartbeat, carries nothing to process.
        if batch.is_empty() {
            return Progress::Done;
        }
        let header = batch.take_header();
        let Some(id) = header.generation_id() else {
            self.reject(Fault::NoGeneration, &header, &batch);
            return Progress::Done;
        };
        let Some(index) = self.generations.iter().position(|live| live.id == id) else {
            self.reject(Fault::UnknownGeneration, &header, &batch);
            return Progress::Done;
        };
        if self.generations[index].failed {
            self.reject(Fault::FailedGeneration, &header, &batch);
            return Progress::Done;
        }
        let Some(entry) = self.generations[index].code.entry_index(&header) else {
            self.reject(Fault::NoEntry, &header, &batch);
            return Progress::Done;
        };

        let Self {
            kernels,
            generations,
            scratch,
            cursor,
            ..
        } = self;
        let LiveGeneration {
            code,
            layout,
            kernels: kernel_slots,
            counts,
            ..
        } = &mut generations[index];
        cursor.generation = Some(id);
        Run {
            layout,
            kernel_slots,
            counts,
            slab: kernels,
            scratch,
            cursor,
            header: &header,
            turn,
            exits,
        }
        .entry(&code.entries[entry], batch);
        *cursor = Cursor::default();
        Progress::Done
    }

    fn control(
        &mut self, turn: &mut RegionTurn<'_>, message: RegionControl, _exits: &mut ExitSink,
    ) -> Result<(), RegionFailure> {
        self.now = Some(turn.now());
        match message {
            RegionControl::Prepare(prepare) => self
                .prepare(*prepare)
                .map_err(|error| RegionFailure::new(GenericError::from(error))),
            RegionControl::Abort { generation } => {
                self.abort(generation);
                Ok(())
            }
            RegionControl::Release => {
                self.prune();
                Ok(())
            }
        }
    }

    fn reset_after_panic(&mut self, panic: &RegionPanic) -> PanicOutcome {
        let cursor = std::mem::take(&mut self.cursor);
        self.scratch.clear();
        for slot in self.kernels.iter_mut() {
            slot.ports.clear();
        }
        let now = self.now.unwrap_or_else(Instant::now);

        let Some(index) = cursor
            .generation
            .and_then(|id| self.generations.iter().position(|live| live.id == id))
        else {
            return PanicOutcome::Fail(generic_error!(
                "Region `{}` panicked outside the program of any generation, which it cannot recover from.",
                self.name
            ));
        };
        let generation = &mut self.generations[index];
        let node = match cursor.node {
            Some(NodeRef::Kernel(kernel)) => generation
                .program
                .kernel(KernelIndex::new(kernel as u16))
                .map(|kernel| format!("node `{}`", kernel.key.node)),
            Some(NodeRef::Decision(decision)) => generation
                .program
                .decision(DecisionIndex::new(decision as u16))
                .map(|decision| format!("node `{}`", decision.node)),
            None => None,
        }
        .unwrap_or_else(|| "the region's own steps".to_string());

        if !generation.panics.record(generation.budget, now) {
            if generation.budget == PanicBudget::Zero {
                return PanicOutcome::Fail(generic_error!(
                    "Region `{}` panicked in {} of generation {}, whose panic budget is zero, as the default \
                     program's is, so the region fails as a native component does.",
                    self.name,
                    node,
                    generation.id
                ));
            }
            let reason = format!(
                "it exceeded its panic budget, with {} panics, the last in {}",
                generation.panics.total(),
                node
            );
            self.fail_generation(index, &reason);
        } else {
            warn!(
                region = %self.name,
                generation = %generation.id,
                lost_events = panic.lost_events(),
                "Region recovered from a panic in {} by rebuilding its kernels: {}",
                node,
                panic
            );
        }

        // A kernel that cannot be rebuilt keeps the instance that panicked, which no generation can trust: every
        // generation using it fails, and the region fails with it if one of them is the default program's.
        let Err((kernel, reason)) = self.rebuild_kernels(index) else {
            return PanicOutcome::Continue;
        };
        let node = self.kernels.get_mut(kernel).key.node.clone();
        let reason = format!(
            "the kernel of node `{}` could not be rebuilt after a panic: {}",
            node, reason
        );
        let users = (0..self.generations.len())
            .filter(|&other| self.generations[other].kernels.contains(&kernel))
            .collect::<Vec<_>>();
        if let Some(zero) = users
            .iter()
            .find(|&&user| self.generations[user].budget == PanicBudget::Zero)
        {
            return PanicOutcome::Fail(generic_error!(
                "Region `{}` cannot continue generation {}, whose panic budget is zero: {}.",
                self.name,
                self.generations[*zero].id,
                reason
            ));
        }
        for user in users {
            self.fail_generation(user, &reason);
        }
        PanicOutcome::Continue
    }

    fn publish_telemetry(&mut self) {
        self.prune();
        for generation in &mut self.generations {
            generation.telemetry.publish(&mut generation.counts);
        }
        let invalid = std::mem::take(&mut self.invalid.pending);
        if invalid.points > 0 {
            self.invalid.counter.increment(invalid.points);
        }
    }
}

/// Returns the number of points of the metrics in a buffer.
fn points_in(buffer: &EventsBuffer) -> u64 {
    buffer.into_iter().map(points_of).sum()
}

/// Returns the number of points of an event: a metric's values, and none for any other event.
fn points_of(event: &Event) -> u64 {
    event.try_as_metric().map_or(0, |metric| metric.values().len() as u64)
}

/// Adds the loss a kernel reported in one call to its counts.
fn fold_loss(total: &mut TurnCounters, call: &mut TurnCounters) {
    if call.is_empty() {
        return;
    }
    let call = call.take();
    for class in LOSS_CLASSES {
        let loss = call.loss(class);
        if loss != LossCount::default() {
            total.record_loss(class, loss.events, loss.points);
        }
    }
}

/// Runs one kernel over one buffer, leaving what it produced in its port outputs.
fn consume(
    slot: &mut KernelSlot, header: &BatchHeader, turn: &mut RegionTurn<'_>, counters: &mut TurnCounters,
    mut buffer: EventsBuffer,
) {
    let KernelSlot {
        key,
        context,
        kernel,
        ports,
        ..
    } = slot;
    // The kernel's work is accounted to the kernel, not to the region that runs it.
    context.accounted(|| {
        let mut yields = 0;
        loop {
            let progress = {
                let mut kernel_turn = Turn::new(header, turn.now(), turn.budget_mut(), ports, counters);
                kernel.consume(&mut kernel_turn, &mut buffer)
            };
            if progress != Progress::Yielded {
                break;
            }
            yields += 1;
            assert!(
                yields < MAX_YIELDS,
                "kernel `{}` yielded {} times on one batch, but stateless kernels complete every batch in one call",
                key.node,
                yields
            );
        }
    });
    ports.complete(buffer);
}

/// The execution of one batch through one entry's ops.
struct Run<'a, 't> {
    layout: &'a Layout,
    kernel_slots: &'a [usize],
    counts: &'a mut ProgramCounts,
    slab: &'a mut KernelSlab,
    scratch: &'a mut Scratch,
    cursor: &'a mut Cursor,
    header: &'a BatchHeader,
    turn: &'a mut RegionTurn<'t>,
    exits: &'a mut ExitSink,
}

impl Run<'_, '_> {
    fn entry(self, entry: &EntryCode, batch: EventsBuffer) {
        let Run {
            layout,
            kernel_slots,
            counts,
            slab,
            scratch,
            cursor,
            header,
            turn,
            exits,
        } = self;
        let Scratch {
            registers,
            pending,
            matches,
            counters,
        } = scratch;
        if registers.len() < entry.registers {
            registers.resize_with(entry.registers, Vec::new);
        }
        registers[0].push(batch);

        for op in &entry.ops {
            match op {
                Op::Map { kernel, register } => {
                    cursor.node = Some(NodeRef::Kernel(*kernel));
                    let slot = slab.get_mut(kernel_slots[*kernel]);
                    std::mem::swap(pending, &mut registers[*register]);
                    for buffer in pending.drain(..) {
                        consume(slot, header, turn, counters, buffer);
                        registers[*register].extend(slot.ports.drain(PortIndex::PRIMARY));
                    }
                    let events = registers[*register]
                        .iter()
                        .map(|buffer| buffer.len() as u64)
                        .sum::<u64>();
                    counts.kernel_ports[layout.kernel_ports[*kernel]] += events;
                    fold_loss(&mut counts.kernel_loss[*kernel], counters);
                }
                Op::Expand { kernel, input, ports } => {
                    cursor.node = Some(NodeRef::Kernel(*kernel));
                    let slot = slab.get_mut(kernel_slots[*kernel]);
                    let base = layout.kernel_ports[*kernel];
                    std::mem::swap(pending, &mut registers[*input]);
                    for buffer in pending.drain(..) {
                        consume(slot, header, turn, counters, buffer);
                        for (port, target) in ports.iter().enumerate() {
                            // A kernel has at most as many ports as a port index numbers: its step was translated.
                            for output in slot.ports.drain(PortIndex::new(port as u16)) {
                                counts.kernel_ports[base + port] += output.len() as u64;
                                deliver(*target, output, registers, counts);
                            }
                        }
                    }
                    fold_loss(&mut counts.kernel_loss[*kernel], counters);
                }
                Op::Split {
                    decision,
                    input,
                    predicate,
                    matched,
                    unmatched,
                } => {
                    cursor.node = Some(NodeRef::Decision(*decision));
                    let base = layout.decision_ports[*decision];
                    let is_match = |event: &Event| {
                        event
                            .try_as_metric()
                            .is_some_and(|metric| predicate.evaluate(&mut EventView::new(metric)))
                    };
                    std::mem::swap(pending, &mut registers[*input]);
                    for mut buffer in pending.drain(..) {
                        let total = buffer.len() as u64;
                        match (*matched, *unmatched) {
                            (Target::Register(keep), Target::Drop(drop))
                            | (Target::Drop(drop), Target::Register(keep)) => {
                                // The common filter: keep one side in place, in order, and count the other as loss.
                                let keep_matches = matches!(matched, Target::Register(_));
                                let mut lost = LossCount::default();
                                buffer.retain(|event| {
                                    if is_match(event) == keep_matches {
                                        return true;
                                    }
                                    add_loss(&mut lost, 1, points_of(event));
                                    false
                                });
                                let kept = buffer.len() as u64;
                                let matched_events = if keep_matches { kept } else { lost.events };
                                counts.decision_ports[base] += matched_events;
                                counts.decision_ports[base + 1] += total - matched_events;
                                add_loss(&mut counts.drops[drop], lost.events, lost.points);
                                if !buffer.is_empty() {
                                    registers[keep].push(buffer);
                                }
                            }
                            (Target::Drop(matched_drop), Target::Drop(unmatched_drop)) => {
                                let mut matched_loss = LossCount::default();
                                let mut unmatched_loss = LossCount::default();
                                for event in &buffer {
                                    let loss = if is_match(event) {
                                        &mut matched_loss
                                    } else {
                                        &mut unmatched_loss
                                    };
                                    add_loss(loss, 1, points_of(event));
                                }
                                counts.decision_ports[base] += matched_loss.events;
                                counts.decision_ports[base + 1] += unmatched_loss.events;
                                add_loss(
                                    &mut counts.drops[matched_drop],
                                    matched_loss.events,
                                    matched_loss.points,
                                );
                                add_loss(
                                    &mut counts.drops[unmatched_drop],
                                    unmatched_loss.events,
                                    unmatched_loss.points,
                                );
                            }
                            (Target::Register(matched_register), Target::Register(unmatched_register)) => {
                                matches.clear();
                                matches.extend((&buffer).into_iter().map(is_match));
                                let matched_events = matches.iter().filter(|matched| **matched).count();
                                counts.decision_ports[base] += matched_events as u64;
                                counts.decision_ports[base + 1] += total - matched_events as u64;
                                if matched_events == buffer.len() {
                                    registers[matched_register].push(buffer);
                                } else if matched_events == 0 {
                                    registers[unmatched_register].push(buffer);
                                } else {
                                    // The original buffer keeps the unmatched events and goes to the last target; the
                                    // matched events move to a buffer of exactly their size.
                                    let mut moved = EventsBuffer::with_capacity(matched_events);
                                    let mut flags = matches.iter();
                                    buffer.move_into(&mut moved, |_| flags.next().copied().unwrap_or(false));
                                    registers[matched_register].push(moved);
                                    registers[unmatched_register].push(buffer);
                                }
                            }
                        }
                    }
                }
                Op::Tee {
                    decision,
                    input,
                    targets,
                    last,
                } => {
                    cursor.node = Some(NodeRef::Decision(*decision));
                    let base = layout.decision_ports[*decision];
                    let drops = targets.iter().any(|target| matches!(target, Target::Drop(_)));
                    std::mem::swap(pending, &mut registers[*input]);
                    for buffer in pending.drain(..) {
                        let events = buffer.len() as u64;
                        for port in 0..targets.len() {
                            counts.decision_ports[base + port] += events;
                        }
                        if drops {
                            let points = points_in(&buffer);
                            for target in targets {
                                if let Target::Drop(drop) = target {
                                    add_loss(&mut counts.drops[*drop], events, points);
                                }
                            }
                        }
                        let Some((last_position, last_register)) = *last else {
                            continue;
                        };
                        // Every register target but the last gets a copy; the last gets the original.
                        for (position, target) in targets.iter().enumerate() {
                            if let Target::Register(register) = target {
                                if position != last_position {
                                    registers[*register].push(buffer.clone());
                                }
                            }
                        }
                        registers[last_register].push(buffer);
                    }
                }
                Op::Concat { inputs, output } => {
                    cursor.node = None;
                    for input in inputs {
                        if input == output {
                            continue;
                        }
                        let mut moved = std::mem::take(&mut registers[*input]);
                        registers[*output].append(&mut moved);
                        registers[*input] = moved;
                    }
                }
                Op::Exit { register, exit, entry } => {
                    cursor.node = None;
                    for mut buffer in registers[*register].drain(..) {
                        if buffer.is_empty() {
                            continue;
                        }
                        let mut stamped = header.clone();
                        stamped.entry = *entry;
                        buffer.set_header(stamped);
                        exits.emit(*exit, buffer);
                    }
                }
                Op::Drop { register, drop } => {
                    cursor.node = None;
                    for buffer in registers[*register].drain(..) {
                        add_loss(&mut counts.drops[*drop], buffer.len() as u64, points_in(&buffer));
                    }
                }
            }
        }

        // Every compiled program consumes all of its registers; anything left would be a compilation bug.
        let used = &mut registers[..entry.registers];
        debug_assert!(
            used.iter().all(Vec::is_empty),
            "a program left buffers in its registers"
        );
        for register in used {
            register.clear();
        }
    }
}

/// Moves a buffer to where a kernel port leads.
fn deliver(target: Target, buffer: EventsBuffer, registers: &mut [Vec<EventsBuffer>], counts: &mut ProgramCounts) {
    match target {
        Target::Register(register) => registers[register].push(buffer),
        Target::Drop(drop) => add_loss(&mut counts.drops[drop], buffer.len() as u64, points_in(&buffer)),
    }
}
