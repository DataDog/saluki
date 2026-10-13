//! The reference interpreter: literal execution of a plan over a trace.
//!
//! The interpreter is the oracle that the compiler and planner are checked against, and what preview runs. It never
//! runs in production. It executes a plan over a [`Trace`] literally: one trace event at a time, in trace order,
//! visiting every node, with no rewrites and no fusion.
//!
//! - Routing operators (`split`, `tee`, `merge`, `emit`, `drop`) are executed by the interpreter itself, with
//!   [compiled predicates][crate::predicate::CompiledPredicate] whose operands come from the plan's resolved values,
//!   or, for a binding the plan leaves unresolved, from the value the catalog carries for it. A catalog exported by a
//!   host carries its bindings' values, so an exported program, whose bindings are unresolved, can be previewed.
//! - Operators with a [kernel][crate::operators::KernelSpec] run it, with an unlimited budget. Every synchronous native
//!   operator other than the routing operators has one, extracted from its native component.
//! - State owners behind an isolation fence ([`Boundary::Isolated`][crate::operators::Boundary]), such as
//!   aggregation, run their stateful kernels as an isolation slot runs them: see
//!   [isolated state owners](#isolated-state-owners).
//! - Opaque hosts run through an [`OpaqueHostAdapter`] registered per node, whose output is **re-admitted** downstream.
//! - Sources are where `admit` events enter.
//!
//! # Trace events
//!
//! | Event | Effect |
//! |---|---|
//! | `admit(source, lane, t_a, batch)` | Stamps the batch at a lane of the source and delivers it ([lanes](#lanes)). |
//! | `heartbeat(source, lane, t_a)` | Stamps a header without events, which moves watermarks but delivers nothing. |
//! | `advance(clock)` | Runs a flush tick at host time `clock`; see [state owners](#isolated-state-owners). |
//! | `install(plan)` | Makes a valid plan active, at once or at a cut; see [installations](#installations). |
//! | `cancel` | Cancels the cut in flight, if it can be; see [installations](#installations). |
//! | `close` | Ends input: every kernel and host finishes under [`DrainPolicy::Shutdown`]. |
//! | `signal(name, value)` | Records the signal's value. No operator reads signals yet, so it has no effect. |
//! | `capacity(output, n)` | Bounds the output to `n` more records until the next bound; see [capacity](#capacity). |
//!
//! # Delivery order
//!
//! Each trace event that moves records (`admit`, `advance`, `close`, and the retirement of an installation's old plan)
//! is one **pass** over the plan, or, where it moves records of several generations, one pass over each generation's
//! plan, the oldest first. Nodes run in topological order, choosing among ready nodes in plan order, with the `emit`s
//! last, in plan order. When its turn comes, a node runs every batch waiting at it, input port by input port, and in
//! arrival order within a port; a state owner then runs its tick or finish, if the event calls for one. A `merge`
//! therefore passes its inputs on in port order, as a region's `concat` step does, and every port carries the records
//! of its node's input ports in that order, each path keeping its own order.
//!
//! The order in which an output receives records thus follows from the plan's dataflow alone: from what each node
//! does to the order of its input, from `merge` port order, and from the plan order of the `emit`s. It never depends on
//! how many nodes a path passes, on how the records a port carries are cut into batches, or on which of two nodes that
//! do not reach each other runs first. A rewrite that keeps every remaining port's records and order (removing a node
//! a path passes without changing what it carries, or a `merge` input that carries nothing) keeps what every output
//! receives, in order, and with it what a `capacity` bound accepts: bounds count records in this order. The
//! specification leaves the order of records arriving at a `merge` from different paths open; this is the
//! interpreter's choice.
//!
//! # Time
//!
//! The trace states two clocks. Each `admit` carries the admission time its source stamped, and each `advance` the
//! host time of a flush tick, which is also the wall clock the tick reads. Ticks follow a monotonic timer, so `advance`
//! clocks never decrease, unless the wall clock they read jumps back by more than the clock-reset threshold, which
//! starts a new timeline for the ticks as for the admission clocks. A batch may be stamped earlier than the last tick,
//! as a batch that waited in a queue would be: it **arrives** at the later of the last tick and its admission time.
//!
//! A source's admission clock stamps the later of its latest time and the wall clock, so its admission times never
//! decrease, except at a **clock reset**: a stamp more than the
//! [clock-reset threshold][crate::limits::values::CLOCK_RESET_THRESHOLD] earlier than the clock's latest one resets
//! it, and it continues in a new epoch. A stamp earlier than its own point's latest one by no more than the threshold
//! is an error: the clock would have stamped its latest time again, so the trace states that time instead.
//!
//! # Lanes
//!
//! A source given lanes ([`Interpreter::with_source_lanes`]) admits at each lane as an admission point of its own, as
//! the producer lanes of a partitioned frame do, and every lane stamps by the source's **one clock**: one latest time,
//! one epoch, and one reset decision, so a backward jump at any lane is one reset for every lane, and a lane that has
//! not stamped since stamps its next batch in the new epoch. The trace is in arrival order, and lanes reach a state
//! owner on paths of their own, so a lane may state a stamp earlier than another lane's latest one: within the
//! threshold that is no reset, and only each lane's own stamps must not move back. A state owner's watermark is the
//! earliest of its lanes' latest times; a lane that has stamped nothing holds it back.
//!
//! The interpreter has no lanes of its own beyond that: one instance per lineage holds every lane's contexts, and is
//! the lane-less oracle that a frame's state lanes, which share their lineage's tick and flush rules, are checked
//! against. A cut's boundary records each lane at its clock's epoch, and is armed from the clock's latest time.
//!
//! **Reset windows.** A state owner whose upstream points are the lanes of one source runs a reset as the shared [reset
//! window][crate::runtime::state::rules::ResetWindow] says, as a state lane does: the first header of a newer epoch
//! from any lane opens the window, and every instance holding older-epoch data stops ticking (a tick waiting at the
//! gate is dropped). A lineage's first newer-epoch data gets a fresh, empty instance of that epoch, which inherits the
//! lineage's cut, while the older-epoch instance waits. Once every lane has stamped in the newest epoch, the window
//! closes: each older epoch, oldest first, drains at its own watermark (each lane's final latest time in it, a lane
//! that skipped it left out), a cut-retiring instance at the cut's time at the latest and, in the cut's publication
//! epoch, for good; the output clock starts a new epoch after each; and each lineage continues on its newest-epoch
//! instance, or on its drained one, empty. A retiring lineage inside a window crosses at its close; its cut finalizes
//! once every point passed it, as the installer does, and the program the lineage served stays live until its final
//! flush. With one lane the window opens and closes on one header, which is a reset at one point. A state
//! owner over points of several clocks keeps the one-point rule at each of them: a reset drains everything it holds.
//!
//! # Candidate mode
//!
//! [`Interpreter::with_candidates`] records, for every bucket of every series an isolated state owner emits, what an
//! interleaving of its lanes could have made it hold instead ([`Candidates`]): each admission point's last admission
//! into the bucket (untimestamped values in the bucket of their admission time, timestamped ones at their timestamps,
//! as aggregation buckets them), with its kind as the state owner emits it (aggregation emits a counter as a rate over
//! its bucket width), a gauge's value, the instance and epoch that admitted it, and its place among the run's
//! admissions; and the kinds and metadata the series' entry was admitted with. Each emitted record carries the
//! candidates of every bucket of its entry through everything downstream (a stateless kernel runs a tagged record
//! alone, and whatever the record becomes carries them), and each output records them under its own view of the
//! series, for [`compare_partitioned`][crate::equivalence::compare_partitioned]: buckets whose points a kind replaced
//! before any flush included. The candidates of an entry's kinds and metadata cover everything admitted to the
//! instance under its context in its epoch, which contains what the entry was admitted with while it lived.
//!
//! # Watermarks
//!
//! There are no queues, so every state owner sees every stamp at its upstream admission points at once, and its
//! watermark is the earliest of the latest admission times of the admission points upstream of it: sources, and the
//! outputs of opaque hosts, which re-admit. An admission point that has admitted nothing holds the watermark back.
//! Opaque hosts receive their watermark at every tick.
//!
//! # Capacity
//!
//! `capacity(output, n)` bounds an output: from that event on it accepts at most `n` more records, until the next
//! `capacity` event for it replaces the bound. Bounds belong to outputs, so they hold across installations. An output
//! accepts records in delivery order while its bound lasts; every record beyond it is **capacity loss**, counted as
//! `capacity` loss of the `emit` node that delivered it and kept in [`RunOutput::refused`], so that two executions'
//! losses can be compared with [`compare_under_capacity`][crate::equivalence::compare_under_capacity].
//!
//! # Isolated state owners
//!
//! A state owner behind an isolation fence runs its stateful kernel as an isolation slot's region does, by the shared
//! [time rules][crate::runtime::state::rules]. Its state belongs to a **lineage**, keyed by its node and its state
//! contract (for aggregation, its window), not to a generation: generations whose plans keep the lineage share one
//! instance of it, and each call passes the kernel the parameters of the generation it runs under
//! ([`OperatorDef::step_params`][crate::operators::OperatorDef::step_params]): a batch's, or a flush's.
//!
//! - Its watermark comes from a [watermark tracker][crate::runtime::state::rules::WatermarkTracker] over the
//!   admission points upstream of it, which observes every stamp at those points as it is made.
//! - `advance(clock)` is a flush tick with wall time `clock`, decided by the [`TickGate`]: it fires at the tick's whole
//!   second once the watermark reached it, and is otherwise gated. A gated tick fires before the batch of the first
//!   later `admit` or `heartbeat` that raises the watermark to that second, or, at the latest, before the tick of the
//!   first later `advance` whose clock is at least the grace past the gated one, at the watermark's whole second.
//!   [`Interpreter::with_heartbeats_at_ticks`] makes every source heartbeat at each `advance`'s clock first, so that
//!   every tick fires at once: the no-queueing mode, in which a tick flushes exactly as native aggregation's would.
//! - A clock reset at an upstream point runs a reset drain before the batch that caused it: the kernel flushes at the
//!   pre-reset watermark, emits everything it holds, and continues empty, in the new timeline. Over the lanes of one
//!   source, the reset runs as a [reset window](#lanes), which the last lane to switch closes.
//! - `close` finishes it at the later of the latest admission time it saw in its epoch and the last tick's clock,
//!   closing an open reset window first.
//! - Its time-driven output, from ticks, reset drains, final retirements, and `close`, is stamped as an admission
//!   point of its own would stamp it: at the flush time, never earlier than its previous output in the same epoch,
//!   which a reset drain ends. It belongs to the generation the shared rules choose when the flush starts
//!   ([`stamping_generation`][crate::runtime::state::rules::stamping_generation]): the one a retiring lineage served,
//!   or else the schedule's, the new generation of a cut once every upstream point has switched to it. It flows through
//!   that generation's plan.
//! - Every lineage of every state owner draws its contexts from one state budget
//!   ([`Interpreter::with_state_budget`]), and every lineage of one node from that node's.
//!
//! Every isolated state owner fed by the same admission points stamps its time-driven output by one output clock, as
//! the isolation slot that runs every state owner over a point in a host does: a slot's latch is one, whichever of its
//! state owners flushed last. The frame gives each state owner of a boot plan a slot of its own, so a region compared
//! with the interpreter over a boot plan with several state owners over one point latches each owner apart.
//!
//! # Opaque hosts and segments
//!
//! An opaque host's output is re-admitted at the host's output: stamped by the host's own admission clock, which reads
//! the arrival time for output emitted while admitting a batch and the tick's clock for output emitted at a tick, and
//! with the generation active at that moment. At every tick, every output of the host either re-admits output or
//! heartbeats at the tick's clock. Isolated state owners downstream observe those stamps for their watermark, but a
//! gated tick waits for the next stamp at a source, or the next `advance`, to fire.
//!
//! # Admission
//!
//! Sources and opaque host outputs are where records enter the plan, so the interpreter checks each record entering
//! there against the port's type, which validation computed from the source's binding or the host's declared outputs.
//! A record outside it is an error: no host produces it, and validation's typing assumes it never arrives.
//!
//! A source's stamp is assigned a generation by the schedule, as an admission point's is: the active generation, or,
//! while a cut is in flight, the old generation for a stamp before the cut's time and the new one from it on (and
//! after a clock reset since the cut was armed, which counts as reaching it). The batch then runs through that
//! generation's plan.
//!
//! # Installations
//!
//! An invalid plan is rejected and the active plan stays, as a failed preparation leaves the active generation
//! unchanged. State owners correspond by node ID, and each one's transition is classified by contract, as the installer
//! classifies it:
//!
//! - **`retain`**: a state owner whose lineage the new plan keeps, whatever its other parameters, keeps its instance
//!   and its state; time-driven output then flows through the newest generation that binds it.
//! - **`cut`**: a lineage the new plan changes (a changed window), adds, or removes. The installation **arms a cut** at
//!   `T`, the first instant at least the [cut margin][crate::limits::values::CUT_MARGIN] after the latest of the
//!   affected admission points' latest stamps and the last tick's clock that is a multiple of the least common
//!   multiple of the widths involved, which must be at most
//!   [`MAX_TRANSITION_LCM`][crate::limits::values::MAX_TRANSITION_LCM]. Stamps before `T` run in the old generation and
//!   stamps from it on in the new one. A retiring lineage keeps flushing, never at or after `T`, until the first stamp
//!   of the new generation reaches it from every upstream point: then, before that batch is admitted, it makes its
//!   final flush at `T` and emits every point it still holds, under the old generation. The cut **finalizes** once
//!   every affected point stamps only the new generation: the old plan then retires.
//! - **`reject`**: the installation is rejected.
//!
//! An installation without a cut commits at once: there are no queues, so no batch is in flight when it does. The
//! kernels of the old plan that are not carried over finish under [`DrainPolicy::Retire`], in the old generation.
//!
//! An `install` is validated and prepared when it is proposed, and rejected at once if it is invalid. While a cut is
//! in flight, a valid one waits: it starts when the cut finalizes, classified against the plan then active, and a later
//! valid `install` replaces it ([`InstallOutcome::Superseded`]). A `cancel` cancels the cut if no admission point
//! stamped a batch or heartbeat with the new generation yet: the old generation stays active, and the installation
//! waiting for the cut, if any, is superseded by the cancel, since what cancels a cut proposes its own installation
//! next. A cancel that is refused changes nothing. The installer's proposals map onto these events: a newer revision is
//! a `cancel` followed by an `install`; a re-composition of the revision in flight, or a default-program change, is an
//! `install`, which waits; a revocation is a `cancel` followed by an `install` of the default program. Either way, a
//! refused cancel leaves the `install` that follows it waiting for the cut, replacing what waited before it.
//!
//! # Missing
//!
//! - State owners outside an isolation fence, such as aggregation in its host component, keep their earlier rules:
//!   they carry over only unchanged, a clock reset upstream of one is not supported, and no cut is armed while a plan
//!   has one.
//! - A clock reset at one of several admission points of different clocks upstream of a state owner drains everything
//!   it holds, as a reset at its only point does: the frame runs no state owner over points of several clocks.
//! - A gated tick does not fire before a batch an opaque host re-admits upstream of it: it fires at the next stamp of a
//!   source, or the next `advance`.
//!
//! # Runtime
//!
//! Kernels are built and run inside a current-thread Tokio context that the interpreter creates and enters for each
//! run, so that a kernel may spawn its cache maintenance tasks. Those tasks are never driven: their effects are
//! unobservable.

use std::{
    any::Any,
    collections::{BTreeMap, BTreeSet},
    fmt,
    num::NonZeroU64,
    sync::Arc,
    time::{Duration, Instant},
};

use saluki_core::{
    data_model::event::{metric::Metric, Event},
    execution::{
        AdmissionPointId, AdmissionSchedule, AdmissionTime, BatchHeader, Budget, DrainPolicy, GenerationId,
        GenerationRef, LossClass, PortIndex, PortOutputs, Progress, Turn, TurnCounters,
    },
    topology::EventsBuffer,
};
use snafu::Snafu;

use crate::{
    compile::LineageKey,
    equivalence::{BucketCandidates, Candidates, ContextIdentity},
    extension::TransitionClass,
    ir::{NodeId, OutputId, PortName, SignalName},
    limits,
    model::{EventView, Kind, TimestampForm},
    operators::{ConfiguredOperator, KernelBuildError, KernelSpec, OperatorRegistry},
    predicate::PredicateError,
    runtime::state::{
        rules::{CutError, OutputClock, TickGate},
        StateBudgets,
    },
    validate::{describe_port_type, Catalog, ValidationReport},
};

mod capacity;
use self::capacity::OutputCapacity;

mod drains;
mod installation;

mod host;
pub use self::host::{AdapterError, HostBatch, HostFactory, HostFinish, HostOutput, HostTick, OpaqueHostAdapter};

mod program;
use self::program::{AdmissionPoints, Exec, PrepareError, Program, Stamp};

#[cfg(any(test, feature = "test-util"))]
pub mod replay;

mod state;
use self::state::{CutInFlight, Instance, Instances, Retiring, SlotTime};

mod trace;
pub use self::trace::{SignalValue, Trace, TraceEvent};

/// The most times in a row a kernel may yield before the interpreter gives up on it.
///
/// Kernels run with an unlimited budget here, so a kernel that keeps yielding is not making progress.
const MAX_YIELDS: usize = 100_000;

/// Every loss class, for adding up per-turn counters.
const LOSS_CLASSES: [LossClass; 4] = [
    LossClass::Policy,
    LossClass::Capacity,
    LossClass::Invalid,
    LossClass::Shutdown,
];

/// Returns the batch size limit, in events.
fn batch_size() -> usize {
    limits::values::BATCH_SIZE
}

/// Why an installation was rejected. The active plan, if any, stays active.
#[derive(Debug)]
pub enum InstallRejection {
    /// The plan does not validate.
    Invalid {
        /// The validation report, with every finding.
        report: ValidationReport,
    },

    /// A state owner's transition from the active plan is `reject`.
    Transition {
        /// The state owner.
        node: NodeId,

        /// The transition class.
        class: TransitionClass,
    },

    /// The installation needs a cut whose bucket widths line up less often than a cut may wait for.
    CutWidths {
        /// Why no cut time can be computed.
        source: CutError,
    },

    /// A predicate's configuration binding has no value: the plan has not resolved it, and the catalog carries none.
    UnresolvedBinding {
        /// The node.
        node: NodeId,

        /// The underlying error.
        source: PredicateError,
    },

    /// A kernel could not be built from its parameters.
    KernelBuild {
        /// The node.
        node: NodeId,

        /// The underlying error.
        source: KernelBuildError,
    },

    /// An opaque host's adapter could not be built.
    HostBuild {
        /// The node.
        node: NodeId,

        /// The underlying error.
        source: AdapterError,
    },
}

impl fmt::Display for InstallRejection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid { report } => {
                let errors = report.errors().map(ToString::to_string).collect::<Vec<_>>();
                write!(f, "the plan is invalid: {}", errors.join("; "))
            }
            Self::Transition { node, class } => write!(
                f,
                "node `{}` keeps state, and its transition to the new plan is `{}`",
                node, class
            ),
            Self::CutWidths { source } => write!(f, "the installation needs a cut: {}", source),
            Self::UnresolvedBinding { node, source } => write!(f, "node `{}`: {}", node, source),
            Self::KernelBuild { node, source } => write!(f, "node `{}`: {}", node, source),
            Self::HostBuild { node, source } => write!(f, "node `{}`: {}", node, source),
        }
    }
}

/// What happened to one `install` or `cancel` event.
#[derive(Debug)]
pub enum InstallOutcome {
    /// The plan became active at once, as the given generation.
    Active {
        /// The new generation.
        generation: GenerationId,
    },

    /// The plan's state owners change their lineages, so the plan becomes active at a cut: batches stamped from `at`
    /// on run as the given generation, earlier ones as the generation it replaces.
    Cut {
        /// The new generation.
        generation: GenerationId,

        /// The cut time.
        at: AdmissionTime,
    },

    /// A `cancel` event cancelled the cut in flight before any admission point stamped a batch or heartbeat with its
    /// generation: the generation it replaced stays active, and the installation waiting for the cut, if any, is
    /// superseded by the cancel.
    Cancelled,

    /// A `cancel` event cancelled nothing: an admission point had already stamped a batch or heartbeat with the
    /// generation of the cut in flight, so the cut proceeds, or no cut was in flight.
    CancelRefused,

    /// The installation waited for a cut in flight to finish, and a later event, at the given position in the trace,
    /// replaced it before it started: a valid `install`, since the latest proposal wins, or a `cancel` that cancelled
    /// the cut, since what cancels a cut proposes its own installation next.
    Superseded {
        /// The position of the replacing `install` or `cancel` event in the trace.
        by: usize,
    },

    /// The trace ended while the installation waited for a cut in flight to finish.
    Waiting,

    /// The installation was rejected.
    Rejected(InstallRejection),
}

/// One `install` or `cancel` event and its outcome.
#[derive(Debug)]
pub struct InstallRecord {
    /// The position of the event in the trace, counting from zero.
    pub event: usize,

    /// What happened.
    pub outcome: InstallOutcome,
}

/// One record delivered to an output.
#[derive(Clone, Debug, PartialEq)]
pub struct EmittedRecord {
    /// The generation whose plan delivered the record.
    pub generation: GenerationId,

    /// The position in the trace of the `install` event that created that generation.
    pub install: usize,

    /// The admission time of the batch that carried the record: the time its last admission point stamped, or, for
    /// a state owner's time-driven output, its flush time. `None` for time-driven output of a kernel that is not
    /// isolated when no upstream admission point has stamped anything.
    pub admitted_at: Option<AdmissionTime>,

    /// The record.
    pub metric: Metric,
}

/// What one run of the interpreter over a trace produced.
#[derive(Debug, Default)]
pub struct RunOutput {
    outputs: BTreeMap<OutputId, Vec<EmittedRecord>>,
    refused: BTreeMap<OutputId, Vec<EmittedRecord>>,
    installs: Vec<InstallRecord>,
    loss: BTreeMap<NodeId, TurnCounters>,
    watermarks: BTreeMap<NodeId, Option<AdmissionTime>>,
    signals: BTreeMap<SignalName, SignalValue>,
    candidates: Candidates,
}

impl RunOutput {
    /// Returns every output any installed plan declared, with the records it received, in delivery order.
    pub fn outputs(&self) -> impl Iterator<Item = (&OutputId, &[EmittedRecord])> {
        self.outputs.iter().map(|(id, records)| (id, records.as_slice()))
    }

    /// Returns the records an output received, in delivery order. An output no plan declared received nothing.
    pub fn output(&self, id: &OutputId) -> &[EmittedRecord] {
        self.outputs.get(id).map_or(&[], Vec::as_slice)
    }

    /// Returns the records an output did not accept, because a `capacity` event bounded it, in delivery order: its
    /// capacity loss. An output never bounded refused nothing.
    pub fn refused(&self, id: &OutputId) -> &[EmittedRecord] {
        self.refused.get(id).map_or(&[], Vec::as_slice)
    }

    /// Returns every output that refused records, with the records it refused, in delivery order.
    pub fn refused_outputs(&self) -> impl Iterator<Item = (&OutputId, &[EmittedRecord])> {
        self.refused.iter().map(|(id, records)| (id, records.as_slice()))
    }

    /// Returns every `install` and `cancel` event's outcome, in trace order.
    pub fn installs(&self) -> &[InstallRecord] {
        &self.installs
    }

    /// Returns the loss a node counted, by class, over the whole run.
    ///
    /// `drop` nodes count policy loss; kernels count what their turns report. Loss is telemetry: it is not part of
    /// equivalence.
    pub fn loss(&self, node: &NodeId) -> TurnCounters {
        self.loss.get(node).copied().unwrap_or_default()
    }

    /// Returns a state owner's watermark at the end of the run, under the plan active then.
    ///
    /// Returns `None` if the node is not a state owner of that plan, or if one of its upstream admission points has
    /// admitted nothing.
    pub fn watermark(&self, node: &NodeId) -> Option<AdmissionTime> {
        self.watermarks.get(node).copied().flatten()
    }

    /// Returns the last value the trace gave a signal.
    pub fn signal(&self, name: &SignalName) -> Option<SignalValue> {
        self.signals.get(name).copied()
    }

    /// Returns what candidate mode recorded ([`Interpreter::with_candidates`]): empty for a run without it.
    pub fn candidates(&self) -> &Candidates {
        &self.candidates
    }
}

/// What went wrong running a trace.
#[derive(Debug, Snafu)]
#[snafu(context(suffix(false)))]
pub enum InterpretErrorKind {
    /// The Tokio runtime the interpreter runs kernels in could not be built.
    #[snafu(display("Failed to build the interpreter's Tokio runtime: {}", source))]
    Runtime {
        /// The underlying error.
        source: std::io::Error,
    },

    /// An event needs an active plan, and none is active.
    #[snafu(display(
        "No plan is active; begin the trace with an `install` of a valid plan{}",
        last_rejection.as_ref().map(|reason| format!(" (the last installation was rejected: {})", reason))
            .unwrap_or_default()
    ))]
    NoActivePlan {
        /// Why the last installation was rejected, if one was.
        last_rejection: Option<String>,
    },

    /// An `admit` or `heartbeat` names a node that is not a source of the plan its stamp runs in.
    #[snafu(display(
        "`{}` is not a `source` node of the active plan; `admit` and `heartbeat` events name the source a batch \
         enters at",
        node
    ))]
    UnknownSource {
        /// The named node.
        node: NodeId,
    },

    /// An `admit` or `heartbeat` names a lane its source does not have.
    #[snafu(display(
        "Source `{}` has {} lane(s), so it has no lane {}; give it lanes with `Interpreter::with_source_lanes`",
        node,
        lanes,
        lane
    ))]
    UnknownLane {
        /// The source.
        node: NodeId,

        /// The lane the event names.
        lane: u16,

        /// The source's lanes.
        lanes: u16,
    },

    /// An `advance` moves the host clock back by no more than the clock-reset threshold.
    #[snafu(display(
        "The host clock moves back from {} to {}; flush ticks follow a monotonic timer, so `advance` clocks never \
         decrease, unless the wall clock they read jumps back by more than the clock-reset threshold",
        previous,
        clock
    ))]
    ClockWentBackward {
        /// The clock of the previous tick.
        previous: AdmissionTime,

        /// The clock of this tick.
        clock: AdmissionTime,
    },

    /// A source's admission time moves back by no more than the clock-reset threshold, which an admission clock never
    /// stamps.
    #[snafu(display(
        "Admission point `{}` stamps {} after {}, a backward jump of at most the clock-reset threshold of {} s, which \
         an admission clock absorbs by stamping its latest time again: state the batch's admission time as {}, or \
         make the jump larger than the threshold, which resets the clock",
        point,
        admitted_at,
        previous,
        threshold.as_secs(),
        previous
    ))]
    AdmissionTimeMovedBack {
        /// The admission point, as `node.port`.
        point: String,

        /// The point's latest admission time.
        previous: AdmissionTime,

        /// The stamp the trace states.
        admitted_at: AdmissionTime,

        /// The clock-reset threshold.
        threshold: Duration,
    },

    /// An `admit` carries more events than one batch holds.
    #[snafu(display(
        "The batch has {} events, more than the batch size of {}; split it into several `admit` events",
        events,
        limit
    ))]
    BatchTooLarge {
        /// The number of events.
        events: usize,

        /// The batch size.
        limit: usize,
    },

    /// An event follows `close`.
    #[snafu(display("The trace continues after `close`, which ends the input; end the trace there"))]
    AfterClose,

    /// The trace or plan needs a capability that a later stage adds.
    #[snafu(display(
        "{} is not supported until stage {}; the reference interpreter never approximates it",
        capability,
        stage
    ))]
    Unsupported {
        /// What is not supported.
        capability: String,

        /// The capability stage that adds it.
        stage: u8,
    },

    /// An opaque host node has no adapter.
    #[snafu(display(
        "Opaque host `{}` has no adapter; register one with `Interpreter::with_opaque_host` to run it",
        node
    ))]
    MissingAdapter {
        /// The node.
        node: NodeId,
    },

    /// A kernel emitted an event that is not a metric.
    #[snafu(display("Node `{}` emitted an event that is not a metric; plans process only metrics", node))]
    NonMetricEvent {
        /// The node.
        node: NodeId,
    },

    /// A batch enters the plan with a record that its admission point's port type rules out.
    #[snafu(display(
        "Port `{}.{}` carries {}, but the batch has {}; a source admits only what its binding produces, and an opaque \
         host emits only the types its output ports declare, so correct the trace or the adapter",
        node,
        port,
        port_type,
        record
    ))]
    IllTyped {
        /// The node the batch enters the plan at: a source, or an opaque host.
        node: NodeId,

        /// The output port the batch leaves the node by.
        port: PortName,

        /// The port's type, described.
        port_type: String,

        /// The record the port cannot carry, described.
        record: String,
    },

    /// A kernel kept yielding without finishing its work.
    #[snafu(display(
        "The kernel of node `{}` yielded {} times in a row; with an unlimited budget, a kernel finishes its work",
        node,
        yields
    ))]
    KernelStalled {
        /// The node.
        node: NodeId,

        /// How many times it yielded.
        yields: usize,
    },

    /// The interpreter found a validated plan in a state validation rules out.
    #[snafu(display("Internal interpreter error: {}; this is a bug", message))]
    Internal {
        /// What was found.
        message: String,
    },
}

/// An error running a trace, with the position of the event that caused it.
#[derive(Debug)]
pub struct InterpretError {
    /// The position of the event in the trace, counting from zero, or `None` if the run could not start.
    pub event: Option<usize>,

    /// What went wrong.
    pub kind: InterpretErrorKind,
}

impl fmt::Display for InterpretError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.event {
            Some(event) => write!(f, "Trace event {}: {}", event, self.kind),
            None => write!(f, "{}", self.kind),
        }
    }
}

impl std::error::Error for InterpretError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.kind)
    }
}

/// The reference interpreter.
///
/// An interpreter validates every installed plan against its operator registry and catalog, and runs opaque hosts
/// through the adapters registered for their nodes. One interpreter can run any number of traces; each run starts
/// from nothing.
pub struct Interpreter<'a> {
    registry: &'a OperatorRegistry,
    catalog: &'a dyn Catalog,
    hosts: BTreeMap<NodeId, Box<HostFactory<'a>>>,
    heartbeats_at_ticks: bool,
    state_budget: Option<u64>,
    candidates: bool,
    source_lanes: BTreeMap<NodeId, u16>,
    arrival_order: bool,
}

impl<'a> Interpreter<'a> {
    /// Creates an interpreter that validates plans against `registry` and `catalog`.
    pub fn new(registry: &'a OperatorRegistry, catalog: &'a dyn Catalog) -> Self {
        Self {
            registry,
            catalog,
            hosts: BTreeMap::new(),
            heartbeats_at_ticks: false,
            state_budget: None,
            candidates: false,
            source_lanes: BTreeMap::new(),
            arrival_order: false,
        }
    }

    /// Reads the trace's stamps in arrival order, as an arrival replay (`replay::ArrivalReplay`) states them: a lane's
    /// stamp may be earlier than the lane's latest one by no more than the clock-reset threshold, where a trace in
    /// stamp order would be ill-formed. The batch is bucketed by its own admission time, and the state owners
    /// downstream see the lane's latest time unchanged, as each state lane saw its stamps in order.
    pub fn in_arrival_order(mut self) -> Self {
        self.arrival_order = true;
        self
    }

    /// Makes the source `source` admit on `lanes` lanes: each lane is an admission point of its own, and every lane
    /// stamps by the source's one clock, as the producer lanes of a partitioned frame do. A trace names the lane of
    /// each `admit` and `heartbeat`; a source without lanes has lane 0 only.
    ///
    /// The interpreter itself has no lanes: every state owner downstream sees every lane's stamps, in trace order, and
    /// its watermark is the earliest of the lanes' latest times. A lane that stamps nothing holds it back.
    pub fn with_source_lanes(mut self, source: NodeId, lanes: u16) -> Self {
        self.source_lanes.insert(source, lanes.max(1));
        self
    }

    /// Records, for every emitted gauge point, the last value each admission point admitted to its bucket, and for
    /// every entry the kinds and metadata admitted while it lived, carried with each emitted record to the outputs
    /// ([`RunOutput::candidates`]): what [`compare_partitioned`][crate::equivalence::compare_partitioned] checks a
    /// partitioned execution's gauges, kinds, and metadata against.
    ///
    /// See [candidate mode](#candidate-mode) for what is recorded, and how.
    pub fn with_candidates(mut self) -> Self {
        self.candidates = true;
        self
    }

    /// Makes every source of the live plans send a heartbeat at each `advance`'s clock before the tick, unless it
    /// already stamped a later time, as an idle source's admission point does: every isolated state owner's watermark
    /// then reaches the tick's clock, so every tick fires at once instead of waiting at the tick gate.
    pub fn with_heartbeats_at_ticks(mut self) -> Self {
        self.heartbeats_at_ticks = true;
        self
    }

    /// Bounds the contexts every lineage of every isolated state owner holds together to `capacity`, as a host's
    /// state budget does. Without it, each lineage is bounded by its own context limit, and every lineage of one node
    /// by that node's.
    pub fn with_state_budget(mut self, capacity: u64) -> Self {
        self.state_budget = Some(capacity);
        self
    }

    /// Registers how to build the adapter for the opaque host node `node`.
    ///
    /// The factory is called when a plan containing the node is first installed in a run. A later installation that
    /// keeps the node unchanged keeps its adapter, and its state.
    pub fn with_opaque_host<F>(mut self, node: NodeId, factory: F) -> Self
    where
        F: Fn(&ConfiguredOperator) -> Result<Box<dyn OpaqueHostAdapter>, AdapterError> + 'a,
    {
        self.hosts.insert(node, Box::new(factory));
        self
    }

    /// Runs a trace from the beginning, and returns what every output received.
    ///
    /// # Errors
    ///
    /// Returns an error if the trace is malformed, such as an `admit` before any plan is active, a batch larger than
    /// the batch size, or an admission time that moves back within the clock-reset threshold; if it needs a capability
    /// a later stage adds, such as an operator without a kernel; or if an opaque host has no adapter. A rejected
    /// installation is not an error: it is recorded in the output's [installs][RunOutput::installs].
    pub fn run(&self, trace: &Trace) -> Result<RunOutput, InterpretError> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .map_err(|source| InterpretError {
                event: None,
                kind: InterpretErrorKind::Runtime { source },
            })?;
        let result = {
            let _context = runtime.enter();
            Run::new(self).execute(trace)
        };
        // A runtime that may have been created inside another runtime's context cannot block when it shuts down.
        runtime.shutdown_background();
        result
    }
}

/// What candidate mode carries with one record an isolated state owner emitted, through everything downstream of it:
/// the candidates of every bucket of the record's entry, by timestamp.
type RecordTag = Arc<Vec<(Option<NonZeroU64>, BucketCandidates)>>;

/// A batch moving between nodes.
struct Batch {
    header: BatchHeader,
    metrics: Vec<Metric>,
    /// In candidate mode, the tag of each record, in order; empty when no record carries one.
    tags: Vec<Option<RecordTag>>,
}

/// Returns the tag of the record at `index` of a batch.
fn tag_at(tags: &[Option<RecordTag>], index: usize) -> Option<RecordTag> {
    tags.get(index).cloned().flatten()
}

/// A batch on its way into one input port: `port` of `node`.
struct Delivery {
    node: usize,
    port: usize,
    batch: Batch,
}

/// The deliveries one node's run causes, in the order it makes them.
type Queue = Vec<Delivery>;

/// The batches waiting at the nodes of one pass, by node, each with the input port it enters, in arrival order.
struct Pending {
    batches: Vec<Vec<(usize, Batch)>>,
}

impl Pending {
    fn new(program: &Program) -> Self {
        Self {
            batches: program.nodes.iter().map(|_| Vec::new()).collect(),
        }
    }

    fn add(&mut self, deliveries: Queue) {
        for Delivery { node, port, batch } in deliveries {
            self.batches[node].push((port, batch));
        }
    }

    /// Takes the batches waiting at a node: input port by input port, and in arrival order within each port.
    fn take(&mut self, node: usize) -> Vec<Batch> {
        let mut batches = std::mem::take(&mut self.batches[node]);
        // A stable sort, so that each port keeps its arrival order.
        batches.sort_by_key(|(port, _)| *port);
        batches.into_iter().map(|(_, batch)| batch).collect()
    }

    /// Returns a node that still has batches waiting, if any does.
    fn waiting(&self) -> Option<usize> {
        self.batches.iter().position(|batches| !batches.is_empty())
    }
}

/// What an isolated state owner's instance does in a drain: one call of time-driven work.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DrainKind {
    /// A flush tick.
    Tick,

    /// A clock reset's drain: a flush at the pre-reset watermark, then everything held.
    Reset,

    /// The final flush of a retiring lineage at its cut, then everything held.
    FinalRetire,

    /// The end of input.
    Shutdown,
}

/// One drain of one instance: its kind, the flush time its kernel runs at, the time its output is stamped at, and the
/// generation it runs under, decided when it starts.
struct Drain {
    instance: usize,
    kind: DrainKind,
    flush: Option<AdmissionTime>,
    stamp_at: AdmissionTime,
    generation: GenerationRef,
}

/// An installation waiting for the cut in flight to finish: the position of its `install` event, and its plan,
/// validated and prepared when it was proposed.
struct Queued {
    event: usize,
    program: Program,
}

/// How the state owners of a candidate correspond to those of the active plan.
#[derive(Default)]
struct Transition {
    /// The `(old, new)` positions of the state owners that are not isolated whose instances carry over.
    carried_local: Vec<(usize, usize)>,

    /// The candidate positions of isolated state owners that keep a lineage, with its instance.
    retained: Vec<(usize, usize)>,

    /// The candidate positions of isolated state owners that start a new lineage.
    added: Vec<usize>,

    /// The instances whose lineages end: those of state owners the candidate changes or removes.
    retiring: Vec<usize>,

    /// The bucket widths of the lineages that start and end.
    widths: Vec<Duration>,

    /// Whether there is an active plan to change from.
    replaces: bool,
}

impl Transition {
    /// Returns `true` if the transition needs a cut: a lineage starts or ends while another plan is active.
    fn is_cut(&self) -> bool {
        self.replaces && (!self.added.is_empty() || !self.retiring.is_empty())
    }
}

/// The state of one run.
struct Run<'r, 'a> {
    interpreter: &'r Interpreter<'a>,
    points: AdmissionPoints,
    gate: TickGate,
    /// The host clock: the clock of the last flush tick.
    clock: AdmissionTime,
    /// The instant the host clock's Unix epoch maps to, for the `now` of turns.
    origin: Instant,
    /// The live programs, by generation: the active one, and, while a cut is in flight, the one it replaces.
    programs: BTreeMap<GenerationId, Program>,
    /// The schedule that assigns generations to stamps, once a plan is active.
    schedule: Option<AdmissionSchedule>,
    instances: Instances,
    budgets: StateBudgets,
    /// The output clocks of the isolated state owners, by the admission points upstream of them, in ID order: every
    /// lineage of every state owner fed by the same points shares one, as the isolation slot running them does.
    output_clocks: BTreeMap<Vec<AdmissionPointId>, OutputClock>,
    /// The time of each isolation slot, by the same key: its points' progress and its reset window.
    slots: BTreeMap<Vec<AdmissionPointId>, SlotTime>,
    cut: Option<CutInFlight>,
    /// The generations a finalized cut replaced whose programs stay live while a lineage they served still retires
    /// inside a reset window: its final flush runs through the program it served.
    replaced: BTreeSet<GenerationId>,
    queued: Option<Queued>,
    generations: u64,
    closed: bool,
    last_rejection: Option<String>,
    output: RunOutput,
    capacity: OutputCapacity,
    /// The admissions to isolated state owners so far, which order candidate mode's records.
    admissions: u64,
}

impl<'r, 'a> Run<'r, 'a> {
    fn new(interpreter: &'r Interpreter<'a>) -> Self {
        Self {
            interpreter,
            points: AdmissionPoints::default(),
            gate: TickGate::default(),
            clock: AdmissionTime::UNIX_EPOCH,
            origin: Instant::now(),
            programs: BTreeMap::new(),
            schedule: None,
            instances: Instances::default(),
            budgets: interpreter
                .state_budget
                .map_or_else(StateBudgets::unlimited, StateBudgets::new),
            output_clocks: BTreeMap::new(),
            slots: BTreeMap::new(),
            cut: None,
            replaced: BTreeSet::new(),
            queued: None,
            generations: 0,
            closed: false,
            last_rejection: None,
            output: RunOutput::default(),
            capacity: OutputCapacity::default(),
            admissions: 0,
        }
    }

    fn execute(mut self, trace: &Trace) -> Result<RunOutput, InterpretError> {
        for (position, event) in trace.events().iter().enumerate() {
            self.event(position, event).map_err(|kind| InterpretError {
                event: Some(position),
                kind,
            })?;
        }
        if let Some(queued) = self.queued.take() {
            self.record(queued.event, InstallOutcome::Waiting);
        }

        if let Some(program) = self.active_id().and_then(|id| self.programs.get(&id)) {
            for (position, node) in program.nodes.iter().enumerate() {
                if !node.is_stateful() {
                    continue;
                }
                let watermark = match node.instance() {
                    Some(instance) => self
                        .instances
                        .get(instance)
                        .and_then(|instance| instance.tracker.watermark()),
                    None => self.watermark(program, position),
                };
                self.output.watermarks.insert(node.id.clone(), watermark);
            }
        }
        // Records are made when installations are decided, which a cut can delay; they are reported in trace order.
        self.output.installs.sort_by_key(|record| record.event);
        Ok(self.output)
    }

    fn event(&mut self, position: usize, event: &TraceEvent) -> Result<(), InterpretErrorKind> {
        if self.closed {
            return Err(InterpretErrorKind::AfterClose);
        }
        match event {
            TraceEvent::Admit {
                source,
                lane,
                admitted_at,
                batch,
            } => self.admit(source, *lane, *admitted_at, batch),
            TraceEvent::Heartbeat {
                source,
                lane,
                admitted_at,
            } => self.admit(source, *lane, *admitted_at, &[]),
            TraceEvent::Advance { clock } => self.advance(*clock),
            TraceEvent::Install { plan } => self.install(position, plan),
            TraceEvent::Capacity { output, limit } => {
                self.capacity.bound(output, *limit);
                Ok(())
            }
            TraceEvent::Signal { name, value } => {
                // Installation refuses plans with signal readers, so a signal changes nothing yet.
                self.output.signals.insert(name.clone(), *value);
                Ok(())
            }
            TraceEvent::Close => self.close(),
            TraceEvent::Cancel => self.cancel(position),
        }
    }

    fn no_active_plan(&self) -> InterpretErrorKind {
        InterpretErrorKind::NoActivePlan {
            last_rejection: self.last_rejection.clone(),
        }
    }

    /// Returns the active generation: the schedule's newest.
    fn active_id(&self) -> Option<GenerationId> {
        match self.schedule.as_ref()? {
            AdmissionSchedule::Steady(generation) => Some(generation.id()),
            AdmissionSchedule::Cut { new, .. } => Some(new.id()),
        }
    }

    fn record(&mut self, event: usize, outcome: InstallOutcome) {
        self.output.installs.push(InstallRecord { event, outcome });
    }

    // Admission.

    /// Stamps a batch at lane `lane` of a source and delivers it; an empty batch is a heartbeat.
    fn admit(
        &mut self, source: &NodeId, lane: u16, admitted_at: AdmissionTime, batch: &[Metric],
    ) -> Result<(), InterpretErrorKind> {
        let active = self.active_id().ok_or_else(|| self.no_active_plan())?;
        let limit = batch_size();
        if batch.len() > limit {
            return Err(InterpretErrorKind::BatchTooLarge {
                events: batch.len(),
                limit,
            });
        }
        // The source's admission points are the host's, whichever live plan names it.
        let group = std::iter::once(active)
            .chain(self.programs.keys().copied().filter(|id| *id != active))
            .find_map(|id| {
                let program = self.programs.get(&id)?;
                let node = program
                    .index
                    .get(source)
                    .copied()
                    .filter(|&node| matches!(program.nodes[node].exec, Exec::Source))?;
                program.points[node].first().copied().flatten()
            })
            .ok_or_else(|| InterpretErrorKind::UnknownSource { node: source.clone() })?;
        let point = group.lane(lane).ok_or_else(|| InterpretErrorKind::UnknownLane {
            node: source.clone(),
            lane,
            lanes: group.lanes,
        })?;

        let stamp = self
            .points
            .admit(point, admitted_at, self.interpreter.arrival_order)
            .map_err(|previous| InterpretErrorKind::AdmissionTimeMovedBack {
                point: self.points.name(point),
                previous,
                admitted_at,
                threshold: self.points.reset_threshold(),
            })?;
        let generation = self.generation_for(point, &stamp)?;
        let header = BatchHeader {
            admitted_at: Some(stamp.admitted_at),
            admission_point: Some(point),
            epoch: stamp.epoch,
            generation: Some(generation.clone()),
            entry: None,
        };
        let arrival = self.clock.max(admitted_at);

        self.before_admission(&header, &stamp, arrival)?;

        if !batch.is_empty() {
            let Some(mut program) = self.programs.remove(&generation.id()) else {
                return Err(InterpretErrorKind::Internal {
                    message: format!("generation {} has no plan", generation.id()),
                });
            };
            let result = self.deliver(&mut program, source, header, batch, arrival);
            self.programs.insert(generation.id(), program);
            result?;
        }
        self.check_finalization(arrival)
    }

    /// Returns the generation the schedule assigns to a stamp at `point`, and records a stamp of a cut's new
    /// generation, which makes the cut irrevocable.
    fn generation_for(&mut self, point: AdmissionPointId, stamp: &Stamp) -> Result<GenerationRef, InterpretErrorKind> {
        let Some(schedule) = &self.schedule else {
            return Err(self.no_active_plan());
        };
        let epoch_at_publish = self.cut.as_ref().map_or(stamp.epoch, |cut| cut.epoch_at_publish(point));
        let generation = schedule
            .generation_for(stamp.admitted_at, stamp.epoch, epoch_at_publish)
            .clone();
        if let Some(cut) = &mut self.cut {
            if generation.id() == cut.new.id() {
                cut.admitted_to_new.insert(point);
            }
        }
        Ok(generation)
    }

    /// Delivers a stamped batch from a source through `program`.
    fn deliver(
        &mut self, program: &mut Program, source: &NodeId, header: BatchHeader, batch: &[Metric],
        arrival: AdmissionTime,
    ) -> Result<(), InterpretErrorKind> {
        let node = program
            .index
            .get(source)
            .copied()
            .filter(|&node| matches!(program.nodes[node].exec, Exec::Source))
            .ok_or_else(|| InterpretErrorKind::UnknownSource { node: source.clone() })?;
        check_port_type(program, node, 0, batch)?;
        let mut queue = Queue::new();
        forward(program, &mut queue, node, 0, header, batch.to_vec(), Vec::new())?;
        self.pass(program, queue, arrival, |_, _, _| Ok(Queue::new()))
    }

    // Execution.

    /// Returns the watermark of a state owner that is not isolated: the earliest latest admission time of its upstream
    /// admission points.
    fn watermark(&self, program: &Program, node: usize) -> Option<AdmissionTime> {
        let upstream = &program.upstream[node];
        if upstream.is_empty() {
            return None;
        }
        upstream
            .iter()
            .map(|point| self.points.latest(*point))
            .try_fold(None, |earliest: Option<AdmissionTime>, latest| {
                let latest = latest?;
                Some(Some(earliest.map_or(latest, |earliest| earliest.min(latest))))
            })
            .flatten()
    }

    /// Returns the instant a host time maps to, for the `now` of turns.
    ///
    /// Kernels never decide anything by the time they run at, only by admission times, so the mapping only needs to be
    /// monotonic.
    fn instant(&self, time: AdmissionTime) -> Instant {
        self.origin
            .checked_add(Duration::from_nanos(time.as_unix_nanos()))
            .unwrap_or(self.origin)
    }

    /// Runs one pass over the program at host time `now`, starting with the deliveries `initial`: every node, in the
    /// program's order, runs the batches waiting at it, and then `after` runs for it, adding what it produces (see the
    /// module documentation's delivery order).
    fn pass<F>(
        &mut self, program: &mut Program, initial: Queue, now: AdmissionTime, mut after: F,
    ) -> Result<(), InterpretErrorKind>
    where
        F: FnMut(&mut Self, &mut Program, usize) -> Result<Queue, InterpretErrorKind>,
    {
        let mut pending = Pending::new(program);
        pending.add(initial);
        for position in 0..program.order.len() {
            let node = program.order[position];
            for batch in pending.take(node) {
                let more = with_exec(program, node, |program, exec| {
                    self.run_node(program, node, exec, batch, now)
                })?;
                pending.add(more);
            }
            let more = after(self, program, node)?;
            pending.add(more);
        }
        // Every edge leads to a node later in the order, so nothing can be left.
        match pending.waiting() {
            None => Ok(()),
            Some(node) => Err(InterpretErrorKind::Internal {
                message: format!(
                    "a batch reached node `{}` after its turn in the pass",
                    program.nodes[node].id
                ),
            }),
        }
    }

    /// Runs one node, whose execution state is `exec`, over one batch, and returns the deliveries it causes.
    fn run_node(
        &mut self, program: &Program, node: usize, exec: &mut Exec, batch: Batch, now: AdmissionTime,
    ) -> Result<Queue, InterpretErrorKind> {
        let Batch { header, metrics, tags } = batch;
        let ports = port_count(program, node);
        let id = &program.nodes[node].id;
        let mut queue = Queue::new();
        match exec {
            Exec::Source => {
                return Err(InterpretErrorKind::Internal {
                    message: format!("source `{}` received a batch", id),
                })
            }
            Exec::Split {
                predicate,
                true_port,
                false_port,
            } => {
                let (mut matched, mut unmatched) = (Vec::new(), Vec::new());
                let (mut matched_tags, mut unmatched_tags) = (Vec::new(), Vec::new());
                for (index, metric) in metrics.into_iter().enumerate() {
                    let tag = tag_at(&tags, index);
                    if predicate.evaluate(&mut EventView::new(&metric)) {
                        matched.push(metric);
                        matched_tags.push(tag);
                    } else {
                        unmatched.push(metric);
                        unmatched_tags.push(tag);
                    }
                }
                if tags.is_empty() {
                    matched_tags.clear();
                    unmatched_tags.clear();
                }
                forward(
                    program,
                    &mut queue,
                    node,
                    *true_port,
                    header.clone(),
                    matched,
                    matched_tags,
                )?;
                forward(
                    program,
                    &mut queue,
                    node,
                    *false_port,
                    header,
                    unmatched,
                    unmatched_tags,
                )?;
            }
            Exec::Tee => {
                // Every port but the last receives a copy; the last receives the original.
                if let Some(last) = ports.checked_sub(1) {
                    for port in 0..last {
                        forward(
                            program,
                            &mut queue,
                            node,
                            port,
                            header.clone(),
                            metrics.clone(),
                            tags.clone(),
                        )?;
                    }
                    forward(program, &mut queue, node, last, header, metrics, tags)?;
                }
            }
            Exec::Merge => forward(program, &mut queue, node, 0, header, metrics, tags)?,
            Exec::Emit(output) => {
                if !tags.is_empty() {
                    self.record_candidates(output, &metrics, &tags);
                }
                let generation = program.generation.id();
                let record = |metric| EmittedRecord {
                    generation,
                    install: program.install,
                    admitted_at: header.admitted_at,
                    metric,
                };
                let mut metrics = metrics;
                let refused = metrics.split_off(self.capacity.accept(output, metrics.len()));
                let records = self.output.outputs.entry(output.clone()).or_default();
                records.extend(metrics.into_iter().map(record));
                if !refused.is_empty() {
                    let points = refused.iter().map(|metric| metric.values().len() as u64).sum();
                    self.output.loss.entry(id.clone()).or_default().record_loss(
                        LossClass::Capacity,
                        refused.len() as u64,
                        points,
                    );
                    let records = self.output.refused.entry(output.clone()).or_default();
                    records.extend(refused.into_iter().map(record));
                }
            }
            Exec::Drop => {
                let points = metrics.iter().map(|metric| metric.values().len() as u64).sum();
                self.output.loss.entry(id.clone()).or_default().record_loss(
                    LossClass::Policy,
                    metrics.len() as u64,
                    points,
                );
            }
            Exec::Kernel {
                kernel: Some(kernel), ..
            } if tags.iter().any(Option::is_some) => {
                // A stateless kernel's output does not depend on how its input is cut into batches, so tagged records
                // run one at a time, and whatever each one becomes carries its tag.
                for (index, metric) in metrics.into_iter().enumerate() {
                    let tag = tag_at(&tags, index);
                    let mut input = to_buffer(vec![metric])?;
                    let mut call = KernelCall::new(&header, self.instant(now), ports);
                    call.repeat(id, |turn| kernel.consume(turn, &mut input))?;
                    call.outputs.complete(input);
                    queue.extend(self.kernel_output(program, node, call, &header, |_| tag.clone())?);
                }
            }
            Exec::Kernel {
                kernel: Some(kernel), ..
            } => {
                let mut input = to_buffer(metrics)?;
                let mut call = KernelCall::new(&header, self.instant(now), ports);
                call.repeat(id, |turn| kernel.consume(turn, &mut input))?;
                call.outputs.complete(input);
                queue = self.kernel_output(program, node, call, &header, |_| None)?;
            }
            Exec::State {
                instance: Some(bound),
                step,
                spec,
            } => {
                let Some(index) = self.state_instance(program, node, *bound, spec, &header)? else {
                    // A batch of an epoch the slot already closed: its timeline's instances drained.
                    let points = metrics.iter().map(|metric| metric.values().len() as u64).sum();
                    self.output.loss.entry(id.clone()).or_default().record_loss(
                        LossClass::Invalid,
                        metrics.len() as u64,
                        points,
                    );
                    return Ok(queue);
                };
                let step = step.clone();
                let mut call = KernelCall::new(&header, self.instant(now), ports).with_param(step.as_deref());
                let Some(instance) = self.instances.get_mut(index) else {
                    return Err(InterpretErrorKind::Internal {
                        message: format!("node `{}` binds instance {}, which is gone", id, index),
                    });
                };
                if let (Some(admitted), Some(point), Some(admitted_at)) =
                    (&mut instance.admitted, header.admission_point, header.admitted_at)
                {
                    // One entry per instance and epoch: a lineage that continues in a newer epoch starts its entries
                    // anew.
                    let entry = (index as u64) << 32 | u64::from(instance.epoch);
                    for metric in &metrics {
                        self.admissions += 1;
                        admitted.entry(ContextIdentity::of(metric)).or_default().admit(
                            point,
                            admitted_at,
                            instance.width,
                            metric,
                            entry,
                            self.admissions,
                        );
                    }
                }
                let mut input = to_buffer(metrics)?;
                let kernel = &mut instance.kernel;
                call.repeat(id, |turn| kernel.consume(turn, &mut input))?;
                call.outputs.complete(input);
                queue = self.kernel_output(program, node, call, &header, |_| None)?;
            }
            Exec::Host(Some(adapter)) => {
                let mut output = HostOutput::new(ports);
                adapter.admit(
                    HostBatch {
                        admitted_at: header.admitted_at,
                        arrived_at: now,
                        events: metrics,
                    },
                    &mut output,
                );
                queue = self.readmit(program, node, output, now, false)?;
            }
            Exec::Kernel { kernel: None, .. } | Exec::Host(None) | Exec::State { instance: None, .. } => {
                return Err(InterpretErrorKind::Internal {
                    message: format!("node `{}` has no instance", id),
                })
            }
        }
        Ok(queue)
    }

    /// Returns the instance of the lineage bound at `bound` that admits a batch with `header`: the bound one, or,
    /// inside a reset window, the lineage's instance of the batch's epoch, which the batch's first data of that epoch
    /// creates, empty, from the lineage's kernel spec, with the lineage's cut. Returns `None` for a batch of an epoch
    /// the slot already closed, which no instance holds.
    fn state_instance(
        &mut self, program: &Program, node: usize, bound: usize, spec: &KernelSpec, header: &BatchHeader,
    ) -> Result<Option<usize>, InterpretErrorKind> {
        let id = &program.nodes[node].id;
        let Some(instance) = self.instances.get(bound) else {
            return Err(InterpretErrorKind::Internal {
                message: format!("node `{}` binds instance {}, which is gone", id, bound),
            });
        };
        let Some(slot) = self.slots.get(&instance.output_clock()).filter(|slot| slot.one_clock) else {
            return Ok(Some(bound));
        };
        if instance.epoch == header.epoch {
            return Ok(Some(bound));
        }
        let existing = self.instances.indices().into_iter().find(|&index| {
            self.instances
                .get(index)
                .is_some_and(|other| !other.retired && other.root(index) == bound && other.epoch == header.epoch)
        });
        if existing.is_some() {
            return Ok(existing);
        }
        if !slot.window.holds(header.epoch) {
            return Ok(None);
        }
        let reset_retiring = slot.window.is_open() && header.epoch != slot.window.target();
        let kernel =
            program::build_state_kernel(id, spec, self.budgets.context_budget(id)).map_err(|error| match error {
                PrepareError::Fatal(kind) => kind,
                PrepareError::Rejected(rejection) => InterpretErrorKind::Internal {
                    message: format!("a reset window's instance of `{}` does not build: {}", id, rejection),
                },
            })?;
        let fresh = Instance {
            key: instance.key.clone(),
            kernel,
            tracker: instance.tracker.clone(),
            gated: None,
            last_flush: None,
            retiring: instance.retiring.clone(),
            retired: false,
            epoch: header.epoch,
            primary: Some(bound),
            reset_retiring,
            width: instance.width,
            admitted: instance.admitted.as_ref().map(|_| BTreeMap::new()),
        };
        Ok(Some(self.instances.insert(fresh)))
    }

    /// Adds loss a node reported to its tally in the run's output.
    fn tally_loss(&mut self, node: &NodeId, counters: &TurnCounters) {
        if counters.is_empty() {
            return;
        }
        let tally = self.output.loss.entry(node.clone()).or_default();
        for class in LOSS_CLASSES {
            let loss = counters.loss(class);
            tally.record_loss(class, loss.events, loss.points);
        }
    }

    /// Records, in candidate mode, the candidates of every point of the tagged records `output` receives, under the
    /// output's own view of each record's context.
    fn record_candidates(&mut self, output: &OutputId, metrics: &[Metric], tags: &[Option<RecordTag>]) {
        for (index, metric) in metrics.iter().enumerate() {
            let Some(tag) = tag_at(tags, index) else { continue };
            let context = ContextIdentity::of(metric);
            // Every bucket of the record's entry, those whose points a kind replaced included.
            for (timestamp, bucket) in tag.iter() {
                self.output
                    .candidates
                    .add((output.clone(), context.clone(), *timestamp), bucket);
            }
        }
    }

    /// Collects a kernel call's loss counts, and turns its output into deliveries carrying `header`, each record with
    /// the tag `tag_of` gives it.
    fn kernel_output<T>(
        &mut self, program: &Program, node: usize, call: KernelCall, header: &BatchHeader, tag_of: T,
    ) -> Result<Queue, InterpretErrorKind>
    where
        T: Fn(&Metric) -> Option<RecordTag>,
    {
        let KernelCall {
            mut outputs, counters, ..
        } = call;
        let id = &program.nodes[node].id;
        self.tally_loss(id, &counters);

        let mut queue = Queue::new();
        for port in 0..outputs.port_count() {
            let Ok(index) = u16::try_from(port) else { break };
            for buffer in outputs.drain(PortIndex::new(index)).collect::<Vec<_>>() {
                let metrics = buffer
                    .into_iter()
                    .map(Event::try_into_metric)
                    .collect::<Option<Vec<_>>>()
                    .ok_or_else(|| InterpretErrorKind::NonMetricEvent { node: id.clone() })?;
                let mut tags = metrics.iter().map(&tag_of).collect::<Vec<_>>();
                if tags.iter().all(Option::is_none) {
                    tags.clear();
                }
                forward(program, &mut queue, node, port, header.clone(), metrics, tags)?;
            }
        }
        Ok(queue)
    }

    /// Re-admits an opaque host's output at its output ports, at host time `now`, and collects the loss it reported.
    ///
    /// With `heartbeat`, every port's admission clock reads `now`, whether or not it emitted anything, as at a tick.
    /// Isolated state owners downstream observe each stamp for their watermark.
    fn readmit(
        &mut self, program: &Program, node: usize, mut output: HostOutput, now: AdmissionTime, heartbeat: bool,
    ) -> Result<Queue, InterpretErrorKind> {
        let (ports, counters) = output.take();
        self.tally_loss(&program.nodes[node].id, &counters);
        let mut queue = Queue::new();
        let limit = batch_size();
        for (port, metrics) in ports.into_iter().enumerate() {
            let Some(point) = program.points[node]
                .get(port)
                .copied()
                .flatten()
                .map(|group| group.first)
            else {
                return Err(InterpretErrorKind::Internal {
                    message: format!("opaque host `{}` has no admission point", program.nodes[node].id),
                });
            };
            if metrics.is_empty() && !heartbeat {
                continue;
            }
            check_port_type(program, node, port, &metrics)?;
            let stamped = self.points.stamp(point, now);
            let header = BatchHeader {
                admitted_at: Some(stamped),
                admission_point: Some(point),
                epoch: self.points.epoch(point),
                generation: Some(program.generation.clone()),
                entry: None,
            };
            for index in self.instances.indices() {
                if let Some(instance) = self
                    .instances
                    .get_mut(index)
                    .filter(|instance| instance.observes(point))
                {
                    instance.tracker.observe(&header);
                }
            }
            let mut metrics = metrics.into_iter().peekable();
            while metrics.peek().is_some() {
                let chunk = metrics.by_ref().take(limit).collect();
                forward(program, &mut queue, node, port, header.clone(), chunk, Vec::new())?;
            }
        }
        Ok(queue)
    }
}

/// The work an event gives the nodes of a plan that are not isolated state owners.
#[derive(Clone, Copy, Debug)]
struct LocalWork {
    /// `None` for a tick of every state owner, or the policy every kernel and host finishes under.
    policy: Option<DrainPolicy>,

    /// Whether every kernel takes part, not only the state owners: true when finishing.
    all_kernels: bool,
}

impl LocalWork {
    fn tick() -> Self {
        Self {
            policy: None,
            all_kernels: false,
        }
    }

    fn finish(policy: DrainPolicy) -> Self {
        Self {
            policy: Some(policy),
            all_kernels: true,
        }
    }
}

/// Describes a state owner that keeps its state in its own program, outside an isolation fence.
fn local(node: &program::ProgramNode) -> String {
    if node.is_host() {
        format!("the host component `{}`", node.id)
    } else {
        format!("the state owner `{}` outside an isolation fence", node.id)
    }
}

/// Returns the lineage key of a state owner.
fn lineage_key(node: &NodeId, op: &ConfiguredOperator) -> Result<LineageKey, PrepareError> {
    LineageKey::of(node, op).map_err(|error| internal(&format!("state owner `{}` has no lineage: {}", node, error)))
}

fn internal(message: &str) -> PrepareError {
    PrepareError::Fatal(InterpretErrorKind::Internal {
        message: message.to_string(),
    })
}

/// Runs `run` with a node's execution state taken out of the program, so that it can read the rest of the program,
/// and puts the state back afterwards.
fn with_exec<T, F>(program: &mut Program, node: usize, run: F) -> T
where
    F: FnOnce(&Program, &mut Exec) -> T,
{
    // A drop holds nothing, so it stands in while the node runs.
    let mut exec = std::mem::replace(&mut program.nodes[node].exec, Exec::Drop);
    let result = run(program, &mut exec);
    program.nodes[node].exec = exec;
    result
}

/// Queues a batch for the input port that a node's output port feeds. Empty batches carry nothing and are not
/// delivered.
fn forward(
    program: &Program, queue: &mut Queue, node: usize, port: usize, header: BatchHeader, metrics: Vec<Metric>,
    tags: Vec<Option<RecordTag>>,
) -> Result<(), InterpretErrorKind> {
    if metrics.is_empty() {
        return Ok(());
    }
    let Some((target, input)) = program.targets[node].get(port).copied().flatten() else {
        return Err(InterpretErrorKind::Internal {
            message: format!("output port {} of node `{}` has no edge", port, program.nodes[node].id),
        });
    };
    queue.push(Delivery {
        node: target,
        port: input,
        batch: Batch { header, metrics, tags },
    });
    Ok(())
}

fn port_count(program: &Program, node: usize) -> usize {
    program.nodes[node].op.ports().outputs.len()
}

/// Checks that a port can carry every metric of a batch entering the plan there, at a source or an opaque host's
/// output.
///
/// Validation types every port from the sources' bindings and the hosts' declared outputs, so a record outside its
/// port's type is a trace or adapter that no host could produce. Running it would deliver what validation proved cannot
/// arrive, so the interpreter refuses it.
fn check_port_type(program: &Program, node: usize, port: usize, metrics: &[Metric]) -> Result<(), InterpretErrorKind> {
    let op = &program.nodes[node].op;
    let (Some(port_type), Some(name)) = (
        program.port_types[node].get(port).copied().flatten(),
        op.ports().outputs.get(port).map(|port| port.name.clone()),
    ) else {
        return Err(InterpretErrorKind::Internal {
            message: format!(
                "output port {} of validated node `{}` has no type",
                port, program.nodes[node].id
            ),
        });
    };
    for metric in metrics {
        let values = metric.values();
        let timestamps = match port_type.timestamps {
            TimestampForm::Mixed => true,
            TimestampForm::Timestamped => values.all_timestamped(),
            TimestampForm::Untimestamped => !values.any_timestamped(),
        };
        if !timestamps || !port_type.kinds.contains(Kind::of(values)) {
            let form = if values.all_timestamped() {
                "timestamped"
            } else if values.any_timestamped() {
                "partly timestamped"
            } else {
                "untimestamped"
            };
            return Err(InterpretErrorKind::IllTyped {
                node: program.nodes[node].id.clone(),
                port: name,
                port_type: describe_port_type(&port_type),
                record: format!("the {} {} `{}`", form, Kind::of(values), metric.context().name()),
            });
        }
    }
    Ok(())
}

/// The header of the time-driven output of a state owner that is not isolated: its watermark as `t_a`, under the
/// program's generation.
fn time_driven_header(program: &Program, watermark: Option<AdmissionTime>) -> BatchHeader {
    BatchHeader {
        admitted_at: watermark,
        admission_point: None,
        epoch: 0,
        generation: Some(program.generation.clone()),
        entry: None,
    }
}

fn to_buffer(metrics: Vec<Metric>) -> Result<EventsBuffer, InterpretErrorKind> {
    let mut buffer = EventsBuffer::with_capacity(metrics.len());
    for metric in metrics {
        if buffer.try_push(Event::Metric(metric)).is_some() {
            return Err(InterpretErrorKind::Internal {
                message: "a batch is larger than an events buffer".to_string(),
            });
        }
    }
    Ok(buffer)
}

/// The context of one kernel call: its turn's budget, outputs, and counters.
struct KernelCall<'h> {
    header: &'h BatchHeader,
    watermark: Option<AdmissionTime>,
    param: Option<&'h (dyn Any + Send + Sync)>,
    now: Instant,
    budget: Budget,
    outputs: PortOutputs,
    counters: TurnCounters,
}

impl<'h> KernelCall<'h> {
    fn new(header: &'h BatchHeader, now: Instant, port_count: usize) -> Self {
        Self {
            header,
            watermark: None,
            param: None,
            now,
            budget: Budget::unlimited(),
            outputs: PortOutputs::new(port_count),
            counters: TurnCounters::default(),
        }
    }

    /// Returns this call with the watermark its turns carry, for time-driven calls.
    fn with_watermark(mut self, watermark: Option<AdmissionTime>) -> Self {
        self.watermark = watermark;
        self
    }

    /// Returns this call with the per-call parameters its turns carry, from the generation it runs under.
    fn with_param(mut self, param: Option<&'h (dyn Any + Send + Sync)>) -> Self {
        self.param = param;
        self
    }

    /// Calls `step` in a fresh turn until it no longer yields. A deadline is only a scheduling hint: flush ticks come
    /// from the trace.
    fn repeat<F>(&mut self, node: &NodeId, mut step: F) -> Result<(), InterpretErrorKind>
    where
        F: FnMut(&mut Turn<'_>) -> Progress,
    {
        for _ in 0..MAX_YIELDS {
            let mut turn = Turn::new(
                self.header,
                self.now,
                &mut self.budget,
                &mut self.outputs,
                &mut self.counters,
            )
            .with_watermark(self.watermark);
            if let Some(param) = self.param {
                turn = turn.with_param(param);
            }
            match step(&mut turn) {
                Progress::Yielded => continue,
                Progress::Done | Progress::WaitUntil(_) => return Ok(()),
            }
        }
        Err(InterpretErrorKind::KernelStalled {
            node: node.clone(),
            yields: MAX_YIELDS,
        })
    }
}

#[cfg(test)]
mod bucket_tests;
#[cfg(test)]
mod compatibility_tests;
#[cfg(test)]
mod default_program_tests;
#[cfg(test)]
mod isolated_tests;
#[cfg(test)]
mod lane_tests;
#[cfg(test)]
pub(crate) mod test_operators;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod transition_tests;
