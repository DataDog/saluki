//! The region driver of one slot.

use std::{
    any::Any,
    collections::{BTreeMap, BTreeSet},
    fmt,
    sync::{Arc, Weak},
    time::Instant,
};

use saluki_core::{
    data_model::event::Event,
    execution::{
        AdmissionPointId, AdmissionTime, BatchHeader, DrainPolicy, ExitId, ExitSink, GenerationId, GenerationRef,
        GenerationToken, LossClass, LossCount, PanicOutcome, PortIndex, Progress, RegionDriver, RegionFailure,
        RegionPanic, RegionTurn, ScheduleHandle, SystemWallClock, Turn, TurnCounters, WallClock,
    },
    topology::EventsBuffer,
};
use saluki_error::{generic_error, GenericError};
use saluki_metrics::Counter;
use tracing::{debug, error, warn};

use super::{
    code::{DecisionCode, EntryCode, Op, ProgramCode, Target},
    kernels::{KernelSlab, KernelSlot, PreparedKernel},
    state::{slab::InstanceHandle, StateBudgets},
    supervision::{PanicBudget, PanicHistory},
    telemetry::{add_loss, Layout, ProgramCounts, ProgramTelemetry, RegisterTelemetry, StateTelemetry, LOSS_CLASSES},
    RegionError,
};
use crate::{
    compile::{
        DecisionIndex, DecisionMemo, ExitSet, ExitSetCounts, KernelIndex, LeaseTable, RegionProgram, Slot, SlotId,
        SlotKind, StateParamsEntry, COUNTED_EXIT_SETS,
    },
    install::StateBinding,
    ir::{NativeComponentId, NodeId},
    limits::values::{DECISION_MEMO_ENTRIES, PROGRESS_PERIOD},
};

mod isolation;
use self::isolation::{StateResolution, StateSlot};

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

    /// The number of entries of the memo the region keeps for each memoized decision (R4) of each generation:
    /// [`DECISION_MEMO_ENTRIES`] from the limits table.
    pub(crate) memo_entries: usize,

    /// The schedule of the topology, which an isolation slot reads to stamp its time-driven output.
    pub(crate) schedule: Option<ScheduleHandle>,

    /// The wall clock an isolation slot reads for its tick gate, its shutdown time, and its watermark lag.
    pub(crate) wall_clock: Option<Arc<dyn WallClock>>,

    /// The budgets an isolation slot's state owners draw their state from.
    pub(crate) state_budgets: Option<StateBudgets>,

    /// What kind of slot the region runs.
    pub(crate) kind: SlotKind,

    /// The exits that forward the progress of admission points upstream of state owners, with those points.
    pub(crate) progress: Vec<(ExitId, Vec<AdmissionPointId>)>,

    /// The handles an isolation slot publishes its own telemetry to.
    pub(crate) state_telemetry: Option<StateTelemetry>,
}

impl RegionConfig {
    /// Returns the configuration of the region of a frame's slot, registering its counters with `telemetry`: its
    /// invalid loss, and an isolation slot's own telemetry ([`RegisterTelemetry::state`]).
    ///
    /// An isolation slot's region also needs the schedule of the topology ([`with_schedule`][Self::with_schedule]), and
    /// takes the wall clock and the host's state budgets ([`with_wall_clock`][Self::with_wall_clock],
    /// [`with_state_budgets`][Self::with_state_budgets]).
    pub fn for_slot(slot: &Slot, telemetry: &dyn RegisterTelemetry) -> Self {
        let progress = slot
            .exits
            .iter()
            .filter(|exit| !exit.progress_points.is_empty())
            .map(|exit| (ExitId::new(exit.index.get() as u16), exit.progress_points.clone()))
            .collect();
        Self {
            slot: slot.id,
            name: slot.name.clone(),
            exits: slot.exits.len(),
            invalid_loss: telemetry.loss(slot.name.as_str(), LossClass::Invalid),
            failures: None,
            // The limit is a count of entries, which every platform's memory addresses.
            memo_entries: DECISION_MEMO_ENTRIES as usize,
            schedule: None,
            wall_clock: None,
            state_budgets: None,
            kind: slot.kind.clone(),
            progress,
            state_telemetry: slot.is_isolation().then(|| telemetry.state(slot.name.as_str())),
        }
    }

    /// Gives the region the schedule handle of the topology, read only: an isolation slot decides from it which
    /// generation its time-driven output belongs to. An isolation slot's region needs it.
    pub fn with_schedule(mut self, schedule: ScheduleHandle) -> Self {
        self.schedule = Some(schedule);
        self
    }

    /// Gives the region the wall clock an isolation slot reads for its tick gate, the time it closes at when its input
    /// ends, and its watermark lag; the operating system's by default. Kernels never read it.
    pub fn with_wall_clock(mut self, wall_clock: Arc<dyn WallClock>) -> Self {
        self.wall_clock = Some(wall_clock);
        self
    }

    /// Gives the region the host's state budgets, which an isolation slot's state owners draw their state from.
    ///
    /// A region given budgets refuses a preparation whose state owners' kernels were built without them.
    pub fn with_state_budgets(mut self, budgets: StateBudgets) -> Self {
        self.state_budgets = Some(budgets);
        self
    }

    /// Returns the schedule handle the region was given, if any.
    pub fn schedule(&self) -> Option<&ScheduleHandle> {
        self.schedule.as_ref()
    }

    /// Returns the wall clock the region was given, if any.
    pub fn wall_clock(&self) -> Option<&Arc<dyn WallClock>> {
        self.wall_clock.as_ref()
    }

    /// Returns the state budgets the region was given, if any.
    pub fn state_budgets(&self) -> Option<&StateBudgets> {
        self.state_budgets.as_ref()
    }

    /// Reports the generations the region fails to `failures`.
    pub fn with_failure_reporter(mut self, failures: GenerationFailureReporter) -> Self {
        self.failures = Some(failures);
        self
    }

    /// Sets the number of entries of each memoized decision's memo, in place of the limits table's
    /// [`DECISION_MEMO_ENTRIES`]; zero memoizes nothing.
    ///
    /// For tests and benchmarks that compare memoized decisions with evaluated ones, or exercise eviction. Production
    /// regions use the limits table's value.
    #[cfg(any(test, feature = "test-util"))]
    pub fn with_memo_entries(mut self, entries: usize) -> Self {
        self.memo_entries = entries;
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
            .field("memo_entries", &self.memo_entries)
            .field("schedule", &self.schedule.is_some())
            .field("wall_clock", &self.wall_clock)
            .field("state_budgets", &self.state_budgets)
            .field("kind", &self.kind)
            .field("progress", &self.progress)
            .finish_non_exhaustive()
    }
}

/// What a region prepares for one generation: its program, the kernels to build it with, its panic budget and the
/// nodes it covers, and the counters it publishes to.
pub struct PrepareRegion {
    generation: GenerationRef,
    program: Arc<RegionProgram>,
    kernels: Vec<PreparedKernel>,
    budget: PanicBudget,
    /// The nodes whose panics the budget covers, or `None` for every node.
    budgeted: Option<BTreeSet<NodeId>>,
    /// How the program's state owners bind to instances of their state, in an isolation slot.
    state: Vec<StateBinding>,
    /// The lease of each output the program delivers to, or `None` if exits to outputs write no entry.
    leases: Option<Arc<LeaseTable>>,
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
            budgeted: None,
            state: Vec::new(),
            leases: None,
            layout,
            telemetry,
        }
    }

    /// Gives the preparation its generation's lease table: the lease of each output the program delivers to, which
    /// each exit toward an encode host writes as the entry of every buffer it sends, so that the encode host knows
    /// which encoder stream the buffer belongs to.
    ///
    /// A program with an exit toward an encode host needs a table with a lease for every output it delivers to there,
    /// or the region rejects the preparation. A program whose exits lead only to regions and fixed destinations needs
    /// none.
    pub fn with_leases(mut self, leases: Arc<LeaseTable>) -> Self {
        self.leases = Some(leases);
        self
    }

    /// Returns the preparation's lease table ([`with_leases`]), if it has one.
    ///
    /// [`with_leases`]: Self::with_leases
    pub fn leases(&self) -> Option<&Arc<LeaseTable>> {
        self.leases.as_ref()
    }

    /// Says how the program's state owners bind to instances of their state, in an isolation slot: retain an instance
    /// another generation uses, take a new one from the preparation's kernels, or retire an instance another generation
    /// uses at the preparing generation's cut.
    ///
    /// Without bindings, a state owner binds to the region's instance of its lineage that is not retiring, if there is
    /// one, and otherwise to the new instance the preparation brings: what a preparation without a cut needs.
    pub fn with_state_bindings(mut self, bindings: Vec<StateBinding>) -> Self {
        self.state = bindings;
        self
    }

    /// Limits the panic budget to the given nodes: those of the generation's plan with an artifact's provenance.
    ///
    /// A panic in any other node of the program, one native configuration placed, fails the region at once, as under a
    /// zero budget, whatever the generation's budget: a native node fails as the native component does, even in an
    /// artifact's generation, and its panics are never charged to the artifact. A decision runs its nodes as one step,
    /// so a panic while it runs is charged to the budget if the budget covers any of its nodes. A panic in the
    /// region's own steps between nodes is charged to the budget. Without this, the budget covers every node.
    pub fn with_artifact_nodes<I>(mut self, nodes: I) -> Self
    where
        I: IntoIterator<Item = NodeId>,
    {
        self.budgeted = Some(nodes.into_iter().collect());
        self
    }

    /// Returns the generation.
    pub fn generation(&self) -> &GenerationRef {
        &self.generation
    }

    /// Returns the program.
    pub fn program(&self) -> &Arc<RegionProgram> {
        &self.program
    }

    /// Returns how the program's state owners bind to instances of their state ([`with_state_bindings`]).
    ///
    /// [`with_state_bindings`]: Self::with_state_bindings
    pub fn state_bindings(&self) -> &[StateBinding] {
        &self.state
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
            .field("budgeted", &self.budgeted)
            .field("state", &self.state)
            .field("leases", &self.leases)
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

    /// Delivers a control message to the kernels it targets, then replies with the message.
    Kernel(KernelControl),
}

/// Called with a [`KernelControl`]'s message once every kernel it targets has handled it.
pub type KernelReply = Box<dyn FnOnce(Box<dyn Any + Send>) + Send>;

/// Which kernels of a region a [`KernelControl`] goes to.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum KernelTarget {
    /// Every live instance of the kernel of one node, in every lineage.
    Node(NodeId),

    /// Every live instance of every state owner's kernel in the region: every lineage of every node.
    Stateful,
}

/// A control message for a region's kernels, such as a request for a state owner's contexts.
///
/// The region delivers `message` to each kernel `target` selects, in turn, through
/// [`Kernel::control`][saluki_core::execution::Kernel::control], then calls `reply` with it, so that each kernel
/// can add to it, as a snapshot request collects every lineage's contexts. A read-only message may be delivered while a
/// kernel's flush is yielded. A region that stops, or a kernel that panics, drops the reply without calling it, which
/// the requester sees as its sender dropped.
pub struct KernelControl {
    /// The kernels the message goes to.
    pub target: KernelTarget,

    /// The message, which each kernel reads as the type it handles.
    pub message: Box<dyn Any + Send>,

    /// Called with the message once every target handled it.
    pub reply: KernelReply,
}

impl fmt::Debug for KernelControl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("KernelControl")
            .field("target", &self.target)
            .finish_non_exhaustive()
    }
}

/// What a region keeps for one decision of one generation's program.
struct DecisionState {
    /// The decision's R4 memo, if it is memoized.
    memo: Option<DecisionMemo>,
    /// Events per exit set since the region last published.
    counts: ExitSetCounts,
}

/// Where one of a program's kernels lives in the region.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum KernelBinding {
    /// A stateless kernel's instance, by its index in the kernel slab.
    Slab(usize),

    /// An instance of a state owner's state, in an isolation slot.
    Instance(InstanceHandle),
}

/// How a region treats a generation after its program panicked.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum GenerationMode {
    /// Its batches are processed.
    Healthy,

    /// A state owner's kernel panicked under it, and the installer was asked to roll it back: its batches are still
    /// processed, on the rebuilt instance, until a replacement takes over.
    RollingBack,

    /// It exceeded its panic budget, or a kernel it uses could not be rebuilt: its batches are dropped as invalid
    /// loss.
    Dropping,
}

impl GenerationMode {
    /// Returns `true` if the generation's batches are processed.
    fn processes(self) -> bool {
        !matches!(self, Self::Dropping)
    }
}

/// A generation the region can process batches of.
struct LiveGeneration {
    id: GenerationId,
    token: Weak<GenerationToken>,
    program: Arc<RegionProgram>,
    code: ProgramCode,
    layout: Layout,
    /// Where each of the program's kernels lives, by kernel index.
    kernels: Vec<KernelBinding>,
    /// The memo and counts of each of the program's decisions, by decision index.
    decisions: Vec<DecisionState>,
    counts: ProgramCounts,
    telemetry: ProgramTelemetry,
    budget: PanicBudget,
    /// Whether each of the program's kernels, by kernel index, is a node the budget does not cover: a panic in it
    /// fails the region at once.
    native_kernels: Vec<bool>,
    /// Whether each of the program's decisions, by decision index, has only nodes the budget does not cover.
    native_decisions: Vec<bool>,
    panics: PanicHistory,
    /// What the region does with the generation's batches after its program panicked.
    mode: GenerationMode,
}

impl LiveGeneration {
    /// Derives every decision's per-port counts and policy loss from its counts per exit set, and adds what the
    /// generation counted to its counters.
    fn publish(&mut self) {
        for (index, (decision, state)) in self.code.decisions.iter().zip(&mut self.decisions).enumerate() {
            if let Some(decision) = decision {
                let base = self.layout.decision_ports[index];
                let ports = &mut self.counts.decision_ports[base..base + decision.function.port_count()];
                derive_counts(decision, &state.counts, ports, &mut self.counts.drops);
            }
            state.counts.clear();
        }
        self.telemetry.publish(&mut self.counts);
    }
}

/// Adds the per-port counts and the policy loss of each `drop` a decision leads to, derived from its counts per exit
/// set: an event left a port when its exit set has an exit below the port, and was lost to a `drop` when its exit set
/// has the exit leading to it.
fn derive_counts(decision: &DecisionCode, counts: &ExitSetCounts, ports: &mut [u64], drops: &mut [LossCount]) {
    decision.function.port_counts(counts, ports);
    for (set, events, points) in counts.iter() {
        for &(exit, drop) in &decision.drops {
            if set.contains(exit) {
                add_loss(&mut drops[drop], events, points);
            }
        }
    }
}

/// The node a call is running, for charging a panic to it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum NodeRef {
    Kernel(usize),
    Decision(usize),
}

/// What the driver is running, recorded as it goes so that a panic can be charged to its generation, node, and, for a
/// state owner, the instance whose kernel ran.
#[derive(Clone, Copy, Debug, Default)]
struct Cursor {
    generation: Option<GenerationId>,
    node: Option<NodeRef>,
    instance: Option<InstanceHandle>,
}

/// Storage reused by every batch.
#[derive(Default)]
struct Scratch {
    registers: Vec<Vec<EventsBuffer>>,
    pending: Vec<EventsBuffer>,
    decide: DecideScratch,
    counters: TurnCounters,
    /// Whether the call wrote to each exit, for progress forwarding.
    written: Vec<bool>,
}

impl Scratch {
    fn clear(&mut self) {
        for register in &mut self.registers {
            register.clear();
        }
        self.pending.clear();
        self.decide.clear();
        self.counters = TurnCounters::default();
        self.written.fill(false);
    }
}

/// Storage a decision step reuses for every buffer.
#[derive(Default)]
struct DecideScratch {
    /// Each event's exit set, in buffer order.
    sets: Vec<ExitSet>,
    /// The events leaving by each exit.
    exit_events: Vec<u64>,
    /// The buffer being filled for each exit, other than the one the original buffer goes to.
    outputs: Vec<Option<EventsBuffer>>,
}

impl DecideScratch {
    fn clear(&mut self) {
        self.sets.clear();
        self.exit_events.clear();
        self.outputs.clear();
    }
}

/// A wiring fault: a batch no program can process.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Fault {
    NoGeneration,
    UnknownGeneration,
    NoEntry,
    FailedGeneration,
    NotUpstream,
}

impl Fault {
    const COUNT: usize = 5;

    const fn index(self) -> usize {
        match self {
            Self::NoGeneration => 0,
            Self::UnknownGeneration => 1,
            Self::NoEntry => 2,
            Self::FailedGeneration => 3,
            Self::NotUpstream => 4,
        }
    }
}

/// One exit that forwards the progress of admission points upstream of a state owner, and the progress it last
/// forwarded for each, as `(epoch, admission time)`.
struct ProgressExit {
    exit: ExitId,
    points: Vec<AdmissionPointId>,
    last: Vec<Option<(u32, AdmissionTime)>>,
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
/// with the consumer's entry set for an exit to another slot and cleared otherwise. A decision computes each event's
/// exit set with its compiled [function][crate::compile::DecisionFunction], from its generation's memo when it is
/// memoized (R4), then moves each event to its targets in order: a buffer whose events all take the same exits moves
/// whole, a buffer whose kept events all go to one target is filtered in place, and the others' events are copied, or
/// moved for the last target, into buffers of exactly their size. Stateless kernels complete every batch, so the driver
/// never yields.
///
/// # Memos
///
/// Each generation's program has a memo per memoized decision, of the limits table's
/// [`DECISION_MEMO_ENTRIES`] entries, allocated when the generation is prepared and dropped with it: a generation
/// never sees the results of another. A lookup is one probe and an insertion overwrites, so the data path never
/// allocates.
///
/// # Telemetry
///
/// The driver counts in plain integers as it runs: events per output port of every kernel, events per exit set of
/// every decision, policy loss per `drop` node, the loss each kernel reports, and invalid loss of the region. When the
/// host calls [`publish_telemetry`][RegionDriver::publish_telemetry], or the generation is released, it derives every
/// decision node's per-port counts from the counts per exit set (exact, as a decision has no `merge`), and adds
/// everything to the counters registered with each generation's preparation. A decision whose events take more
/// distinct exit sets than it counts ([`COUNTED_EXIT_SETS`]) derives its per-port counts early, and counts on.
///
/// # Supervision
///
/// After a panic, the driver discards every buffer the call that panicked held, and its kernels' unrecorded loss
/// counts, and charges the panic to the generation and node it was running. What the call counted before it panicked
/// stays counted: it is work the nodes did. Under the generation's [`PanicBudget::Zero`], the default program's, the
/// region fails, as a native component does, and so it does after a panic in a node the budget does not cover
/// ([`PrepareRegion::with_artifact_nodes`]): one native configuration placed, in an artifact's generation. Under a
/// limited budget, the driver rebuilds the kernels of the generation that panicked from their specs and continues; a
/// generation that exceeds its budget fails, its later batches are dropped as invalid loss, and the failure goes to the
/// [failure reporter][RegionConfig::failures]. A kernel that cannot be rebuilt fails every generation that uses it, and
/// the region if one of them has a zero budget. A panic outside any program's steps fails the region.
///
/// # Accounting
///
/// The driver builds, calls, and rebuilds each kernel inside the resource group its build context names, if any, so
/// that each kernel's memory and CPU time are accounted under its own node, as a native component's are.
///
/// # Progress
///
/// An exit that leads toward a state owner forwards the progress of the admission points upstream of it
/// ([`Exit::progress_points`][crate::compile::Exit::progress_points]), so that the state owner's watermark advances
/// even when a batch leaves the slot without output, as when a filter drops all of it. After each batch from such a
/// point, if the call wrote nothing to the exit, the driver writes a header-only batch with the batch's header and no
/// entry: always for a header-only batch, such as a heartbeat, and otherwise when the point's epoch changed or its
/// admission time moved at least [`PROGRESS_PERIOD`] past what the exit last carried. Progress follows the call's data
/// on the same exit, so each point's batches stay in stamping order.
///
/// # Isolation slots
///
/// The region of an isolation slot runs one or more lineages of its state owners' state: **instances**, keyed by their
/// lineage and the generation that created them, named by tagged handles that a freed instance's place never reuses,
/// and bound by every live generation whose program has a state owner of that lineage. A batch's [state
/// step][crate::compile::Step::State] hands it to the instance its generation binds, with that generation's per-call
/// parameters.
///
/// - **Watermark.** Every header that reaches the slot, data or progress, advances the progress of its admission point
///   by the shared [time rules][crate::runtime::state::rules]; a header of another epoch is a clock reset.
/// - **Ticks.** Each instance ticks every flush interval on the host's clock, from one interval after the region
///   starts, in the phase of the instance of its node it replaces when a cut creates it. At a tick the [tick
///   gate][crate::runtime::state::rules::TickGate] decides: fire at the tick's wall-clock second once the watermark
///   reached it, wait for it at most the grace, or skip, moving the deadline on, when no published generation binds
///   the instance or the watermark is unknown.
/// - **Before a batch.** A header can start work that runs before its batch is admitted, in this order: a clock reset
///   of every instance (a flush at the watermark before the reset, then everything held, continuing empty), the final
///   retirement of every instance retiring by a generation every upstream point has now switched to (a flush at the
///   cut's time, then everything held), and gated ticks the header's watermark releases.
/// - **Drains.** Time-driven work runs as a **drain**: a queue of instances, each called with its kernel's time-driven
///   method, a turn watermark of its flush time, and the per-call parameters of the generation its output is stamped
///   with, decided when its drain starts and held until it ends ([`stamping_generation`]). The output runs that
///   generation's entry for the state owner's time-driven output, stamped by the slot's [output
///   clock][crate::runtime::state::rules::OutputClock], which moves only when an instance's drain emits something. A
///   drain yields when the turn's budget runs out, and the host
///   resumes it; no input is read until it completes, and the batch that started it is admitted afterwards.
/// - **Shutdown.** When input ends, every instance makes its final flush at the later of the latest admission time and
///   the wall clock, and discards its open buckets as shutdown loss.
///
/// A panic in a state owner's kernel is charged to the generation whose call ran: the batch's for a batch, and the
/// drain's for time-driven work. The instance restarts empty, its points counted as invalid loss, and every
/// generation binding it continues on it. Under a zero budget the region fails; an artifact's generation asks to be
/// rolled back and keeps processing its input until it is replaced, and only one that exceeds its budget has its
/// batches dropped. A drain the panic ended keeps the rest of its queue, and the next `advance_time`, or `finish` once
/// input has ended, resumes it.
///
/// [`stamping_generation`]: crate::runtime::state::rules::stamping_generation
pub struct MetricRegionDriver {
    slot: SlotId,
    name: NativeComponentId,
    exit_count: usize,
    memo_entries: usize,
    kernels: KernelSlab,
    generations: Vec<LiveGeneration>,
    scratch: Scratch,
    invalid: InvalidLoss,
    cursor: Cursor,
    now: Option<Instant>,
    failures: Option<GenerationFailureReporter>,
    /// The exits that forward the progress of admission points toward state owners.
    progress: Vec<ProgressExit>,
    /// The state of an isolation slot, which only an isolation slot's region has.
    state: Option<Box<StateSlot>>,
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
    /// exits, or lacks a kernel, or if the region runs an isolation slot and was given no schedule.
    pub fn new(config: RegionConfig, boot: PrepareRegion) -> Result<Self, RegionError> {
        let RegionConfig {
            slot,
            name,
            exits,
            invalid_loss,
            failures,
            memo_entries,
            schedule,
            wall_clock,
            state_budgets,
            kind,
            progress,
            state_telemetry,
        } = config;
        let state = match kind {
            SlotKind::Fused => None,
            SlotKind::Isolation { output_point, upstream } => {
                let schedule = schedule.ok_or_else(|| RegionError::StateConfig {
                    region: name.to_string(),
                    message: "it runs an isolation slot, which needs the topology's schedule to decide which \
                              generation its time-driven output belongs to; give its configuration the schedule \
                              with `RegionConfig::with_schedule`"
                        .to_string(),
                })?;
                Some(Box::new(StateSlot::new(
                    output_point,
                    upstream,
                    schedule,
                    wall_clock.unwrap_or_else(|| Arc::new(SystemWallClock)),
                    state_budgets,
                    state_telemetry.unwrap_or_else(StateTelemetry::noop),
                )))
            }
        };
        let progress = progress
            .into_iter()
            .map(|(exit, points)| ProgressExit {
                exit,
                last: vec![None; points.len()],
                points,
            })
            .collect();
        let mut driver = Self {
            slot,
            name,
            exit_count: exits,
            memo_entries,
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
            progress,
            state,
        };
        driver.scratch.written = vec![false; exits];
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

    /// Returns the number of kernel instances the driver holds: stateless kernels' and, in an isolation slot, the
    /// instances of its state owners' state.
    pub fn kernel_count(&self) -> usize {
        self.kernels.len() + self.state.as_ref().map_or(0, |state| state.instances.len())
    }

    /// Returns the instances of the isolation slot's state, as `(state owner, generation that created it, retired)`, in
    /// the order the region drives them; none for a fused slot.
    pub fn lineages(&self) -> Vec<(NodeId, GenerationId, bool)> {
        self.state.as_ref().map_or_else(Vec::new, |state| {
            state
                .instances
                .iter()
                .map(|(_, instance)| (instance.key.node.clone(), instance.created_by, instance.retired))
                .collect()
        })
    }

    /// Prepares a generation.
    fn prepare(&mut self, prepare: PrepareRegion) -> Result<(), RegionError> {
        self.prune();
        let PrepareRegion {
            generation,
            program,
            kernels,
            budget,
            budgeted,
            state: bindings,
            leases,
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
        let code = ProgramCode::translate(
            &program,
            &layout,
            self.exit_count,
            self.state.is_some(),
            leases.as_deref(),
        )?;
        let state_error = |message: String| RegionError::StateBinding {
            region: self.name.to_string(),
            generation: id,
            message,
        };

        // Resolve every kernel before changing anything, so that a rejected preparation leaves the region as it was.
        let mut supplied = kernels
            .into_iter()
            .map(|kernel| (kernel.key().clone(), kernel))
            .collect::<BTreeMap<_, _>>();
        let mut resolved = Vec::with_capacity(program.kernels().len());
        for kernel in program.kernels() {
            if kernel.lineage.is_some() {
                // Translation lets a state owner's kernel run only in an isolation slot.
                let resolution = self
                    .resolve_state(kernel, &bindings, &mut supplied)
                    .map_err(state_error)?;
                if let StateResolution::New(prepared, _) = &resolution {
                    self.check_budget(prepared).map_err(state_error)?;
                }
                resolved.push(Resolved::State(resolution));
                continue;
            }
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
        // At a cut, the instances the generation replaces retire, holding both generations until their final flush.
        let mut retiring = Vec::new();
        for binding in &bindings {
            let StateBinding::Retire { node, key, from } = binding else {
                continue;
            };
            let found = self.state.as_ref().and_then(|_| self.instance_of(*from, key));
            let owner = self
                .generations
                .iter()
                .find(|live| live.id == *from)
                .and_then(|live| live.token.upgrade());
            let (Some(handle), Some(owner)) = (found, owner) else {
                return Err(state_error(format!(
                    "the preparation retires the instance of `{}` that generation {} uses, which the region does not \
                     hold",
                    node, from
                )));
            };
            retiring.push((handle, owner));
        }

        let now = self.now.unwrap_or_else(Instant::now);
        let kernels = resolved
            .into_iter()
            .zip(program.kernels())
            .map(|(resolved, kernel)| match resolved {
                Resolved::Reuse(index) => {
                    self.kernels.acquire(index);
                    KernelBinding::Slab(index)
                }
                Resolved::New(prepared, ports) => KernelBinding::Slab(self.kernels.insert(*prepared, ports)),
                Resolved::State(StateResolution::Retain(handle)) => {
                    if let Some(instance) = self
                        .state
                        .as_deref_mut()
                        .and_then(|state| state.instances.get_mut(handle))
                    {
                        instance.users += 1;
                        if let Some(lineage) = &kernel.lineage {
                            instance.interval = lineage.flush_interval;
                        }
                    }
                    KernelBinding::Instance(handle)
                }
                Resolved::State(StateResolution::New(prepared, ports)) => {
                    KernelBinding::Instance(self.insert_instance(id, kernel, *prepared, ports, now))
                }
            })
            .collect::<Vec<_>>();
        if let Some(state) = self.state.as_deref_mut() {
            for (handle, owner) in retiring {
                if let Some(instance) = state.instances.get_mut(handle) {
                    instance.retiring = Some(isolation::Retiring {
                        owner,
                        by: Arc::clone(&generation),
                    });
                }
            }
        }
        // Each memoized decision's memo is allocated here, off the data path, and lives as long as its generation.
        let decisions = code
            .decisions
            .iter()
            .map(|decision| DecisionState {
                memo: decision
                    .as_ref()
                    .filter(|decision| decision.memoized && self.memo_entries > 0)
                    .map(|decision| DecisionMemo::for_exits(self.memo_entries, decision.function.exit_count())),
                // A decision no step runs counts nothing.
                counts: ExitSetCounts::with_capacity(if decision.is_some() { COUNTED_EXIT_SETS } else { 0 }),
            })
            .collect();
        let native = |node: &NodeId| budgeted.as_ref().is_some_and(|budgeted| !budgeted.contains(node));
        let native_kernels = program
            .kernels()
            .iter()
            .map(|kernel| native(&kernel.key.node))
            .collect();
        let native_decisions = program
            .decisions()
            .iter()
            .map(|decision| decision.nodes.iter().all(|node| native(&node.node)))
            .collect();
        let live = LiveGeneration {
            id,
            token: Arc::downgrade(&generation),
            decisions,
            counts: ProgramCounts::new(&layout),
            program,
            code,
            layout,
            kernels,
            telemetry,
            budget,
            native_kernels,
            native_decisions,
            panics: PanicHistory::default(),
            mode: GenerationMode::Healthy,
        };
        if self.scratch.registers.len() < live.code.registers {
            self.scratch.registers.resize_with(live.code.registers, Vec::new);
        }
        let position = self.generations.partition_point(|other| other.id < id);
        self.generations.insert(position, live);
        debug!(region = %self.name, generation = %id, "Prepared a generation.");
        Ok(())
    }

    /// Drops a generation that was never committed, with the retiring marks its preparation set and the instances of
    /// state it created.
    fn abort(&mut self, id: GenerationId) {
        if let Some(position) = self.generations.iter().position(|live| live.id == id) {
            self.abort_state(id);
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

    /// Publishes what a generation counted, and releases its kernels and instances of state.
    fn release(&mut self, mut generation: LiveGeneration) {
        generation.publish();
        for binding in &generation.kernels {
            if let KernelBinding::Slab(index) = *binding {
                self.kernels.release(index);
            }
        }
        self.release_instances(&generation.kernels);
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
            Fault::NotUpstream => warn!(
                region = %self.name,
                admission_point = ?header.admission_point,
                "Isolation slot received a batch that no admission point upstream of it stamped, which only a wiring \
                 fault produces: its state owner's watermark cannot account for it. Dropping it as invalid loss; \
                 later such batches are dropped without a warning."
            ),
        }
    }

    /// Rebuilds the stateless kernels of the generation at `index` from their specs, after a panic in its program.
    ///
    /// The call that panicked ran only that generation's program, so only its kernels can have been left inconsistent.
    /// Generations that share a kernel with it share the rebuilt instance. Returns the slab index and the reason of the
    /// first kernel that could not be rebuilt, because its factory failed or panicked; that kernel keeps the instance
    /// that panicked. Instances of state owners' state are rebuilt only when their own kernel panicked.
    fn rebuild_kernels(&mut self, index: usize) -> Result<(), (usize, String)> {
        for binding in &self.generations[index].kernels {
            let KernelBinding::Slab(kernel) = *binding else {
                continue;
            };
            self.kernels
                .get_mut(kernel)
                .rebuild()
                .map_err(|reason| (kernel, reason))?;
        }
        Ok(())
    }

    /// Fails the live generation at `index`: its later batches are dropped as invalid loss, and the failure is
    /// reported, unless it was reported already.
    fn fail_generation(&mut self, index: usize, reason: &str) {
        let generation = &mut self.generations[index];
        let previous = std::mem::replace(&mut generation.mode, GenerationMode::Dropping);
        if previous == GenerationMode::Dropping {
            return;
        }
        error!(
            region = %self.name,
            generation = %generation.id,
            reason,
            "Generation failed; its batches are dropped as invalid loss from now on."
        );
        if previous == GenerationMode::Healthy {
            if let Some(failures) = &self.failures {
                failures(generation.id, reason);
            }
        }
    }

    /// Asks for the live generation at `index` to be rolled back, once, while the region keeps processing its batches.
    fn roll_back_generation(&mut self, index: usize, node: &str, reason: &str) {
        if self.generations[index].mode != GenerationMode::Healthy {
            return;
        }
        self.generations[index].mode = GenerationMode::RollingBack;
        self.log_rolling_back(index, node, reason);
        if let Some(failures) = &self.failures {
            failures(self.generations[index].id, reason);
        }
    }

    /// Returns the live generation a batch runs under, or rejects the batch: one without a generation, of a generation
    /// the region was never prepared for, or of a generation whose batches are dropped.
    fn live_index_for(&mut self, header: &BatchHeader, batch: &EventsBuffer) -> Option<usize> {
        let Some(id) = header.generation_id() else {
            self.reject(Fault::NoGeneration, header, batch);
            return None;
        };
        let Some(index) = self.generations.iter().position(|live| live.id == id) else {
            self.reject(Fault::UnknownGeneration, header, batch);
            return None;
        };
        if !self.generations[index].mode.processes() {
            self.reject(Fault::FailedGeneration, header, batch);
            return None;
        }
        Some(index)
    }

    /// Runs a batch through a fused slot's program.
    fn run_fused(
        &mut self, turn: &mut RegionTurn<'_>, header: &BatchHeader, batch: EventsBuffer, exits: &mut ExitSink,
    ) {
        let Some(index) = self.live_index_for(header, &batch) else {
            return;
        };
        let Some(entry) = self.generations[index].code.entry_index(header) else {
            self.reject(Fault::NoEntry, header, &batch);
            return;
        };
        let Self {
            kernels,
            generations,
            scratch,
            cursor,
            ..
        } = self;
        let LiveGeneration {
            id,
            program,
            code,
            layout,
            kernels: kernel_slots,
            decisions,
            counts,
            ..
        } = &mut generations[index];
        cursor.generation = Some(*id);
        Run {
            layout,
            kernel_slots,
            functions: &code.decisions,
            decisions,
            counts,
            slab: kernels,
            instances: None,
            state_params: program.state_params(),
            scratch,
            cursor,
            header,
            turn,
            exits,
        }
        .entry(&code.entries[entry], std::iter::once(batch));
        *cursor = Cursor::default();
    }

    /// Forwards the progress of the batch's admission point on every exit toward a state owner that the call wrote
    /// nothing to, as a header-only batch, when it is due (see [Progress](Self#progress)).
    fn forward_progress(&mut self, header: &BatchHeader, header_only: bool, exits: &mut ExitSink) {
        if self.progress.is_empty() {
            return;
        }
        let (Some(point), Some(admitted_at)) = (header.admission_point, header.admitted_at) else {
            return;
        };
        for exit in &mut self.progress {
            let Some(position) = exit.points.iter().position(|upstream| *upstream == point) else {
                continue;
            };
            let last = &mut exit.last[position];
            if self.scratch.written.get(exit.exit.get()).copied().unwrap_or(false) {
                *last = Some((header.epoch, admitted_at));
                continue;
            }
            let due = header_only
                || last.is_none_or(|(epoch, sent)| {
                    epoch != header.epoch || admitted_at >= sent.saturating_add(PROGRESS_PERIOD)
                });
            if due {
                let mut progress = header.clone();
                progress.entry = None;
                exits.emit(exit.exit, EventsBuffer::with_capacity(0).with_header(progress));
                *last = Some((header.epoch, admitted_at));
            }
        }
    }

    /// Recovers a fused slot from a panic in its program: charges it to the generation and node that ran, and rebuilds
    /// the generation's kernels.
    fn recover_fused(&mut self, panic: &RegionPanic, cursor: Cursor, now: Instant) -> PanicOutcome {
        let Some(index) = cursor
            .generation
            .and_then(|id| self.generations.iter().position(|live| live.id == id))
        else {
            return PanicOutcome::Fail(generic_error!(
                "Region `{}` panicked outside the program of any generation, which it cannot recover from.",
                self.name
            ));
        };
        if let Some(outcome) = self.charge(index, panic, &cursor, now) {
            return outcome;
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
            .filter(|&other| self.generations[other].kernels.contains(&KernelBinding::Slab(kernel)))
            .collect::<Vec<_>>();
        self.fail_users(&users, &reason)
    }

    /// Recovers an isolation slot from a panic: charges it to the generation whose call ran, rebuilds the instance
    /// whose kernel panicked, and keeps the rest of a drain the panic ended.
    fn recover_isolated(&mut self, panic: &RegionPanic, cursor: Cursor, now: Instant) -> PanicOutcome {
        let panicked = self.recover_state(&cursor);
        let Some(index) = cursor
            .generation
            .and_then(|id| self.generations.iter().position(|live| live.id == id))
        else {
            return PanicOutcome::Fail(generic_error!(
                "Region `{}` panicked outside the program of any generation, which it cannot recover from.",
                self.name
            ));
        };
        if let Some(outcome) = self.charge(index, panic, &cursor, now) {
            return outcome;
        }
        let Some(handle) = panicked else {
            // The region's own steps panicked, and no state owner's kernel: every instance is as it was.
            return PanicOutcome::Continue;
        };

        let node = self
            .state
            .as_ref()
            .and_then(|state| state.instances.get(handle))
            .map_or_else(String::new, |instance| instance.key.node.to_string());
        let rebuilt = self.rebuild_instance(handle, index);
        let users = self.binders(handle);
        match rebuilt {
            Ok(()) => {
                // Every generation binding the instance continues on it, empty: a panic under an artifact's parameters
                // never fails the default program. An artifact's generation asks to be rolled back.
                if self.generations[index].budget != PanicBudget::Zero {
                    let reason = format!("a state owner's kernel, of node `{}`, panicked: {}", node, panic);
                    self.roll_back_generation(index, &node, &reason);
                }
                PanicOutcome::Continue
            }
            Err(reason) => {
                let reason = format!(
                    "the state of node `{}` could not be rebuilt after a panic: {}",
                    node, reason
                );
                self.fail_users(&users, &reason)
            }
        }
    }

    /// Records a panic against the budget of the live generation at `index`, as charged to the node the cursor names.
    ///
    /// Returns the outcome if the region fails: a zero budget, or a native node's panic. A generation that exceeds a
    /// limited budget fails, and the region continues.
    fn charge(&mut self, index: usize, panic: &RegionPanic, cursor: &Cursor, now: Instant) -> Option<PanicOutcome> {
        let generation = &mut self.generations[index];
        let node = match cursor.node {
            Some(NodeRef::Kernel(kernel)) => generation
                .program
                .kernel(KernelIndex::new(kernel as u16))
                .map(|kernel| format!("node `{}`", kernel.key.node)),
            Some(NodeRef::Decision(decision)) => generation
                .program
                .decision(DecisionIndex::new(decision as u16))
                .map(|decision| format!("node `{}`", decision.root().node)),
            None => None,
        }
        .unwrap_or_else(|| "the region's own steps".to_string());
        // A node native configuration placed has a zero budget, even in an artifact's generation.
        let native = match cursor.node {
            Some(NodeRef::Kernel(kernel)) => generation.native_kernels.get(kernel).copied(),
            Some(NodeRef::Decision(decision)) => generation.native_decisions.get(decision).copied(),
            None => None,
        }
        .unwrap_or(false);
        let budget = if native { PanicBudget::Zero } else { generation.budget };

        if !generation.panics.record(budget, now) {
            if native && generation.budget != PanicBudget::Zero {
                return Some(PanicOutcome::Fail(generic_error!(
                    "Region `{}` panicked in {} of generation {}, which native configuration placed: a native node's \
                     panic budget is zero, as the default program's is, even in an artifact's generation, so the \
                     region fails as a native component does.",
                    self.name,
                    node,
                    generation.id
                )));
            }
            if budget == PanicBudget::Zero {
                return Some(PanicOutcome::Fail(generic_error!(
                    "Region `{}` panicked in {} of generation {}, whose panic budget is zero, as the default \
                     program's is, so the region fails as a native component does.",
                    self.name,
                    node,
                    generation.id
                )));
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
                "Region recovered from a panic in {}: {}",
                node,
                panic
            );
        }
        None
    }

    /// Fails every listed generation, after a kernel or instance they use could not be rebuilt, and the region if one
    /// of them has a zero budget.
    fn fail_users(&mut self, users: &[usize], reason: &str) -> PanicOutcome {
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
        for &user in users {
            self.fail_generation(user, reason);
        }
        PanicOutcome::Continue
    }
}

/// How a preparation gets one of its program's kernels: an instance the region holds, by slab index, a new one, with
/// its number of ports, or a state owner's instance of its state.
enum Resolved {
    Reuse(usize),
    New(Box<PreparedKernel>, usize),
    State(StateResolution),
}

impl RegionDriver for MetricRegionDriver {
    type Control = RegionControl;

    fn start(&mut self, turn: &mut RegionTurn<'_>) {
        self.now = Some(turn.now());
        if self.state.is_some() {
            self.start_isolated(turn);
        }
    }

    fn accepts_input(&self) -> bool {
        self.state.is_none() || self.accepts_isolated()
    }

    fn process(&mut self, turn: &mut RegionTurn<'_>, mut batch: EventsBuffer, exits: &mut ExitSink) -> Progress {
        // Retired generations are released off the data path: a batch holds its own generation, so the program it
        // needs is never released under it.
        self.now = Some(turn.now());
        if self.state.is_some() {
            return self.process_isolated(turn, batch, exits);
        }
        self.scratch.written.fill(false);
        // A header-only batch, such as a heartbeat, carries nothing to process, only its progress.
        let header_only = batch.is_empty();
        let header = batch.take_header();
        if !header_only {
            self.run_fused(turn, &header, batch, exits);
        }
        self.forward_progress(&header, header_only, exits);
        Progress::Done
    }

    fn resume(&mut self, turn: &mut RegionTurn<'_>, exits: &mut ExitSink) -> Progress {
        self.now = Some(turn.now());
        if self.state.is_some() {
            return self.continue_isolated(turn, exits);
        }
        Progress::Done
    }

    fn next_deadline(&self) -> Option<Instant> {
        if self.state.is_some() {
            return self.next_deadline_isolated();
        }
        None
    }

    fn advance_time(&mut self, turn: &mut RegionTurn<'_>, exits: &mut ExitSink) -> Progress {
        self.now = Some(turn.now());
        if self.state.is_some() {
            return self.advance_isolated(turn, exits);
        }
        Progress::Done
    }

    fn finish(&mut self, turn: &mut RegionTurn<'_>, exits: &mut ExitSink, policy: DrainPolicy) -> Progress {
        self.now = Some(turn.now());
        // A region finishes only when the process shuts down: a retiring generation's state retires at its cut.
        debug_assert_eq!(policy, DrainPolicy::Shutdown, "regions finish only at shutdown");
        if self.state.is_some() {
            return self.finish_isolated(turn, exits);
        }
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
            RegionControl::Kernel(control) => {
                if self.state.is_some() {
                    self.kernel_control_isolated(turn, control);
                } else {
                    // A stateless kernel handles no control message, so nothing here adds to the message.
                    let KernelControl { target, message, reply } = control;
                    debug!(
                        region = %self.name,
                        target = ?target,
                        "No kernel of the region handles the control message."
                    );
                    reply(message);
                }
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
        if self.state.is_some() {
            self.recover_isolated(panic, cursor, now)
        } else {
            self.recover_fused(panic, cursor, now)
        }
    }

    fn publish_telemetry(&mut self) {
        self.prune();
        for generation in &mut self.generations {
            generation.publish();
        }
        let invalid = std::mem::take(&mut self.invalid.pending);
        if invalid.points > 0 {
            self.invalid.counter.increment(invalid.points);
        }
        self.publish_state();
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

/// Hands one buffer to a state owner's kernel, with the per-call parameters of the batch's generation.
///
/// A state owner takes what it receives into its state and emits nothing while it does: anything it leaves in its
/// input or writes to its ports is counted as invalid loss and dropped.
fn consume_state(
    slot: &mut KernelSlot, header: &BatchHeader, turn: &mut RegionTurn<'_>, counters: &mut TurnCounters,
    param: &(dyn Any + Send + Sync), mut buffer: EventsBuffer,
) {
    let KernelSlot {
        key,
        context,
        kernel,
        ports,
        ..
    } = slot;
    context.accounted(|| {
        let mut yields = 0;
        loop {
            let progress = {
                let mut kernel_turn =
                    Turn::new(header, turn.now(), turn.budget_mut(), ports, counters).with_param(param);
                kernel.consume(&mut kernel_turn, &mut buffer)
            };
            if progress != Progress::Yielded {
                break;
            }
            yields += 1;
            assert!(
                yields < MAX_YIELDS,
                "state owner `{}` yielded {} times on one batch, but a state owner admits every batch in one call",
                key.node,
                yields
            );
        }
    });
    ports.complete(buffer);
    if !ports.is_empty() {
        debug_assert!(false, "state owner `{}` emitted while admitting a batch", key.node);
        for buffer in ports.drain(PortIndex::PRIMARY) {
            counters.record_loss(LossClass::Invalid, buffer.len() as u64, points_in(&buffer));
        }
        ports.clear();
    }
}

/// The execution of one batch through one entry's ops.
struct Run<'a, 't> {
    layout: &'a Layout,
    kernel_slots: &'a [KernelBinding],
    functions: &'a [Option<DecisionCode>],
    decisions: &'a mut [DecisionState],
    counts: &'a mut ProgramCounts,
    slab: &'a mut KernelSlab,
    /// The instances of an isolation slot's state, which its state steps feed.
    instances: Option<&'a mut crate::runtime::state::slab::Slab<isolation::Instance>>,
    /// The program's state parameter table.
    state_params: &'a [StateParamsEntry],
    scratch: &'a mut Scratch,
    cursor: &'a mut Cursor,
    header: &'a BatchHeader,
    turn: &'a mut RegionTurn<'t>,
    exits: &'a mut ExitSink,
}

impl KernelBinding {
    /// Returns the slab index of a stateless kernel's instance.
    ///
    /// # Panics
    ///
    /// Panics for a state owner's instance: translation lets only stateless kernels run as stateless steps.
    fn slab(self) -> usize {
        match self {
            Self::Slab(index) => index,
            Self::Instance(handle) => panic!("a stateless step runs the state owner's instance {}", handle),
        }
    }
}

impl Run<'_, '_> {
    /// Runs an entry's ops over the buffers it starts with, in register 0.
    fn entry<I>(self, entry: &EntryCode, batches: I)
    where
        I: IntoIterator<Item = EventsBuffer>,
    {
        let Run {
            layout,
            kernel_slots,
            functions,
            decisions,
            counts,
            slab,
            mut instances,
            state_params,
            scratch,
            cursor,
            header,
            turn,
            exits,
        } = self;
        let Scratch {
            registers,
            pending,
            decide,
            counters,
            written,
        } = scratch;
        if registers.len() < entry.registers {
            registers.resize_with(entry.registers, Vec::new);
        }
        registers[0].extend(batches);

        for op in &entry.ops {
            match op {
                Op::Map { kernel, register } => {
                    cursor.node = Some(NodeRef::Kernel(*kernel));
                    let slot = slab.get_mut(kernel_slots[*kernel].slab());
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
                    let slot = slab.get_mut(kernel_slots[*kernel].slab());
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
                Op::Decide {
                    decision,
                    input,
                    targets,
                    registers: register_exits,
                    drops: drop_exits,
                } => {
                    cursor.node = Some(NodeRef::Decision(*decision));
                    // Translation compiles the function of every decision a step runs.
                    let (Some(Some(code)), Some(state)) = (functions.get(*decision), decisions.get_mut(*decision))
                    else {
                        continue;
                    };
                    let base = layout.decision_ports[*decision];
                    let ProgramCounts {
                        decision_ports, drops, ..
                    } = &mut *counts;
                    let mut step = DecideStep {
                        code,
                        state,
                        ports: &mut decision_ports[base..base + code.function.port_count()],
                        targets,
                        register_exits: *register_exits,
                        drop_exits: *drop_exits,
                        drops,
                    };
                    std::mem::swap(pending, &mut registers[*input]);
                    for buffer in pending.drain(..) {
                        step.run(buffer, registers, decide);
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
                        if let Some(written) = written.get_mut(exit.get()) {
                            *written = true;
                        }
                        exits.emit(*exit, buffer);
                    }
                }
                Op::Drop { register, drop } => {
                    cursor.node = None;
                    for buffer in registers[*register].drain(..) {
                        add_loss(&mut counts.drops[*drop], buffer.len() as u64, points_in(&buffer));
                    }
                }
                Op::State {
                    kernel,
                    register,
                    params,
                } => {
                    cursor.node = Some(NodeRef::Kernel(*kernel));
                    // Translation lets state steps run only in an isolation slot, whose generations bind instances.
                    let instance = match (instances.as_deref_mut(), kernel_slots[*kernel]) {
                        (Some(instances), KernelBinding::Instance(handle)) => {
                            instances.get_mut(handle).map(|instance| (handle, instance))
                        }
                        _ => None,
                    };
                    let Some((handle, instance)) = instance else {
                        debug_assert!(false, "a state step's generation binds an instance");
                        for buffer in registers[*register].drain(..) {
                            counters.record_loss(LossClass::Invalid, buffer.len() as u64, points_in(&buffer));
                        }
                        continue;
                    };
                    cursor.instance = Some(handle);
                    let param = state_params[*params].params.as_ref();
                    std::mem::swap(pending, &mut registers[*register]);
                    for buffer in pending.drain(..) {
                        consume_state(&mut instance.kernel, header, turn, counters, param, buffer);
                    }
                    cursor.instance = None;
                    fold_loss(&mut counts.kernel_loss[*kernel], counters);
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

/// One decision step over the buffers of its input register.
struct DecideStep<'a> {
    code: &'a DecisionCode,
    state: &'a mut DecisionState,
    /// The decision's per-port counts, derived from its counts per exit set.
    ports: &'a mut [u64],
    targets: &'a [Target],
    register_exits: ExitSet,
    drop_exits: ExitSet,
    /// Policy loss per `drop` node.
    drops: &'a mut [LossCount],
}

impl DecideStep<'_> {
    /// Runs the decision over one buffer.
    ///
    /// It computes each event's exit set (from the memo, for a memoized decision), counts events per exit set for
    /// telemetry and policy loss (with their points, for the sets that lead to a `drop`) and per exit for the buffers'
    /// capacities, then moves each event to the target of every exit in its set: the original to one target and
    /// copies to the others, in buffer order everywhere. Three ways of moving them, from cheapest:
    ///
    /// - every event has the same exit set: the buffer moves whole to its last register target, and every other
    ///   register target receives a copy;
    /// - every event leaving by a register target leaves by the one most events leave by: the original buffer keeps
    ///   those events, in place, and every other target receives copies in a buffer of exactly their size;
    /// - otherwise: every target receives a buffer of exactly its size, each event moving to the last target in its set
    ///   and copied to the others.
    fn run(&mut self, mut buffer: EventsBuffer, registers: &mut [Vec<EventsBuffer>], scratch: &mut DecideScratch) {
        let events = buffer.len();
        if events == 0 {
            return;
        }
        let function = &self.code.function;
        let DecideScratch {
            sets,
            exit_events,
            outputs,
        } = scratch;
        exit_events.clear();
        exit_events.resize(self.targets.len(), 0);

        // Each event's exit set, counted per run of equal sets, with the points of the runs that lead to a `drop`.
        let mut with_register = 0u64;
        let uniform = match function.constant() {
            Some(set) => {
                let points = if set.intersects(&self.drop_exits) {
                    points_in(&buffer)
                } else {
                    0
                };
                with_register += self.count(set, events as u64, points, exit_events);
                Some(set)
            }
            None => {
                sets.clear();
                match self.state.memo.as_mut() {
                    Some(memo) => sets.extend(
                        (&buffer)
                            .into_iter()
                            .map(|event| function.evaluate_memoized(event, memo)),
                    ),
                    None => sets.extend((&buffer).into_iter().map(|event| function.evaluate(event))),
                }
                let mut run = (sets[0], 0u64, 0u64);
                let mut changes = 0;
                for (event, set) in (&buffer).into_iter().zip(sets.iter()) {
                    if *set != run.0 {
                        with_register += self.count(run.0, run.1, run.2, exit_events);
                        changes += 1;
                        run = (*set, 0, 0);
                    }
                    run.1 += 1;
                    if set.intersects(&self.drop_exits) {
                        run.2 += points_of(event);
                    }
                }
                with_register += self.count(run.0, run.1, run.2, exit_events);
                (changes == 0).then(|| sets[0])
            }
        };

        if let Some(set) = uniform {
            let kept = set.intersection(self.register_exits);
            let Some(last) = kept.last() else { return };
            for exit in kept.iter().filter(|exit| *exit != last) {
                if let Target::Register(register) = self.targets[exit.get()] {
                    registers[register].push(buffer.clone());
                }
            }
            if let Target::Register(register) = self.targets[last.get()] {
                registers[register].push(buffer);
            }
            return;
        }

        // The register target most events leave by, the later one of equals: if every event leaving by a register
        // target leaves by it, the original buffer keeps exactly its events.
        let keeper = self
            .register_exits
            .iter()
            .max_by_key(|exit| (exit_events[exit.get()], exit.get()))
            .filter(|keeper| exit_events[keeper.get()] == with_register);
        outputs.clear();
        outputs.resize_with(self.targets.len(), || None);
        for exit in self.register_exits.iter() {
            let count = exit_events[exit.get()];
            if count > 0 && Some(exit) != keeper {
                outputs[exit.get()] = Some(EventsBuffer::with_capacity(count as usize));
            }
        }

        match keeper {
            Some(keeper) => {
                let mut index = 0;
                buffer.retain(|event| {
                    let set = sets[index];
                    index += 1;
                    for exit in set.intersection(self.register_exits).iter() {
                        if exit != keeper {
                            push(&mut outputs[exit.get()], event.clone());
                        }
                    }
                    set.contains(keeper)
                });
                if !buffer.is_empty() {
                    if let Target::Register(register) = self.targets[keeper.get()] {
                        registers[register].push(buffer);
                    }
                }
            }
            None => {
                for (event, set) in buffer.into_iter().zip(sets.iter()) {
                    let kept = set.intersection(self.register_exits);
                    let Some(last) = kept.last() else { continue };
                    for exit in kept.iter().filter(|exit| *exit != last) {
                        push(&mut outputs[exit.get()], event.clone());
                    }
                    push(&mut outputs[last.get()], event);
                }
            }
        }
        for (target, output) in self.targets.iter().zip(outputs.iter_mut()) {
            if let (Target::Register(register), Some(output)) = (target, output.take()) {
                registers[*register].push(output);
            }
        }
    }

    /// Counts `events` events, with `points` points, with the exit set `set`: per exit set for telemetry and policy
    /// loss, and per register exit in `exit_events`. Returns `events` if the set has a register exit, and zero
    /// otherwise.
    fn count(&mut self, set: ExitSet, events: u64, points: u64, exit_events: &mut [u64]) -> u64 {
        if !self.state.counts.add(set, events, points) {
            // The counts hold as many distinct sets as they can: derive the per-port counts and loss early, and count
            // on.
            derive_counts(self.code, &self.state.counts, self.ports, self.drops);
            self.state.counts.clear();
            let counted = self.state.counts.add(set, events, points);
            debug_assert!(counted, "cleared counts have room for a set");
        }
        let kept = set.intersection(self.register_exits);
        for exit in kept.iter() {
            exit_events[exit.get()] += events;
        }
        if kept.is_empty() {
            0
        } else {
            events
        }
    }
}

/// Appends an event to a decision's output buffer, which was created with room for every event it receives.
fn push(output: &mut Option<EventsBuffer>, event: Event) {
    if let Some(output) = output {
        let rejected = output.try_push(event);
        debug_assert!(rejected.is_none(), "decision outputs have room for every event");
    }
}

/// Moves a buffer to where a kernel port leads.
fn deliver(target: Target, buffer: EventsBuffer, registers: &mut [Vec<EventsBuffer>], counts: &mut ProgramCounts) {
    match target {
        Target::Register(register) => registers[register].push(buffer),
        Target::Drop(drop) => add_loss(&mut counts.drops[drop], buffer.len() as u64, points_in(&buffer)),
    }
}
