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
//! - Operators with a [kernel][crate::operators::KernelSpec] run it, with an unlimited budget.
//! - Opaque hosts run through an [`OpaqueHostAdapter`] registered per node, whose output is **re-admitted** downstream.
//! - Sources are where `admit` events enter.
//!
//! Every other operator has no kernel yet, so a plan using one is not supported until kernels are extracted.
//!
//! # Trace events
//!
//! | Event | Effect |
//! |---|---|
//! | `admit(source, t_a, batch)` | Delivers the batch from the source through the plan; see [admission](#admission). |
//! | `heartbeat(source, t_a)` | Advances the source's latest admission time without delivering anything. |
//! | `advance(clock)` | Runs one flush tick for every state owner at host time `clock`, in topological order. |
//! | `install(plan)` | Validates the plan and, if it is valid, makes it the active plan from the next event on. |
//! | `close` | Ends input: every kernel and host finishes under [`DrainPolicy::Shutdown`]. |
//! | `signal(name, value)` | Records the signal's value. No operator reads signals yet, so it has no effect. |
//! | `capacity(output, n)`, `cancel` | Not supported until delivery policies (stage 3) and cuts (stage 6). |
//!
//! # Time
//!
//! The trace states two clocks. Each `admit` carries the admission time its source stamped, and each `advance` the
//! host time of a flush tick. Ticks follow a monotonic timer, so `advance` clocks never decrease. A batch may be
//! stamped earlier than the last tick, as a batch that waited in a queue would be: it **arrives** at the later of the
//! last tick and its admission time.
//!
//! A source's admission times never decrease either: an earlier stamp can only come from a clock reset, which is not
//! supported until stage 6.
//!
//! # Watermarks
//!
//! There are no queues, so every state owner's watermark is the earliest of the latest admission times of the
//! admission points upstream of it: sources, and the outputs of opaque hosts, which re-admit. An admission point that
//! has admitted nothing holds the watermark back. Opaque hosts receive their watermark at every tick, and a stateful
//! kernel's time-driven output, from its ticks and its finish, is stamped with its watermark as `t_a`.
//!
//! # Opaque hosts and segments
//!
//! An opaque host's output is re-admitted at the host's output: stamped by the host's own admission clock, which reads
//! the arrival time for output emitted while admitting a batch and the tick's clock for output emitted at a tick, and
//! with the generation active at that moment. At every tick, every output of the host either re-admits output or
//! heartbeats at the tick's clock.
//!
//! # Admission
//!
//! Sources and opaque host outputs are where records enter the plan, so the interpreter checks each record entering
//! there against the port's type, which validation computed from the source's binding or the host's declared outputs.
//! A record outside it is an error: no host produces it, and validation's typing assumes it never arrives.
//!
//! # Installations
//!
//! Every installation commits at once: there are no queues, so no batch is in flight when it does. An invalid plan is
//! rejected and the active plan stays, as a failed preparation leaves the active generation unchanged. State owners
//! correspond by node ID: one that is unchanged keeps its instance and its state, so its time-driven output flows
//! through the new plan; one whose transition is `reject` makes the installation rejected. Adding or removing a state
//! owner, changing one's parameters, or a `cut` transition is not supported yet. The kernels of the old plan that are
//! not carried over finish under [`DrainPolicy::Retire`], in the old generation.
//!
//! # Runtime
//!
//! Kernels are built and run inside a current-thread Tokio context that the interpreter creates and enters for each
//! run, so that a kernel may spawn its cache maintenance tasks. Those tasks are never driven: their effects are
//! unobservable.

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    fmt,
    sync::Arc,
    time::{Duration, Instant},
};

use saluki_core::{
    data_model::event::{metric::Metric, Event},
    execution::{
        AdmissionTime, BatchHeader, Budget, DrainPolicy, GenerationId, GenerationToken, LossClass, PortIndex,
        PortOutputs, Progress, Turn, TurnCounters,
    },
    topology::EventsBuffer,
};
use snafu::Snafu;

use crate::{
    extension::TransitionClass,
    ir::{NodeId, OutputId, Plan, PortName, SignalName},
    limits,
    model::{EventView, Kind, TimestampForm},
    operators::{ConfiguredOperator, KernelBuildError, OperatorRegistry},
    predicate::PredicateError,
    validate::{describe_port_type, Catalog, ValidationReport, Validator},
};

mod host;
pub use self::host::{AdapterError, HostBatch, HostFactory, HostFinish, HostOutput, HostTick, OpaqueHostAdapter};

mod program;
use self::program::{build_kernel, AdmissionPoints, Exec, PrepareError, Program};

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
            Self::UnresolvedBinding { node, source } => write!(f, "node `{}`: {}", node, source),
            Self::KernelBuild { node, source } => write!(f, "node `{}`: {}", node, source),
            Self::HostBuild { node, source } => write!(f, "node `{}`: {}", node, source),
        }
    }
}

/// What happened to one `install` event.
#[derive(Debug)]
pub enum InstallOutcome {
    /// The plan became active, as the given generation.
    Active {
        /// The new generation.
        generation: GenerationId,
    },

    /// The installation was rejected.
    Rejected(InstallRejection),
}

/// One `install` event and its outcome.
#[derive(Debug)]
pub struct InstallRecord {
    /// The position of the `install` event in the trace, counting from zero.
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

    /// The admission time of the batch that carried the record: the time its last admission point stamped. `None`
    /// for time-driven kernel output when no upstream admission point has stamped anything.
    pub admitted_at: Option<AdmissionTime>,

    /// The record.
    pub metric: Metric,
}

/// What one run of the interpreter over a trace produced.
#[derive(Debug, Default)]
pub struct RunOutput {
    outputs: BTreeMap<OutputId, Vec<EmittedRecord>>,
    installs: Vec<InstallRecord>,
    loss: BTreeMap<NodeId, TurnCounters>,
    watermarks: BTreeMap<NodeId, Option<AdmissionTime>>,
    signals: BTreeMap<SignalName, SignalValue>,
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

    /// Returns every `install` event's outcome, in trace order.
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

    /// An `admit` or `heartbeat` names a node that is not a source of the active plan.
    #[snafu(display(
        "`{}` is not a `source` node of the active plan; `admit` and `heartbeat` events name the source a batch \
         enters at",
        node
    ))]
    UnknownSource {
        /// The named node.
        node: NodeId,
    },

    /// An `advance` moves the host clock back.
    #[snafu(display(
        "The host clock moves back from {} to {}; flush ticks follow a monotonic timer, so `advance` clocks never \
         decrease",
        previous,
        clock
    ))]
    ClockWentBackward {
        /// The clock of the previous tick.
        previous: AdmissionTime,

        /// The clock of this tick.
        clock: AdmissionTime,
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
}

impl<'a> Interpreter<'a> {
    /// Creates an interpreter that validates plans against `registry` and `catalog`.
    pub fn new(registry: &'a OperatorRegistry, catalog: &'a dyn Catalog) -> Self {
        Self {
            registry,
            catalog,
            hosts: BTreeMap::new(),
        }
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
    /// Returns an error if the trace is malformed, such as an `admit` before any plan is active or a batch larger than
    /// the batch size; if it needs a capability a later stage adds, such as a clock reset or an operator without a
    /// kernel; or if an opaque host has no adapter. A rejected installation is not an error: it is recorded in the
    /// output's [installs][RunOutput::installs].
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

/// A batch moving between nodes.
struct Batch {
    header: BatchHeader,
    metrics: Vec<Metric>,
}

/// A batch on its way into one input port.
struct Delivery {
    node: usize,
    batch: Batch,
}

/// The pending deliveries of one trace event, processed in order.
type Queue = VecDeque<Delivery>;

/// The state of one run.
struct Run<'r, 'a> {
    interpreter: &'r Interpreter<'a>,
    points: AdmissionPoints,
    /// The host clock: the clock of the last flush tick.
    clock: AdmissionTime,
    /// The instant the host clock's Unix epoch maps to, for the `now` of turns.
    origin: Instant,
    active: Option<Program>,
    generations: u64,
    closed: bool,
    last_rejection: Option<String>,
    output: RunOutput,
}

impl<'r, 'a> Run<'r, 'a> {
    fn new(interpreter: &'r Interpreter<'a>) -> Self {
        Self {
            interpreter,
            points: AdmissionPoints::default(),
            clock: AdmissionTime::UNIX_EPOCH,
            origin: Instant::now(),
            active: None,
            generations: 0,
            closed: false,
            last_rejection: None,
            output: RunOutput::default(),
        }
    }

    fn execute(mut self, trace: &Trace) -> Result<RunOutput, InterpretError> {
        for (position, event) in trace.events().iter().enumerate() {
            self.event(position, event).map_err(|kind| InterpretError {
                event: Some(position),
                kind,
            })?;
        }

        if let Some(program) = &self.active {
            for node in 0..program.nodes.len() {
                if program.nodes[node].is_stateful() {
                    let watermark = self.watermark(program, node);
                    self.output.watermarks.insert(program.nodes[node].id.clone(), watermark);
                }
            }
        }
        Ok(self.output)
    }

    fn event(&mut self, position: usize, event: &TraceEvent) -> Result<(), InterpretErrorKind> {
        if self.closed {
            return Err(InterpretErrorKind::AfterClose);
        }
        match event {
            TraceEvent::Admit {
                source,
                admitted_at,
                batch,
            } => self.admit(source, *admitted_at, batch),
            TraceEvent::Heartbeat { source, admitted_at } => self.admit(source, *admitted_at, &[]),
            TraceEvent::Advance { clock } => self.advance(*clock),
            TraceEvent::Install { plan } => self.install(position, plan),
            TraceEvent::Capacity { output, .. } => Err(InterpretErrorKind::Unsupported {
                capability: format!(
                    "bounding the capacity of output `{}`, which needs the delivery-policy slot,",
                    output
                ),
                stage: 3,
            }),
            TraceEvent::Signal { name, value } => {
                // Installation refuses plans with signal readers, so a signal changes nothing yet.
                self.output.signals.insert(name.clone(), *value);
                Ok(())
            }
            TraceEvent::Close => self.close(),
            TraceEvent::Cancel => Err(InterpretErrorKind::Unsupported {
                capability: "cancelling an installation, which can only be pending while it waits for a `cut`,"
                    .to_string(),
                stage: 6,
            }),
        }
    }

    fn no_active_plan(&self) -> InterpretErrorKind {
        InterpretErrorKind::NoActivePlan {
            last_rejection: self.last_rejection.clone(),
        }
    }

    /// Admits a batch at a source; an empty batch is a heartbeat.
    fn admit(
        &mut self, source: &NodeId, admitted_at: AdmissionTime, batch: &[Metric],
    ) -> Result<(), InterpretErrorKind> {
        let mut program = self.active.take().ok_or_else(|| self.no_active_plan())?;
        let result = self.admit_into(&mut program, source, admitted_at, batch);
        self.active = Some(program);
        result
    }

    fn admit_into(
        &mut self, program: &mut Program, source: &NodeId, admitted_at: AdmissionTime, batch: &[Metric],
    ) -> Result<(), InterpretErrorKind> {
        let node = program
            .index
            .get(source)
            .copied()
            .filter(|&node| matches!(program.nodes[node].exec, Exec::Source))
            .ok_or_else(|| InterpretErrorKind::UnknownSource { node: source.clone() })?;
        let limit = batch_size();
        if batch.len() > limit {
            return Err(InterpretErrorKind::BatchTooLarge {
                events: batch.len(),
                limit,
            });
        }
        check_port_type(program, node, 0, batch)?;
        let Some(point) = program.points[node].first().copied().flatten() else {
            return Err(InterpretErrorKind::Internal {
                message: format!("source `{}` has no admission point", source),
            });
        };
        if let Err(previous) = self.points.admit(point, admitted_at) {
            return Err(InterpretErrorKind::Unsupported {
                capability: format!(
                    "a clock reset at admission point `{}`, whose admission time moves back from {} to {},",
                    self.points.name(point),
                    previous,
                    admitted_at
                ),
                stage: 6,
            });
        }

        let header = BatchHeader {
            admitted_at: Some(admitted_at),
            admission_point: Some(point),
            epoch: 0,
            generation: Some(program.generation.clone()),
            entry: None,
        };
        let mut queue = Queue::new();
        forward(program, &mut queue, node, 0, header, batch.to_vec())?;
        let arrival = self.clock.max(admitted_at);
        self.flow(program, queue, arrival)
    }

    /// Runs one flush tick for every state owner, in topological order.
    fn advance(&mut self, clock: AdmissionTime) -> Result<(), InterpretErrorKind> {
        if clock < self.clock {
            return Err(InterpretErrorKind::ClockWentBackward {
                previous: self.clock,
                clock,
            });
        }
        self.clock = clock;
        let Some(mut program) = self.active.take() else {
            return Ok(());
        };
        let result = self.tick(&mut program);
        self.active = Some(program);
        result
    }

    fn tick(&mut self, program: &mut Program) -> Result<(), InterpretErrorKind> {
        for position in 0..program.order.len() {
            let node = program.order[position];
            if !program.nodes[node].is_stateful() {
                continue;
            }
            let watermark = self.watermark(program, node);
            let clock = self.clock;
            let queue = with_exec(program, node, |program, exec| match exec {
                Exec::Kernel {
                    kernel: Some(kernel), ..
                } => {
                    let header = time_driven_header(program, watermark);
                    let mut call = KernelCall::new(&header, self.instant(clock), port_count(program, node))
                        .with_watermark(watermark);
                    call.repeat(&program.nodes[node].id, |turn| kernel.advance_time(turn))?;
                    self.kernel_output(program, node, call, &header)
                }
                Exec::Host(Some(adapter)) => {
                    let mut output = HostOutput::new(port_count(program, node));
                    adapter.tick(HostTick { now: clock, watermark }, &mut output);
                    self.readmit(program, node, output, clock, true)
                }
                _ => Ok(Queue::new()),
            })?;
            self.flow(program, queue, clock)?;
        }
        Ok(())
    }

    /// Finishes every kernel and host under the shutdown policy, in topological order, and ends the input.
    fn close(&mut self) -> Result<(), InterpretErrorKind> {
        self.closed = true;
        let Some(mut program) = self.active.take() else {
            return Ok(());
        };
        let result = self.finish(&mut program, DrainPolicy::Shutdown, &BTreeSet::new());
        self.active = Some(program);
        result
    }

    /// Finishes every kernel and host of `program` except the nodes in `keep`, in topological order.
    fn finish(
        &mut self, program: &mut Program, policy: DrainPolicy, keep: &BTreeSet<usize>,
    ) -> Result<(), InterpretErrorKind> {
        for position in 0..program.order.len() {
            let node = program.order[position];
            if keep.contains(&node) {
                continue;
            }
            let watermark = self.watermark(program, node);
            let clock = self.clock;
            let queue = with_exec(program, node, |program, exec| match exec {
                Exec::Kernel {
                    kernel: Some(kernel), ..
                } => {
                    let header = time_driven_header(program, watermark);
                    let mut call = KernelCall::new(&header, self.instant(clock), port_count(program, node))
                        .with_watermark(watermark);
                    call.repeat(&program.nodes[node].id, |turn| kernel.finish(turn, policy))?;
                    self.kernel_output(program, node, call, &header)
                }
                Exec::Host(Some(adapter)) => {
                    let mut output = HostOutput::new(port_count(program, node));
                    adapter.finish(HostFinish { now: clock, policy }, &mut output);
                    self.readmit(program, node, output, clock, false)
                }
                _ => Ok(Queue::new()),
            })?;
            self.flow(program, queue, clock)?;
        }
        Ok(())
    }

    fn reject(&mut self, position: usize, rejection: InstallRejection) {
        self.last_rejection = Some(rejection.to_string());
        self.output.installs.push(InstallRecord {
            event: position,
            outcome: InstallOutcome::Rejected(rejection),
        });
    }

    fn install(&mut self, position: usize, plan: &Plan) -> Result<(), InterpretErrorKind> {
        let report = Validator::new(self.interpreter.registry, self.interpreter.catalog).validate(plan);
        if !report.is_valid() {
            self.reject(position, InstallRejection::Invalid { report });
            return Ok(());
        }

        // Nothing is stamped with a generation until it commits, so a rejected candidate's ID is never seen.
        let generation = GenerationId::new(self.generations + 1);
        let prepared = Program::prepare(
            plan,
            self.interpreter.registry,
            &report,
            self.interpreter.catalog,
            &mut self.points,
            position,
            Arc::new(GenerationToken::new(generation)),
        );
        let mut program = match prepared {
            Ok(program) => program,
            Err(PrepareError::Rejected(rejection)) => {
                self.reject(position, rejection);
                return Ok(());
            }
            Err(PrepareError::Fatal(kind)) => return Err(kind),
        };

        let carried = match self.correspond(&program) {
            Ok(carried) => carried,
            Err(PrepareError::Rejected(rejection)) => {
                self.reject(position, rejection);
                return Ok(());
            }
            Err(PrepareError::Fatal(kind)) => return Err(kind),
        };
        let carried_new = carried.iter().map(|(_, new)| *new).collect::<BTreeSet<_>>();
        if let Err(error) = self.instantiate(&mut program, &carried_new) {
            match error {
                PrepareError::Rejected(rejection) => {
                    self.reject(position, rejection);
                    return Ok(());
                }
                PrepareError::Fatal(kind) => return Err(kind),
            }
        }

        // Commit: the old program's kernels that are not carried over finish in the old generation, and the carried
        // state owners move to the new one.
        if let Some(mut old) = self.active.take() {
            let carried_old = carried.iter().map(|(old, _)| *old).collect::<BTreeSet<_>>();
            self.finish(&mut old, DrainPolicy::Retire, &carried_old)?;
            for (from, to) in carried {
                let exec = std::mem::replace(&mut old.nodes[from].exec, Exec::Drop);
                program.nodes[to].exec = exec;
            }
        }
        self.generations += 1;
        for output in &program.outputs {
            self.output.outputs.entry(output.clone()).or_default();
        }
        self.active = Some(program);
        self.output.installs.push(InstallRecord {
            event: position,
            outcome: InstallOutcome::Active { generation },
        });
        Ok(())
    }

    /// Matches the state owners of a candidate program with those of the active one, returning the `(old, new)`
    /// positions of the state owners whose instances carry over.
    fn correspond(&self, new: &Program) -> Result<Vec<(usize, usize)>, PrepareError> {
        let Some(old) = &self.active else {
            return Ok(Vec::new());
        };
        let unsupported = |capability: String, stage| {
            Err(PrepareError::Fatal(InterpretErrorKind::Unsupported {
                capability,
                stage,
            }))
        };

        let mut carried = Vec::new();
        for (to, node) in new.nodes.iter().enumerate() {
            let previous = old.index.get(&node.id).map(|&from| (from, &old.nodes[from]));
            match previous {
                Some((from, previous)) if previous.is_stateful() || node.is_stateful() => {
                    match previous.op.transition_to(&node.op) {
                        TransitionClass::Reject => {
                            return Err(PrepareError::Rejected(InstallRejection::Transition {
                                node: node.id.clone(),
                                class: TransitionClass::Reject,
                            }))
                        }
                        TransitionClass::Cut => {
                            return unsupported(format!("the `cut` transition of state owner `{}`", node.id), 6)
                        }
                        TransitionClass::Retain if previous.op.params() == node.op.params() => {
                            carried.push((from, to));
                        }
                        TransitionClass::Retain => {
                            return unsupported(
                                format!(
                                    "changing the parameters of state owner `{}` while it keeps its state",
                                    node.id
                                ),
                                6,
                            )
                        }
                    }
                }
                Some(_) => {}
                None if node.is_stateful() => {
                    let (what, stage) = if node.is_host() {
                        ("adding the host component", 7)
                    } else {
                        ("adding the state owner", 6)
                    };
                    return unsupported(format!("{} `{}` at an installation", what, node.id), stage);
                }
                None => {}
            }
        }
        for node in &old.nodes {
            if node.is_stateful() && !new.index.contains_key(&node.id) {
                let (what, stage) = if node.is_host() {
                    ("removing the host component", 7)
                } else {
                    ("removing the state owner", 6)
                };
                return unsupported(format!("{} `{}` at an installation", what, node.id), stage);
            }
        }
        Ok(carried)
    }

    /// Builds the instances of the stateful kernels and opaque hosts that do not carry over.
    fn instantiate(&self, program: &mut Program, carried: &BTreeSet<usize>) -> Result<(), PrepareError> {
        for (position, node) in program.nodes.iter_mut().enumerate() {
            if carried.contains(&position) {
                continue;
            }
            match &mut node.exec {
                Exec::Kernel {
                    spec,
                    kernel: kernel @ None,
                } => *kernel = Some(build_kernel(&node.id, spec)?),
                Exec::Host(adapter @ None) => {
                    let Some(factory) = self.interpreter.hosts.get(&node.id) else {
                        return Err(PrepareError::Fatal(InterpretErrorKind::MissingAdapter {
                            node: node.id.clone(),
                        }));
                    };
                    let built = factory(&node.op).map_err(|source| {
                        PrepareError::Rejected(InstallRejection::HostBuild {
                            node: node.id.clone(),
                            source,
                        })
                    })?;
                    *adapter = Some(built);
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// Returns a state owner's watermark: the earliest latest admission time of its upstream admission points.
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

    /// Processes deliveries until none are left, at host time `now`.
    fn flow(&mut self, program: &mut Program, mut queue: Queue, now: AdmissionTime) -> Result<(), InterpretErrorKind> {
        while let Some(Delivery { node, batch }) = queue.pop_front() {
            self.deliver(program, &mut queue, node, batch, now)?;
        }
        Ok(())
    }

    /// Runs one node over one batch.
    fn deliver(
        &mut self, program: &mut Program, queue: &mut Queue, node: usize, batch: Batch, now: AdmissionTime,
    ) -> Result<(), InterpretErrorKind> {
        let more = with_exec(program, node, |program, exec| {
            self.run_node(program, node, exec, batch, now)
        })?;
        queue.extend(more);
        Ok(())
    }

    /// Runs one node, whose execution state is `exec`, over one batch, and returns the deliveries it causes.
    fn run_node(
        &mut self, program: &Program, node: usize, exec: &mut Exec, batch: Batch, now: AdmissionTime,
    ) -> Result<Queue, InterpretErrorKind> {
        let Batch { header, metrics } = batch;
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
                for metric in metrics {
                    if predicate.evaluate(&mut EventView::new(&metric)) {
                        matched.push(metric);
                    } else {
                        unmatched.push(metric);
                    }
                }
                forward(program, &mut queue, node, *true_port, header.clone(), matched)?;
                forward(program, &mut queue, node, *false_port, header, unmatched)?;
            }
            Exec::Tee => {
                // Every port but the last receives a copy; the last receives the original.
                if let Some(last) = ports.checked_sub(1) {
                    for port in 0..last {
                        forward(program, &mut queue, node, port, header.clone(), metrics.clone())?;
                    }
                    forward(program, &mut queue, node, last, header, metrics)?;
                }
            }
            Exec::Merge => forward(program, &mut queue, node, 0, header, metrics)?,
            Exec::Emit(output) => {
                let generation = program.generation.id();
                let records = self.output.outputs.entry(output.clone()).or_default();
                records.extend(metrics.into_iter().map(|metric| EmittedRecord {
                    generation,
                    install: program.install,
                    admitted_at: header.admitted_at,
                    metric,
                }));
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
            } => {
                let mut input = to_buffer(metrics)?;
                let mut call = KernelCall::new(&header, self.instant(now), ports);
                call.repeat(id, |turn| kernel.consume(turn, &mut input))?;
                call.outputs.complete(input);
                queue = self.kernel_output(program, node, call, &header)?;
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
            Exec::Kernel { kernel: None, .. } | Exec::Host(None) => {
                return Err(InterpretErrorKind::Internal {
                    message: format!("node `{}` has no instance", id),
                })
            }
        }
        Ok(queue)
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

    /// Collects a kernel call's loss counts, and turns its output into deliveries carrying `header`.
    fn kernel_output(
        &mut self, program: &Program, node: usize, call: KernelCall, header: &BatchHeader,
    ) -> Result<Queue, InterpretErrorKind> {
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
                forward(program, &mut queue, node, port, header.clone(), metrics)?;
            }
        }
        Ok(queue)
    }

    /// Re-admits an opaque host's output at its output ports, at host time `now`, and collects the loss it reported.
    ///
    /// With `heartbeat`, every port's admission clock reads `now`, whether or not it emitted anything, as at a tick.
    fn readmit(
        &mut self, program: &Program, node: usize, mut output: HostOutput, now: AdmissionTime, heartbeat: bool,
    ) -> Result<Queue, InterpretErrorKind> {
        let (ports, counters) = output.take();
        self.tally_loss(&program.nodes[node].id, &counters);
        let mut queue = Queue::new();
        let limit = batch_size();
        for (port, metrics) in ports.into_iter().enumerate() {
            let Some(point) = program.points[node].get(port).copied().flatten() else {
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
                epoch: 0,
                generation: Some(program.generation.clone()),
                entry: None,
            };
            let mut metrics = metrics.into_iter().peekable();
            while metrics.peek().is_some() {
                let chunk = metrics.by_ref().take(limit).collect();
                forward(program, &mut queue, node, port, header.clone(), chunk)?;
            }
        }
        Ok(queue)
    }
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
) -> Result<(), InterpretErrorKind> {
    if metrics.is_empty() {
        return Ok(());
    }
    let Some((target, _)) = program.targets[node].get(port).copied().flatten() else {
        return Err(InterpretErrorKind::Internal {
            message: format!("output port {} of node `{}` has no edge", port, program.nodes[node].id),
        });
    };
    queue.push_back(Delivery {
        node: target,
        batch: Batch { header, metrics },
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

/// The header of a state owner's time-driven output: its watermark as `t_a`, under the program's generation.
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
mod test_operators;
#[cfg(test)]
mod tests;
