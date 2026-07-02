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
use saluki_common::collections::FastHashMap;
use saluki_common::sync::shutdown::ShutdownHandle;
use saluki_error::GenericError;
use snafu::{OptionExt as _, Snafu};
use tokio::{
    pin, select,
    sync::{mpsc, oneshot},
};
use tracing::{debug, error, warn};

use super::{
    dedicated::{spawn_dedicated_runtime, RuntimeConfiguration, RuntimeMode},
    restart::{RestartAction, RestartMode, RestartState, RestartStrategy, RestartType},
    worker_state::WorkerState,
};
use crate::runtime::{
    process::{Process, ProcessExt as _},
    state::DataspaceRegistry,
};

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
pub enum ShutdownStrategy {
    /// Waits for the configured duration for the process to exit, and then forcefully aborts it otherwise.
    Graceful(Duration),

    /// Forcefully aborts the process without waiting.
    Brutal,
}

/// Policy for automatically shutting a supervisor down based on the termination of its _significant_ children.
///
/// A significant child (see [`ChildSpecification::with_significant`]) is one whose termination -- when it isn't restarted -- can
/// drive the supervisor to shut down. This mirrors Erlang/OTP's `auto_shutdown` supervisor flag, and is how an
/// unexpected (or intentional) child exit cascades into the supervisor stopping, and thus propagating up the tree,
/// without that child being restarted.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum AutoShutdown {
    /// Never shut down automatically; significant children have no special effect. This is the default.
    #[default]
    Never,

    /// Shut down as soon as _any_ significant child terminates without being restarted.
    AnySignificant,

    /// Shut down once _all_ significant children have terminated without being restarted.
    AllSignificant,
}

/// How a supervisor shuts its children down.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ShutdownMode {
    /// Shut children down one at a time, in reverse order of starting (last-started first).
    ///
    /// This is the default, and is appropriate when later children may depend on earlier ones: each child is fully
    /// stopped before the next is signalled.
    #[default]
    Ordered,

    /// Shut all children down at once and wait for them concurrently.
    ///
    /// Total shutdown time is bounded by the slowest child rather than the sum of all children, which suits large,
    /// independent child sets -- for example, one task per network connection.
    Concurrent,
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
    /// See [`AutoShutdown`] and [`ChildSpecification::with_significant`]. The supervisor stopped, and drained its remaining
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
/// Generally, callers should prefer to use [`add_worker`][Supervisor::add_worker] directly, which can accept either
/// [`Supervisor`] or any value that implements [`Supervisable`], without needing to explicitly create a
/// [`ChildSpecification`]. This is preferred as it is more concise but also will ensure that relevant settings are
/// configured properly for the given worker type, such as using the proper shutdown strategy for supervisors to allow
/// for complete, graceful shutdown.
///
/// If more control is needed, [`ChildSpecification::worker`] can be used to create a specification directly, allowing
/// access to configuring those more advanced settings. This is currently only valid for worker processes, as
/// supervisors have no additional user-configurable settings.
pub struct ChildSpecification<S = WorkerSpec> {
    spec_inner: S,
}

/// Child specification state for a worker.
pub struct WorkerSpec {
    worker: Arc<dyn Supervisable>,
    config: ChildConfig,
}

/// Child specification state for a supervisor.
pub struct SupervisorSpec {
    supervisor: Supervisor,
}

impl ChildSpecification<WorkerSpec> {
    /// Creates a specification for the given worker.
    pub fn worker<T: Supervisable + 'static>(worker: T) -> Self {
        Self {
            spec_inner: WorkerSpec {
                worker: Arc::new(worker),
                config: ChildConfig::default(),
            },
        }
    }

    /// Sets the restart policy for this worker.
    ///
    /// Defaults to [`RestartType::Permanent`].
    #[must_use]
    pub fn with_restart_type(mut self, restart_type: RestartType) -> Self {
        self.spec_inner.config.restart = restart_type;
        self
    }

    /// Sets whether this worker is _significant_.
    ///
    /// A significant worker's termination (when it isn't restarted) can drive the supervisor to shut down, per the
    /// supervisor's [`AutoShutdown`] policy. Only meaningful for non-permanent workers, since a permanent worker is
    /// always restarted and so never terminates without being restarted.
    #[must_use]
    pub fn with_significant(mut self, significant: bool) -> Self {
        self.spec_inner.config.significant = significant;
        self
    }

    /// Lowers this worker specification into its type-erased child and configuration.
    fn into_worker_parts(self) -> (SupervisedChild, ChildConfig) {
        (SupervisedChild::Worker(self.spec_inner.worker), self.spec_inner.config)
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
            spec_inner: SupervisorSpec { supervisor },
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
/// [`WorkerSpec`] and [`SupervisorSpec`]. It exists so that [`Supervisor::add_worker`] can accept a
/// [`ChildSpecification`] in either state (as well as bare workers and supervisors) while lowering each into the
/// supervisor's internal representation.
pub trait ChildState: sealed::Sealed + Sized {
    #[doc(hidden)]
    fn register(spec: ChildSpecification<Self>, supervisor: &mut Supervisor);
}

impl ChildState for WorkerSpec {
    fn register(spec: ChildSpecification<Self>, supervisor: &mut Supervisor) {
        let (child, config) = spec.into_worker_parts();
        supervisor.push_child(ChildEntry {
            spec: child,
            config,
            dynamic: false,
        });
    }
}

impl ChildState for SupervisorSpec {
    fn register(spec: ChildSpecification<Self>, supervisor: &mut Supervisor) {
        supervisor.push_child(ChildEntry {
            spec: SupervisedChild::Supervisor(spec.spec_inner.supervisor),
            config: ChildConfig::default(),
            dynamic: false,
        });
    }
}

/// The type-erased, runnable form of a child: either a worker or a nested supervisor.
pub(super) enum SupervisedChild {
    Worker(Arc<dyn Supervisable>),
    Supervisor(Supervisor),
}

impl SupervisedChild {
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

    pub(super) fn shutdown_strategy(&self) -> ShutdownStrategy {
        match self {
            Self::Worker(worker) => worker.shutdown_strategy(),

            // Supervisors should always be given as much time as necessary shutdown down gracefully to ensure that the
            // entire supervision subtree can be shutdown cleanly.
            Self::Supervisor(_) => ShutdownStrategy::Graceful(Duration::MAX),
        }
    }

    pub(super) fn create_process(&self, parent_process: &Process) -> Result<Process, SupervisorError> {
        match self {
            Self::Worker(worker) => Process::worker(worker.name(), parent_process).context(InvalidName {
                name: worker.name().to_string(),
            }),
            Self::Supervisor(sup) => {
                Process::supervisor(&sup.supervisor_id, Some(parent_process)).context(InvalidName {
                    name: sup.supervisor_id.to_string(),
                })
            }
        }
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

/// Per-child configuration: its [`RestartType`] and whether it is _significant_ (see [`AutoShutdown`]).
///
/// Defaults to a permanent, non-significant child. On a worker, this is set through
/// [`ChildSpecification::with_restart_type`] and [`ChildSpecification::with_significant`].
#[derive(Clone, Copy, Debug)]
struct ChildConfig {
    restart: RestartType,
    significant: bool,
}

impl Default for ChildConfig {
    fn default() -> Self {
        Self {
            restart: RestartType::Permanent,
            significant: false,
        }
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

/// Error returned when spawning a dynamic child on a [`Supervisor`] fails.
#[derive(Debug, Snafu)]
pub enum SpawnError {
    /// The supervisor isn't currently running, so it can't accept the spawn.
    ///
    /// Returned when the supervisor hasn't started yet, is between restarts, or has shut down -- and also if the run
    /// ends after the request is accepted but before the child is started. To add children before the supervisor
    /// starts, configure them statically with [`Supervisor::add_worker`] instead.
    #[snafu(display("supervisor is gone"))]
    SupervisorGone,

    /// The supervisor was running but rejected the spawn (for example, an invalid child name).
    ///
    /// Unlike [`SupervisorGone`](Self::SupervisorGone), the supervisor accepted the request and then couldn't start the
    /// child; the underlying error is preserved as the source.
    #[snafu(display("supervisor rejected the spawn: {}", source))]
    Rejected {
        /// The underlying error that caused the spawn to be rejected.
        source: GenericError,
    },
}

/// A dynamic spawn request sent from a [`SupervisorHandle`] to the running supervisor.
struct PendingSpawn {
    id: u64,
    spec: SupervisedChild,
    config: ChildConfig,
    ack: oneshot::Sender<Result<(), SpawnError>>,
}

/// Capacity of the per-run channel that carries dynamic spawn requests from handles to the running supervisor.
///
/// Each request is short-lived -- the supervisor processes it and signals the waiting caller promptly -- so this only
/// bounds how many spawns can be in flight before a caller's send applies backpressure.
const DYNAMIC_SPAWN_CHANNEL_CAPACITY: usize = 1024;

/// A handle for spawning dynamic children on a running [`Supervisor`].
///
/// Obtained from [`Supervisor::handle`]. Handles are cheap to clone and can be shared across tasks. Spawning is async:
/// the request is handed to the running supervisor and the call returns once the child has been started. If the
/// supervisor isn't currently running, spawning returns [`SpawnError::SupervisorGone`].
#[derive(Clone)]
pub struct SupervisorHandle {
    name: Arc<str>,
    // The currently running supervisor publishes its command sender here so handles can reach the live run; it's
    // cleared when no run is active, at which point spawns observe `SupervisorGone`.
    current_tx: Arc<Mutex<Option<mpsc::Sender<PendingSpawn>>>>,
    id_counter: Arc<AtomicU64>,
    active: Arc<AtomicUsize>,
}

impl SupervisorHandle {
    /// Returns the name of the supervisor this handle refers to.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Spawns a new dynamic worker.
    ///
    /// Dynamic workers are temporary children that are not restarted by the supervisor when they die or when the
    /// supervisor itself is restarted. They are useful for short-lived, non-critical background tasks that require
    /// structured concurrency: the process should be cancelled when the supervisor itself is restarted or terminated,
    /// and so on.
    ///
    /// Use [`spawn_with`](Self::spawn_with) to configure the child's restart policy or significance.
    ///
    /// # Errors
    ///
    /// If the supervisor isn't current running, or if the child specification is invalid, an error is returned.
    pub async fn spawn<T: Supervisable + 'static>(&self, worker: T) -> Result<ChildId, SpawnError> {
        self.spawn_with(ChildSpecification::worker(worker).with_restart_type(RestartType::Temporary))
            .await
    }

    /// Spawns a new dynamic child from a fully configured [`ChildSpecification`].
    ///
    /// Dynamic workers are temporary children that are not restarted by the supervisor when they die or when the
    /// supervisor itself is restarted. They are useful for short-lived, non-critical background tasks that require
    /// structured concurrency: the process should be cancelled when the supervisor itself is restarted or terminated,
    /// and so on.
    ///
    /// This method allows for configuring more advanced aspects of the child process, such as its restart type and
    /// significance.
    ///
    /// # Errors
    ///
    /// If the supervisor isn't current running, or if the child specification is invalid, an error is returned.
    pub async fn spawn_with(&self, spec: ChildSpecification<WorkerSpec>) -> Result<ChildId, SpawnError> {
        let id = self.id_counter.fetch_add(1, Ordering::Relaxed);
        let (spec, config) = spec.into_worker_parts();
        let (ack_tx, ack_rx) = oneshot::channel();
        self.send(PendingSpawn {
            id,
            spec,
            config,
            ack: ack_tx,
        })
        .await?;

        // Wait for the supervisor to start (or reject) the child. A dropped ack channel means the run ended before it
        // got to us, which is indistinguishable from `SupervisorGone` to the caller.
        ack_rx
            .await
            .map_err(|_| SpawnError::SupervisorGone)?
            .map(|()| ChildId(id))
    }

    /// Returns whether the supervisor is currently running.
    pub fn is_running(&self) -> bool {
        self.current_tx.lock().unwrap().is_some()
    }

    /// Returns the number of dynamic children currently running under the supervisor.
    pub fn active_children(&self) -> usize {
        self.active.load(Ordering::Relaxed)
    }

    /// Hands a spawn request to the currently running supervisor, applying backpressure if its channel is full.
    ///
    /// Returns [`SpawnError::SupervisorGone`] if no run is active, or if the run ends before the request is accepted.
    async fn send(&self, spawn: PendingSpawn) -> Result<(), SpawnError> {
        // Clone the sender out from under the lock so we don't hold the (synchronous) mutex guard across the await.
        let tx = self.current_tx.lock().unwrap().clone();
        match tx {
            Some(tx) => tx.send(spawn).await.map_err(|_| SpawnError::SupervisorGone),
            None => Err(SpawnError::SupervisorGone),
        }
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
/// ([`TrackingAllocator`][resource_accounting::TrackingAllocator]), which is used to track both the memory usage of the
/// supervisor itself and its children. Additionally, individual worker processes are wrapped in a dedicated
/// [`tracing::Span`] to allow tracing the causal relationship between arbitrary code and the worker executing it.
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
    restart_strategy: RestartStrategy,
    auto_shutdown: AutoShutdown,
    shutdown_mode: ShutdownMode,
    runtime_mode: RuntimeMode,
    // Shared across clones (a nested supervisor is cloned each time it runs) and across all handles. While a run is
    // active it holds that run's spawn-command sender so handles can reach the live supervisor; it's `None` whenever no
    // run is active, at which point spawns observe `SupervisorGone`. Doubles as the `is_running` signal.
    current_tx: Arc<Mutex<Option<mpsc::Sender<PendingSpawn>>>>,
    id_counter: Arc<AtomicU64>,
    // Number of dynamic children currently running, shared with handles so it can be surfaced as a gauge.
    active: Arc<AtomicUsize>,
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

        Ok(Self {
            supervisor_id: supervisor_id.as_ref().into(),
            child_specs: Vec::new(),
            restart_strategy: RestartStrategy::default(),
            auto_shutdown: AutoShutdown::default(),
            shutdown_mode: ShutdownMode::default(),
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
    pub fn with_restart_strategy(mut self, strategy: RestartStrategy) -> Self {
        self.restart_strategy = strategy;
        self
    }

    /// Sets the supervisor's automatic-shutdown policy.
    ///
    /// Controls whether the termination of _significant_ children (see [`ChildSpecification::with_significant`]) drives the
    /// supervisor to shut down. Defaults to [`AutoShutdown::Never`].
    pub fn with_auto_shutdown(mut self, auto_shutdown: AutoShutdown) -> Self {
        self.auto_shutdown = auto_shutdown;
        self
    }

    /// Sets the supervisor's shutdown mode. See [`ShutdownMode`]. Defaults to [`ShutdownMode::Ordered`].
    pub fn with_shutdown_mode(mut self, mode: ShutdownMode) -> Self {
        self.shutdown_mode = mode;
        self
    }

    /// Returns a handle for spawning dynamic children on this supervisor while it runs.
    ///
    /// The handle can be created before the supervisor starts and cloned freely. Spawns only succeed while the
    /// supervisor is actually running; if it hasn't started yet, is between restarts, or has shut down, they return
    /// [`SpawnError::SupervisorGone`].
    pub fn handle(&self) -> SupervisorHandle {
        SupervisorHandle {
            name: Arc::clone(&self.supervisor_id),
            current_tx: Arc::clone(&self.current_tx),
            id_counter: Arc::clone(&self.id_counter),
            active: Arc::clone(&self.active),
        }
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
    /// See [`ChildSpecification`] for more details on how workers are represented internally and what options are
    /// available to configure.
    pub fn add_worker<S, T>(&mut self, child: T)
    where
        S: ChildState,
        T: Into<ChildSpecification<S>>,
    {
        S::register(child.into(), self);
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
        self.child_specs.push(entry);
    }

    fn spawn_static_children(
        &self, children: &mut FastHashMap<u64, ChildEntry>, worker_state: &mut WorkerState,
    ) -> Result<(), SupervisorError> {
        debug!(supervisor_id = %self.supervisor_id, "Spawning all static child processes.");
        for entry in &self.child_specs {
            let id = self.id_counter.fetch_add(1, Ordering::Relaxed);
            worker_state.add_worker(id, &entry.spec)?;
            children.insert(id, entry.clone());
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
        &self, children: &mut FastHashMap<u64, ChildEntry>, worker_state: &mut WorkerState,
    ) -> Result<(), SupervisorError> {
        debug!(supervisor_id = %self.supervisor_id, "Restarting all eligible static child processes.");
        for entry in &self.child_specs {
            // Temporary children are never restarted by a group restart (matching OTP): they are shut down with the
            // group but not brought back.
            if entry.config.restart == RestartType::Temporary {
                continue;
            }
            let id = self.id_counter.fetch_add(1, Ordering::Relaxed);
            worker_state.add_worker(id, &entry.spec)?;
            children.insert(id, entry.clone());
        }

        Ok(())
    }

    /// Spawns one dynamic child into the running supervisor's worker set and roster, signaling the requesting handle.
    fn spawn_dynamic_child(
        &self, spawn: PendingSpawn, worker_state: &mut WorkerState, children: &mut FastHashMap<u64, ChildEntry>,
        significant_remaining: &mut usize,
    ) {
        let PendingSpawn { id, spec, config, ack } = spawn;
        let entry = ChildEntry {
            spec,
            config,
            dynamic: true,
        };
        match worker_state.add_worker(id, &entry.spec) {
            Ok(()) => {
                if config.significant {
                    *significant_remaining += 1;
                }
                self.active.fetch_add(1, Ordering::Relaxed);
                children.insert(id, entry);
                let _ = ack.send(Ok(()));
            }
            Err(e) => {
                // Registration failed (e.g. an invalid child name). Report it to the waiting caller as `Rejected` --
                // distinct from `SupervisorGone` -- so the underlying cause isn't lost.
                error!(supervisor_id = %self.supervisor_id, error = %e, "Failed to spawn dynamic child.");
                let _ = ack.send(Err(SpawnError::Rejected { source: e.into() }));
            }
        }
    }

    async fn run_inner(&self, process: Process, process_shutdown: ShutdownHandle) -> Result<(), SupervisorError> {
        // Publish a fresh command channel for this run so handles can spawn dynamic children into it; while it's set,
        // handles observe us as running.
        let (cmd_tx, cmd_rx) = mpsc::channel(DYNAMIC_SPAWN_CHANNEL_CAPACITY);
        *self.current_tx.lock().unwrap() = Some(cmd_tx);

        let result = self.supervise(process, process_shutdown, cmd_rx).await;

        // The run is over. Clear the sender so later spawns observe `SupervisorGone`, and reset the dynamic-children
        // gauge. Dropping the receiver (owned by `supervise`) already rejected anything still in flight.
        *self.current_tx.lock().unwrap() = None;
        self.active.store(0, Ordering::Relaxed);
        result
    }

    async fn supervise(
        &self, process: Process, process_shutdown: ShutdownHandle, mut cmd_rx: mpsc::Receiver<PendingSpawn>,
    ) -> Result<(), SupervisorError> {
        let mut restart_state = RestartState::new(self.restart_strategy);
        let mut worker_state = WorkerState::new(process, self.shutdown_mode);

        // The live roster of children -- both static (seeded below) and dynamic (added via the handle) -- keyed by a
        // stable id. A restart re-runs a child by id; a child that isn't restarted is removed from the roster.
        let mut children: FastHashMap<u64, ChildEntry> = FastHashMap::default();

        // Spawn the static children. Initialization is folded into each worker's task, so this returns immediately --
        // children initialize concurrently in the background.
        self.spawn_static_children(&mut children, &mut worker_state)?;

        // Track how many significant children are still running, for `AutoShutdown` evaluation.
        let mut significant_remaining = children.values().filter(|entry| entry.config.significant).count();

        // Now we supervise.
        pin!(process_shutdown);

        let outcome = loop {
            select! {
                // Shutdown takes priority so a flood of dynamic spawns can't starve it.
                biased;

                // Shutdown has been triggered; break out of the loop with a clean outcome and tear down below. (We
                // can't touch `cmd_rx` in any arm's handler -- the `recv` arm below borrows it for the whole
                // `select!` -- so all teardown happens after the loop.)
                _ = &mut process_shutdown => break Ok(()),

                // A handle asked us to spawn a dynamic child. The published sender keeps the channel open for the whole
                // run, so `recv` only yields `None` once we close it during teardown.
                spawn = cmd_rx.recv() => {
                    if let Some(spawn) = spawn {
                        self.spawn_dynamic_child(spawn, &mut worker_state, &mut children, &mut significant_remaining);
                    }
                }

                (child_id, worker_result) = worker_state.wait_for_next_worker() => {
                    // Pull out what we need from the roster before we mutate it.
                    let (child_name, config, dynamic) = {
                        let entry = children.get(&child_id).expect("completed worker must be present in the roster");
                        (entry.spec.name().to_string(), entry.config, entry.dynamic)
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
                        debug!(supervisor_id = %self.supervisor_id, worker_name = %child_name, restart = ?config.restart, ?worker_result, "Child process exited and is not eligible for restart.");
                        children.remove(&child_id);
                        if dynamic {
                            self.active.fetch_sub(1, Ordering::Relaxed);
                        }

                        // A significant child terminating without restart can drive the supervisor to shut down, per its
                        // `AutoShutdown` policy -- cascading an unexpected (or intentional) child exit into the
                        // supervisor stopping and propagating up the tree.
                        if config.significant {
                            significant_remaining = significant_remaining.saturating_sub(1);
                            let auto_shutdown = match self.auto_shutdown {
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
                                    let spec = children.get(&child_id).expect("present for restart").spec.clone();
                                    if let Err(e) = worker_state.add_worker(child_id, &spec) {
                                        break Err(e);
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
                                    children.clear();
                                    self.active.store(0, Ordering::Relaxed);
                                    let respawn = self.respawn_children_one_for_all(&mut children, &mut worker_state);
                                    if let Err(e) = respawn {
                                        break Err(e);
                                    }
                                    significant_remaining =
                                        children.values().filter(|entry| entry.config.significant).count();
                                }
                            },
                            RestartAction::Shutdown => {
                                error!(supervisor_id = %self.supervisor_id, worker_name = %child_name, ?worker_result, "Supervisor shutting down due to restart limits.");
                                break Err(SupervisorError::Shutdown);
                            }
                        }
                    }
                }
            }
        };

        // The run is ending -- either cleanly (shutdown was signalled) or with an error (a child failed to initialize
        // or restart, the restart limit was exceeded, or a significant child exited). On every path: stop accepting
        // spawns and reject anything still queued -- rather
        // than starting children only to tear them down immediately -- then shut down all children. Closing the channel
        // before the (possibly slow) shutdown also unblocks any handle parked on a full channel, so a spawn racing the
        // teardown observes `SupervisorGone` promptly instead of hanging until shutdown finishes.
        cmd_rx.close();
        while let Ok(spawn) = cmd_rx.try_recv() {
            let _ = spawn.ack.send(Err(SpawnError::SupervisorGone));
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
            restart_strategy: self.restart_strategy,
            auto_shutdown: self.auto_shutdown,
            shutdown_mode: self.shutdown_mode,
            runtime_mode: self.runtime_mode.clone(),
            current_tx: Arc::clone(&self.current_tx),
            id_counter: Arc::clone(&self.id_counter),
            active: Arc::clone(&self.active),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        future::pending,
        sync::atomic::{AtomicUsize, Ordering},
    };

    use async_trait::async_trait;
    use tokio::{
        sync::oneshot,
        task::JoinHandle,
        time::{sleep, timeout},
    };

    use super::*;

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
                brutal_shutdown: false,
                graceful_timeout: Duration::from_millis(500),
            }
        }

        /// Returns a shared handle to the start count for this worker.
        fn start_count(&self) -> Arc<AtomicUsize> {
            Arc::clone(&self.start_count)
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
                                Err(GenericError::msg(msg))
                            }
                            _ = process_shutdown => {
                                Ok(())
                            }
                        }
                    }
                    RunBehavior::CompleteAfter(delay) => {
                        select! {
                            _ = sleep(delay) => Ok(()),
                            _ = process_shutdown => Ok(()),
                        }
                    }
                    RunBehavior::SlowShutdown(delay) => {
                        process_shutdown.await;
                        sleep(delay).await;
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
    /// Returns the supervisor result and provides the shutdown sender.
    async fn run_supervisor_with_trigger(
        mut supervisor: Supervisor,
    ) -> (oneshot::Sender<()>, JoinHandle<Result<(), SupervisorError>>) {
        let (tx, rx) = oneshot::channel();
        let handle = tokio::spawn(async move { supervisor.run_with_shutdown(rx).await });
        // Give the supervisor a moment to start and spawn children.
        sleep(Duration::from_millis(50)).await;
        (tx, handle)
    }

    // -- Supervisor run mode tests ---------------------------------------------------------

    #[tokio::test]
    async fn standalone_supervisor_shuts_down_cleanly() {
        let mut sup = Supervisor::new("test-sup").unwrap();
        sup.add_worker(MockWorker::long_running("worker1"));
        sup.add_worker(MockWorker::long_running("worker2"));

        let (tx, handle) = run_supervisor_with_trigger(sup).await;
        tx.send(()).unwrap();

        let result = timeout(Duration::from_secs(2), handle).await.unwrap().unwrap();
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

        let result = timeout(Duration::from_secs(2), handle).await.unwrap().unwrap();
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
        let result = timeout(Duration::from_secs(2), handle).await.unwrap().unwrap();
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

        // Wait for a few restarts to happen.
        sleep(Duration::from_millis(300)).await;
        let _ = tx.send(());

        let result = timeout(Duration::from_secs(2), handle).await.unwrap().unwrap();
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

        // Wait for at least one restart cycle.
        sleep(Duration::from_millis(300)).await;
        let _ = tx.send(());

        let result = timeout(Duration::from_secs(2), handle).await.unwrap().unwrap();
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

        // Let several one-for-all cycles occur.
        sleep(Duration::from_millis(300)).await;
        let _ = tx.send(());

        let result = timeout(Duration::from_secs(2), handle).await.unwrap().unwrap();
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

        sleep(Duration::from_millis(300)).await;
        let _ = tx.send(());

        let result = timeout(Duration::from_secs(2), handle).await.unwrap().unwrap();
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

        sleep(Duration::from_millis(300)).await;
        let _ = tx.send(());

        let result = timeout(Duration::from_secs(2), handle).await.unwrap().unwrap();
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

        let result = timeout(Duration::from_secs(2), handle).await.unwrap().unwrap();
        drop(tx);

        assert!(matches!(result, Err(SupervisorError::Shutdown)));
    }

    // -- Restart type tests ----------------------------------------------------------------

    #[tokio::test]
    async fn temporary_child_is_not_restarted() {
        // A temporary worker that fails quickly, alongside a long-running worker that keeps the supervisor alive.
        let temp = MockWorker::failing("temp-worker", Duration::from_millis(50));
        let temp_count = temp.start_count();

        let stable = MockWorker::long_running("stable-worker");

        let mut sup = Supervisor::new("test-sup").unwrap().with_restart_strategy(
            RestartStrategy::one_to_one().with_intensity_and_period(20, Duration::from_secs(10)),
        );
        sup.add_worker(stable);
        sup.add_worker(ChildSpecification::worker(temp).with_restart_type(RestartType::Temporary));

        let (tx, handle) = run_supervisor_with_trigger(sup).await;

        // Give the temporary worker time to fail; it must not be restarted.
        sleep(Duration::from_millis(300)).await;
        let _ = tx.send(());

        let result = timeout(Duration::from_secs(2), handle).await.unwrap().unwrap();
        assert!(result.is_ok());
        assert_eq!(
            temp_count.load(Ordering::SeqCst),
            1,
            "temporary worker must not be restarted"
        );
    }

    #[tokio::test]
    async fn transient_child_is_not_restarted_on_clean_exit() {
        let transient = MockWorker::completing("transient-worker", Duration::from_millis(50));
        let transient_count = transient.start_count();

        let stable = MockWorker::long_running("stable-worker");

        let mut sup = Supervisor::new("test-sup").unwrap().with_restart_strategy(
            RestartStrategy::one_to_one().with_intensity_and_period(20, Duration::from_secs(10)),
        );
        sup.add_worker(stable);
        sup.add_worker(ChildSpecification::worker(transient).with_restart_type(RestartType::Transient));

        let (tx, handle) = run_supervisor_with_trigger(sup).await;

        sleep(Duration::from_millis(300)).await;
        let _ = tx.send(());

        let result = timeout(Duration::from_secs(2), handle).await.unwrap().unwrap();
        assert!(result.is_ok());
        assert_eq!(
            transient_count.load(Ordering::SeqCst),
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

        sleep(Duration::from_millis(300)).await;
        let _ = tx.send(());

        let result = timeout(Duration::from_secs(2), handle).await.unwrap().unwrap();
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

        sleep(Duration::from_millis(300)).await;
        let _ = tx.send(());

        let result = timeout(Duration::from_secs(2), handle).await.unwrap().unwrap();
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
        let counts: Vec<_> = workers.iter().map(|w| w.start_count()).collect();
        for worker in workers {
            sup.add_worker(ChildSpecification::worker(worker).with_restart_type(RestartType::Temporary));
        }
        // A long-running worker so the supervisor doesn't simply idle once the temporaries are gone.
        sup.add_worker(MockWorker::long_running("stable-worker"));

        let (tx, handle) = run_supervisor_with_trigger(sup).await;
        sleep(Duration::from_millis(300)).await;
        let _ = tx.send(());

        let result = timeout(Duration::from_secs(2), handle).await.unwrap().unwrap();
        assert!(
            result.is_ok(),
            "supervisor must not trip its restart limit on temporary exits"
        );
        for count in counts {
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
        let counts: Vec<_> = workers.iter().map(|w| w.start_count()).collect();
        for worker in workers {
            sup.add_worker(ChildSpecification::worker(worker).with_restart_type(RestartType::Transient));
        }
        // A long-running worker so the supervisor doesn't simply idle once the transients have completed.
        sup.add_worker(MockWorker::long_running("stable-worker"));

        let (tx, handle) = run_supervisor_with_trigger(sup).await;
        sleep(Duration::from_millis(300)).await;
        let _ = tx.send(());

        let result = timeout(Duration::from_secs(2), handle).await.unwrap().unwrap();
        assert!(
            result.is_ok(),
            "supervisor must not trip its restart limit on clean transient exits"
        );
        for count in counts {
            assert_eq!(
                count.load(Ordering::SeqCst),
                1,
                "each transient worker runs exactly once"
            );
        }
    }

    #[tokio::test]
    async fn supervisor_idles_when_all_temporary_children_exit() {
        // When every child is temporary and they all exit, the worker set drains. The supervisor must not panic or exit
        // on its own; it should keep waiting until shutdown is triggered.
        let mut sup = Supervisor::new("test-sup").unwrap();
        sup.add_worker(
            ChildSpecification::worker(MockWorker::completing("temp-a", Duration::from_millis(30)))
                .with_restart_type(RestartType::Temporary),
        );
        sup.add_worker(
            ChildSpecification::worker(MockWorker::completing("temp-b", Duration::from_millis(30)))
                .with_restart_type(RestartType::Temporary),
        );

        let (tx, handle) = run_supervisor_with_trigger(sup).await;

        // Both children complete well within this window; the supervisor should still be running (idling).
        sleep(Duration::from_millis(200)).await;
        assert!(
            !handle.is_finished(),
            "supervisor must keep running after all children exit"
        );

        let _ = tx.send(());
        let result = timeout(Duration::from_secs(2), handle).await.unwrap().unwrap();
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
    async fn non_significant_exit_does_not_auto_shutdown() {
        // Even with `AnySignificant` set, a non-significant child exiting must not shut the supervisor down.
        let mut sup = Supervisor::new("test-sup")
            .unwrap()
            .with_auto_shutdown(AutoShutdown::AnySignificant);
        sup.add_worker(MockWorker::long_running("stable"));
        sup.add_worker(
            ChildSpecification::worker(MockWorker::completing("plain", Duration::from_millis(50)))
                .with_restart_type(RestartType::Temporary),
        );

        let (tx, handle) = run_supervisor_with_trigger(sup).await;
        sleep(Duration::from_millis(200)).await;
        assert!(
            !handle.is_finished(),
            "a non-significant child exiting must not trigger auto-shutdown"
        );

        tx.send(()).unwrap();
        let result = timeout(Duration::from_secs(2), handle).await.unwrap().unwrap();
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

    async fn wait_running(handle: &SupervisorHandle) {
        for _ in 0..200 {
            if handle.is_running() {
                return;
            }
            sleep(Duration::from_millis(5)).await;
        }
        panic!("supervisor did not start in time");
    }

    async fn wait_until(condition: impl Fn() -> bool) {
        for _ in 0..200 {
            if condition() {
                return;
            }
            sleep(Duration::from_millis(5)).await;
        }
        panic!("condition not met in time");
    }

    #[tokio::test]
    async fn dynamic_children_spawn_after_start() {
        let sup = Supervisor::new("dyn-sup").unwrap();
        let handle = sup.handle();
        let (tx, run) = run_supervisor_with_trigger(sup).await;
        wait_running(&handle).await;

        let c1 = MockWorker::long_running("c1");
        let c2 = MockWorker::long_running("c2");
        let c1_count = c1.start_count();
        let c2_count = c2.start_count();
        handle.spawn(c1).await.unwrap();
        handle.spawn(c2).await.unwrap();

        wait_until(|| c1_count.load(Ordering::SeqCst) == 1 && c2_count.load(Ordering::SeqCst) == 1).await;
        assert_eq!(handle.active_children(), 2);

        tx.send(()).unwrap();
        let result = timeout(Duration::from_secs(2), run).await.unwrap().unwrap();
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
        wait_running(&handle).await;

        let failing = MockWorker::failing("boom", Duration::from_millis(20));
        let failing_count = failing.start_count();
        handle.spawn(failing).await.unwrap();
        wait_until(|| failing_count.load(Ordering::SeqCst) == 1).await;
        wait_until(|| handle.active_children() == 0).await;

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
        handle.spawn(MockWorker::long_running("c2")).await.unwrap();
        wait_until(|| handle.active_children() == 1).await;

        tx.send(()).unwrap();
        let result = timeout(Duration::from_secs(2), run).await.unwrap().unwrap();
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn temporary_dynamic_child_panic_is_isolated() {
        // A panicking temporary, non-significant child is isolated exactly like an error exit.
        let sup = Supervisor::new("dyn-sup").unwrap();
        let handle = sup.handle();
        let (tx, run) = run_supervisor_with_trigger(sup).await;
        wait_running(&handle).await;

        handle
            .spawn(MockWorker::panicking("boom", Duration::from_millis(20)))
            .await
            .unwrap();
        wait_until(|| handle.active_children() == 0).await;

        sleep(Duration::from_millis(50)).await;
        assert!(handle.is_running(), "supervisor stays up after an isolated child panic");

        tx.send(()).unwrap();
        let result = timeout(Duration::from_secs(2), run).await.unwrap().unwrap();
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
        wait_running(&handle).await;

        handle
            .spawn_with(
                ChildSpecification::worker(MockWorker::failing("boom", Duration::from_millis(20)))
                    .with_restart_type(RestartType::Temporary)
                    .with_significant(true),
            )
            .await
            .unwrap();

        let result = timeout(Duration::from_secs(2), run).await.unwrap().unwrap();
        assert!(matches!(result, Err(SupervisorError::SignificantChildExited)));
    }

    #[tokio::test]
    async fn dynamic_spawn_fails_before_start_and_after_shutdown() {
        let sup = Supervisor::new("dyn-sup").unwrap();
        let handle = sup.handle();

        // Before the supervisor is running there's nothing to accept the spawn, so it's rejected outright (static
        // children should be configured up front via `add_worker` instead).
        assert!(!handle.is_running());
        let err = handle
            .spawn(MockWorker::long_running("before-start"))
            .await
            .unwrap_err();
        assert!(matches!(err, SpawnError::SupervisorGone));

        // Once it's running, spawns succeed.
        let (tx, run) = run_supervisor_with_trigger(sup).await;
        wait_running(&handle).await;
        let worker = MockWorker::long_running("after-start");
        let started = worker.start_count();
        handle.spawn(worker).await.unwrap();
        wait_until(|| started.load(Ordering::SeqCst) == 1).await;

        tx.send(()).unwrap();
        let result = timeout(Duration::from_secs(2), run).await.unwrap().unwrap();
        assert!(result.is_ok());

        // Once the supervisor has shut down, the run is gone and spawns are rejected again.
        wait_until(|| !handle.is_running()).await;
        let err = handle
            .spawn(MockWorker::long_running("after-shutdown"))
            .await
            .unwrap_err();
        assert!(matches!(err, SpawnError::SupervisorGone));
    }

    #[tokio::test]
    async fn dynamic_spawn_returns_after_registration() {
        let sup = Supervisor::new("dyn-sup").unwrap();
        let handle = sup.handle();
        let (tx, run) = run_supervisor_with_trigger(sup).await;
        wait_running(&handle).await;

        let worker = MockWorker::long_running("c");
        let started = worker.start_count();
        let id = handle.spawn(worker).await.unwrap();
        // No static children, so the first dynamic child takes id 0.
        assert_eq!(id.as_u64(), 0);
        wait_until(|| started.load(Ordering::SeqCst) == 1).await;

        tx.send(()).unwrap();
        let result = timeout(Duration::from_secs(2), run).await.unwrap().unwrap();
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn dynamic_spawn_rejects_invalid_child_name() {
        // While running, a spawn that fails registration (here, an empty/invalid child name) is reported as
        // `Rejected` with the underlying cause -- not `SupervisorGone`, which means the supervisor isn't running.
        let sup = Supervisor::new("dyn-sup").unwrap();
        let handle = sup.handle();
        let (tx, run) = run_supervisor_with_trigger(sup).await;
        wait_running(&handle).await;

        let err = handle.spawn(MockWorker::long_running("")).await.unwrap_err();
        assert!(matches!(err, SpawnError::Rejected { .. }), "got {err:?}");

        // The supervisor stays up and still accepts valid children.
        assert!(handle.is_running());
        handle.spawn(MockWorker::long_running("ok")).await.unwrap();
        wait_until(|| handle.active_children() == 1).await;

        tx.send(()).unwrap();
        let result = timeout(Duration::from_secs(2), run).await.unwrap().unwrap();
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn concurrent_shutdown_drains_many_children_quickly() {
        const CHILDREN: usize = 500;
        const SHUTDOWN_DELAY: Duration = Duration::from_millis(50);

        let sup = Supervisor::new("dyn-sup")
            .unwrap()
            .with_shutdown_mode(ShutdownMode::Concurrent);
        let handle = sup.handle();
        let (tx, run) = run_supervisor_with_trigger(sup).await;
        wait_running(&handle).await;

        for _ in 0..CHILDREN {
            handle
                .spawn(MockWorker::slow_shutdown("conn", SHUTDOWN_DELAY))
                .await
                .unwrap();
        }
        wait_until(|| handle.active_children() == CHILDREN).await;

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
        let sup = Supervisor::new("dyn-sup")
            .unwrap()
            .with_shutdown_mode(ShutdownMode::Concurrent);
        let handle = sup.handle();
        let (tx, run) = run_supervisor_with_trigger(sup).await;
        wait_running(&handle).await;

        handle.spawn(MockWorker::ignore_shutdown("stuck")).await.unwrap();
        wait_until(|| handle.active_children() == 1).await;

        // The child never reacts to shutdown, so it must be aborted once its graceful deadline (500ms) elapses rather
        // than hanging the supervisor.
        let start = std::time::Instant::now();
        tx.send(()).unwrap();
        let result = timeout(Duration::from_secs(2), run).await.unwrap().unwrap();
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
        let sup = Supervisor::new("dyn-sup")
            .unwrap()
            .with_shutdown_mode(ShutdownMode::Concurrent);
        let handle = sup.handle();
        let (tx, run) = run_supervisor_with_trigger(sup).await;
        wait_running(&handle).await;

        // Responds to shutdown promptly, but its deadline is effectively infinite.
        handle
            .spawn(MockWorker::long_running("responsive").with_graceful_timeout(Duration::MAX))
            .await
            .unwrap();
        // Never responds; must be aborted at its own short deadline.
        handle
            .spawn(MockWorker::ignore_shutdown("stuck").with_graceful_timeout(Duration::from_millis(200)))
            .await
            .unwrap();
        wait_until(|| handle.active_children() == 2).await;

        let start = std::time::Instant::now();
        tx.send(()).unwrap();
        let result = timeout(Duration::from_secs(2), run).await.unwrap().unwrap();
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
    async fn ordered_shutdown_aborts_unresponsive_child() {
        // Under the default `ShutdownMode::Ordered`, a child that never reacts to shutdown must be aborted once its
        // graceful deadline (500ms) elapses, rather than hanging the supervisor.
        let mut sup = Supervisor::new("test-sup").unwrap();
        sup.add_worker(MockWorker::ignore_shutdown("stuck"));

        let (tx, handle) = run_supervisor_with_trigger(sup).await;

        let start = std::time::Instant::now();
        tx.send(()).unwrap();
        let result = timeout(Duration::from_secs(2), handle).await.unwrap().unwrap();
        let elapsed = start.elapsed();

        assert!(
            matches!(result, Err(SupervisorError::ShutdownTimedOut { aborted: 1 })),
            "aborting a stuck child under ordered shutdown must surface as an unclean shutdown, got {result:?}"
        );
        assert!(
            elapsed < Duration::from_secs(1),
            "unresponsive child must be aborted at its deadline under ordered shutdown (took {elapsed:?})"
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
        let result = timeout(Duration::from_secs(2), handle).await.unwrap().unwrap();
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
        let mut child_sup = Supervisor::new("child-sup")
            .unwrap()
            .with_shutdown_mode(ShutdownMode::Concurrent);
        child_sup
            .add_worker(MockWorker::ignore_shutdown("child-stuck").with_graceful_timeout(Duration::from_millis(200)));

        let mut parent_sup = Supervisor::new("parent-sup")
            .unwrap()
            .with_shutdown_mode(ShutdownMode::Concurrent);
        parent_sup
            .add_worker(MockWorker::ignore_shutdown("parent-stuck").with_graceful_timeout(Duration::from_millis(200)));
        parent_sup.add_worker(MockWorker::long_running("parent-clean"));
        parent_sup.add_worker(child_sup);

        let (tx, handle) = run_supervisor_with_trigger(parent_sup).await;
        tx.send(()).unwrap();

        let result = timeout(Duration::from_secs(2), handle).await.unwrap().unwrap();
        assert!(
            matches!(result, Err(SupervisorError::ShutdownTimedOut { aborted: 2 })),
            "forced aborts must aggregate across the tree (1 direct + 1 nested), got {result:?}"
        );
    }
}
