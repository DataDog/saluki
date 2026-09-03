use std::{
    future::Future,
    pin::Pin,
    sync::{
        atomic::{AtomicU64, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use async_trait::async_trait;
use saluki_common::sync::shutdown::ShutdownHandle;
use saluki_error::GenericError;
use serde::{Deserialize, Serialize};
use snafu::{OptionExt as _, Snafu};
use tokio::{pin, runtime::Handle, select, sync::mpsc};
use tracing::{debug, error, warn};

use super::{
    dedicated::{spawn_dedicated_runtime, RuntimeConfiguration, RuntimeMode},
    restart::{RestartAction, RestartMode, RestartState, RestartStrategy, RestartType},
    tree::{ChildFacts, ChildKey, NodeConfig, Roster, SupervisionTreeHandle, SupervisorNode},
    worker_state::WorkerState,
};
use crate::runtime::{
    process::{Process, ProcessExt as _},
    state::DataspaceRegistry,
};

/// Process name segment used for a child whose own name can't be turned into a valid process name.
///
/// See [`SupervisedChild::create_process`].
const UNNAMED_CHILD: &str = "unnamed";

/// A `Future` that represents the execution of a supervised process.
pub type SupervisorFuture = Pin<Box<dyn Future<Output = Result<(), GenericError>> + Send>>;

/// A `Future` that represents the full lifecycle of a worker, including initialization.
///
/// Unlike [`SupervisorFuture`], which only represents the runtime phase, this future first performs async
/// initialization and then runs the worker. This allows initialization to happen concurrently when multiple workers are
/// spawned, and keeps the supervisor loop responsive to shutdown signals during initialization.
pub(super) type WorkerFuture = Pin<Box<dyn Future<Output = Result<(), WorkerError>> + Send>>;

/// Worker lifecycle errors.
///
/// Distinguishes between initialization failures (which shouldn't trigger restart logic) and runtime failures (which
/// are eligible for restart).
#[derive(Debug)]
pub(super) enum WorkerError {
    /// The worker failed during async initialization.
    ///
    /// The optional `child_name` carries the name of the original failing child when the error originates from a
    /// nested supervisor. This allows the parent to include it in its own `FailedToInitialize` error for better
    /// diagnostics across supervision tree levels.
    Initialization {
        child_name: Option<String>,
        source: InitializationError,
    },

    /// The worker failed during runtime execution.
    Runtime(GenericError),

    /// The worker was a nested supervisor that completed a requested shutdown after forcefully aborting one or more of
    /// its own workers.
    ///
    /// Carried as a distinct variant (rather than collapsed into [`Runtime`][WorkerError::Runtime]) so the parent's
    /// shutdown drain can recover the structured count and merge it into its own tally, aggregating forced aborts up
    /// the supervision tree.
    ShutdownTimedOut {
        /// The number of workers the nested supervisor forcefully aborted, summed across its own supervision tree.
        aborted: usize,
    },
}

impl From<SupervisorError> for WorkerError {
    fn from(err: SupervisorError) -> Self {
        match err {
            // Propagate initialization failures so the parent supervisor does NOT attempt to restart.
            // Preserve the original child name so the parent can include it in diagnostics.
            SupervisorError::FailedToInitialize { child_name, source } => WorkerError::Initialization {
                child_name: Some(child_name),
                source,
            },
            // Preserve the structured abort count so the parent can merge it into its own shutdown tally.
            SupervisorError::ShutdownTimedOut { aborted } => WorkerError::ShutdownTimedOut { aborted },
            // All other supervisor errors (shutdown, no children, invalid name) are runtime-level.
            other => WorkerError::Runtime(other.into()),
        }
    }
}

/// Process errors.
#[derive(Debug, Snafu)]
pub enum ProcessError {
    /// The child process was aborted by the supervisor.
    #[snafu(display("Child process was aborted by the supervisor."))]
    Aborted,

    /// The child process panicked.
    #[snafu(display("Child process panicked."))]
    Panicked,

    /// The child process terminated with an error.
    #[snafu(display("Child process terminated with an error: {}", source))]
    Terminated {
        /// The error that caused the termination.
        source: GenericError,
    },
}

/// Initialization errors.
///
/// Initialization errors are distinct from runtime errors: they indicate that a process couldn't be started at all
/// (for example, failed to bind a port, missing configuration). These errors don't trigger restart logic; instead, they
/// immediately propagate up and fail the supervisor.
#[derive(Debug, Snafu)]
#[snafu(context(suffix(false)))]
pub enum InitializationError {
    /// The process couldn't be initialized due to an error.
    #[snafu(display("Process failed to initialize: {}", source))]
    Failed {
        /// The underlying error that caused initialization to fail.
        source: GenericError,
    },
}

impl From<GenericError> for InitializationError {
    fn from(source: GenericError) -> Self {
        Self::Failed { source }
    }
}

/// Strategy for shutting down a process.
#[derive(Clone, Copy, Debug)]
pub enum ShutdownStrategy {
    /// Waits for the configured duration for the process to exit, and then forcefully aborts it otherwise.
    Graceful(Duration),

    /// Forcefully aborts the process without waiting.
    Brutal,
}

/// Policy for automatically shutting a supervisor down based on the termination of its _significant_ children.
///
/// A significant child (see [`ChildBuilder::with_significant`][crate::runtime::ChildBuilder::with_significant]) is one whose termination -- when it isn't restarted -- can
/// drive the supervisor to shut down. This mirrors Erlang/OTP's `auto_shutdown` supervisor flag, and is how an
/// unexpected (or intentional) child exit cascades into the supervisor stopping, and thus propagating up the tree,
/// without that child being restarted.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AutoShutdown {
    /// Never shut down automatically; significant children have no special effect. This is the default.
    #[default]
    Never,

    /// Shut down as soon as _any_ significant child terminates without being restarted.
    AnySignificant,

    /// Shut down once _all_ significant children have terminated without being restarted.
    AllSignificant,
}

/// A supervisable process.
#[async_trait]
pub trait Supervisable: Send + Sync {
    /// Returns the name of the process.
    fn name(&self) -> &str;

    /// Returns the shutdown strategy for the process.
    fn shutdown_strategy(&self) -> ShutdownStrategy {
        ShutdownStrategy::Graceful(Duration::from_secs(5))
    }

    /// Returns whether this process observes the shutdown signal it is given.
    ///
    /// Shutting a subtree down is a _trigger_, not an enforcement: many workers ignore the signal entirely and stop
    /// only when they reach their own terminal condition, such as an input channel closing. Reporting `false` lets the
    /// supervisor skip creating a shutdown coordinator it would never usefully fire, and hand the process a
    /// [`ShutdownHandle::noop`] instead.
    ///
    /// This says nothing about _whether_ the supervisor waits for the process -- that's
    /// [`shutdown_strategy`][Self::shutdown_strategy]. A process that ignores the signal is still waited for, up to
    /// whatever deadline applies to it.
    ///
    /// Defaults to `true`.
    fn wants_shutdown_signal(&self) -> bool {
        true
    }

    /// Initializes the process asynchronously.
    ///
    /// During initialization, any resources or configuration for the process can be created asynchronously, and the
    /// same runtime that's used for running the process is used for initialization. The resulting future is expected to
    /// complete as soon as reasonably possible after `shutdown` resolves.
    ///
    /// **Important:** The `process_shutdown` signal must be moved into the returned [`SupervisorFuture`] so the worker
    /// can respond to supervisor-initiated shutdown. If `process_shutdown` is dropped during initialization, the worker
    /// will be unable to shut down gracefully and will be forcefully aborted after the shutdown timeout.
    ///
    /// # Errors
    ///
    /// If the process can't be initialized, an error is returned.
    async fn initialize(&self, process_shutdown: ShutdownHandle) -> Result<SupervisorFuture, InitializationError>;
}

/// Supervisor errors.
#[derive(Debug, Snafu)]
#[snafu(context(suffix(false)))]
pub enum SupervisorError {
    /// Supervisor or worker name is invalid.
    #[snafu(display("Invalid name for supervisor or worker: '{}'", name))]
    InvalidName {
        /// The name of the supervisor is invalid.
        name: String,
    },

    /// A child process failed to initialize.
    ///
    /// This error indicates that a child couldn't complete its async initialization. This is distinct from runtime
    /// failures and doesn't trigger restart logic.
    #[snafu(display("Child process '{}' failed to initialize: {}", child_name, source))]
    FailedToInitialize {
        /// The name of the child that failed to initialize.
        child_name: String,

        /// The underlying initialization error.
        source: InitializationError,
    },

    /// The supervisor exceeded its restart limits and was forced to shutdown.
    #[snafu(display("Supervisor has exceeded restart limits and was forced to shutdown."))]
    Shutdown,

    /// The supervisor shut down because a significant child terminated.
    ///
    /// See [`AutoShutdown`] and [`ChildBuilder::with_significant`][crate::runtime::ChildBuilder::with_significant]. The supervisor stopped, and drained its remaining
    /// children, because a child marked significant terminated without being restarted.
    #[snafu(display("Supervisor shut down after a significant child terminated."))]
    SignificantChildExited,

    /// The supervisor completed a requested shutdown, but one or more workers ignored graceful shutdown and had to be
    /// forcefully aborted after exceeding their shutdown timeout.
    ///
    /// The shutdown itself was requested and otherwise orderly; this variant exists so that having to forcefully stop a
    /// worker is surfaced as a failure rather than reported as a clean shutdown. The count aggregates forced aborts
    /// across the entire supervision tree: a parent merges in the counts reported by any child supervisors that also
    /// timed out, so the value observed at the root supervisor is the total number of workers that had to be
    /// force-stopped tree-wide.
    #[snafu(display(
        "Shutdown completed uncleanly: {} worker(s) were forcefully aborted after exceeding their shutdown timeout.",
        aborted
    ))]
    ShutdownTimedOut {
        /// The number of workers that had to be forcefully aborted.
        aborted: usize,
    },
}

/// A specification for a process to be added to a [`Supervisor`].
///
/// A child specification describes how the supervisor should create and manage a child: the underlying future that
/// represents the process, along with metadata such as its name and shutdown strategy. All processes in a supervisor,
/// whether a worker or a (nested) supervisor, are represented by a [`ChildSpecification`].
///
/// A specification is a description, not a control surface: it carries no public methods of its own. There are two
/// ways to obtain one, matching the two levels of control:
///
/// - Pass a worker or supervisor directly to [`add_worker`][Supervisor::add_worker], [`spawn`][crate::runtime::spawn],
///   or [`SupervisorHandle::spawn`], all of which accept a [`Supervisor`] or any [`Supervisable`] and convert it for
///   you, applying the defaults appropriate to that kind of child -- including the shutdown strategy that lets a
///   nested supervisor drain its whole subtree.
/// - Configure one with [`ChildBuilder`][crate::runtime::ChildBuilder], which is the only way to set a restart policy,
///   significance, placement, or a shutdown deadline. The builder exposes only the settings that make sense for the
///   kind of child being described, and [`build`][crate::runtime::ChildBuilder::build] hands the result to
///   [`add_worker`][Supervisor::add_worker].
///
/// Supervisors have no per-child settings of their own, so there is nothing to configure for a nested supervisor.
pub struct ChildSpecification<S = WorkerSpec> {
    spec_inner: S,
}

/// Child specification state for a worker.
pub struct WorkerSpec {
    worker: Arc<dyn Supervisable>,
    options: ChildOptions,
}

/// Child specification state for a supervisor.
pub struct SupervisorSpec {
    supervisor: Supervisor,
    options: ChildOptions,
}

// The configuration surface below is deliberately crate-internal: `ChildBuilder` is the public front end for all of
// it, and is what decides which settings are offered for which kind of child. Keeping these methods off the public API
// means a combination the builder refuses to express -- a permanent child marked significant, say -- can't be reached
// by going around it. In-crate callers use them directly where the builder would be a layering inversion: this
// module's own tests, which exercise the lowering these methods feed.
impl ChildSpecification<WorkerSpec> {
    /// Creates a specification for the given worker.
    pub(crate) fn worker<T: Supervisable + 'static>(worker: T) -> Self {
        Self {
            spec_inner: WorkerSpec {
                worker: Arc::new(worker),
                options: ChildOptions::default(),
            },
        }
    }

    /// Creates a specification for a worker that can only run once.
    ///
    /// This function is shorthand for calling [`worker`][Self::worker] followed by
    /// [`with_restart_type`][Self::with_restart_type] set to [`RestartType::Temporary`][RestartType::Temporary].
    pub(crate) fn one_shot_worker<T: Supervisable + 'static>(worker: T) -> Self {
        Self::worker(worker).with_restart_type(RestartType::Temporary)
    }

    /// Sets the restart policy for this worker.
    ///
    /// When left unset, the policy depends on how the child is registered: a child added up front with
    /// [`Supervisor::add_worker`] defaults to [`RestartType::Permanent`], while one spawned dynamically with
    /// [`SupervisorHandle::spawn`] defaults to [`RestartType::Temporary`].
    #[must_use]
    pub(crate) fn with_restart_type(mut self, restart_type: RestartType) -> Self {
        self.spec_inner.options.restart = Some(restart_type);
        self
    }

    /// Sets whether this worker is _significant_.
    ///
    /// A significant worker's termination (when it isn't restarted) can drive the supervisor to shut down, per the
    /// supervisor's [`AutoShutdown`] policy. Only meaningful for non-permanent workers, since a permanent worker is
    /// always restarted and so never terminates without being restarted.
    #[must_use]
    pub(crate) fn with_significant(mut self, significant: bool) -> Self {
        self.spec_inner.options.significant = significant;
        self
    }

    /// Runs this worker on the given Tokio runtime rather than the supervisor's own runtime.
    ///
    /// By default, a worker runs on whatever runtime its supervisor runs on. Use this for compute-heavy workers that
    /// shouldn't contend with the supervisor's runtime -- for example, a topology component offloading encoding work
    /// onto a shared worker pool.
    ///
    /// Note that this only affects where the worker's task is spawned. Supervision itself -- shutdown signalling,
    /// restart handling, and abort-on-timeout -- is unchanged, and is still driven from the supervisor's runtime.
    #[must_use]
    pub(crate) fn with_runtime(mut self, handle: Handle) -> Self {
        self.spec_inner.options.runtime = Some(handle);
        self
    }

    /// Overrides the shutdown strategy for this worker.
    ///
    /// By default, a worker's strategy comes from [`Supervisable::shutdown_strategy`], which itself defaults to
    /// `Graceful(5s)`. Use this when the grace period depends on where the worker is used rather than on the worker
    /// type: a worker that a component drains during its own shutdown needs at least as long as the component itself,
    /// otherwise it is forcefully aborted while the component is still waiting on it.
    #[must_use]
    pub(crate) fn with_shutdown_strategy(mut self, strategy: ShutdownStrategy) -> Self {
        self.spec_inner.options.shutdown = ChildShutdown::Explicit(strategy);
        self
    }

    /// Gives this worker no shutdown deadline of its own, leaving it bounded solely by its supervisor's shutdown
    /// budget.
    ///
    /// Use this for a worker whose acceptable drain time is a property of the subtree it belongs to rather than of the
    /// worker itself -- a task spawned by a topology component, for instance, where what matters is that the component
    /// as a whole stops in time. See [`Supervisor::with_shutdown_budget`].
    ///
    /// A supervisor with no budget has nothing to bound the worker with, so in that case the worker falls back to the
    /// strategy it reports through [`Supervisable::shutdown_strategy`] rather than being left to stall the drain
    /// indefinitely.
    #[must_use]
    pub(crate) fn with_budget_bounded_shutdown(mut self) -> Self {
        self.spec_inner.options.shutdown = ChildShutdown::BudgetBounded;
        self
    }
}

// Crate-internal for the same reason as the worker surface above: `NestedSupervisorBuilder` is the public front end,
// and going around it would allow combinations the builder refuses to express.
//
// Deliberately narrower than the worker surface, though. A nested supervisor bounds its own subtree through its
// children's deadlines: `SupervisedChild::shutdown_strategy` reports `Graceful(Duration::MAX)` for one, and
// `WorkerState::add_worker` exempts it from the parent's budget. Offering a shutdown setting here would let a caller
// truncate a drain the subtree is already responsible for, so there isn't one. Placement is likewise absent: a nested
// supervisor runs wherever its parent does, and its children carry their own placement.
impl ChildSpecification<SupervisorSpec> {
    /// Sets the restart policy for this nested supervisor.
    ///
    /// When left unset, the policy depends on how the child is registered: a child added up front with
    /// [`Supervisor::add_worker`] defaults to [`RestartType::Permanent`], while one spawned dynamically with
    /// [`SupervisorHandle::spawn`] defaults to [`RestartType::Temporary`].
    #[must_use]
    pub(crate) fn with_restart_type(mut self, restart_type: RestartType) -> Self {
        self.spec_inner.options.restart = Some(restart_type);
        self
    }

    /// Sets whether this nested supervisor is _significant_.
    ///
    /// A significant child's termination (when it isn't restarted) can drive the parent supervisor to shut down, per
    /// the parent's [`AutoShutdown`] policy.
    #[must_use]
    pub(crate) fn with_significant(mut self, significant: bool) -> Self {
        self.spec_inner.options.significant = significant;
        self
    }
}

impl<T> From<T> for ChildSpecification<WorkerSpec>
where
    T: Supervisable + 'static,
{
    fn from(worker: T) -> Self {
        Self::worker(worker)
    }
}

impl From<Supervisor> for ChildSpecification<SupervisorSpec> {
    fn from(supervisor: Supervisor) -> Self {
        Self {
            spec_inner: SupervisorSpec {
                supervisor,
                options: ChildOptions::default(),
            },
        }
    }
}

mod sealed {
    pub trait Sealed {}
}

impl sealed::Sealed for WorkerSpec {}
impl sealed::Sealed for SupervisorSpec {}

/// Child specification state.
///
/// This trait is sealed -- it cannot be implemented outside of this crate -- and is implemented only for
/// [`WorkerSpec`] and [`SupervisorSpec`]. It exists so that [`Supervisor::add_worker`] and
/// [`SupervisorHandle::spawn`] can both accept a [`ChildSpecification`] in either state (as well as bare workers and
/// supervisors) while lowering each into the supervisor's internal representation.
pub trait ChildState: sealed::Sealed + Sized {
    /// Lowers a specification into the supervisor's internal representation of a child.
    ///
    /// `default_restart` supplies the restart policy for a specification that didn't set one, which differs by
    /// registration path: children added up front are permanent, dynamically spawned children are temporary.
    #[doc(hidden)]
    fn into_child_parts(spec: ChildSpecification<Self>, default_restart: RestartType) -> LoweredChild;
}

/// A child specification lowered into the supervisor's internal representation.
///
/// Opaque to callers: it exists only to carry the output of [`ChildState::into_child_parts`] to the supervisor that
/// registers the child, and is public only because [`ChildState`] is.
pub struct LoweredChild {
    spec: SupervisedChild,
    config: ChildConfig,
}

impl ChildState for WorkerSpec {
    fn into_child_parts(spec: ChildSpecification<Self>, default_restart: RestartType) -> LoweredChild {
        let WorkerSpec { worker, options } = spec.spec_inner;
        LoweredChild {
            spec: SupervisedChild::Worker(worker),
            config: options.resolve(default_restart),
        }
    }
}

impl ChildState for SupervisorSpec {
    fn into_child_parts(spec: ChildSpecification<Self>, default_restart: RestartType) -> LoweredChild {
        let SupervisorSpec { supervisor, options } = spec.spec_inner;
        LoweredChild {
            spec: SupervisedChild::Supervisor(supervisor),
            config: options.resolve(default_restart),
        }
    }
}

/// The type-erased, runnable form of a child: either a worker or a nested supervisor.
pub(super) enum SupervisedChild {
    Worker(Arc<dyn Supervisable>),
    Supervisor(Supervisor),
}

impl SupervisedChild {
    /// Returns whether this child is a nested supervisor rather than a leaf worker.
    pub(super) fn is_supervisor(&self) -> bool {
        matches!(self, Self::Supervisor(_))
    }

    /// Returns the child's own supervision-tree bookkeeping, if the child is a nested supervisor.
    ///
    /// This is the link that makes the tree walkable: a parent records its child's node alongside its own, so an
    /// observer holding the parent's node can descend into the child's.
    pub(super) fn node(&self) -> Option<Arc<SupervisorNode>> {
        match self {
            Self::Worker(_) => None,
            Self::Supervisor(supervisor) => Some(Arc::clone(&supervisor.node)),
        }
    }

    fn process_type(&self) -> &'static str {
        match self {
            Self::Worker(_) => "worker",
            Self::Supervisor(_) => "supervisor",
        }
    }

    fn name(&self) -> &str {
        match self {
            Self::Worker(worker) => worker.name(),
            Self::Supervisor(supervisor) => &supervisor.supervisor_id,
        }
    }

    /// Returns whether this child observes the shutdown signal it is given.
    ///
    /// Always true for a nested supervisor: the signal is how it learns to drain its own subtree, and a supervisor on
    /// a dedicated runtime receives it across the thread boundary through `spawn_dedicated_runtime`, where aborting
    /// the awaiting future wouldn't stop the runtime thread anyway.
    pub(super) fn wants_shutdown_signal(&self) -> bool {
        match self {
            Self::Worker(worker) => worker.wants_shutdown_signal(),
            Self::Supervisor(_) => true,
        }
    }

    pub(super) fn shutdown_strategy(&self) -> ShutdownStrategy {
        match self {
            Self::Worker(worker) => worker.shutdown_strategy(),

            // Supervisors should always be given as much time as necessary shutdown down gracefully to ensure that the
            // entire supervision subtree can be shutdown cleanly.
            Self::Supervisor(_) => ShutdownStrategy::Graceful(Duration::MAX),
        }
    }

    /// Creates the process for this child under `parent_process`.
    ///
    /// A name that sanitizes to nothing at all (an empty string, or one made up entirely of separators) can't be used
    /// as a process name. Rather than refuse to start the child -- which for a dynamically spawned child would mean
    /// silently losing work that the caller was told had been accepted -- the child runs under
    /// [`UNNAMED_CHILD`] instead, and the substitution is logged.
    pub(super) fn create_process(&self, parent_process: &Process) -> Process {
        let name = self.name();
        let process = match self {
            Self::Worker(_) => Process::worker(name, parent_process),
            Self::Supervisor(_) => Process::supervisor(name, Some(parent_process)),
        };

        process.unwrap_or_else(|| {
            warn!(
                parent_process = parent_process.name(),
                child_name = name,
                "Child process name is not usable as a process name; falling back to '{}'.",
                UNNAMED_CHILD
            );

            match self {
                Self::Worker(_) => Process::worker(UNNAMED_CHILD, parent_process),
                Self::Supervisor(_) => Process::supervisor(UNNAMED_CHILD, Some(parent_process)),
            }
            .expect("placeholder child name is always a valid process name")
        })
    }

    pub(super) fn create_worker_future(
        &self, process: Process, process_shutdown: ShutdownHandle,
    ) -> Result<WorkerFuture, SupervisorError> {
        match self {
            Self::Worker(worker) => {
                let worker = Arc::clone(worker);
                Ok(Box::pin(async move {
                    let run_future =
                        worker
                            .initialize(process_shutdown)
                            .await
                            .map_err(|source| WorkerError::Initialization {
                                child_name: None,
                                source,
                            })?;
                    run_future.await.map_err(WorkerError::Runtime)
                }))
            }
            Self::Supervisor(sup) => {
                match sup.runtime_mode() {
                    RuntimeMode::Ambient => {
                        // Run on the parent's ambient runtime.
                        Ok(sup.as_nested_process(process, process_shutdown))
                    }
                    RuntimeMode::Dedicated(config) => {
                        // Spawn in a dedicated runtime on a new OS thread, passing the parent's
                        // dataspace so the nested supervisor inherits it across the thread boundary.
                        //
                        // TODO: Only the dataspace is carried across, so the supervisor re-roots its own process name
                        // when it starts (`run_with_shutdown_inner` passes no parent) rather than staying scoped
                        // under us. That also leaves the process we build here registered as a resource group that
                        // nothing ever enters, so it reads zero forever. Threading this process through instead would
                        // fix both, at the cost of renaming the affected resource groups and their metric labels.
                        let child_name = sup.supervisor_id.to_string();
                        let dataspace = process.dataspace().clone();
                        let handle =
                            spawn_dedicated_runtime(sup.inner_clone(), config.clone(), process_shutdown, dataspace)
                                .map_err(|e| SupervisorError::FailedToInitialize {
                                    child_name,
                                    source: e.into(),
                                })?;

                        Ok(Box::pin(async move { handle.await.map_err(WorkerError::from) }))
                    }
                }
            }
        }
    }
}

impl Clone for SupervisedChild {
    fn clone(&self) -> Self {
        match self {
            Self::Worker(worker) => Self::Worker(Arc::clone(worker)),
            Self::Supervisor(supervisor) => Self::Supervisor(supervisor.inner_clone()),
        }
    }
}

/// How a child's shutdown strategy is determined.
#[derive(Clone, Copy, Debug, Default)]
pub(super) enum ChildShutdown {
    /// Use whatever the worker reports through [`Supervisable::shutdown_strategy`]. This is the default.
    #[default]
    Worker,

    /// Use this strategy, overriding whatever the worker reports.
    Explicit(ShutdownStrategy),

    /// The child carries no deadline of its own and is bounded solely by its supervisor's shutdown budget.
    ///
    /// A supervisor with no budget has nothing to bound the child with, so this falls back to the worker's own
    /// strategy rather than leaving the child free to stall the drain indefinitely.
    BudgetBounded,
}

/// Per-child settings as configured on a [`ChildSpecification`], before they are resolved for a specific registration
/// path.
///
/// Separate from [`ChildConfig`] because the restart policy has no single default: a child registered up front with
/// [`Supervisor::add_worker`] is permanent, while one spawned dynamically with [`SupervisorHandle::spawn`] is
/// temporary. Leaving the policy unset here is what lets both paths share one specification type.
#[derive(Clone, Debug, Default)]
pub(super) struct ChildOptions {
    restart: Option<RestartType>,
    significant: bool,

    /// Runtime to spawn the child on. `None` means the supervisor's own runtime.
    runtime: Option<Handle>,

    shutdown: ChildShutdown,
}

impl ChildOptions {
    /// Resolves these options into a concrete configuration, applying `default_restart` if no policy was set.
    fn resolve(self, default_restart: RestartType) -> ChildConfig {
        ChildConfig {
            restart: self.restart.unwrap_or(default_restart),
            significant: self.significant,
            runtime: self.runtime,
            shutdown: self.shutdown,
        }
    }
}

/// Per-child configuration: its [`RestartType`], whether it is _significant_ (see [`AutoShutdown`]), the runtime it
/// runs on, and how its shutdown strategy is decided.
#[derive(Clone, Debug)]
pub(super) struct ChildConfig {
    restart: RestartType,
    significant: bool,
    runtime: Option<Handle>,
    shutdown: ChildShutdown,
}

impl ChildConfig {
    /// Returns the runtime the child should be spawned on, if it isn't the supervisor's own.
    pub(super) fn runtime(&self) -> Option<&Handle> {
        self.runtime.as_ref()
    }

    /// Returns how the child's shutdown strategy should be determined.
    pub(super) fn shutdown(&self) -> ChildShutdown {
        self.shutdown
    }
}

/// A registered child: its specification together with the configuration chosen at registration time.
#[derive(Clone)]
struct ChildEntry {
    spec: SupervisedChild,
    config: ChildConfig,
    /// Whether this child was added dynamically (via [`SupervisorHandle`]) rather than statically before the run. Used
    /// to maintain the dynamic-children gauge.
    dynamic: bool,
}

/// Identifier for a child managed by a [`Supervisor`].
///
/// Returned by [`SupervisorHandle::spawn`] for dynamically spawned children. Unique within a single process for the
/// lifetime of a supervisor run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ChildId(u64);

impl ChildId {
    /// Returns the raw numeric value of this identifier.
    pub const fn as_u64(self) -> u64 {
        self.0
    }
}

/// A dynamic spawn request handed from a [`SupervisorHandle`] to the running supervisor.
struct PendingSpawn {
    id: u64,
    spec: SupervisedChild,
    config: ChildConfig,
}

/// Number of queued spawn requests the supervisor takes in one go before returning to its loop.
///
/// Draining in batches keeps a burst of spawns to a single wake-up rather than one per child, while still bounding how
/// long the supervisor can spend registering children before it re-checks the rest of its loop (most importantly,
/// shutdown).
const SPAWN_DRAIN_BATCH: usize = 64;

/// A handle for spawning dynamic children on a running [`Supervisor`].
///
/// Obtained from [`Supervisor::handle`]. Handles are cheap to clone and can be shared across tasks.
///
/// Spawning is synchronous and infallible, in the spirit of [`tokio::spawn`]: the child is queued for the running
/// supervisor and the call returns immediately with the child's [`ChildId`]. Also as with [`tokio::spawn`], being
/// accepted is not a promise of being run -- if the supervisor isn't running, or shuts down before it gets to the
/// queued child, the child is never started at all.
///
/// # Ambient spawning
///
/// Code running under supervision usually doesn't need a handle at all: [`spawn`][crate::runtime::spawn] targets the
/// supervisor of whatever process is currently running. Use a handle when spawning from outside supervision, or when
/// targeting a supervisor other than the ambient one. [`scope`][Self::scope] bridges the two by making a handle the
/// ambient supervisor for a future.
#[derive(Clone)]
pub struct SupervisorHandle {
    name: Arc<str>,
    // The currently running supervisor publishes its spawn queue here so handles can reach the live run; it's cleared
    // when no run is active, at which point spawns are accepted and dropped.
    current_tx: Arc<Mutex<Option<mpsc::UnboundedSender<PendingSpawn>>>>,
    id_counter: Arc<AtomicU64>,
    active: Arc<AtomicUsize>,
}

impl SupervisorHandle {
    /// Returns the name of the supervisor this handle refers to.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Spawns a new dynamic child.
    ///
    /// Accepts anything [`Supervisor::add_worker`] accepts: a bare [`Supervisable`], a [`Supervisor`] to run as a
    /// nested supervision subtree, or a [`ChildSpecification`] configured in detail.
    ///
    /// Unless [`ChildBuilder`][crate::runtime::ChildBuilder] says otherwise, dynamic children are
    /// [`temporary`][RestartType::Temporary]: they
    /// aren't restarted when they die, and they aren't restored when the supervisor itself restarts. That suits
    /// short-lived, non-critical work that still wants structured concurrency -- the child is stopped when the
    /// supervisor is restarted or terminated.
    ///
    /// The returned [`ChildId`] identifies the child for the lifetime of the supervisor run. The child is queued
    /// rather than started synchronously, so it may not have begun running by the time this returns; if the supervisor
    /// isn't running, or shuts down before reaching the child, it never runs at all.
    pub fn spawn<S, T>(&self, child: T) -> ChildId
    where
        S: ChildState,
        T: Into<ChildSpecification<S>>,
    {
        let LoweredChild { spec, config } = S::into_child_parts(child.into(), RestartType::Temporary);

        // Take the id before we try to enqueue: the caller gets a stable identifier either way, and ids are only
        // meaningful within a run.
        let id = self.id_counter.fetch_add(1, Ordering::Relaxed);
        let pending = PendingSpawn { id, spec, config };

        // Clone the sender out from under the lock rather than sending while holding the guard.
        //
        // The queue behind it is unbounded on purpose: a queued child is a child that will be started, so the only
        // thing a depth limit could buy is discarding work the caller was told had been accepted. A backlog only forms
        // while the supervisor can't drain -- mid-restart, or mid-drain -- and holding it until it can is the whole
        // point.
        let tx = self.current_tx.lock().unwrap().clone();
        match tx {
            // Racing a teardown is normal rather than exceptional -- a source that spawns a child per connection will
            // do it every time it is shut down mid-accept -- so this stays at debug level.
            Some(tx) => {
                if let Err(e) = tx.send(pending) {
                    debug!(
                        supervisor_id = %self.name,
                        child_name = e.0.spec.name(),
                        "Supervisor is shutting down; dynamic child will not be started."
                    );
                }
            }
            // Spawning against a supervisor that never ran, on the other hand, is a wiring mistake: nothing about the
            // program's normal operation produces it, and the child is silently lost.
            None => warn!(
                supervisor_id = %self.name,
                child_name = pending.spec.name(),
                "Supervisor is not running; dynamic child will not be started."
            ),
        }

        ChildId(id)
    }

    /// Returns whether the supervisor is currently running.
    pub fn is_running(&self) -> bool {
        self.current_tx.lock().unwrap().is_some()
    }

    /// Returns the number of dynamic children currently running under the supervisor.
    ///
    /// Counts children the supervisor has actually started, so a child that has been spawned but not yet picked up
    /// isn't included yet.
    pub fn active_children(&self) -> usize {
        self.active.load(Ordering::Relaxed)
    }
}

/// Supervises a set of workers.
///
/// # Workers
///
/// All workers are defined through implementation of the [`Supervisable`] trait, which provides the logic for both
/// creating the underlying worker future that's spawned, as well as other metadata, such as the worker's name, how the
/// worker should be shutdown, and so on.
///
/// Supervisors also (indirectly) implement the [`Supervisable`] trait, allowing them to be supervised by other
/// supervisors in order to construct _supervision trees_.
///
/// # Instrumentation
///
/// Supervisors automatically create their own allocation group
/// ([`TrackingAllocator`][saluki_common::resource_tracking::TrackingAllocator]), which is used to track both the memory
/// usage of the supervisor itself and its children. Additionally, individual worker processes are wrapped in a
/// dedicated [`tracing::Span`] to allow tracing the causal relationship between arbitrary code and the worker executing
/// it, and statistics about task polls (poll count, poll duration) are collected.
///
/// # Restart Strategies
///
/// As the main purpose of a supervisor, restart behavior is fully configurable. A number of restart strategies are
/// available, which generally relate to the purpose of the supervisor: whether the workers being managed are
/// independent or interdependent.
///
/// All restart strategies are configured through [`RestartStrategy`], which has more information on the available
/// strategies and configuration settings.
pub struct Supervisor {
    supervisor_id: Arc<str>,
    child_specs: Vec<ChildEntry>,
    runtime_mode: RuntimeMode,
    // Shared across clones (a nested supervisor is cloned each time it runs) and across all handles. While a run is
    // active it holds that run's spawn queue so handles can reach the live supervisor; it's `None` whenever no run is
    // active, at which point spawned children are dropped rather than queued. Doubles as the `is_running` signal.
    current_tx: Arc<Mutex<Option<mpsc::UnboundedSender<PendingSpawn>>>>,
    id_counter: Arc<AtomicU64>,
    // Number of dynamic children currently running, shared with handles so it can be surfaced as a gauge.
    active: Arc<AtomicUsize>,
    // Observable bookkeeping for this supervisor, shared with clones (so every generation writes to the same place),
    // with this supervisor's parent (so the tree can be walked downward), and with any tree handle.
    node: Arc<SupervisorNode>,
}

impl Supervisor {
    /// Creates an empty `Supervisor` with the default restart strategy.
    pub fn new<S: AsRef<str>>(supervisor_id: S) -> Result<Self, SupervisorError> {
        // We try to throw an error about invalid names as early as possible. This is a manual check, so we might still
        // encounter an error later when actually running the supervisor, but this is a good first step to catch the
        // bulk of invalid names.
        if supervisor_id.as_ref().is_empty() {
            return Err(SupervisorError::InvalidName {
                name: supervisor_id.as_ref().to_string(),
            });
        }

        let supervisor_id: Arc<str> = supervisor_id.as_ref().into();

        Ok(Self {
            node: Arc::new(SupervisorNode::new(Arc::clone(&supervisor_id))),
            supervisor_id,
            child_specs: Vec::new(),
            runtime_mode: RuntimeMode::default(),
            current_tx: Arc::new(Mutex::new(None)),
            id_counter: Arc::new(AtomicU64::new(0)),
            active: Arc::new(AtomicUsize::new(0)),
        })
    }

    /// Returns the supervisor's ID.
    pub fn id(&self) -> &str {
        &self.supervisor_id
    }

    /// Sets the restart strategy for the supervisor.
    pub fn with_restart_strategy(self, strategy: RestartStrategy) -> Self {
        self.node.update_config(|config| config.restart_strategy = strategy);
        self
    }

    /// Sets the supervisor's automatic-shutdown policy.
    ///
    /// Controls whether the termination of _significant_ children (see [`ChildBuilder::with_significant`][crate::runtime::ChildBuilder::with_significant]) drives the
    /// supervisor to shut down. Defaults to [`AutoShutdown::Never`].
    pub fn with_auto_shutdown(self, auto_shutdown: AutoShutdown) -> Self {
        self.node.update_config(|config| config.auto_shutdown = auto_shutdown);
        self
    }

    /// Bounds how long this supervisor waits for its worker children during shutdown.
    ///
    /// Without a budget, a supervisor waits as long as each child's own [`ShutdownStrategy`] allows, and waits
    /// indefinitely for any child that has no finite deadline of its own. A budget makes the supervisor responsible for
    /// the deadline instead: children need no individual timeouts, and whatever is still running when the budget
    /// elapses is forcefully aborted -- each one named in the logs, and counted in the resulting
    /// [`SupervisorError::ShutdownTimedOut`].
    ///
    /// The budget is a ceiling, not a replacement: a child that also carries its own finite deadline is still held to
    /// whichever elapses first.
    ///
    /// Since children are always drained concurrently, the budget bounds the drain as a whole rather than accruing
    /// per child: it is measured from the moment shutdown begins, and every child is held to it simultaneously.
    ///
    /// Two kinds of child are outside it. A nested supervisor is never cut off by its parent's budget -- it bounds its
    /// own subtree, and aborting it would both truncate that drain and, for a supervisor running on a dedicated
    /// runtime, fail to stop it at all. A [`ShutdownStrategy::Brutal`] child is aborted up front and never waited on.
    /// Neither can a budget bound work that ignores cancellation, since an abort only takes effect at an await point.
    ///
    /// Use this where one deadline for a whole subtree is more meaningful than a guess per worker -- a topology
    /// component and its background tasks, for instance, where what matters is that the component as a whole stops in
    /// time.
    #[must_use]
    pub fn with_shutdown_budget(self, budget: Duration) -> Self {
        self.node.update_config(|config| config.shutdown_budget = Some(budget));
        self
    }

    /// Returns a handle for spawning dynamic children on this supervisor while it runs.
    ///
    /// The handle can be created before the supervisor starts and cloned freely. Spawning through it always succeeds,
    /// but a child is only ever started while the supervisor is actually running: one spawned before the supervisor
    /// starts, or after it has shut down, is accepted and then dropped.
    pub fn handle(&self) -> SupervisorHandle {
        SupervisorHandle {
            name: Arc::clone(&self.supervisor_id),
            current_tx: Arc::clone(&self.current_tx),
            id_counter: Arc::clone(&self.id_counter),
            active: Arc::clone(&self.active),
        }
    }

    /// Returns a read-only handle for taking snapshots of this supervisor and the subtree beneath it.
    ///
    /// The handle can be created before the supervisor starts and remains valid across every restart of it. Unlike
    /// [`handle`][Self::handle], it grants no ability to affect the supervisor -- only to observe it.
    pub fn tree_handle(&self) -> SupervisionTreeHandle {
        SupervisionTreeHandle::new(Arc::clone(&self.node))
    }

    /// Configures this supervisor to run in a dedicated runtime.
    ///
    /// When this supervisor is added as a child to another supervisor, it will spawn its own OS threads and Tokio
    /// runtime instead of running on the parent's ambient runtime.
    ///
    /// This provides runtime isolation, which can be useful for:
    /// - CPU-bound work that shouldn't block the parent's runtime
    /// - Isolating failures in one part of the system
    /// - Using different runtime configurations (for example, single-threaded vs multi-threaded)
    pub fn with_dedicated_runtime(mut self, config: RuntimeConfiguration) -> Self {
        let worker_threads = config.worker_threads();
        self.node
            .update_config(|node_config| node_config.dedicated_threads = Some(worker_threads));
        self.runtime_mode = RuntimeMode::Dedicated(config);
        self
    }

    /// Returns the runtime mode for this supervisor.
    pub(crate) fn runtime_mode(&self) -> &RuntimeMode {
        &self.runtime_mode
    }

    /// Adds a worker (or nested supervisor) to the supervisor.
    ///
    /// A worker can be anything that implements the [`Supervisable`] trait. A [`Supervisor`] can also be added as a
    /// worker and managed in a nested fashion, known as a supervision tree.
    ///
    /// Anything that needs configuring -- a restart policy, significance, placement, a shutdown deadline -- is
    /// described with [`ChildBuilder`][crate::runtime::ChildBuilder] and handed over via
    /// [`build`][crate::runtime::ChildBuilder::build]. See [`ChildSpecification`] for how children are represented
    /// internally.
    pub fn add_worker<S, T>(&mut self, child: T)
    where
        S: ChildState,
        T: Into<ChildSpecification<S>>,
    {
        let LoweredChild { spec, config } = S::into_child_parts(child.into(), RestartType::Permanent);
        self.push_child(ChildEntry {
            spec,
            config,
            dynamic: false,
        });
    }

    /// Warns when a child was marked significant but nothing will act on it.
    ///
    /// Significance only has an effect for a child that can terminate without being restarted, under a supervisor
    /// whose [`AutoShutdown`] policy isn't [`Never`][AutoShutdown::Never]. Either mismatch makes the flag inert, which
    /// is worth saying out loud: a caller who marked a child significant is asserting that its termination matters,
    /// and silently ignoring that is how a supervisor ends up outliving something it can't work without.
    ///
    /// Called as children are started rather than as they are registered, because the policy half of the question
    /// isn't answerable any earlier: [`with_auto_shutdown`][Self::with_auto_shutdown] consumes the supervisor while
    /// [`add_worker`][Self::add_worker] borrows it, so a caller is free to add children first and set the policy
    /// afterwards. Checking at registration time would flag that -- entirely correct -- ordering as a mistake.
    ///
    /// Warn-only: the child still starts, since an inert flag is useless rather than unsafe.
    fn warn_if_significance_is_inert(&self, config: &ChildConfig, child_name: &str, auto_shutdown: AutoShutdown) {
        if !config.significant {
            return;
        }

        if config.restart == RestartType::Permanent {
            warn!(
                supervisor_id = %self.supervisor_id,
                child_name,
                "Child is marked significant but is permanent, so it is always restarted and the flag has no effect."
            );
        }

        if auto_shutdown == AutoShutdown::Never {
            warn!(
                supervisor_id = %self.supervisor_id,
                child_name,
                "Child is marked significant but the supervisor's auto-shutdown policy is `Never`, so the flag has \
                 no effect."
            );
        }
    }

    fn push_child(&mut self, entry: ChildEntry) {
        debug!(
            supervisor_id = %self.supervisor_id,
            "Adding new static child process #{}. ({}, {}, {:?})",
            self.child_specs.len(),
            entry.spec.process_type(),
            entry.spec.name(),
            entry.config,
        );

        // The policy half of the inert-significance check has to wait until the supervisor runs (see
        // `warn_if_significance_is_inert`), but this half doesn't depend on anything but the child itself, and here we
        // are still in the caller's frame. `ChildBuilder` makes the combination unreachable from outside the crate, so
        // this guards in-crate construction.
        debug_assert!(
            !(entry.config.significant && entry.config.restart == RestartType::Permanent),
            "child '{}' was marked significant but is permanent, so it is always restarted and its termination can \
             never drive auto-shutdown",
            entry.spec.name()
        );

        self.child_specs.push(entry);
    }

    /// Describes a child for the supervision-tree bookkeeping.
    ///
    /// Assembled here rather than inside the roster because only the supervisor can see a child's specification and
    /// resolved configuration.
    fn child_facts(entry: &ChildEntry, key: ChildKey) -> ChildFacts {
        ChildFacts {
            key,
            name: entry.spec.name().into(),
            node: entry.spec.node(),
            restart: entry.config.restart,
            significant: entry.config.significant,
        }
    }

    fn spawn_static_children(
        &self, roster: &mut Roster<ChildEntry>, worker_state: &mut WorkerState, auto_shutdown: AutoShutdown,
    ) -> Result<(), SupervisorError> {
        debug!(supervisor_id = %self.supervisor_id, "Spawning all static child processes.");
        for (index, entry) in self.child_specs.iter().enumerate() {
            self.warn_if_significance_is_inert(&entry.config, entry.spec.name(), auto_shutdown);

            let id = self.id_counter.fetch_add(1, Ordering::Relaxed);
            let started = worker_state.add_worker(id, &entry.spec, &entry.config)?;
            let facts = Self::child_facts(entry, ChildKey::Static(index));
            roster.insert(id, entry.clone(), facts, started);
        }

        Ok(())
    }

    /// Respawns children after a one-for-all restart, honoring each child's [`RestartType`].
    ///
    /// Every child except [`RestartType::Temporary`] is restarted, matching Erlang/OTP: a group restart restarts all
    /// permanent and transient children -- regardless of how they last exited, including a transient child that had
    /// already exited cleanly -- but never temporary children, which are shut down with the group and not brought back.
    /// A transient child's "restart only on abnormal exit" rule governs its _own_ termination, not a group restart
    /// driven by a sibling. Dynamic children are not restored (they are lost on a supervisor-level restart).
    fn respawn_children_one_for_all(
        &self, roster: &mut Roster<ChildEntry>, worker_state: &mut WorkerState,
    ) -> Result<(), SupervisorError> {
        debug!(supervisor_id = %self.supervisor_id, "Restarting all eligible static child processes.");
        for (index, entry) in self.child_specs.iter().enumerate() {
            // Temporary children are never restarted by a group restart (matching OTP): they are shut down with the
            // group but not brought back.
            if entry.config.restart == RestartType::Temporary {
                continue;
            }
            let id = self.id_counter.fetch_add(1, Ordering::Relaxed);
            let started = worker_state.add_worker(id, &entry.spec, &entry.config)?;
            // Keyed by position in the static child list rather than by roster id: a group restart hands out fresh
            // ids, and the child's creation time and restart count have to survive that.
            let facts = Self::child_facts(entry, ChildKey::Static(index));
            roster.insert(id, entry.clone(), facts, started);
        }

        Ok(())
    }

    /// Registers one dynamic child into the running supervisor's worker set and roster.
    fn spawn_dynamic_child(
        &self, spawn: PendingSpawn, worker_state: &mut WorkerState, roster: &mut Roster<ChildEntry>,
        significant_remaining: &mut usize, auto_shutdown: AutoShutdown,
    ) {
        let PendingSpawn { id, spec, config } = spawn;
        let entry = ChildEntry {
            spec,
            config,
            dynamic: true,
        };
        self.warn_if_significance_is_inert(&entry.config, entry.spec.name(), auto_shutdown);

        match worker_state.add_worker(id, &entry.spec, &entry.config) {
            Ok(started) => {
                if entry.config.significant {
                    *significant_remaining += 1;
                }
                self.active.fetch_add(1, Ordering::Relaxed);
                let facts = Self::child_facts(&entry, ChildKey::Dynamic(id));
                roster.insert(id, entry, facts, started);
            }
            Err(e) => {
                // The only way registration fails now that child names always resolve is a nested supervisor on a
                // dedicated runtime failing to get an OS thread. There's no caller left to report it to -- spawning is
                // infallible -- so the child is dropped and the failure is logged here.
                error!(
                    supervisor_id = %self.supervisor_id,
                    child_name = entry.spec.name(),
                    error = %e,
                    "Failed to start dynamic child."
                );
            }
        }
    }

    async fn run_inner(&self, process: Process, process_shutdown: ShutdownHandle) -> Result<(), SupervisorError> {
        // Publish a fresh spawn queue for this run so handles can spawn dynamic children into it; while it's set,
        // handles observe us as running.
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        *self.current_tx.lock().unwrap() = Some(cmd_tx);

        // Record the process we're actually running under. It has to come from here rather than from whatever our
        // parent created for us, because a supervisor on a dedicated runtime re-roots its process name when it
        // starts, and only the resulting name matches the resource group its allocations land in.
        self.node.begin_run(&process);

        let result = self.supervise(process, process_shutdown, cmd_rx).await;

        // The run is over. Clear the sender so later spawns are dropped rather than queued, and reset the
        // dynamic-children gauge. Dropping the receiver (owned by `supervise`) already discarded anything in flight.
        *self.current_tx.lock().unwrap() = None;
        self.active.store(0, Ordering::Relaxed);

        // Report as stopped rather than presenting the children of a generation that has ended. Every way out of
        // `supervise` -- clean shutdown, a child failing to initialize, the restart limit being exceeded, a
        // significant child exiting -- returns through here, so this covers all of them.
        self.node.end_run();

        result
    }

    async fn supervise(
        &self, process: Process, process_shutdown: ShutdownHandle, mut cmd_rx: mpsc::UnboundedReceiver<PendingSpawn>,
    ) -> Result<(), SupervisorError> {
        // Read once: configuration can't change while a run is in flight, and this keeps the reap and spawn arms
        // below off the node's lock entirely.
        let NodeConfig {
            restart_strategy,
            auto_shutdown,
            shutdown_budget,
            ..
        } = self.node.config();

        let mut restart_state = RestartState::new(restart_strategy);
        let mut worker_state = WorkerState::new(process, self.handle(), shutdown_budget, Arc::clone(&self.node));

        // The live roster of children -- both static (seeded below) and dynamic (added via the handle) -- keyed by a
        // stable id. A restart re-runs a child by id; a child that isn't restarted is removed from the roster.
        //
        // Every change also lands in this supervisor's supervision-tree bookkeeping, which is what makes the tree
        // observable. Going through the roster for all of it is what keeps the two from drifting apart.
        let mut roster = Roster::new(Arc::clone(&self.node));

        // Spawn the static children. Initialization is folded into each worker's task, so this returns immediately --
        // children initialize concurrently in the background.
        self.spawn_static_children(&mut roster, &mut worker_state, auto_shutdown)?;

        // Track how many significant children are still running, for `AutoShutdown` evaluation.
        let mut significant_remaining = roster.values().filter(|entry| entry.config.significant).count();

        // Scratch space reused across every batched drain of the spawn queue.
        let mut spawn_batch = Vec::with_capacity(SPAWN_DRAIN_BATCH);

        // Now we supervise.
        pin!(process_shutdown);

        let outcome = loop {
            select! {
                // Shutdown first, then reaping, then taking on new work -- so neither a flood of spawns nor a stream
                // of exiting children can starve anything ranked above it.
                biased;

                // Shutdown has been triggered; break out of the loop with a clean outcome and tear down below. (We
                // can't touch `cmd_rx` in any arm's handler -- the `recv_many` arm below borrows it for the whole
                // `select!` -- so all teardown happens after the loop.)
                _ = &mut process_shutdown => break Ok(()),

                // Reaping outranks taking on new work: it is the only place children leave the join set, the roster
                // and the `active` gauge, so a steady stream of spawns must not be able to starve it. It parks
                // whenever there are no children, so it can't starve spawning in return.
                (child_id, worker_result) = worker_state.wait_for_next_worker() => {
                    // Pull out what we need from the roster before we mutate it.
                    let (child_name, config, dynamic) = {
                        let entry = roster.get(child_id).expect("completed worker must be present in the roster");
                        (entry.spec.name().to_string(), entry.config.clone(), entry.dynamic)
                    };

                    // Initialization failures are not eligible for restart -- they propagate immediately.
                    if let Err(WorkerError::Initialization { child_name: inner, source }) = worker_result {
                        // If the error came from a nested supervisor, include the original child name to make the error
                        // chain more informative (e.g., "ctrl-pln/privileged-api").
                        let full_name = match inner {
                            Some(inner) => format!("{}/{}", child_name, inner),
                            None => child_name.clone(),
                        };

                        error!(supervisor_id = %self.supervisor_id, worker_name = full_name, "Child process failed to initialize: {}", source);
                        break Err(SupervisorError::FailedToInitialize { child_name: full_name, source });
                    }

                    // A worker exited abnormally if it returned an error, panicked, or was aborted; a clean exit is
                    // `Ok(())`. Together with the worker's restart policy, this determines whether we restart it.
                    let abnormal = worker_result.is_err();
                    let worker_result = worker_result.map_err(|e| match e {
                        WorkerError::Runtime(e) => ProcessError::Terminated { source: e },
                        WorkerError::Initialization { .. } => unreachable!("handled above"),
                        // A nested supervisor only reports `ShutdownTimedOut` while draining, which is driven by its own
                        // `process_shutdown` -- and that fires only when *this* supervisor is itself draining it, i.e.
                        // from `shutdown_workers` below, never from this main-loop arm. Treat it as a runtime
                        // termination defensively rather than asserting unreachable.
                        WorkerError::ShutdownTimedOut { aborted } => ProcessError::Terminated {
                            source: SupervisorError::ShutdownTimedOut { aborted }.into(),
                        },
                    });

                    if !config.restart.should_restart(abnormal) {
                        // Not eligible for restart given how it exited. Drop it from the roster, and free its slot/gauge
                        // if it was dynamic. Crucially, we do NOT consult `evaluate_restart` here: non-restarts must not
                        // consume the restart-intensity budget, otherwise a steady stream of terminating temporary
                        // children would eventually trip the limit and tear the supervisor (and its siblings) down.
                        //
                        // An abnormal exit is reported at `warn` rather than `debug`: a child that isn't restarted --
                        // every dynamically-spawned child, in practice -- has no other path back to its owner, so this
                        // is the only place its failure is surfaced.
                        if abnormal {
                            warn!(supervisor_id = %self.supervisor_id, worker_name = %child_name, restart = ?config.restart, ?worker_result, "Child process exited with an error and is not eligible for restart.");
                        } else {
                            debug!(supervisor_id = %self.supervisor_id, worker_name = %child_name, restart = ?config.restart, "Child process exited and is not eligible for restart.");
                        }
                        roster.remove(child_id);
                        if dynamic {
                            self.active.fetch_sub(1, Ordering::Relaxed);
                        }

                        // A significant child terminating without restart can drive the supervisor to shut down, per its
                        // `AutoShutdown` policy -- cascading an unexpected (or intentional) child exit into the
                        // supervisor stopping and propagating up the tree.
                        if config.significant {
                            significant_remaining = significant_remaining.saturating_sub(1);
                            let auto_shutdown = match auto_shutdown {
                                AutoShutdown::Never => false,
                                AutoShutdown::AnySignificant => true,
                                AutoShutdown::AllSignificant => significant_remaining == 0,
                            };
                            if auto_shutdown {
                                warn!(supervisor_id = %self.supervisor_id, worker_name = %child_name, ?worker_result, "Significant child terminated; shutting down supervisor.");
                                break Err(SupervisorError::SignificantChildExited);
                            }
                        }
                    } else {
                        match restart_state.evaluate_restart() {
                            RestartAction::Restart(mode) => match mode {
                                RestartMode::OneForOne => {
                                    warn!(supervisor_id = %self.supervisor_id, worker_name = %child_name, ?worker_result, "Child process terminated, restarting.");
                                    let spec = roster.get(child_id).expect("present for restart").spec.clone();
                                    match worker_state.add_worker(child_id, &spec, &config) {
                                        Ok(started) => roster.restart_in_place(child_id, started),
                                        Err(e) => break Err(e),
                                    }
                                }
                                RestartMode::OneForAll => {
                                    warn!(supervisor_id = %self.supervisor_id, worker_name = %child_name, ?worker_result, "Child process terminated, restarting all processes.");
                                    // This drain is part of a restart, not a shutdown: any forced aborts here are
                                    // already logged per-worker, and the supervisor keeps running, so the count does
                                    // not feed the unclean-shutdown signal.
                                    let _ = worker_state.shutdown_workers().await;
                                    // A one-for-all restart resets to the static roster; dynamic children are not
                                    // restored (they're lost on a supervisor-level restart, matching Erlang/OTP), and
                                    // temporary children are not restarted.
                                    roster.clear_for_group_restart();
                                    self.active.store(0, Ordering::Relaxed);
                                    let respawn = self.respawn_children_one_for_all(&mut roster, &mut worker_state);
                                    if let Err(e) = respawn {
                                        break Err(e);
                                    }
                                    significant_remaining =
                                        roster.values().filter(|entry| entry.config.significant).count();
                                }
                            },
                            RestartAction::Shutdown => {
                                error!(supervisor_id = %self.supervisor_id, worker_name = %child_name, ?worker_result, "Supervisor shutting down due to restart limits.");
                                break Err(SupervisorError::Shutdown);
                            }
                        }
                    }
                }

                // A handle asked us to spawn one or more dynamic children. The published sender keeps the queue open
                // for the whole run, so this only yields zero once we close it during teardown. Draining in batches
                // keeps a burst of spawns to a single wake-up.
                _ = cmd_rx.recv_many(&mut spawn_batch, SPAWN_DRAIN_BATCH) => {
                    for spawn in spawn_batch.drain(..) {
                        self.spawn_dynamic_child(
                            spawn,
                            &mut worker_state,
                            &mut roster,
                            &mut significant_remaining,
                            auto_shutdown,
                        );
                    }
                }
            }
        };

        // The run is ending -- either cleanly (shutdown was signalled) or with an error (a child failed to initialize
        // or restart, the restart limit was exceeded, or a significant child exited). On every path: stop accepting
        // spawns and discard anything still queued, rather than starting children only to tear them down immediately,
        // and then shut down all children.
        cmd_rx.close();
        let mut discarded = 0;
        while cmd_rx.try_recv().is_ok() {
            discarded += 1;
        }
        if discarded > 0 {
            debug!(
                supervisor_id = %self.supervisor_id,
                discarded,
                "Discarded queued dynamic children during shutdown."
            );
        }
        let aborted = worker_state.shutdown_workers().await;

        // A requested shutdown that nonetheless had to forcefully abort one or more workers (here or anywhere in the
        // subtree below us) is surfaced as an unclean shutdown so it propagates up the tree rather than being reported
        // as success. An outcome that already carries an error (initialization, restart limit, significant child)
        // takes precedence -- that's the root cause -- and the forced aborts are left to the per-worker warnings.
        match outcome {
            Ok(()) if aborted > 0 => {
                warn!(supervisor_id = %self.supervisor_id, aborted, "Shutdown completed uncleanly; workers were forcefully aborted.");
                Err(SupervisorError::ShutdownTimedOut { aborted })
            }
            outcome => outcome,
        }
    }

    fn as_nested_process(&self, process: Process, process_shutdown: ShutdownHandle) -> WorkerFuture {
        // Simple wrapper around `run_inner` to satisfy the return type signature needed when running the supervisor as
        // a nested child process in another supervisor.
        debug!(supervisor_id = %self.supervisor_id, "Nested supervisor starting.");

        // Create a standalone clone of ourselves so we can fulfill the future signature.
        let sup = self.inner_clone();

        Box::pin(async move {
            sup.run_inner(process, process_shutdown)
                .await
                .map_err(WorkerError::from)
        })
    }

    /// Runs the supervisor forever.
    ///
    /// # Errors
    ///
    /// If the supervisor exceeds its restart limits, or fails to initialize a child process, an error is returned.
    pub async fn run(&mut self) -> Result<(), SupervisorError> {
        // Create a no-op `ShutdownHandle` to satisfy the `run_inner` function. This is never used since we want to run
        // forever, but we need to satisfy the signature.
        let process_shutdown = ShutdownHandle::noop();
        let process = Process::supervisor(&self.supervisor_id, None).context(InvalidName {
            name: self.supervisor_id.to_string(),
        })?;

        debug!(supervisor_id = %self.supervisor_id, "Supervisor starting.");
        self.run_inner(process.clone(), process_shutdown)
            .into_process_future(process)
            .await
    }

    /// Runs the supervisor until shutdown is triggered.
    ///
    /// When `shutdown` resolves, the supervisor will shutdown all child processes according to their shutdown strategy,
    /// and then return.
    ///
    /// # Errors
    ///
    /// If the supervisor exceeds its restart limits, or fails to initialize a child process, an error is returned.
    pub async fn run_with_shutdown<F: Future + Send + 'static>(&mut self, shutdown: F) -> Result<(), SupervisorError> {
        // Drive the caller-provided shutdown future into a trigger so the supervisor can begin shutting down its
        // children once `shutdown` resolves. The trigger fires at most once (guarded), and otherwise fires on drop if
        // the supervisor returns on its own first.
        let (shutdown_coordinator, shutdown_handle) = ShutdownHandle::paired();
        let run = self.run_with_shutdown_inner(shutdown_handle, None);
        pin!(run, shutdown);

        let mut shutdown_coordinator = Some(shutdown_coordinator);
        loop {
            select! {
                result = &mut run => return result,
                _ = &mut shutdown, if shutdown_coordinator.is_some() => {
                    shutdown_coordinator.take().expect("coordinator present per select guard").shutdown();
                }
            }
        }
    }

    /// Runs the supervisor until the given `ShutdownHandle` signal is received.
    ///
    /// This is an internal variant of `run_with_shutdown` that takes a `ShutdownHandle` directly, used when spawning
    /// supervisors in dedicated runtimes where the shutdown signal is already wrapped in a `ShutdownHandle`.
    ///
    /// If `dataspace` is provided, the supervisor will use it instead of creating a new one. This is used to propagate
    /// the parent's dataspace across OS thread boundaries for dedicated runtimes.
    ///
    /// # Errors
    ///
    /// If the supervisor exceeds its restart limits, or fails to initialize a child process, an error is returned.
    pub(crate) async fn run_with_shutdown_inner(
        &mut self, process_shutdown: ShutdownHandle, dataspace: Option<DataspaceRegistry>,
    ) -> Result<(), SupervisorError> {
        let process =
            Process::supervisor_with_dataspace(&self.supervisor_id, None, dataspace).context(InvalidName {
                name: self.supervisor_id.to_string(),
            })?;

        debug!(supervisor_id = %self.supervisor_id, "Supervisor starting.");
        self.run_inner(process.clone(), process_shutdown)
            .into_process_future(process)
            .await
    }

    fn inner_clone(&self) -> Self {
        // This is no different than if we just implemented `Clone` directly, but it allows us to avoid exposing a
        // _public_ implementation of `Clone`, which we don't want normal users to be able to do. We only need this
        // internally to support nested supervisors.
        Self {
            supervisor_id: Arc::clone(&self.supervisor_id),
            child_specs: self.child_specs.clone(),
            runtime_mode: self.runtime_mode.clone(),
            current_tx: Arc::clone(&self.current_tx),
            id_counter: Arc::clone(&self.id_counter),
            active: Arc::clone(&self.active),
            node: Arc::clone(&self.node),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        future::pending,
        sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    };

    use async_trait::async_trait;
    use saluki_common::sync::shutdown::ShutdownCoordinator;
    use saluki_metrics::test::TestRecorder;
    use tokio::{
        sync::oneshot,
        task::JoinHandle,
        time::{sleep, timeout},
    };

    use super::*;
    use crate::runtime::{self, FnWorker, NodeKind, NodeSnapshot, NodeState};
    use crate::test_support::wait_until;

    /// Behavior for a mock worker during initialization.
    #[derive(Clone)]
    enum InitBehavior {
        /// Initialization succeeds immediately.
        Instant,

        /// Initialization takes the given duration before succeeding.
        Slow(Duration),

        /// Initialization fails with the given message.
        Fail(&'static str),
    }

    /// Behavior for a mock worker during runtime (after initialization).
    #[derive(Clone)]
    enum RunBehavior {
        /// Runs until shutdown is received.
        UntilShutdown,

        /// Fails with the given error message after the given delay.
        FailAfter(Duration, &'static str),

        /// Completes successfully after the given delay.
        CompleteAfter(Duration),

        /// On shutdown, sleeps for the given duration before exiting (to exercise concurrent draining).
        SlowShutdown(Duration),

        /// Ignores shutdown entirely and runs forever (to exercise abort-at-deadline).
        IgnoreShutdown,

        /// Panics after the given delay, unless shutdown arrives first.
        PanicAfter(Duration),
    }

    /// A configurable mock worker for testing supervisor behavior.
    struct MockWorker {
        name: &'static str,
        init_behavior: InitBehavior,
        run_behavior: RunBehavior,
        start_count: Arc<AtomicUsize>,
        finish_count: Arc<AtomicUsize>,
        brutal_shutdown: bool,
        graceful_timeout: Duration,
    }

    impl MockWorker {
        /// Creates a worker that runs until shutdown.
        fn long_running(name: &'static str) -> Self {
            Self {
                name,
                init_behavior: InitBehavior::Instant,
                run_behavior: RunBehavior::UntilShutdown,
                start_count: Arc::new(AtomicUsize::new(0)),
                finish_count: Arc::new(AtomicUsize::new(0)),
                brutal_shutdown: false,
                graceful_timeout: Duration::from_millis(500),
            }
        }

        /// Creates a worker that fails after the given delay.
        fn failing(name: &'static str, delay: Duration) -> Self {
            Self {
                name,
                init_behavior: InitBehavior::Instant,
                run_behavior: RunBehavior::FailAfter(delay, "worker failed"),
                start_count: Arc::new(AtomicUsize::new(0)),
                finish_count: Arc::new(AtomicUsize::new(0)),
                brutal_shutdown: false,
                graceful_timeout: Duration::from_millis(500),
            }
        }

        /// Creates a worker that completes successfully after the given delay.
        fn completing(name: &'static str, delay: Duration) -> Self {
            Self {
                name,
                init_behavior: InitBehavior::Instant,
                run_behavior: RunBehavior::CompleteAfter(delay),
                start_count: Arc::new(AtomicUsize::new(0)),
                finish_count: Arc::new(AtomicUsize::new(0)),
                brutal_shutdown: false,
                graceful_timeout: Duration::from_millis(500),
            }
        }

        /// Creates a worker that sleeps for `delay` after observing shutdown before exiting.
        fn slow_shutdown(name: &'static str, delay: Duration) -> Self {
            Self {
                name,
                init_behavior: InitBehavior::Instant,
                run_behavior: RunBehavior::SlowShutdown(delay),
                start_count: Arc::new(AtomicUsize::new(0)),
                finish_count: Arc::new(AtomicUsize::new(0)),
                brutal_shutdown: false,
                graceful_timeout: Duration::from_millis(500),
            }
        }

        /// Creates a worker that never reacts to shutdown.
        fn ignore_shutdown(name: &'static str) -> Self {
            Self {
                name,
                init_behavior: InitBehavior::Instant,
                run_behavior: RunBehavior::IgnoreShutdown,
                start_count: Arc::new(AtomicUsize::new(0)),
                finish_count: Arc::new(AtomicUsize::new(0)),
                brutal_shutdown: false,
                graceful_timeout: Duration::from_millis(500),
            }
        }

        /// Creates a worker that panics after the given delay.
        fn panicking(name: &'static str, delay: Duration) -> Self {
            Self {
                name,
                init_behavior: InitBehavior::Instant,
                run_behavior: RunBehavior::PanicAfter(delay),
                start_count: Arc::new(AtomicUsize::new(0)),
                finish_count: Arc::new(AtomicUsize::new(0)),
                brutal_shutdown: false,
                graceful_timeout: Duration::from_millis(500),
            }
        }

        /// Creates a worker that fails during initialization.
        fn init_failure(name: &'static str) -> Self {
            Self {
                name,
                init_behavior: InitBehavior::Fail("init failed"),
                run_behavior: RunBehavior::UntilShutdown,
                start_count: Arc::new(AtomicUsize::new(0)),
                finish_count: Arc::new(AtomicUsize::new(0)),
                brutal_shutdown: false,
                graceful_timeout: Duration::from_millis(500),
            }
        }

        /// Creates a worker with slow initialization.
        fn slow_init(name: &'static str, init_delay: Duration) -> Self {
            Self {
                name,
                init_behavior: InitBehavior::Slow(init_delay),
                run_behavior: RunBehavior::UntilShutdown,
                start_count: Arc::new(AtomicUsize::new(0)),
                finish_count: Arc::new(AtomicUsize::new(0)),
                brutal_shutdown: false,
                graceful_timeout: Duration::from_millis(500),
            }
        }

        /// Returns a shared handle to the start count for this worker.
        ///
        /// The start count ticks up the instant the worker's run future begins executing, which is *before* any
        /// programmed delay elapses. It records that the worker started (or was restarted), not that it ran to any
        /// particular outcome.
        fn start_count(&self) -> Arc<AtomicUsize> {
            Arc::clone(&self.start_count)
        }

        /// Returns a shared handle to the finish count for this worker.
        ///
        /// The finish count ticks up only when the worker runs to its *own* programmed terminal state -- a
        /// [`RunBehavior::FailAfter`] failure, a [`RunBehavior::CompleteAfter`] completion, or a
        /// [`RunBehavior::SlowShutdown`] drain that finished -- and not when it is cut short by an abort. Tests use it
        /// to wait for a worker to actually fail or complete (rather than merely start) before asserting on restart
        /// behavior, so the failure/completion path is genuinely exercised.
        fn finish_count(&self) -> Arc<AtomicUsize> {
            Arc::clone(&self.finish_count)
        }

        /// Configures this worker to use a `Brutal` shutdown strategy (immediate abort, no graceful wait).
        fn with_brutal_shutdown(mut self) -> Self {
            self.brutal_shutdown = true;
            self
        }

        /// Overrides the worker's graceful shutdown timeout (defaults to 500 milliseconds).
        fn with_graceful_timeout(mut self, timeout: Duration) -> Self {
            self.graceful_timeout = timeout;
            self
        }
    }

    #[async_trait]
    impl Supervisable for MockWorker {
        fn name(&self) -> &str {
            self.name
        }

        fn shutdown_strategy(&self) -> ShutdownStrategy {
            if self.brutal_shutdown {
                ShutdownStrategy::Brutal
            } else {
                ShutdownStrategy::Graceful(self.graceful_timeout)
            }
        }

        async fn initialize(&self, process_shutdown: ShutdownHandle) -> Result<SupervisorFuture, InitializationError> {
            match &self.init_behavior {
                InitBehavior::Instant => {}
                InitBehavior::Slow(delay) => {
                    sleep(*delay).await;
                }
                InitBehavior::Fail(msg) => {
                    return Err(InitializationError::Failed {
                        source: GenericError::msg(*msg),
                    });
                }
            }

            let start_count = Arc::clone(&self.start_count);
            let finish_count = Arc::clone(&self.finish_count);
            let run_behavior = self.run_behavior.clone();

            Ok(Box::pin(async move {
                start_count.fetch_add(1, Ordering::SeqCst);

                match run_behavior {
                    RunBehavior::UntilShutdown => {
                        process_shutdown.await;
                        Ok(())
                    }
                    RunBehavior::FailAfter(delay, msg) => {
                        select! {
                            _ = sleep(delay) => {
                                // Ran to our own programmed failure rather than being cut short by shutdown; record
                                // it so tests can wait for the failure to actually happen before asserting.
                                finish_count.fetch_add(1, Ordering::SeqCst);
                                Err(GenericError::msg(msg))
                            }
                            _ = process_shutdown => {
                                Ok(())
                            }
                        }
                    }
                    RunBehavior::CompleteAfter(delay) => {
                        select! {
                            _ = sleep(delay) => {
                                // Ran to our own programmed completion rather than being cut short by shutdown.
                                finish_count.fetch_add(1, Ordering::SeqCst);
                                Ok(())
                            }
                            _ = process_shutdown => Ok(()),
                        }
                    }
                    RunBehavior::SlowShutdown(delay) => {
                        process_shutdown.await;
                        sleep(delay).await;
                        // Finished draining rather than being aborted partway through it.
                        finish_count.fetch_add(1, Ordering::SeqCst);
                        Ok(())
                    }
                    RunBehavior::IgnoreShutdown => {
                        // Hold the handle (so the supervisor counts us as outstanding) but never react to it.
                        let _hold = process_shutdown;
                        pending().await
                    }
                    RunBehavior::PanicAfter(delay) => {
                        select! {
                            _ = sleep(delay) => panic!("worker panicked"),
                            _ = process_shutdown => Ok(()),
                        }
                    }
                }
            }))
        }
    }

    /// Helper: run a supervisor with a oneshot-based shutdown trigger.
    ///
    /// Returns the shutdown sender and a join handle for the run. The supervisor is polled to a running state (its
    /// static children spawned) via readiness polling rather than a blind startup sleep, so callers can rely on it
    /// being live on return.
    async fn run_supervisor_with_trigger(
        supervisor: Supervisor,
    ) -> (oneshot::Sender<()>, JoinHandle<Result<(), SupervisorError>>) {
        // Grab a handle before moving the supervisor into the run task so we can observe when it actually starts.
        let sup_handle = supervisor.handle();
        let mut supervisor = supervisor;

        let (tx, rx) = oneshot::channel();
        let handle = tokio::spawn(async move { supervisor.run_with_shutdown(rx).await });

        wait_until("supervisor is running", || sup_handle.is_running()).await;
        (tx, handle)
    }

    /// Helper: awaits a spawned supervisor run to completion under a bounded timeout, unwrapping the join.
    ///
    /// Collapses the `timeout(..).await.unwrap().unwrap()` suffix repeated across the restart/shutdown tests into one
    /// call with useful panic messages.
    async fn join_supervisor(handle: JoinHandle<Result<(), SupervisorError>>) -> Result<(), SupervisorError> {
        timeout(Duration::from_secs(2), handle)
            .await
            .expect("supervisor should exit promptly")
            .expect("supervisor task should not panic")
    }

    // -- Supervisor run mode tests ---------------------------------------------------------

    #[tokio::test]
    async fn standalone_supervisor_shuts_down_cleanly() {
        let mut sup = Supervisor::new("test-sup").unwrap();
        sup.add_worker(MockWorker::long_running("worker1"));
        sup.add_worker(MockWorker::long_running("worker2"));

        let (tx, handle) = run_supervisor_with_trigger(sup).await;
        tx.send(()).unwrap();

        let result = join_supervisor(handle).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn nested_supervisor_shuts_down_cleanly() {
        let mut child_sup = Supervisor::new("child-sup").unwrap();
        child_sup.add_worker(MockWorker::long_running("inner-worker"));

        let mut parent_sup = Supervisor::new("parent-sup").unwrap();
        parent_sup.add_worker(MockWorker::long_running("outer-worker"));
        parent_sup.add_worker(child_sup);

        let (tx, handle) = run_supervisor_with_trigger(parent_sup).await;
        tx.send(()).unwrap();

        let result = join_supervisor(handle).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn empty_supervisor_idles_until_shutdown() {
        // A supervisor with no static children is valid: it idles, waiting for dynamic children, and shuts down
        // cleanly when signalled. (Before dynamic children were folded in, this returned a `NoChildren` error.)
        let sup = Supervisor::new("empty-sup").unwrap();

        let (tx, handle) = run_supervisor_with_trigger(sup).await;
        assert!(!handle.is_finished(), "an empty supervisor must idle rather than exit");

        tx.send(()).unwrap();
        let result = join_supervisor(handle).await;
        assert!(result.is_ok());
    }

    // -- Child restart behavior tests ------------------------------------------------------

    #[tokio::test]
    async fn one_for_one_restarts_only_failed_child() {
        let failing = MockWorker::failing("failing-worker", Duration::from_millis(50));
        let failing_count = failing.start_count();

        let stable = MockWorker::long_running("stable-worker");
        let stable_count = stable.start_count();

        let mut sup = Supervisor::new("test-sup").unwrap().with_restart_strategy(
            RestartStrategy::one_to_one().with_intensity_and_period(20, Duration::from_secs(10)),
        );
        sup.add_worker(stable);
        sup.add_worker(failing);

        let (tx, handle) = run_supervisor_with_trigger(sup).await;

        // Wait until the failing worker has actually been restarted (its second start), then shut down.
        wait_until("the failing worker has been restarted", || {
            failing_count.load(Ordering::SeqCst) >= 2
        })
        .await;
        let _ = tx.send(());

        let result = join_supervisor(handle).await;
        assert!(result.is_ok());

        // The failing worker should have been started multiple times.
        assert!(
            failing_count.load(Ordering::SeqCst) >= 2,
            "failing worker should have been restarted"
        );
        // The stable worker should only have been started once (never restarted).
        assert_eq!(
            stable_count.load(Ordering::SeqCst),
            1,
            "stable worker should not have been restarted"
        );
    }

    #[tokio::test]
    async fn one_for_all_restarts_all_children() {
        let failing = MockWorker::failing("failing-worker", Duration::from_millis(50));
        let failing_count = failing.start_count();

        let stable = MockWorker::long_running("stable-worker");
        let stable_count = stable.start_count();

        let mut sup = Supervisor::new("test-sup").unwrap().with_restart_strategy(
            RestartStrategy::one_for_all().with_intensity_and_period(20, Duration::from_secs(10)),
        );
        sup.add_worker(stable);
        sup.add_worker(failing);

        let (tx, handle) = run_supervisor_with_trigger(sup).await;

        // Wait until a one-for-all cycle has restarted both workers (each on its second start), then shut down.
        wait_until("both workers have been restarted", || {
            failing_count.load(Ordering::SeqCst) >= 2 && stable_count.load(Ordering::SeqCst) >= 2
        })
        .await;
        let _ = tx.send(());

        let result = join_supervisor(handle).await;
        assert!(result.is_ok());

        // Both workers should have been started multiple times.
        assert!(
            failing_count.load(Ordering::SeqCst) >= 2,
            "failing worker should have been restarted"
        );
        assert!(
            stable_count.load(Ordering::SeqCst) >= 2,
            "stable worker should also have been restarted"
        );
    }

    #[tokio::test]
    async fn one_for_all_does_not_restart_temporary_children() {
        // A permanent worker that fails repeatedly drives one-for-all restarts; a temporary sibling is shut down with
        // the group on each cycle but, per OTP semantics, must never be brought back.
        let failing = MockWorker::failing("failing-worker", Duration::from_millis(50));
        let failing_count = failing.start_count();

        let temp = MockWorker::long_running("temp-worker");
        let temp_count = temp.start_count();

        let mut sup = Supervisor::new("test-sup").unwrap().with_restart_strategy(
            RestartStrategy::one_for_all().with_intensity_and_period(20, Duration::from_secs(10)),
        );
        sup.add_worker(ChildSpecification::worker(temp).with_restart_type(RestartType::Temporary));
        sup.add_worker(failing);

        let (tx, handle) = run_supervisor_with_trigger(sup).await;

        // Wait until the permanent worker has driven at least one one-for-all restart, then shut down.
        wait_until("the permanent worker has been restarted", || {
            failing_count.load(Ordering::SeqCst) >= 2
        })
        .await;
        let _ = tx.send(());

        let result = join_supervisor(handle).await;
        assert!(result.is_ok());
        assert!(
            failing_count.load(Ordering::SeqCst) >= 2,
            "permanent worker should have been restarted by one-for-all"
        );
        assert_eq!(
            temp_count.load(Ordering::SeqCst),
            1,
            "temporary child must not be restarted by a one-for-all group restart"
        );
    }

    #[tokio::test]
    async fn one_for_all_restarts_transient_children() {
        // A transient child that exits cleanly is not restarted on its own, but a one-for-all restart triggered by a
        // sibling restarts it anyway -- matching OTP, where only temporary children are exempt from group restarts.
        let transient = MockWorker::completing("transient-worker", Duration::from_millis(30));
        let transient_count = transient.start_count();

        // Fails after the transient has already exited cleanly, so the group restart is what brings the transient back.
        let failing = MockWorker::failing("failing-worker", Duration::from_millis(80));

        let mut sup = Supervisor::new("test-sup").unwrap().with_restart_strategy(
            RestartStrategy::one_for_all().with_intensity_and_period(20, Duration::from_secs(10)),
        );
        sup.add_worker(ChildSpecification::worker(transient).with_restart_type(RestartType::Transient));
        sup.add_worker(failing);

        let (tx, handle) = run_supervisor_with_trigger(sup).await;

        wait_until("the transient worker has been restarted by the group", || {
            transient_count.load(Ordering::SeqCst) >= 2
        })
        .await;
        let _ = tx.send(());

        let result = join_supervisor(handle).await;
        assert!(result.is_ok());
        assert!(
            transient_count.load(Ordering::SeqCst) >= 2,
            "transient child must be restarted by a one-for-all group restart, even after a clean exit"
        );
    }

    #[tokio::test]
    async fn transient_abnormal_exit_triggers_one_for_all() {
        // A transient child's *own* abnormal exit is restartable, so under one-for-all it triggers a whole-group
        // restart -- the sibling is restarted too, not just the transient.
        let transient = MockWorker::failing("transient-worker", Duration::from_millis(50));
        let transient_count = transient.start_count();

        let stable = MockWorker::long_running("stable-worker");
        let stable_count = stable.start_count();

        let mut sup = Supervisor::new("test-sup").unwrap().with_restart_strategy(
            RestartStrategy::one_for_all().with_intensity_and_period(20, Duration::from_secs(10)),
        );
        sup.add_worker(ChildSpecification::worker(transient).with_restart_type(RestartType::Transient));
        sup.add_worker(stable);

        let (tx, handle) = run_supervisor_with_trigger(sup).await;

        wait_until("the abnormal exit has restarted both workers", || {
            transient_count.load(Ordering::SeqCst) >= 2 && stable_count.load(Ordering::SeqCst) >= 2
        })
        .await;
        let _ = tx.send(());

        let result = join_supervisor(handle).await;
        assert!(result.is_ok());
        assert!(
            transient_count.load(Ordering::SeqCst) >= 2,
            "transient worker must be restarted after its own abnormal exit"
        );
        assert!(
            stable_count.load(Ordering::SeqCst) >= 2,
            "the transient's abnormal exit must trigger a one-for-all that also restarts the sibling"
        );
    }

    #[tokio::test]
    async fn restart_limit_exceeded_shuts_down_supervisor() {
        let mut sup = Supervisor::new("test-sup")
            .unwrap()
            .with_restart_strategy(RestartStrategy::one_to_one().with_intensity_and_period(1, Duration::from_secs(10)));
        // This worker fails immediately, which will exhaust the restart budget quickly.
        sup.add_worker(MockWorker::failing("fast-fail", Duration::ZERO));

        let (tx, rx) = oneshot::channel::<()>();
        let handle = tokio::spawn(async move { sup.run_with_shutdown(rx).await });

        let result = join_supervisor(handle).await;
        drop(tx);

        assert!(matches!(result, Err(SupervisorError::Shutdown)));
    }

    // -- Restart type tests ----------------------------------------------------------------

    #[tokio::test]
    async fn temporary_child_is_not_restarted() {
        // A temporary worker that fails quickly, alongside a long-running worker that keeps the supervisor alive.
        let temp = MockWorker::failing("temp-worker", Duration::from_millis(50));
        let temp_started = temp.start_count();
        let temp_failed = temp.finish_count();

        let stable = MockWorker::long_running("stable-worker");

        let mut sup = Supervisor::new("test-sup").unwrap().with_restart_strategy(
            RestartStrategy::one_to_one().with_intensity_and_period(20, Duration::from_secs(10)),
        );
        sup.add_worker(stable);
        sup.add_worker(ChildSpecification::worker(temp).with_restart_type(RestartType::Temporary));

        let (tx, handle) = run_supervisor_with_trigger(sup).await;

        // Wait for the worker to *actually fail*, not merely start. `start_count` ticks up the instant the worker
        // begins running -- well before its 50ms failure -- so shutting down as soon as it reached 1 would tear the
        // supervisor down before the failure -> no-restart path ever ran, hiding a regression that restarted a
        // temporary child (or charged the failure against restart intensity). `finish_count` ticks only once the
        // worker runs to its own failure, so waiting on it genuinely exercises that path before we shut down.
        wait_until("the temporary worker has failed once", || {
            temp_failed.load(Ordering::SeqCst) == 1
        })
        .await;
        let _ = tx.send(());

        let result = join_supervisor(handle).await;
        assert!(result.is_ok());
        assert_eq!(
            temp_started.load(Ordering::SeqCst),
            1,
            "temporary worker must not be restarted after it fails"
        );
    }

    #[tokio::test]
    async fn transient_child_is_not_restarted_on_clean_exit() {
        let transient = MockWorker::completing("transient-worker", Duration::from_millis(50));
        let transient_started = transient.start_count();
        let transient_finished = transient.finish_count();

        let stable = MockWorker::long_running("stable-worker");

        let mut sup = Supervisor::new("test-sup").unwrap().with_restart_strategy(
            RestartStrategy::one_to_one().with_intensity_and_period(20, Duration::from_secs(10)),
        );
        sup.add_worker(stable);
        sup.add_worker(ChildSpecification::worker(transient).with_restart_type(RestartType::Transient));

        let (tx, handle) = run_supervisor_with_trigger(sup).await;

        // Wait for the worker to *actually complete*, not merely start: `start_count` ticks the instant it begins
        // running, so shutting down as soon as it reached 1 would drive the supervisor's teardown before the clean
        // exit -> no-restart path ran, hiding a regression that restarted a transient child after a clean exit.
        // `finish_count` ticks only once the worker runs to its own completion.
        wait_until("the transient worker has completed once", || {
            transient_finished.load(Ordering::SeqCst) == 1
        })
        .await;
        let _ = tx.send(());

        let result = join_supervisor(handle).await;
        assert!(result.is_ok());
        assert_eq!(
            transient_started.load(Ordering::SeqCst),
            1,
            "transient worker must not be restarted after a clean exit"
        );
    }

    #[tokio::test]
    async fn transient_child_is_restarted_on_failure() {
        let transient = MockWorker::failing("transient-worker", Duration::from_millis(50));
        let transient_count = transient.start_count();

        let mut sup = Supervisor::new("test-sup").unwrap().with_restart_strategy(
            RestartStrategy::one_to_one().with_intensity_and_period(20, Duration::from_secs(10)),
        );
        sup.add_worker(ChildSpecification::worker(transient).with_restart_type(RestartType::Transient));

        let (tx, handle) = run_supervisor_with_trigger(sup).await;

        wait_until("the transient worker has been restarted", || {
            transient_count.load(Ordering::SeqCst) >= 2
        })
        .await;
        let _ = tx.send(());

        let result = join_supervisor(handle).await;
        assert!(result.is_ok());
        assert!(
            transient_count.load(Ordering::SeqCst) >= 2,
            "transient worker must be restarted after an abnormal exit"
        );
    }

    #[tokio::test]
    async fn permanent_child_is_restarted_on_clean_exit() {
        // A permanent worker that completes cleanly must still be restarted -- this is what distinguishes
        // `Permanent` from `Transient`, which is left stopped after a clean exit.
        let permanent = MockWorker::completing("permanent-worker", Duration::from_millis(50));
        let permanent_count = permanent.start_count();

        let mut sup = Supervisor::new("test-sup").unwrap().with_restart_strategy(
            RestartStrategy::one_to_one().with_intensity_and_period(20, Duration::from_secs(10)),
        );
        // Added with the default restart policy, which is `Permanent`.
        sup.add_worker(permanent);

        let (tx, handle) = run_supervisor_with_trigger(sup).await;

        wait_until("the permanent worker has been restarted", || {
            permanent_count.load(Ordering::SeqCst) >= 2
        })
        .await;
        let _ = tx.send(());

        let result = join_supervisor(handle).await;
        assert!(result.is_ok());
        assert!(
            permanent_count.load(Ordering::SeqCst) >= 2,
            "permanent worker must be restarted even after a clean exit"
        );
    }

    #[tokio::test]
    async fn temporary_failures_do_not_consume_restart_intensity() {
        // With intensity=1, two *restartable* failures within the period would shut the supervisor down. Here several
        // temporary workers all fail quickly. Because temporary exits aren't eligible for restart, they must not consume
        // the restart-intensity budget, and the supervisor must stay up.
        let mut sup = Supervisor::new("test-sup")
            .unwrap()
            .with_restart_strategy(RestartStrategy::one_to_one().with_intensity_and_period(1, Duration::from_secs(10)));

        let workers = [
            MockWorker::failing("temp-0", Duration::from_millis(20)),
            MockWorker::failing("temp-1", Duration::from_millis(20)),
            MockWorker::failing("temp-2", Duration::from_millis(20)),
            MockWorker::failing("temp-3", Duration::from_millis(20)),
            MockWorker::failing("temp-4", Duration::from_millis(20)),
        ];
        let started: Vec<_> = workers.iter().map(|w| w.start_count()).collect();
        let failed: Vec<_> = workers.iter().map(|w| w.finish_count()).collect();
        for worker in workers {
            sup.add_worker(ChildSpecification::worker(worker).with_restart_type(RestartType::Temporary));
        }
        // A long-running worker so the supervisor doesn't simply idle once the temporaries are gone.
        sup.add_worker(MockWorker::long_running("stable-worker"));

        let (tx, handle) = run_supervisor_with_trigger(sup).await;
        // Wait for every temporary worker to *actually fail* on its own. Keying off `start_count` would let shutdown
        // cut them short before their failures ran, so the supervisor would never get the chance to (mis)charge those
        // failures against its intensity=1 budget -- hiding the very regression this guards against.
        wait_until("every temporary worker has failed once", || {
            failed.iter().all(|c| c.load(Ordering::SeqCst) == 1)
        })
        .await;
        let _ = tx.send(());

        let result = join_supervisor(handle).await;
        assert!(
            result.is_ok(),
            "supervisor must not trip its restart limit on temporary exits"
        );
        for count in started {
            assert_eq!(
                count.load(Ordering::SeqCst),
                1,
                "each temporary worker runs exactly once"
            );
        }
    }

    #[tokio::test]
    async fn transient_clean_exits_do_not_consume_restart_intensity() {
        // With intensity=1, two *restartable* exits within the period would shut the supervisor down. Here several
        // transient workers all complete cleanly. A transient child's clean exit isn't eligible for restart, so it
        // must not consume the restart-intensity budget, and the supervisor must stay up.
        let mut sup = Supervisor::new("test-sup")
            .unwrap()
            .with_restart_strategy(RestartStrategy::one_to_one().with_intensity_and_period(1, Duration::from_secs(10)));

        let workers = [
            MockWorker::completing("transient-0", Duration::from_millis(20)),
            MockWorker::completing("transient-1", Duration::from_millis(20)),
            MockWorker::completing("transient-2", Duration::from_millis(20)),
            MockWorker::completing("transient-3", Duration::from_millis(20)),
            MockWorker::completing("transient-4", Duration::from_millis(20)),
        ];
        let started: Vec<_> = workers.iter().map(|w| w.start_count()).collect();
        let finished: Vec<_> = workers.iter().map(|w| w.finish_count()).collect();
        for worker in workers {
            sup.add_worker(ChildSpecification::worker(worker).with_restart_type(RestartType::Transient));
        }
        // A long-running worker so the supervisor doesn't simply idle once the transients have completed.
        sup.add_worker(MockWorker::long_running("stable-worker"));

        let (tx, handle) = run_supervisor_with_trigger(sup).await;
        // Wait for every transient to *actually complete* on its own. Keying off `start_count` would let shutdown cut
        // the workers short before their clean exits ran, so the supervisor would never get the chance to (mis)charge
        // those exits against its intensity=1 budget -- hiding the very regression this guards against.
        wait_until("every transient worker has completed once", || {
            finished.iter().all(|c| c.load(Ordering::SeqCst) == 1)
        })
        .await;
        let _ = tx.send(());

        let result = join_supervisor(handle).await;
        assert!(
            result.is_ok(),
            "supervisor must not trip its restart limit on clean transient exits"
        );
        for count in started {
            assert_eq!(
                count.load(Ordering::SeqCst),
                1,
                "each transient worker runs exactly once"
            );
        }
    }

    #[tokio::test]
    async fn supervisor_idles_when_all_temporary_children_exit() {
        // When every static child is temporary and they all exit, the worker set drains. The supervisor must not panic
        // or exit on its own; it must keep running and remain able to accept new (dynamic) work until shutdown is
        // triggered.
        let temp_a = MockWorker::completing("temp-a", Duration::from_millis(10));
        let a_finished = temp_a.finish_count();
        let temp_b = MockWorker::completing("temp-b", Duration::from_millis(10));
        let b_finished = temp_b.finish_count();

        let mut sup = Supervisor::new("test-sup").unwrap();
        let handle = sup.handle();
        sup.add_worker(ChildSpecification::worker(temp_a).with_restart_type(RestartType::Temporary));
        sup.add_worker(ChildSpecification::worker(temp_b).with_restart_type(RestartType::Temporary));

        let (tx, run) = run_supervisor_with_trigger(sup).await;

        // Wait for both temporary children to actually complete -- draining the worker set to empty -- before probing.
        // Keying off `start_count` could spawn the probe child before the set ever emptied, letting a supervisor that
        // (wrongly) exited once its last child left slip through.
        wait_until("both temporary children have completed", || {
            a_finished.load(Ordering::SeqCst) == 1 && b_finished.load(Ordering::SeqCst) == 1
        })
        .await;

        // The supervisor must still be alive after its worker set empties: spawning a new dynamic child succeeds and
        // runs, which is only possible if the supervise loop kept running rather than exiting when the last child left.
        let dynamic = MockWorker::long_running("late-comer");
        let dynamic_count = dynamic.start_count();
        handle.spawn(dynamic);
        wait_until("the late dynamic child has started", || {
            dynamic_count.load(Ordering::SeqCst) == 1
        })
        .await;
        assert!(
            handle.is_running(),
            "supervisor must keep running after all temporary children exit"
        );

        tx.send(()).unwrap();
        let result = join_supervisor(run).await;
        assert!(result.is_ok());
    }

    // -- Significant child / auto-shutdown tests -------------------------------------------

    #[tokio::test]
    async fn significant_child_drives_auto_shutdown() {
        // With `AnySignificant`, a significant child terminating (even cleanly, and without being restarted) must
        // shut the supervisor down, surfacing the significant-exit error.
        let mut sup = Supervisor::new("test-sup")
            .unwrap()
            .with_auto_shutdown(AutoShutdown::AnySignificant);
        sup.add_worker(MockWorker::long_running("stable"));
        sup.add_worker(
            ChildSpecification::worker(MockWorker::completing("significant", Duration::from_millis(50)))
                .with_restart_type(RestartType::Temporary)
                .with_significant(true),
        );

        // Hold the shutdown sender so the only thing that can stop the supervisor is the significant child.
        let (_tx, rx) = oneshot::channel::<()>();
        let result = timeout(Duration::from_secs(2), sup.run_with_shutdown(rx))
            .await
            .unwrap();
        assert!(matches!(result, Err(SupervisorError::SignificantChildExited)));
    }

    #[tokio::test]
    async fn significant_child_added_before_the_auto_shutdown_policy_still_drives_it() {
        // `with_auto_shutdown` consumes the supervisor while `add_worker` borrows it, so adding children first and
        // setting the policy afterwards is a perfectly good way to build one up. Nothing about registration may
        // assume the policy is already final -- an earlier version of the inert-significance check read it at
        // registration time and flagged this ordering as a mistake.
        let mut sup = Supervisor::new("test-sup").unwrap();
        sup.add_worker(MockWorker::long_running("stable"));
        sup.add_worker(
            runtime::supervisable(MockWorker::completing("significant", Duration::from_millis(50)))
                .temporary()
                .with_significant(true)
                .build(),
        );
        let mut sup = sup.with_auto_shutdown(AutoShutdown::AnySignificant);

        let (_tx, rx) = oneshot::channel::<()>();
        let result = timeout(Duration::from_secs(2), sup.run_with_shutdown(rx))
            .await
            .unwrap();
        assert!(
            matches!(result, Err(SupervisorError::SignificantChildExited)),
            "the policy set after registration should still have applied, got {result:?}"
        );
    }

    #[tokio::test]
    async fn non_significant_exit_does_not_auto_shutdown() {
        // Even with `AnySignificant` set, a non-significant child exiting must not shut the supervisor down.
        let plain = MockWorker::completing("plain", Duration::from_millis(10));
        let plain_finished = plain.finish_count();

        let mut sup = Supervisor::new("test-sup")
            .unwrap()
            .with_auto_shutdown(AutoShutdown::AnySignificant);
        let handle = sup.handle();
        sup.add_worker(MockWorker::long_running("stable"));
        sup.add_worker(ChildSpecification::worker(plain).with_restart_type(RestartType::Temporary));

        let (tx, run) = run_supervisor_with_trigger(sup).await;

        // Let the non-significant child actually run to completion -- not merely start. Its completion is what could
        // (wrongly) trip `AnySignificant`, so we must observe the real exit before probing liveness; keying off
        // `start_count` could assert before the completion was ever processed.
        wait_until("the non-significant child has completed", || {
            plain_finished.load(Ordering::SeqCst) == 1
        })
        .await;

        // The supervisor must still be alive after the non-significant child exits (had it been treated as
        // significant, `AnySignificant` would have torn the supervisor down). Spawning a dynamic child and observing
        // it start proves the supervise loop is still running.
        let dynamic = MockWorker::long_running("late-comer");
        let dynamic_count = dynamic.start_count();
        handle.spawn(dynamic);
        wait_until("the late dynamic child has started", || {
            dynamic_count.load(Ordering::SeqCst) == 1
        })
        .await;
        assert!(
            handle.is_running(),
            "a non-significant child exiting must not trigger auto-shutdown"
        );

        tx.send(()).unwrap();
        let result = join_supervisor(run).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn all_significant_waits_for_last() {
        // With `AllSignificant`, the supervisor shuts down only once *all* significant children have terminated.
        let mut sup = Supervisor::new("test-sup")
            .unwrap()
            .with_auto_shutdown(AutoShutdown::AllSignificant);
        sup.add_worker(
            ChildSpecification::worker(MockWorker::completing("sig-a", Duration::from_millis(50)))
                .with_restart_type(RestartType::Temporary)
                .with_significant(true),
        );
        sup.add_worker(
            ChildSpecification::worker(MockWorker::completing("sig-b", Duration::from_millis(250)))
                .with_restart_type(RestartType::Temporary)
                .with_significant(true),
        );

        let (_tx, rx) = oneshot::channel::<()>();
        let start = std::time::Instant::now();
        let result = timeout(Duration::from_secs(2), sup.run_with_shutdown(rx))
            .await
            .unwrap();
        let elapsed = start.elapsed();

        assert!(matches!(result, Err(SupervisorError::SignificantChildExited)));
        // The first significant child exits at ~50ms but must NOT trigger shutdown; only the second (~250ms) does.
        assert!(
            elapsed >= Duration::from_millis(200),
            "auto-shutdown must wait for all significant children (took {elapsed:?})"
        );
    }

    // -- Initialization failure tests ------------------------------------------------------

    #[tokio::test]
    async fn init_failure_propagates_with_child_name() {
        let mut sup = Supervisor::new("test-sup").unwrap();
        sup.add_worker(MockWorker::long_running("good-worker"));
        sup.add_worker(MockWorker::init_failure("bad-worker"));

        let (_tx, rx) = oneshot::channel::<()>();
        let result = timeout(Duration::from_secs(2), sup.run_with_shutdown(rx))
            .await
            .unwrap();

        match result {
            Err(SupervisorError::FailedToInitialize { child_name, .. }) => {
                assert_eq!(child_name, "bad-worker");
            }
            other => panic!("expected FailedToInitialize, got: {:?}", other),
        }
    }

    #[tokio::test]
    async fn init_failure_does_not_trigger_restart() {
        let init_fail = MockWorker::init_failure("bad-worker");
        let start_count = init_fail.start_count();

        let mut sup = Supervisor::new("test-sup").unwrap().with_restart_strategy(
            RestartStrategy::one_to_one().with_intensity_and_period(10, Duration::from_secs(10)),
        );
        sup.add_worker(init_fail);

        let (_tx, rx) = oneshot::channel::<()>();
        let result = timeout(Duration::from_secs(2), sup.run_with_shutdown(rx))
            .await
            .unwrap();

        assert!(matches!(result, Err(SupervisorError::FailedToInitialize { .. })));
        // The worker never got past init, so start_count should be 0.
        assert_eq!(start_count.load(Ordering::SeqCst), 0);
    }

    // -- Shutdown responsiveness tests -----------------------------------------------------

    #[tokio::test]
    async fn shutdown_completes_promptly_in_steady_state() {
        let mut sup = Supervisor::new("test-sup").unwrap();
        sup.add_worker(MockWorker::long_running("worker1"));
        sup.add_worker(MockWorker::long_running("worker2"));

        let (tx, handle) = run_supervisor_with_trigger(sup).await;
        tx.send(()).unwrap();

        // Shutdown should complete well within 1 second (workers respond to shutdown signal immediately).
        let result = timeout(Duration::from_secs(1), handle).await;
        assert!(result.is_ok(), "shutdown should complete promptly");
    }

    #[tokio::test]
    async fn shutdown_during_slow_init_completes_promptly() {
        let mut sup = Supervisor::new("test-sup").unwrap();
        // This worker takes 30 seconds to initialize — but we'll trigger shutdown immediately.
        sup.add_worker(MockWorker::slow_init("slow-worker", Duration::from_secs(30)));

        let (tx, rx) = oneshot::channel();
        let handle = tokio::spawn(async move { sup.run_with_shutdown(rx).await });

        // Give the supervisor just enough time to spawn the task, then trigger shutdown.
        sleep(Duration::from_millis(20)).await;
        tx.send(()).unwrap();

        // Shutdown should complete quickly even though the worker hasn't finished initializing.
        // The supervisor loop sees the shutdown signal and aborts the still-initializing task.
        let result = timeout(Duration::from_secs(2), handle).await;
        assert!(result.is_ok(), "shutdown during slow init should complete promptly");
    }

    // -- Dynamic children tests ------------------------------------------------------------

    #[tokio::test]
    async fn dynamic_children_spawn_after_start() {
        let sup = Supervisor::new("dyn-sup").unwrap();
        let handle = sup.handle();
        let (tx, run) = run_supervisor_with_trigger(sup).await;
        wait_until("supervisor is running", || handle.is_running()).await;

        let c1 = MockWorker::long_running("c1");
        let c2 = MockWorker::long_running("c2");
        let c1_count = c1.start_count();
        let c2_count = c2.start_count();
        handle.spawn(c1);
        handle.spawn(c2);

        wait_until("both dynamic children have started", || {
            c1_count.load(Ordering::SeqCst) == 1 && c2_count.load(Ordering::SeqCst) == 1
        })
        .await;
        assert_eq!(handle.active_children(), 2);

        tx.send(()).unwrap();
        let result = join_supervisor(run).await;
        assert!(result.is_ok());
        assert_eq!(
            handle.active_children(),
            0,
            "all dynamic children must be drained on shutdown"
        );
    }

    #[tokio::test]
    async fn temporary_dynamic_child_failure_is_isolated() {
        // A dynamic child added with the default config (temporary, not significant) is fault-isolated: its failure is
        // reaped and removed without restarting it or disturbing the supervisor or its siblings.
        let sup = Supervisor::new("dyn-sup").unwrap();
        let handle = sup.handle();
        let (tx, run) = run_supervisor_with_trigger(sup).await;
        wait_until("supervisor is running", || handle.is_running()).await;

        let failing = MockWorker::failing("boom", Duration::from_millis(20));
        let failing_count = failing.start_count();
        handle.spawn(failing);
        wait_until("the failing dynamic child has run once", || {
            failing_count.load(Ordering::SeqCst) == 1
        })
        .await;
        wait_until("all dynamic children have drained", || handle.active_children() == 0).await;

        sleep(Duration::from_millis(50)).await;
        assert!(
            handle.is_running(),
            "supervisor stays up after an isolated child failure"
        );
        assert_eq!(
            failing_count.load(Ordering::SeqCst),
            1,
            "a temporary child is never restarted"
        );

        // It still accepts new children.
        handle.spawn(MockWorker::long_running("c2"));
        wait_until("one dynamic child is running", || handle.active_children() == 1).await;

        tx.send(()).unwrap();
        let result = join_supervisor(run).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn temporary_dynamic_child_panic_is_isolated() {
        // A panicking temporary, non-significant child is isolated exactly like an error exit.
        let sup = Supervisor::new("dyn-sup").unwrap();
        let handle = sup.handle();
        let (tx, run) = run_supervisor_with_trigger(sup).await;
        wait_until("supervisor is running", || handle.is_running()).await;

        handle.spawn(MockWorker::panicking("boom", Duration::from_millis(20)));
        wait_until("all dynamic children have drained", || handle.active_children() == 0).await;

        sleep(Duration::from_millis(50)).await;
        assert!(handle.is_running(), "supervisor stays up after an isolated child panic");

        tx.send(()).unwrap();
        let result = join_supervisor(run).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn significant_dynamic_child_failure_shuts_down_supervisor() {
        // A dynamic child added as significant, under `AutoShutdown::AnySignificant`, drives the supervisor to shut
        // down when it terminates -- the opt-in mechanism that replaces the old escalate-on-error behavior.
        let sup = Supervisor::new("dyn-sup")
            .unwrap()
            .with_auto_shutdown(AutoShutdown::AnySignificant);
        let handle = sup.handle();
        let (_tx, run) = run_supervisor_with_trigger(sup).await;
        wait_until("supervisor is running", || handle.is_running()).await;

        handle.spawn(
            ChildSpecification::worker(MockWorker::failing("boom", Duration::from_millis(20))).with_significant(true),
        );

        let result = join_supervisor(run).await;
        assert!(matches!(result, Err(SupervisorError::SignificantChildExited)));
    }

    #[tokio::test]
    async fn dynamic_spawn_outside_a_run_is_accepted_and_dropped() {
        // Spawning is infallible in the same sense `tokio::spawn` is: the child is always accepted, but a child handed
        // to a supervisor that isn't running is never started. That holds both before a run and after one, and a child
        // spawned before the run must not be held over and started by it -- children belong to a run, not to the
        // supervisor across runs.
        let sup = Supervisor::new("dyn-sup").unwrap();
        let handle = sup.handle();

        assert!(!handle.is_running());
        let before = MockWorker::long_running("before-start");
        let before_count = before.start_count();
        handle.spawn(before);

        // Once it's running, spawns do start children.
        let (tx, run) = run_supervisor_with_trigger(sup).await;
        wait_until("supervisor is running", || handle.is_running()).await;
        let worker = MockWorker::long_running("after-start");
        let started = worker.start_count();
        handle.spawn(worker);
        wait_until("the dynamic child has started", || started.load(Ordering::SeqCst) == 1).await;
        assert_eq!(
            before_count.load(Ordering::SeqCst),
            0,
            "a child spawned before the run must not be started by it"
        );

        tx.send(()).unwrap();
        let result = join_supervisor(run).await;
        assert!(result.is_ok());

        // And once it has shut down there is nothing left to start children either.
        wait_until("the supervisor has stopped", || !handle.is_running()).await;
        let after = MockWorker::long_running("after-shutdown");
        let after_count = after.start_count();
        handle.spawn(after);

        sleep(Duration::from_millis(50)).await;
        assert_eq!(
            after_count.load(Ordering::SeqCst),
            0,
            "a child spawned after shutdown must never start"
        );
    }

    #[tokio::test]
    async fn dynamic_spawn_allocates_an_id_eagerly() {
        // The id comes back synchronously, before the supervisor has picked the child up, so it can't depend on
        // registration having happened. Ids come from the supervisor's shared counter, so with no static children the
        // first dynamic child takes id 0.
        let sup = Supervisor::new("dyn-sup").unwrap();
        let handle = sup.handle();
        let (tx, run) = run_supervisor_with_trigger(sup).await;
        wait_until("supervisor is running", || handle.is_running()).await;

        let worker = MockWorker::long_running("c");
        let started = worker.start_count();
        let id = handle.spawn(worker);
        assert_eq!(id.as_u64(), 0);
        wait_until("the dynamic child has started", || started.load(Ordering::SeqCst) == 1).await;

        tx.send(()).unwrap();
        let result = join_supervisor(run).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn dynamic_child_with_an_unusable_name_runs_under_a_placeholder() {
        // A name that sanitizes to nothing can't be used as a process name. Spawning is infallible, so rather than
        // quietly discarding work the caller was told had been accepted, the child runs under a placeholder segment.
        // The poll metric is what proves the substituted name is what the child actually ran as, rather than the child
        // merely having started somehow.
        let recorder = TestRecorder::default();
        let _guard = metrics::set_default_local_recorder(&recorder);

        let sup = Supervisor::new("dyn-sup").unwrap();
        let handle = sup.handle();
        let (tx, run) = run_supervisor_with_trigger(sup).await;
        wait_until("supervisor is running", || handle.is_running()).await;

        let worker = MockWorker::long_running("");
        let started = worker.start_count();
        handle.spawn(worker);
        wait_until("the unnamed dynamic child has started", || {
            started.load(Ordering::SeqCst) == 1
        })
        .await;

        // The supervisor stays up and still accepts normally-named children.
        assert!(handle.is_running());
        handle.spawn(MockWorker::long_running("ok"));
        wait_until("both dynamic children are running", || handle.active_children() == 2).await;

        tx.send(()).unwrap();
        let result = join_supervisor(run).await;
        assert!(result.is_ok());

        let polls = recorder.counter(("runtime_task_poll_count", &[("task_name", "dyn_sup.unnamed")]));
        assert!(
            polls.is_some_and(|polls| polls > 0),
            "the child should have run under the placeholder name, got {polls:?}"
        );
    }

    #[tokio::test]
    async fn dynamic_spawns_are_not_capped() {
        // Spawn requests are queued without a bound, so a burst well past what any fixed-capacity channel would hold
        // still starts every child. Being accepted means being started -- a depth limit could only deliver that by
        // discarding work the caller was already told had been taken.
        const CHILDREN: usize = 2048;

        let sup = Supervisor::new("dyn-sup").unwrap();
        let handle = sup.handle();
        let (tx, run) = run_supervisor_with_trigger(sup).await;
        wait_until("supervisor is running", || handle.is_running()).await;

        for _ in 0..CHILDREN {
            // A generous deadline: this test is about every child starting, not about how fast a couple of thousand
            // of them can be reaped, which is slow enough in a debug build to trip a short per-child timeout.
            handle.spawn(MockWorker::long_running("burst").with_graceful_timeout(Duration::from_secs(30)));
        }

        wait_until("every child in the burst has started", || {
            handle.active_children() == CHILDREN
        })
        .await;

        tx.send(()).unwrap();
        let result = timeout(Duration::from_secs(30), run)
            .await
            .expect("supervisor should stop")
            .expect("supervisor task should not panic");
        assert!(result.is_ok(), "the burst should have drained cleanly: {result:?}");
    }

    #[tokio::test]
    async fn dynamically_spawned_supervisor_runs_and_drains() {
        // A dynamic child can be a whole supervision subtree, not just a worker: spawning a `Supervisor` runs it
        // nested, and shutting the parent down drains it along with everything under it.
        let child_worker = MockWorker::long_running("nested-child");
        let child_started = child_worker.start_count();
        let mut nested = Supervisor::new("nested-sup").unwrap();
        nested.add_worker(child_worker);

        let sup = Supervisor::new("dyn-sup").unwrap();
        let handle = sup.handle();
        let (tx, run) = run_supervisor_with_trigger(sup).await;
        wait_until("supervisor is running", || handle.is_running()).await;

        handle.spawn(nested);
        wait_until("the nested supervisor's own child has started", || {
            child_started.load(Ordering::SeqCst) == 1
        })
        .await;

        tx.send(()).unwrap();
        let result = join_supervisor(run).await;
        assert!(
            result.is_ok(),
            "the nested subtree should have drained cleanly: {result:?}"
        );
    }

    /// Counts how many times a nested subtree is started, by counting starts of its sole child.
    ///
    /// The child fails immediately and the subtree has a restart intensity of zero, so the subtree gives up the first
    /// time it fails. That makes the child's start count equal to the number of times the *subtree* ran, which is what
    /// these tests are actually asserting on -- without the zero intensity the subtree's own one-for-one restart would
    /// be indistinguishable from the parent restarting the subtree.
    fn failing_subtree(name: &'static str) -> (Supervisor, Arc<AtomicUsize>) {
        let worker = MockWorker::failing("nested-child", Duration::from_millis(5));
        let started = worker.start_count();
        let mut nested = Supervisor::new(name)
            .unwrap()
            .with_restart_strategy(RestartStrategy::new(RestartMode::OneForOne, 0, Duration::from_secs(30)));
        nested.add_worker(worker);

        (nested, started)
    }

    #[tokio::test]
    async fn dynamic_nested_supervisor_defaults_to_temporary() {
        // Spawning a bare `Supervisor` takes the dynamic default, so a subtree that gives up stays gone. For a
        // listener that means it silently disappears while whatever owns it keeps reporting healthy -- which is the
        // reason `nested_supervisor` exists.
        let (nested, started) = failing_subtree("nested-temp");

        let sup = Supervisor::new("dyn-temp-sup").unwrap();
        let handle = sup.handle();
        let (tx, run) = run_supervisor_with_trigger(sup).await;

        handle.spawn(nested);
        wait_until("the subtree has started once", || started.load(Ordering::SeqCst) == 1).await;

        // Give the subtree time to fail and be reaped. Nothing brings it back.
        sleep(Duration::from_millis(200)).await;
        assert_eq!(
            started.load(Ordering::SeqCst),
            1,
            "a temporary subtree must not be restarted"
        );

        tx.send(()).unwrap();
        assert!(join_supervisor(run).await.is_ok());
    }

    #[tokio::test]
    async fn dynamic_nested_supervisor_can_be_made_permanent() {
        // What `nested_supervisor` buys: the same subtree, spawned permanent, is brought back when it terminates.
        let (nested, started) = failing_subtree("nested-perm");

        let sup = Supervisor::new("dyn-perm-sup")
            .unwrap()
            .with_restart_strategy(RestartStrategy::new(
                RestartMode::OneForOne,
                100,
                Duration::from_secs(30),
            ));
        let handle = sup.handle();
        let (tx, run) = run_supervisor_with_trigger(sup).await;

        handle.nested_supervisor(nested).spawn();
        wait_until("the subtree has been restarted", || started.load(Ordering::SeqCst) >= 2).await;

        tx.send(()).unwrap();
        assert!(join_supervisor(run).await.is_ok());
    }

    #[tokio::test]
    async fn dynamic_nested_supervisor_can_be_significant() {
        // The other half: a subtree its parent can't function without takes the parent with it when it terminates,
        // rather than leaving it running with nothing behind it.
        let (nested, _started) = failing_subtree("nested-sig");

        let sup = Supervisor::new("dyn-sig-sup")
            .unwrap()
            .with_auto_shutdown(AutoShutdown::AnySignificant);
        let handle = sup.handle();
        let (tx, run) = run_supervisor_with_trigger(sup).await;

        handle
            .nested_supervisor(nested)
            .temporary()
            .with_significant(true)
            .spawn();

        let result = timeout(Duration::from_secs(5), run)
            .await
            .expect("supervisor should stop once the significant subtree terminates")
            .expect("supervisor task should not panic");
        assert!(
            matches!(result, Err(SupervisorError::SignificantChildExited)),
            "the significant subtree's termination should have stopped the parent, got {result:?}"
        );

        // The run already ended; the trigger is redundant but keeps the sender alive to the end of the test.
        let _ = tx.send(());
    }

    #[tokio::test]
    async fn budget_of_duration_max_does_not_leave_a_budget_bounded_child_unbounded() {
        // `Duration::MAX` is the natural spelling of "no ceiling", and a budget too large to become a deadline bounds
        // nothing at all. A budget-bounded child under one must therefore fall back to its own deadline: without that,
        // setting `MAX` would be strictly *worse* than setting no budget, since the child would never be abandoned and
        // the drain would hang.
        let mut sup = Supervisor::new("test-sup").unwrap().with_shutdown_budget(Duration::MAX);
        sup.add_worker(
            ChildSpecification::one_shot_worker(
                MockWorker::ignore_shutdown("stuck").with_graceful_timeout(Duration::from_millis(100)),
            )
            .with_budget_bounded_shutdown(),
        );

        let (tx, run) = run_supervisor_with_trigger(sup).await;
        tx.send(()).unwrap();

        let started = tokio::time::Instant::now();
        let result = join_supervisor(run).await;
        let elapsed = started.elapsed();

        assert!(
            matches!(result, Err(SupervisorError::ShutdownTimedOut { aborted: 1 })),
            "the worker's own deadline should have aborted it, got {result:?}"
        );
        assert!(
            elapsed < Duration::from_secs(1),
            "the child should have been bounded by its own 100ms deadline; took {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn budget_bounded_child_falls_back_to_its_own_deadline_without_a_budget() {
        // A one-shot child asks to be bounded by its supervisor's budget rather than carrying a deadline of its own.
        // On a supervisor with no budget there'd be nothing bounding it at all, so it falls back to the strategy the
        // worker reports -- here a short one, which is what lets this test finish rather than hang.
        let mut sup = Supervisor::new("test-sup").unwrap();
        sup.add_worker(
            ChildSpecification::one_shot_worker(
                MockWorker::ignore_shutdown("stuck").with_graceful_timeout(Duration::from_millis(100)),
            )
            .with_budget_bounded_shutdown(),
        );

        let (tx, run) = run_supervisor_with_trigger(sup).await;
        tx.send(()).unwrap();

        let started = tokio::time::Instant::now();
        let result = join_supervisor(run).await;
        let elapsed = started.elapsed();

        assert!(
            matches!(result, Err(SupervisorError::ShutdownTimedOut { aborted: 1 })),
            "the worker's own deadline should have aborted it, got {result:?}"
        );
        assert!(
            elapsed < Duration::from_secs(1),
            "the child should have been bounded by its own 100ms deadline; took {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn concurrent_shutdown_drains_many_children_quickly() {
        const CHILDREN: usize = 500;
        const SHUTDOWN_DELAY: Duration = Duration::from_millis(50);

        let sup = Supervisor::new("dyn-sup").unwrap();
        let handle = sup.handle();
        let (tx, run) = run_supervisor_with_trigger(sup).await;
        wait_until("supervisor is running", || handle.is_running()).await;

        for _ in 0..CHILDREN {
            handle.spawn(MockWorker::slow_shutdown("conn", SHUTDOWN_DELAY));
        }
        wait_until("all dynamic children are running", || {
            handle.active_children() == CHILDREN
        })
        .await;

        // Each child sleeps after observing shutdown. Concurrent shutdown drains them all in roughly one delay; an
        // ordered shutdown would take CHILDREN * delay (25s here). Assert it finishes well under that.
        let start = std::time::Instant::now();
        tx.send(()).unwrap();
        let result = timeout(Duration::from_secs(5), run).await.unwrap().unwrap();
        let elapsed = start.elapsed();

        assert!(result.is_ok());
        assert_eq!(handle.active_children(), 0, "active count must return to zero");
        assert!(
            elapsed < Duration::from_secs(2),
            "shutdown must be concurrent (took {elapsed:?})"
        );
    }

    #[tokio::test]
    async fn concurrent_shutdown_aborts_unresponsive_children() {
        let sup = Supervisor::new("dyn-sup").unwrap();
        let handle = sup.handle();
        let (tx, run) = run_supervisor_with_trigger(sup).await;
        wait_until("supervisor is running", || handle.is_running()).await;

        handle.spawn(MockWorker::ignore_shutdown("stuck"));
        wait_until("one dynamic child is running", || handle.active_children() == 1).await;

        // The child never reacts to shutdown, so it must be aborted once its graceful deadline (500ms) elapses rather
        // than hanging the supervisor.
        let start = std::time::Instant::now();
        tx.send(()).unwrap();
        let result = join_supervisor(run).await;
        let elapsed = start.elapsed();

        // Forcefully aborting an unresponsive child is surfaced as an unclean shutdown rather than reported as success.
        assert!(
            matches!(result, Err(SupervisorError::ShutdownTimedOut { aborted: 1 })),
            "aborting a stuck child must surface as an unclean shutdown, got {result:?}"
        );
        assert_eq!(handle.active_children(), 0);
        assert!(
            elapsed < Duration::from_secs(1),
            "stuck child must be aborted at the deadline (took {elapsed:?})"
        );
    }

    #[tokio::test]
    async fn concurrent_shutdown_honors_per_child_deadline() {
        // Each child must be aborted at its OWN graceful deadline, not a single shared one. A responsive child with an
        // effectively-infinite timeout (modeling a nested supervisor, which uses `Graceful(Duration::MAX)`) coexists
        // with an unresponsive child with a short timeout. Under a shared `max` deadline the short-timeout child would
        // never be aborted (the shared deadline would be `MAX`) and shutdown would hang.
        let sup = Supervisor::new("dyn-sup").unwrap();
        let handle = sup.handle();
        let (tx, run) = run_supervisor_with_trigger(sup).await;
        wait_until("supervisor is running", || handle.is_running()).await;

        // Responds to shutdown promptly, but its deadline is effectively infinite.
        handle.spawn(MockWorker::long_running("responsive").with_graceful_timeout(Duration::MAX));
        // Never responds; must be aborted at its own short deadline.
        handle.spawn(MockWorker::ignore_shutdown("stuck").with_graceful_timeout(Duration::from_millis(200)));
        wait_until("both dynamic children are running", || handle.active_children() == 2).await;

        let start = std::time::Instant::now();
        tx.send(()).unwrap();
        let result = join_supervisor(run).await;
        let elapsed = start.elapsed();

        // Only the stuck child is aborted (the responsive one exits cleanly), so the unclean-shutdown tally is exactly 1.
        assert!(
            matches!(result, Err(SupervisorError::ShutdownTimedOut { aborted: 1 })),
            "aborting the stuck child must surface as an unclean shutdown with a count of 1, got {result:?}"
        );
        assert_eq!(handle.active_children(), 0);
        assert!(
            elapsed < Duration::from_secs(1),
            "stuck child must be aborted at its own deadline despite an infinite-timeout sibling (took {elapsed:?})"
        );
    }

    #[tokio::test]
    async fn unresponsive_child_is_aborted_at_its_deadline() {
        // A child that never reacts to shutdown must be aborted once its graceful deadline (500ms) elapses, rather
        // than hanging the supervisor indefinitely.
        let mut sup = Supervisor::new("test-sup").unwrap();
        sup.add_worker(MockWorker::ignore_shutdown("stuck"));

        let (tx, handle) = run_supervisor_with_trigger(sup).await;

        let start = std::time::Instant::now();
        tx.send(()).unwrap();
        let result = join_supervisor(handle).await;
        let elapsed = start.elapsed();

        assert!(
            matches!(result, Err(SupervisorError::ShutdownTimedOut { aborted: 1 })),
            "aborting a stuck child must surface as an unclean shutdown, got {result:?}"
        );
        assert!(
            elapsed < Duration::from_secs(1),
            "unresponsive child must be aborted at its deadline (took {elapsed:?})"
        );
    }

    #[tokio::test]
    async fn brutal_shutdown_aborts_child_immediately() {
        // A child with a `Brutal` shutdown strategy is aborted at once on shutdown, with no graceful wait -- so even a
        // child that ignores shutdown is torn down promptly rather than after the graceful deadline.
        let mut sup = Supervisor::new("test-sup").unwrap();
        sup.add_worker(MockWorker::ignore_shutdown("brutal-stuck").with_brutal_shutdown());

        let (tx, handle) = run_supervisor_with_trigger(sup).await;

        let start = std::time::Instant::now();
        tx.send(()).unwrap();
        let result = join_supervisor(handle).await;
        let elapsed = start.elapsed();

        // A brutal abort is the configured, expected way to stop this child -- not a graceful-timeout overrun -- so it
        // is NOT counted toward the unclean-shutdown tally, and the shutdown reports success.
        assert!(result.is_ok());
        assert!(
            elapsed < Duration::from_millis(200),
            "brutal-shutdown child must be aborted immediately, not after a graceful wait (took {elapsed:?})"
        );
    }

    #[tokio::test]
    async fn shutdown_timeout_aborts_aggregate_to_root() {
        // Forced aborts must surface as an unclean shutdown and aggregate up the tree: a supervisor adds the workers it
        // aborts directly to the counts reported by any child supervisors that also timed out. Here the parent aborts
        // one direct child and a nested supervisor aborts one of its own, so the root observes a total of 2.
        let mut child_sup = Supervisor::new("child-sup").unwrap();
        child_sup
            .add_worker(MockWorker::ignore_shutdown("child-stuck").with_graceful_timeout(Duration::from_millis(200)));

        let mut parent_sup = Supervisor::new("parent-sup").unwrap();
        parent_sup
            .add_worker(MockWorker::ignore_shutdown("parent-stuck").with_graceful_timeout(Duration::from_millis(200)));
        parent_sup.add_worker(MockWorker::long_running("parent-clean"));
        parent_sup.add_worker(child_sup);

        let (tx, handle) = run_supervisor_with_trigger(parent_sup).await;
        tx.send(()).unwrap();

        let result = join_supervisor(handle).await;
        assert!(
            matches!(result, Err(SupervisorError::ShutdownTimedOut { aborted: 2 })),
            "forced aborts must aggregate across the tree (1 direct + 1 nested), got {result:?}"
        );
    }

    // -- Restart-policy edge cases ---------------------------------------------------------

    #[tokio::test]
    async fn restart_intensity_zero_shuts_down_on_first_failure() {
        // A restart intensity of zero means the supervisor gives up the moment any restartable child fails: it shuts
        // down on the very first failure without ever restarting the worker. (See `RestartState::evaluate_restart`,
        // which short-circuits to `Shutdown` when intensity is zero.)
        let worker = MockWorker::failing("boom", Duration::from_millis(20));
        let start_count = worker.start_count();

        let mut sup = Supervisor::new("test-sup")
            .unwrap()
            .with_restart_strategy(RestartStrategy::new(RestartMode::OneForOne, 0, Duration::from_secs(5)));
        sup.add_worker(worker);

        let (_tx, rx) = oneshot::channel::<()>();
        let result = timeout(Duration::from_secs(2), sup.run_with_shutdown(rx))
            .await
            .unwrap();

        assert!(matches!(result, Err(SupervisorError::Shutdown)));
        assert_eq!(
            start_count.load(Ordering::SeqCst),
            1,
            "with intensity zero the worker must run exactly once and never be restarted"
        );
    }

    #[tokio::test]
    async fn one_for_all_restart_loses_dynamic_children() {
        // Documented one-for-all semantics: a group restart resets to the static roster only -- dynamic children are
        // NOT restored (they're lost on a supervisor-level restart, matching Erlang/OTP). A permanent static worker
        // that keeps failing drives repeated one-for-all restarts; a dynamic child spawned before the first restart
        // must be torn down and never brought back.
        let failing = MockWorker::failing("failing-static", Duration::from_millis(50));
        let failing_count = failing.start_count();

        let sup = Supervisor::new("dyn-sup").unwrap().with_restart_strategy(
            RestartStrategy::one_for_all().with_intensity_and_period(20, Duration::from_secs(10)),
        );
        let handle = sup.handle();
        let mut sup = sup;
        sup.add_worker(failing);

        let (tx, run) = run_supervisor_with_trigger(sup).await;

        // Spawn a long-running dynamic child and wait for it to be running.
        let dynamic = MockWorker::long_running("dynamic");
        let dynamic_count = dynamic.start_count();
        handle.spawn(dynamic);
        wait_until("the dynamic child is running", || handle.active_children() == 1).await;

        // Let the static worker drive at least one one-for-all restart (its second start).
        wait_until("the static worker has been restarted", || {
            failing_count.load(Ordering::SeqCst) >= 2
        })
        .await;

        // The one-for-all restart must have discarded the dynamic child: the active count returns to zero, and the
        // dynamic child ran exactly once (it was never restored).
        wait_until("the dynamic child has been discarded", || handle.active_children() == 0).await;
        assert_eq!(
            dynamic_count.load(Ordering::SeqCst),
            1,
            "a dynamic child must be lost -- not restored -- across a one-for-all restart"
        );

        tx.send(()).unwrap();
        let result = join_supervisor(run).await;
        assert!(result.is_ok());
    }

    // -- Dedicated-runtime tests -----------------------------------------------------------

    #[tokio::test]
    async fn dedicated_single_threaded_runtime_runs_nested_worker_and_shuts_down_cleanly() {
        // A nested supervisor configured with a dedicated single-threaded runtime spawns its own OS thread and Tokio
        // runtime (via `spawn_dedicated_runtime`). Its worker must run there, and a shutdown signalled by the parent
        // must propagate across the thread boundary and tear it down cleanly.
        let worker = MockWorker::long_running("dedicated-worker");
        let worker_count = worker.start_count();

        let mut child_sup = Supervisor::new("child-sup")
            .unwrap()
            .with_dedicated_runtime(RuntimeConfiguration::single_threaded());
        child_sup.add_worker(worker);

        let mut parent_sup = Supervisor::new("parent-sup").unwrap();
        parent_sup.add_worker(child_sup);

        let (tx, handle) = run_supervisor_with_trigger(parent_sup).await;

        // The worker starts on the dedicated runtime's own thread.
        wait_until("the dedicated worker has started", || {
            worker_count.load(Ordering::SeqCst) == 1
        })
        .await;

        tx.send(()).unwrap();
        let result = join_supervisor(handle).await;
        assert!(
            result.is_ok(),
            "dedicated-runtime supervisor should shut down cleanly, got {result:?}"
        );
    }

    #[tokio::test]
    async fn dedicated_multi_threaded_runtime_runs_nested_worker() {
        // The same nested-dedicated flow, but exercising the multi-threaded dedicated runtime builder path.
        let worker = MockWorker::long_running("dedicated-worker");
        let worker_count = worker.start_count();

        let mut child_sup = Supervisor::new("child-sup")
            .unwrap()
            .with_dedicated_runtime(RuntimeConfiguration::multi_threaded(2));
        child_sup.add_worker(worker);

        let mut parent_sup = Supervisor::new("parent-sup").unwrap();
        parent_sup.add_worker(child_sup);

        let (tx, handle) = run_supervisor_with_trigger(parent_sup).await;
        wait_until("the dedicated worker has started", || {
            worker_count.load(Ordering::SeqCst) == 1
        })
        .await;

        tx.send(()).unwrap();
        let result = join_supervisor(handle).await;
        assert!(
            result.is_ok(),
            "multi-threaded dedicated-runtime supervisor should shut down cleanly, got {result:?}"
        );
    }

    #[tokio::test]
    async fn dedicated_runtime_forced_abort_aggregates_to_root() {
        // A worker inside a dedicated-runtime nested supervisor that ignores shutdown must be forcefully aborted at its
        // deadline, and that abort tally must survive the OS-thread boundary (`DedicatedRuntimeHandle` -> `WorkerError`)
        // and be observed by the root supervisor as `ShutdownTimedOut`.
        let stuck = MockWorker::ignore_shutdown("stuck").with_graceful_timeout(Duration::from_millis(200));
        let stuck_count = stuck.start_count();

        let mut child_sup = Supervisor::new("child-sup")
            .unwrap()
            .with_dedicated_runtime(RuntimeConfiguration::single_threaded());
        child_sup.add_worker(stuck);

        let mut parent_sup = Supervisor::new("parent-sup").unwrap();
        parent_sup.add_worker(child_sup);

        let (tx, handle) = run_supervisor_with_trigger(parent_sup).await;

        // Make sure the stuck worker is actually running on the dedicated runtime before signalling shutdown, so the
        // forced-abort path (rather than an early exit) is what we exercise.
        wait_until("the stuck worker has started", || {
            stuck_count.load(Ordering::SeqCst) == 1
        })
        .await;

        tx.send(()).unwrap();
        let result = join_supervisor(handle).await;
        assert!(
            matches!(result, Err(SupervisorError::ShutdownTimedOut { aborted: 1 })),
            "a stuck worker in a dedicated runtime must surface as an unclean shutdown aggregated to the root, got {result:?}"
        );
    }

    // -- Per-child override tests ----------------------------------------------------------

    #[tokio::test]
    async fn child_with_runtime_override_runs_on_that_runtime() {
        // `with_runtime` places an individual child's task on a caller-provided runtime instead of the supervisor's
        // own. The child reports the name of the thread it's actually running on, which must belong to that runtime.
        let child_runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .thread_name("child-rt-test")
            .enable_all()
            .build()
            .expect("should build child runtime");

        let (thread_tx, thread_rx) = oneshot::channel();
        let worker = FnWorker::new("placed", async move {
            let thread_name = std::thread::current().name().unwrap_or_default().to_string();
            let _ = thread_tx.send(thread_name);
            pending::<()>().await;
        });

        // The worker only has to stay alive long enough to report where it ran, so it's aborted at shutdown rather
        // than given a terminal condition to reach.
        let mut sup = Supervisor::new("test-sup").unwrap();
        sup.add_worker(
            ChildSpecification::worker(worker)
                .with_restart_type(RestartType::Temporary)
                .with_runtime(child_runtime.handle().clone())
                .with_shutdown_strategy(ShutdownStrategy::Brutal),
        );

        let (tx, handle) = run_supervisor_with_trigger(sup).await;

        let thread_name = timeout(Duration::from_secs(2), thread_rx)
            .await
            .expect("child should report its thread promptly")
            .expect("child should not be dropped before reporting");
        assert!(
            thread_name.starts_with("child-rt-test"),
            "child must run on the runtime given to `with_runtime`, but ran on thread {thread_name:?}"
        );

        tx.send(()).unwrap();
        assert!(join_supervisor(handle).await.is_ok());

        // Dropping a `Runtime` from within an async context panics, so tear it down without blocking.
        child_runtime.shutdown_background();
    }

    #[tokio::test]
    async fn child_shutdown_strategy_override_takes_precedence_over_worker() {
        // `with_shutdown_strategy` overrides what the worker reports for itself. The worker below asks for a 30-second
        // grace period and then ignores shutdown entirely; the override cuts that to 50ms, so the supervisor must
        // abort it and report an unclean shutdown well inside `join_supervisor`'s two-second bound. Without the
        // override taking precedence, this test times out.
        let worker = MockWorker::ignore_shutdown("stuck").with_graceful_timeout(Duration::from_secs(30));

        let mut sup = Supervisor::new("test-sup").unwrap();
        sup.add_worker(
            ChildSpecification::worker(worker)
                .with_restart_type(RestartType::Temporary)
                .with_shutdown_strategy(ShutdownStrategy::Graceful(Duration::from_millis(50))),
        );

        let (tx, handle) = run_supervisor_with_trigger(sup).await;
        tx.send(()).unwrap();

        let result = join_supervisor(handle).await;
        assert!(
            matches!(result, Err(SupervisorError::ShutdownTimedOut { aborted: 1 })),
            "the overridden 50ms deadline should have aborted the stuck child, got {result:?}"
        );
    }

    /// A worker that waits for shutdown and then drains a [`ShutdownCoordinator`] before exiting.
    ///
    /// Stands in for a component that owns background work and waits for it during its own shutdown. Unlike a
    /// closure-based worker it genuinely needs the shutdown signal, which is exactly the case [`Supervisable`] exists
    /// for.
    struct DrainWaiter {
        coordinator: Mutex<Option<ShutdownCoordinator>>,
        finished: Arc<AtomicBool>,
    }

    #[async_trait]
    impl Supervisable for DrainWaiter {
        fn name(&self) -> &str {
            "waiter"
        }

        async fn initialize(&self, process_shutdown: ShutdownHandle) -> Result<SupervisorFuture, InitializationError> {
            let coordinator = self
                .coordinator
                .lock()
                .expect("drain waiter mutex poisoned")
                .take()
                .expect("drain waiter runs once");
            let finished = Arc::clone(&self.finished);

            Ok(Box::pin(async move {
                process_shutdown.await;
                coordinator.shutdown_and_wait().await;
                finished.store(true, Ordering::SeqCst);
                Ok(())
            }))
        }
    }

    /// Builds two children modelling an owner that drains a background task during shutdown.
    ///
    /// `stuck` holds a shutdown handle and never releases it voluntarily, so the only way it goes away is a forced
    /// abort. `waiter` blocks on that handle being dropped, standing in for a component's `shutdown_and_wait`. The
    /// returned flag records whether `waiter` ran to completion rather than being aborted itself.
    fn build_drain_pair(
        stuck_strategy: ShutdownStrategy, waiter_strategy: ShutdownStrategy,
    ) -> (Supervisor, Arc<AtomicBool>) {
        let mut coordinator = ShutdownCoordinator::default();
        let held_handle = coordinator.register();

        let stuck = FnWorker::new("stuck", async move {
            // Hold the handle for as long as this future lives, and ignore shutdown entirely.
            let _held = held_handle;
            pending::<()>().await;
        });

        let waiter_finished = Arc::new(AtomicBool::new(false));
        let waiter = DrainWaiter {
            coordinator: Mutex::new(Some(coordinator)),
            finished: Arc::clone(&waiter_finished),
        };

        let mut sup = Supervisor::new("test-sup").unwrap();
        sup.add_worker(
            ChildSpecification::worker(stuck)
                .with_restart_type(RestartType::Temporary)
                .with_shutdown_strategy(stuck_strategy),
        );
        sup.add_worker(
            ChildSpecification::worker(waiter)
                .with_restart_type(RestartType::Temporary)
                .with_shutdown_strategy(waiter_strategy),
        );

        (sup, waiter_finished)
    }

    #[tokio::test]
    async fn shorter_child_deadline_releases_a_waiting_sibling() {
        // Aborting a stuck child drops the shutdown handle it was holding, which is what releases anything waiting on
        // it. A child bounded more tightly than its waiter therefore stays recoverable: the child is aborted, the
        // waiter unblocks and finishes cleanly, and only one abort is reported.
        let (sup, waiter_finished) = build_drain_pair(
            ShutdownStrategy::Graceful(Duration::from_millis(100)),
            ShutdownStrategy::Graceful(Duration::from_secs(1)),
        );

        let (tx, handle) = run_supervisor_with_trigger(sup).await;
        tx.send(()).unwrap();

        let result = join_supervisor(handle).await;
        assert!(
            matches!(result, Err(SupervisorError::ShutdownTimedOut { aborted: 1 })),
            "only the stuck child should have been aborted, got {result:?}"
        );
        assert!(
            waiter_finished.load(Ordering::SeqCst),
            "the waiter should have been released by the stuck child's abort and run to completion"
        );
    }

    #[tokio::test]
    async fn equal_child_deadlines_abort_the_waiter_too() {
        // The counterpart: concurrent shutdown computes every deadline from one shared instant, so identical timeouts
        // elapse in the same pass and the waiter is aborted alongside the child it was waiting on. This is also what a
        // shutdown budget does to a whole subtree, which is why the budget is set at a level where losing the entire
        // group at once is the intended outcome.
        let (sup, waiter_finished) = build_drain_pair(
            ShutdownStrategy::Graceful(Duration::from_millis(100)),
            ShutdownStrategy::Graceful(Duration::from_millis(100)),
        );

        let (tx, handle) = run_supervisor_with_trigger(sup).await;
        tx.send(()).unwrap();

        let result = join_supervisor(handle).await;
        assert!(
            matches!(result, Err(SupervisorError::ShutdownTimedOut { aborted: 2 })),
            "both children should have been aborted together, got {result:?}"
        );
        assert!(
            !waiter_finished.load(Ordering::SeqCst),
            "the waiter should have been aborted mid-wait, not completed"
        );
    }

    // -- Shutdown budget tests -------------------------------------------------------------

    #[tokio::test]
    async fn budget_bounds_children_that_have_no_deadline_of_their_own() {
        // Without a budget, `Graceful(Duration::MAX)` children are waited on indefinitely and a stuck one hangs
        // shutdown forever. A budget makes the supervisor responsible for the deadline instead, and each child it has
        // to abort is still counted individually -- so an overrun says how many tasks were responsible, not merely
        // that the group as a whole overran.
        let mut sup = Supervisor::new("test-sup")
            .unwrap()
            .with_shutdown_budget(Duration::from_millis(100));

        for name in ["stuck_one", "stuck_two"] {
            sup.add_worker(
                ChildSpecification::worker(FnWorker::new(name, pending::<()>()))
                    .with_restart_type(RestartType::Temporary)
                    .with_shutdown_strategy(ShutdownStrategy::Graceful(Duration::MAX)),
            );
        }

        let (tx, handle) = run_supervisor_with_trigger(sup).await;
        tx.send(()).unwrap();

        let result = join_supervisor(handle).await;
        assert!(
            matches!(result, Err(SupervisorError::ShutdownTimedOut { aborted: 2 })),
            "the budget should have aborted both deadline-less children, got {result:?}"
        );
    }

    #[tokio::test]
    async fn budget_does_not_delay_children_that_stop_on_their_own() {
        // The budget is a ceiling, not a wait. Asserting on elapsed time rather than merely on success is what makes
        // this meaningful: a supervisor that waited out its budget regardless would still report `Ok`.
        let mut sup = Supervisor::new("test-sup")
            .unwrap()
            .with_shutdown_budget(Duration::from_secs(30));
        sup.add_worker(
            ChildSpecification::one_shot_worker(MockWorker::long_running("prompt"))
                .with_shutdown_strategy(ShutdownStrategy::Graceful(Duration::MAX)),
        );

        let (tx, handle) = run_supervisor_with_trigger(sup).await;
        let started = tokio::time::Instant::now();
        tx.send(()).unwrap();

        assert!(join_supervisor(handle).await.is_ok());
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_millis(500),
            "shutdown should finish as soon as the child does, not burn the budget; took {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn child_deadline_shorter_than_budget_still_wins() {
        // A child that carries its own finite deadline is held to whichever elapses first, so a component can still
        // bound one particular task more tightly than the budget covering the rest.
        let mut sup = Supervisor::new("test-sup")
            .unwrap()
            .with_shutdown_budget(Duration::from_secs(30));
        sup.add_worker(
            ChildSpecification::worker(FnWorker::new("stuck", pending::<()>()))
                .with_restart_type(RestartType::Temporary)
                .with_shutdown_strategy(ShutdownStrategy::Graceful(Duration::from_millis(100))),
        );

        let (tx, handle) = run_supervisor_with_trigger(sup).await;
        tx.send(()).unwrap();

        // Again bounded at two seconds: if the 30-second budget had won, this would time out instead.
        let result = join_supervisor(handle).await;
        assert!(
            matches!(result, Err(SupervisorError::ShutdownTimedOut { aborted: 1 })),
            "the child's own 100ms deadline should have won over the budget, got {result:?}"
        );
    }

    #[tokio::test]
    async fn every_worker_records_poll_metrics() {
        // Poll timing is a property of being supervised, not something a child opts into, so a plain statically
        // registered worker gets it too -- tagged with its fully qualified process name.
        let recorder = TestRecorder::default();
        let _guard = metrics::set_default_local_recorder(&recorder);

        // The recorder has to be installed before the worker spawns: its metric handles are resolved once, at spawn.
        let mut sup = Supervisor::new("metrics_sup").unwrap();
        sup.add_worker(ChildSpecification::one_shot_worker(MockWorker::long_running("timed")));

        let (tx, handle) = run_supervisor_with_trigger(sup).await;
        tx.send(()).unwrap();
        assert!(join_supervisor(handle).await.is_ok());

        let polls = recorder.counter(("runtime_task_poll_count", &[("task_name", "metrics_sup.timed")]));
        assert!(
            polls.is_some_and(|polls| polls > 0),
            "a supervised worker should have recorded poll metrics, got {polls:?}"
        );
    }

    #[tokio::test]
    async fn budget_bounds_the_whole_drain_rather_than_each_child() {
        // The budget is measured once, from the start of the drain, and every child is held to that same instant --
        // it does not reset per child. Three children that each ignore a 10-second deadline must all be aborted at
        // the shared 150ms budget, not 30 seconds later.
        let mut sup = Supervisor::new("test-sup")
            .unwrap()
            .with_shutdown_budget(Duration::from_millis(150));

        for name in ["stuck_one", "stuck_two", "stuck_three"] {
            sup.add_worker(
                ChildSpecification::one_shot_worker(FnWorker::new(name, pending::<()>()))
                    .with_shutdown_strategy(ShutdownStrategy::Graceful(Duration::from_secs(10))),
            );
        }

        let (tx, handle) = run_supervisor_with_trigger(sup).await;
        tx.send(()).unwrap();

        let result = join_supervisor(handle).await;
        assert!(
            matches!(result, Err(SupervisorError::ShutdownTimedOut { aborted: 3 })),
            "the budget should have bounded the whole drain, got {result:?}"
        );
    }

    #[tokio::test]
    async fn budget_of_duration_max_is_treated_as_no_budget() {
        // `Duration::MAX` is the natural spelling of "no ceiling" and used to panic the supervisor task on an instant
        // overflow.
        let mut sup = Supervisor::new("test-sup").unwrap().with_shutdown_budget(Duration::MAX);
        sup.add_worker(ChildSpecification::one_shot_worker(MockWorker::long_running("prompt")));

        let (tx, handle) = run_supervisor_with_trigger(sup).await;
        tx.send(()).unwrap();
        assert!(join_supervisor(handle).await.is_ok());
    }

    #[tokio::test]
    async fn near_max_child_timeout_does_not_panic() {
        // Resolving a graceful timeout to an instant makes anything just under `Duration::MAX` overflow unless it is
        // added with `checked_add`. See `resolve_abort_deadline`.
        let mut sup = Supervisor::new("test-sup").unwrap();
        sup.add_worker(
            ChildSpecification::one_shot_worker(MockWorker::long_running("prompt"))
                .with_shutdown_strategy(ShutdownStrategy::Graceful(Duration::MAX - Duration::from_nanos(1))),
        );

        let (tx, handle) = run_supervisor_with_trigger(sup).await;
        tx.send(()).unwrap();
        assert!(join_supervisor(handle).await.is_ok());
    }

    #[tokio::test]
    async fn budget_does_not_cut_off_a_nested_supervisor_mid_drain() {
        // A nested supervisor bounds its own subtree, so a parent's budget must not abort it: doing so truncates the
        // subtree's drain, discards its abort tally, and -- for a supervisor on a dedicated runtime, whose work is on
        // another OS thread -- reports it as stopped without actually stopping it.
        let slow = MockWorker::slow_shutdown("slow", Duration::from_millis(300));
        let drained = slow.finish_count();

        let mut nested = Supervisor::new("nested").unwrap();
        nested.add_worker(
            ChildSpecification::one_shot_worker(slow)
                .with_shutdown_strategy(ShutdownStrategy::Graceful(Duration::from_secs(10))),
        );

        let mut parent = Supervisor::new("parent")
            .unwrap()
            .with_shutdown_budget(Duration::from_millis(50));
        parent.add_worker(nested);

        let (tx, handle) = run_supervisor_with_trigger(parent).await;
        tx.send(()).unwrap();

        let result = join_supervisor(handle).await;
        assert!(
            drained.load(Ordering::SeqCst) == 1,
            "the nested subtree should have drained rather than being cut off by the parent's budget: {result:?}"
        );
        assert!(
            result.is_ok(),
            "the nested drain finished in time, so shutdown was clean: {result:?}"
        );
    }
    // -- Supervision tree snapshot tests ---------------------------------------------------

    /// Finds a node among `nodes` by its bare name.
    fn find_node<'a>(nodes: &'a [NodeSnapshot], name: &str) -> &'a NodeSnapshot {
        nodes.iter().find(|node| node.name == name).unwrap_or_else(|| {
            panic!(
                "no node named '{name}' among {:?}",
                nodes.iter().map(|n| &n.name).collect::<Vec<_>>()
            )
        })
    }

    #[tokio::test]
    async fn snapshot_before_run_reports_a_registered_root() {
        let mut sup = Supervisor::new("test-sup")
            .unwrap()
            .with_restart_strategy(RestartStrategy::one_for_all().with_intensity_and_period(7, Duration::from_secs(11)))
            .with_auto_shutdown(AutoShutdown::AnySignificant)
            .with_shutdown_budget(Duration::from_secs(3));
        sup.add_worker(MockWorker::long_running("worker1"));

        // A tree that hasn't started still has a declared shape, and configuration is known from the moment it is
        // set, so all of it should be readable without running anything.
        let snapshot = sup.tree_handle().snapshot();

        assert_eq!(snapshot.root.name, "test-sup");
        assert_eq!(snapshot.root.kind, NodeKind::Supervisor);
        assert_eq!(snapshot.root.state, NodeState::Registered);
        assert_eq!(snapshot.root.process_id, None);
        assert_eq!(snapshot.root.process_name, None);
        assert_eq!(snapshot.root.started_at, None);
        assert_eq!(snapshot.root.uptime_ms, None);
        assert!(snapshot.root.children.is_empty(), "no children have been started yet");

        let supervision = snapshot.root.supervision.expect("a supervisor reports its settings");
        assert_eq!(supervision.restart_mode, RestartMode::OneForAll);
        assert_eq!(supervision.restart_intensity, 7);
        assert_eq!(supervision.restart_period_ms, 11_000);
        assert_eq!(supervision.auto_shutdown, AutoShutdown::AnySignificant);
        assert_eq!(supervision.shutdown_budget_ms, Some(3_000));
        assert_eq!(supervision.dedicated_threads, None);
        assert_eq!(supervision.generation, 0);

        assert_eq!(snapshot.totals.supervisors, 1);
        assert_eq!(snapshot.totals.workers, 0);
        assert_eq!(snapshot.totals.registered, 1);
        assert_eq!(snapshot.totals.max_depth, 1);
    }

    #[tokio::test]
    async fn snapshot_lists_static_children_in_declaration_order() {
        let mut sup = Supervisor::new("test-sup").unwrap();
        sup.add_worker(MockWorker::long_running("worker1"));
        sup.add_worker(MockWorker::long_running("worker2"));

        let tree = sup.tree_handle();
        let (tx, handle) = run_supervisor_with_trigger(sup).await;
        wait_until("both children appear in the snapshot", || {
            tree.snapshot().root.children.len() == 2
        })
        .await;

        let snapshot = tree.snapshot();
        assert_eq!(snapshot.root.state, NodeState::Running);
        assert!(snapshot.root.process_id.is_some());
        assert_eq!(snapshot.root.process_name.as_deref(), Some("test_sup"));
        assert_eq!(snapshot.root.supervision.expect("supervisor settings").generation, 1);

        // Declaration order, not hash order: it is how the tree is written, so it is how it should read.
        let names: Vec<&str> = snapshot.root.children.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["worker1", "worker2"]);

        for (child, expected_process) in snapshot
            .root
            .children
            .iter()
            .zip(["test_sup.worker1", "test_sup.worker2"])
        {
            assert_eq!(child.kind, NodeKind::Worker);
            assert_eq!(child.state, NodeState::Running);
            assert_eq!(child.restart, RestartType::Permanent);
            assert_eq!(child.restart_count, 0);
            assert_eq!(child.process_name.as_deref(), Some(expected_process));
            assert!(child.process_id.is_some(), "a running child has a process");
            assert!(child.children.is_empty(), "a worker has no children");
            assert!(child.supervision.is_none(), "a worker has no supervision settings");
            assert!(
                child.created_at <= child.started_at.expect("a running child has a start time"),
                "a child cannot start before it is created"
            );
        }

        let first = snapshot.root.children[0].process_id;
        let second = snapshot.root.children[1].process_id;
        assert_ne!(first, second, "each child runs as its own process");

        assert_eq!(snapshot.totals.supervisors, 1);
        assert_eq!(snapshot.totals.workers, 2);
        assert_eq!(snapshot.totals.running, 3);
        assert_eq!(snapshot.totals.max_depth, 2);

        tx.send(()).unwrap();
        let _ = join_supervisor(handle).await;
    }

    #[tokio::test]
    async fn one_for_one_restart_increments_only_the_failing_child() {
        let mut sup = Supervisor::new("test-sup")
            .unwrap()
            .with_restart_strategy(RestartStrategy::one_to_one().with_intensity_and_period(5, Duration::from_secs(5)));
        sup.add_worker(MockWorker::failing("flapper", Duration::from_millis(20)));
        sup.add_worker(MockWorker::long_running("stable"));

        let tree = sup.tree_handle();
        let (tx, handle) = run_supervisor_with_trigger(sup).await;

        wait_until("the failing child has appeared", || {
            tree.snapshot().root.children.len() == 2
        })
        .await;
        let before = tree.snapshot();
        let created_before = find_node(&before.root.children, "flapper").created_at;
        let process_before = find_node(&before.root.children, "flapper").process_id;

        wait_until("the failing child has been restarted", || {
            find_node(&tree.snapshot().root.children, "flapper").restart_count >= 1
        })
        .await;

        let after = tree.snapshot();
        let flapper = find_node(&after.root.children, "flapper");
        let stable = find_node(&after.root.children, "stable");

        assert_eq!(stable.restart_count, 0, "a one-for-one restart leaves siblings alone");
        assert_ne!(flapper.process_id, process_before, "a restart is a new process");
        assert_eq!(
            flapper.created_at, created_before,
            "creation time is a property of the child, not of the incarnation"
        );

        // A single child flapping is distinguishable from the supervisor restarting its whole group.
        assert!(after.root.supervision.expect("supervisor settings").restarts_performed >= 1);

        tx.send(()).unwrap();
        let _ = join_supervisor(handle).await;
    }

    #[tokio::test]
    async fn one_for_all_restart_increments_every_restarted_child() {
        let mut sup = Supervisor::new("test-sup")
            .unwrap()
            .with_restart_strategy(RestartStrategy::one_for_all().with_intensity_and_period(5, Duration::from_secs(5)));
        sup.add_worker(MockWorker::failing("flapper", Duration::from_millis(20)));
        sup.add_worker(MockWorker::long_running("sibling"));

        let tree = sup.tree_handle();
        let (tx, handle) = run_supervisor_with_trigger(sup).await;

        wait_until("both children have appeared", || {
            tree.snapshot().root.children.len() == 2
        })
        .await;
        let before = tree.snapshot();
        let sibling_created = find_node(&before.root.children, "sibling").created_at;
        let sibling_process = find_node(&before.root.children, "sibling").process_id;

        // The whole group is brought back, so every child's count moves -- which is what keeps each child's count
        // consistent with the new process and start time in the same record.
        wait_until("the group has been restarted", || {
            let snapshot = tree.snapshot();
            snapshot.root.children.len() == 2 && snapshot.root.children.iter().all(|child| child.restart_count >= 1)
        })
        .await;

        let after = tree.snapshot();
        let sibling = find_node(&after.root.children, "sibling");
        assert_ne!(sibling.process_id, sibling_process, "a group restart is a new process");
        assert_eq!(
            sibling.created_at, sibling_created,
            "creation time survives a group restart"
        );

        tx.send(()).unwrap();
        let _ = join_supervisor(handle).await;
    }

    #[tokio::test]
    async fn temporary_child_survives_a_group_restart_as_a_tombstone() {
        let mut sup = Supervisor::new("test-sup")
            .unwrap()
            .with_restart_strategy(RestartStrategy::one_for_all().with_intensity_and_period(5, Duration::from_secs(5)));
        sup.add_worker(
            runtime::supervisable(MockWorker::completing("one-shot", Duration::from_millis(10)))
                .temporary()
                .build(),
        );
        sup.add_worker(MockWorker::failing("flapper", Duration::from_millis(30)));

        let tree = sup.tree_handle();
        let (tx, handle) = run_supervisor_with_trigger(sup).await;

        // A temporary child is not brought back by a group restart, but it was declared, so it stays visible as a
        // tombstone rather than vanishing -- "declared, ran, stopped for good" is the useful answer.
        wait_until("the temporary child has exited and the group has restarted", || {
            let snapshot = tree.snapshot();
            snapshot
                .root
                .children
                .iter()
                .any(|child| child.name == "one-shot" && child.state == NodeState::Exited)
                && snapshot
                    .root
                    .children
                    .iter()
                    .any(|child| child.name == "flapper" && child.restart_count >= 1)
        })
        .await;

        let snapshot = tree.snapshot();
        let one_shot = find_node(&snapshot.root.children, "one-shot");
        assert_eq!(one_shot.state, NodeState::Exited);
        assert_eq!(one_shot.restart, RestartType::Temporary);
        assert_eq!(one_shot.restart_count, 0, "a temporary child is never brought back");
        assert!(one_shot.exited_at.is_some(), "a tombstone records when it exited");
        assert_eq!(one_shot.uptime_ms, None, "a node that isn't running has no uptime");
        assert!(snapshot.totals.exited >= 1);

        tx.send(()).unwrap();
        let _ = join_supervisor(handle).await;
    }

    #[tokio::test]
    async fn dynamic_children_leave_no_tombstones() {
        let mut sup = Supervisor::new("test-sup").unwrap();
        sup.add_worker(MockWorker::long_running("static-worker"));

        let tree = sup.tree_handle();
        let sup_handle = sup.handle();
        let (tx, handle) = run_supervisor_with_trigger(sup).await;
        wait_until("the static child is running", || {
            tree.snapshot().root.children.len() == 1
        })
        .await;

        // The canonical dynamic child is one short-lived task per unit of work, so retaining every one that has ever
        // finished would grow without bound. Spawn far more than a tree should ever hold and require the count to
        // come back to the static baseline.
        for _ in 0..500 {
            sup_handle.spawn(MockWorker::completing("ephemeral", Duration::from_millis(1)));
        }

        wait_until("every dynamic child has been reaped", || {
            sup_handle.active_children() == 0 && tree.snapshot().root.children.len() == 1
        })
        .await;

        let snapshot = tree.snapshot();
        assert_eq!(snapshot.root.children.len(), 1, "only the static child remains");
        assert_eq!(snapshot.root.children[0].name, "static-worker");

        tx.send(()).unwrap();
        let _ = join_supervisor(handle).await;
    }

    #[tokio::test]
    async fn snapshot_nests_child_supervisors() {
        let mut child_sup = Supervisor::new("child-sup").unwrap();
        child_sup.add_worker(MockWorker::long_running("inner-worker"));

        let mut parent_sup = Supervisor::new("parent-sup").unwrap();
        parent_sup.add_worker(MockWorker::long_running("outer-worker"));
        parent_sup.add_worker(child_sup);

        let tree = parent_sup.tree_handle();
        let (tx, handle) = run_supervisor_with_trigger(parent_sup).await;

        wait_until("the nested subtree is populated", || {
            let snapshot = tree.snapshot();
            snapshot
                .root
                .children
                .iter()
                .any(|child| child.name == "child-sup" && !child.children.is_empty())
        })
        .await;

        let snapshot = tree.snapshot();
        let nested = find_node(&snapshot.root.children, "child-sup");
        assert_eq!(nested.kind, NodeKind::Supervisor);
        assert!(nested.supervision.is_some(), "a nested supervisor reports its settings");
        assert_eq!(nested.process_name.as_deref(), Some("parent_sup.child_sup"));
        assert_eq!(nested.children.len(), 1);

        let grandchild = &nested.children[0];
        assert_eq!(grandchild.name, "inner-worker");
        assert_eq!(grandchild.kind, NodeKind::Worker);
        assert_eq!(
            grandchild.process_name.as_deref(),
            Some("parent_sup.child_sup.inner_worker")
        );

        assert_eq!(snapshot.totals.supervisors, 2);
        assert_eq!(snapshot.totals.workers, 2);
        assert_eq!(snapshot.totals.max_depth, 3);

        tx.send(()).unwrap();
        let _ = join_supervisor(handle).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dedicated_runtime_subtree_is_visible_across_the_thread_boundary() {
        let mut child_sup = Supervisor::new("child-sup")
            .unwrap()
            .with_dedicated_runtime(RuntimeConfiguration::single_threaded());
        child_sup.add_worker(MockWorker::long_running("inner-worker"));

        let mut parent_sup = Supervisor::new("parent-sup").unwrap();
        parent_sup.add_worker(child_sup);

        // Taken from the parent, before the child is running on an OS thread of its own.
        let tree = parent_sup.tree_handle();
        let (tx, handle) = run_supervisor_with_trigger(parent_sup).await;

        // Read the tree from a thread that is neither the parent's nor the dedicated runtime's.
        let probe = tree.clone();
        let snapshot = tokio::task::spawn_blocking(move || {
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            loop {
                let snapshot = probe.snapshot();
                let populated = snapshot
                    .root
                    .children
                    .iter()
                    .any(|child| child.name == "child-sup" && !child.children.is_empty());
                if populated {
                    return snapshot;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "dedicated-runtime subtree never became visible"
                );
                std::thread::sleep(Duration::from_millis(5));
            }
        })
        .await
        .expect("probe should not panic");

        let nested = find_node(&snapshot.root.children, "child-sup");
        assert_eq!(nested.state, NodeState::Running);
        assert_eq!(
            nested.supervision.expect("supervisor settings").dedicated_threads,
            Some(1)
        );
        assert_eq!(nested.children.len(), 1);

        // A supervisor on a dedicated runtime re-roots its process name when it starts, so it is *not* scoped under
        // its parent. That is a known defect (see the `Dedicated` branch of `create_worker_future`), not a design
        // choice -- but it is also why a node's name has to come from its own run rather than from the process its
        // parent created for it, since only the former names the resource group its allocations land in. Pinned here
        // so that fixing the defect is a deliberate act rather than a silent regression.
        assert_eq!(nested.process_name.as_deref(), Some("child_sup"));
        assert_eq!(
            nested.children[0].process_name.as_deref(),
            Some("child_sup.inner_worker")
        );

        tx.send(()).unwrap();
        let _ = join_supervisor(handle).await;
    }

    #[tokio::test]
    async fn a_running_node_always_has_a_process() {
        // A nested supervisor being restarted is briefly stopped, and a snapshot taken in that window must not
        // present the previous generation's processes as if they were alive. Sampling exactly inside the window is
        // inherently racy, so assert the invariant that has to hold at every instant instead.
        let mut child_sup = Supervisor::new("child-sup")
            .unwrap()
            .with_restart_strategy(RestartStrategy::new(RestartMode::OneForOne, 0, Duration::from_secs(5)));
        child_sup.add_worker(MockWorker::failing("inner", Duration::from_millis(10)));

        let mut parent_sup = Supervisor::new("parent-sup")
            .unwrap()
            .with_restart_strategy(RestartStrategy::one_to_one().with_intensity_and_period(50, Duration::from_secs(5)));
        parent_sup.add_worker(child_sup);

        let tree = parent_sup.tree_handle();
        let (tx, handle) = run_supervisor_with_trigger(parent_sup).await;

        for _ in 0..60 {
            let snapshot = tree.snapshot();
            assert_running_nodes_have_processes(&snapshot.root);
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        wait_until("the nested supervisor has been restarted", || {
            find_node(&tree.snapshot().root.children, "child-sup").restart_count >= 1
        })
        .await;

        tx.send(()).unwrap();
        let _ = join_supervisor(handle).await;
    }

    /// Asserts that every running node in the subtree reports the process it is running as.
    fn assert_running_nodes_have_processes(node: &NodeSnapshot) {
        if node.state == NodeState::Running {
            assert!(
                node.process_id.is_some() && node.process_name.is_some(),
                "node '{}' reports as running but names no process",
                node.name
            );
            assert!(
                node.uptime_ms.is_some(),
                "node '{}' is running but has no uptime",
                node.name
            );
        } else {
            assert!(
                node.uptime_ms.is_none(),
                "node '{}' is not running but reports an uptime",
                node.name
            );
        }

        for child in &node.children {
            assert_running_nodes_have_processes(child);
        }
    }

    #[tokio::test]
    async fn snapshot_after_the_run_reports_a_stopped_tree() {
        fn assert_send_sync<T: Send + Sync + 'static>() {}
        assert_send_sync::<SupervisionTreeHandle>();

        let mut sup = Supervisor::new("test-sup").unwrap();
        sup.add_worker(MockWorker::long_running("worker1"));

        let tree = sup.tree_handle();
        let (tx, handle) = run_supervisor_with_trigger(sup).await;
        wait_until("the child is running", || tree.snapshot().root.children.len() == 1).await;

        tx.send(()).unwrap();
        join_supervisor(handle).await.unwrap();

        // A stopped supervisor reports as stopped rather than presenting a subtree of processes that no longer exist.
        let snapshot = tree.snapshot();
        assert_eq!(snapshot.root.state, NodeState::Registered);
        assert_eq!(snapshot.root.process_id, None);
        assert!(snapshot.root.children.is_empty());
        assert_eq!(
            snapshot.root.supervision.expect("supervisor settings").generation,
            1,
            "the generation count outlives the run"
        );
    }

    #[tokio::test]
    async fn initialization_failure_leaves_nothing_running() {
        let mut sup = Supervisor::new("test-sup").unwrap();
        sup.add_worker(MockWorker::init_failure("broken"));

        let tree = sup.tree_handle();

        // Run directly rather than through the readiness barrier: this supervisor fails almost immediately, so it may
        // never be observed running at all.
        let mut sup = sup;
        let (_tx, rx) = oneshot::channel::<()>();
        let result = timeout(Duration::from_secs(2), sup.run_with_shutdown(rx))
            .await
            .expect("supervisor should exit promptly");
        assert!(matches!(result, Err(SupervisorError::FailedToInitialize { .. })));

        // The failure path returns through `run_inner` like every other, so the tree is cleaned up either way.
        let snapshot = tree.snapshot();
        assert_eq!(snapshot.root.state, NodeState::Registered);
        assert_eq!(snapshot.totals.running, 0);
    }

    #[tokio::test]
    async fn resources_are_attributed_to_supervisors_not_workers() {
        let mut child_sup = Supervisor::new("child-sup").unwrap();
        child_sup.add_worker(MockWorker::long_running("inner-worker"));

        let mut parent_sup = Supervisor::new("parent-sup").unwrap();
        parent_sup.add_worker(child_sup);

        let tree = parent_sup.tree_handle();
        let (tx, handle) = run_supervisor_with_trigger(parent_sup).await;
        wait_until("the nested subtree is populated", || {
            tree.snapshot()
                .root
                .children
                .iter()
                .any(|child| !child.children.is_empty())
        })
        .await;

        let snapshot = tree.snapshot();

        // The tracking allocator is a process-wide facility that the test binary doesn't install, so every byte count
        // here reads zero. That is exactly why the snapshot reports whether tracking is on at all: without it, zero
        // bytes is indistinguishable from nothing being measured.
        assert!(!snapshot.resource_tracking_enabled);

        let nested = find_node(&snapshot.root.children, "child-sup");
        assert!(
            nested.resources.is_some(),
            "a supervisor owns the resource group named by its own process"
        );
        assert_eq!(nested.resource_group.as_deref(), nested.process_name.as_deref());

        // A worker inherits its supervisor's group rather than owning one, so it names the group but carries no
        // figures of its own -- reporting the supervisor's totals against each of its workers would count them twice.
        let worker = &nested.children[0];
        assert!(worker.resources.is_none(), "a worker owns no resource group");
        assert_eq!(worker.resource_group.as_deref(), nested.process_name.as_deref());

        tx.send(()).unwrap();
        let _ = join_supervisor(handle).await;
    }

    #[tokio::test]
    async fn snapshot_serializes_to_the_expected_shape() {
        let mut child_sup = Supervisor::new("child-sup").unwrap();
        child_sup.add_worker(MockWorker::long_running("inner-worker"));

        let mut parent_sup = Supervisor::new("parent-sup").unwrap();
        parent_sup.add_worker(child_sup);

        let tree = parent_sup.tree_handle();
        let (tx, handle) = run_supervisor_with_trigger(parent_sup).await;
        wait_until("the nested subtree is populated", || {
            tree.snapshot()
                .root
                .children
                .iter()
                .any(|child| !child.children.is_empty())
        })
        .await;

        let value = serde_json::to_value(tree.snapshot()).expect("snapshot should serialize");

        // This is the wire contract an HTTP consumer reads, so pin its shape rather than just its serializability.
        assert!(value["captured_at"].is_u64(), "a timestamp is a plain integer");
        assert_eq!(value["root"]["kind"], "supervisor");
        assert_eq!(value["root"]["state"], "running");
        assert_eq!(value["root"]["restart"], "permanent");
        assert_eq!(value["root"]["supervision"]["restart_mode"], "one_for_one");
        assert_eq!(value["root"]["supervision"]["auto_shutdown"], "never");

        let nested = &value["root"]["children"][0];
        assert_eq!(nested["kind"], "supervisor");
        let worker = &nested["children"][0];
        assert_eq!(worker["kind"], "worker");
        assert!(
            worker["children"]
                .as_array()
                .expect("children is always an array")
                .is_empty(),
            "children is present and empty for a worker, so consumers can recurse unconditionally"
        );
        assert!(worker.get("resources").is_none(), "a worker reports no resource usage");
        assert!(
            worker.get("supervision").is_none(),
            "a worker reports no supervision settings"
        );
        assert!(
            worker.get("children_truncated").is_none(),
            "an untruncated node omits the truncation flag"
        );

        tx.send(()).unwrap();
        let _ = join_supervisor(handle).await;
    }

    #[tokio::test]
    async fn a_worker_that_drives_its_own_supervisor_is_shown_as_its_parent() {
        // Some workers own a supervisor rather than being one: they build it after initialization and run it inside
        // their own future, so their parent has no supervisor value to record and the subtree would otherwise be
        // invisible. This is how the largest subtrees in a real process are shaped.
        struct SupervisorDrivingWorker;

        #[async_trait]
        impl Supervisable for SupervisorDrivingWorker {
            fn name(&self) -> &str {
                "driver"
            }

            fn shutdown_strategy(&self) -> ShutdownStrategy {
                ShutdownStrategy::Graceful(Duration::MAX)
            }

            async fn initialize(
                &self, process_shutdown: ShutdownHandle,
            ) -> Result<SupervisorFuture, InitializationError> {
                Ok(Box::pin(async move {
                    let mut inner = Supervisor::new("inner-sup").expect("valid name");
                    inner.add_worker(MockWorker::long_running("inner-worker"));
                    inner
                        .run_with_shutdown_inner(process_shutdown, None)
                        .await
                        .map_err(Into::into)
                }))
            }
        }

        let mut sup = Supervisor::new("test-sup").unwrap();
        sup.add_worker(SupervisorDrivingWorker);

        let tree = sup.tree_handle();
        let (tx, handle) = run_supervisor_with_trigger(sup).await;

        wait_until("the worker's own supervisor has attached itself", || {
            let snapshot = tree.snapshot();
            snapshot
                .root
                .children
                .first()
                .is_some_and(|child| !child.children.is_empty())
        })
        .await;

        let snapshot = tree.snapshot();
        let driver = find_node(&snapshot.root.children, "driver");
        assert_eq!(
            driver.kind,
            NodeKind::Worker,
            "the worker is what its parent supervises"
        );

        let inner_sup = find_node(&driver.children, "inner-sup");
        assert_eq!(inner_sup.kind, NodeKind::Supervisor);
        assert_eq!(inner_sup.children.len(), 1);
        assert_eq!(inner_sup.children[0].name, "inner-worker");
        assert_eq!(snapshot.totals.max_depth, 4);

        tx.send(()).unwrap();
        let _ = join_supervisor(handle).await;
    }

    #[tokio::test]
    async fn mixed_child_traffic_keeps_the_roster_consistent() {
        // Exercises every path that mutates the roster in one run -- static spawn, dynamic spawn, restart in place,
        // and removal without restart. It asserts little itself: the drift check inside the roster is what is under
        // test, and it runs on every mutation.
        let mut sup = Supervisor::new("test-sup").unwrap().with_restart_strategy(
            RestartStrategy::one_to_one().with_intensity_and_period(100, Duration::from_secs(5)),
        );
        sup.add_worker(MockWorker::long_running("stable"));
        sup.add_worker(MockWorker::failing("flapper", Duration::from_millis(5)));

        let tree = sup.tree_handle();
        let sup_handle = sup.handle();
        let (tx, handle) = run_supervisor_with_trigger(sup).await;

        for _ in 0..25 {
            sup_handle.spawn(MockWorker::completing("ephemeral", Duration::from_millis(2)));
            sup_handle.spawn(MockWorker::long_running("lingering"));
            let snapshot = tree.snapshot();
            assert_running_nodes_have_processes(&snapshot.root);
            tokio::time::sleep(Duration::from_millis(2)).await;
        }

        wait_until("the flapping child has restarted several times", || {
            find_node(&tree.snapshot().root.children, "flapper").restart_count >= 3
        })
        .await;

        tx.send(()).unwrap();
        let _ = join_supervisor(handle).await;
    }
}
