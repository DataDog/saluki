use std::{future::Future, pin::Pin, sync::Arc, time::Duration};

use async_trait::async_trait;
use saluki_common::collections::FastIndexMap;
use saluki_common::sync::shutdown::{ShutdownCoordinator, ShutdownHandle};
use saluki_error::GenericError;
use snafu::{OptionExt as _, Snafu};
use tokio::{
    pin, select,
    task::{AbortHandle, Id, JoinSet},
};
use tracing::{debug, error, warn};

use super::{
    dedicated::{spawn_dedicated_runtime, RuntimeConfiguration, RuntimeMode},
    restart::{RestartAction, RestartMode, RestartState, RestartStrategy, RestartType},
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
type WorkerFuture = Pin<Box<dyn Future<Output = Result<(), WorkerError>> + Send>>;

/// Worker lifecycle errors.
///
/// Distinguishes between initialization failures (which shouldn't trigger restart logic) and runtime failures (which
/// are eligible for restart).
#[derive(Debug)]
enum WorkerError {
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

    /// The supervisor has no child processes.
    #[snafu(display("Supervisor has no child processes."))]
    NoChildren,

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
    restart_type: RestartType,
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
                restart_type: RestartType::Permanent,
            },
        }
    }

    /// Sets the restart policy for this worker.
    ///
    /// Defaults to [`RestartType::Permanent`].
    #[must_use]
    pub fn with_restart_type(mut self, restart_type: RestartType) -> Self {
        self.spec_inner.restart_type = restart_type;
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
/// This trait is implemented only for [`WorkerSpec`] and [`SupervisorSpec`], and is sealed: it cannot be implemented
/// outside of this crate. It exists so that [`Supervisor::add_worker`] can accept a [`ChildSpecification`] in either
/// state (as well as bare workers and supervisors) while lowering each into the supervisor's internal representation.
pub trait ChildState: sealed::Sealed + Sized {
    #[doc(hidden)]
    fn register(spec: ChildSpecification<Self>, supervisor: &mut Supervisor);
}

impl ChildState for WorkerSpec {
    fn register(spec: ChildSpecification<Self>, supervisor: &mut Supervisor) {
        supervisor.push_child(ChildEntry {
            child: SupervisedChild::Worker(spec.spec_inner.worker),
            restart: spec.spec_inner.restart_type,
        });
    }
}

impl ChildState for SupervisorSpec {
    fn register(spec: ChildSpecification<Self>, supervisor: &mut Supervisor) {
        supervisor.push_child(ChildEntry {
            child: SupervisedChild::Supervisor(spec.spec_inner.supervisor),
            restart: RestartType::Permanent,
        });
    }
}

/// The type-erased, runnable form of a child: either a worker or a nested supervisor.
///
/// This carries the behavior shared by both kinds of child -- creating the process and worker future, naming, and
/// shutdown strategy. Public [`ChildSpecification`]s are lowered into this type when registered via
/// [`ChildState::register`].
enum SupervisedChild {
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

    fn shutdown_strategy(&self) -> ShutdownStrategy {
        match self {
            Self::Worker(worker) => worker.shutdown_strategy(),

            // Supervisors should always be given as much time as necessary shutdown down gracefully to ensure that the
            // entire supervision subtree can be shutdown cleanly.
            Self::Supervisor(_) => ShutdownStrategy::Graceful(Duration::MAX),
        }
    }

    fn create_process(&self, parent_process: &Process) -> Result<Process, SupervisorError> {
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

    fn create_worker_future(
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

/// A registered child: its specification together with the restart policy chosen at registration time.
#[derive(Clone)]
struct ChildEntry {
    child: SupervisedChild,
    restart: RestartType,
}

/// Supervises a set of workers.
///
/// # Workers
///
/// All workers are defined through implementation of the [`Supervisable`] trait, which provides the logic for both
/// creating the underlying worker future that's spawned, as well as other metadata, such as the worker's name, how the
/// worker should be shutdown, and so on.
///
/// Supervisors can themselves be supervised by other supervisors, allowing _supervision trees_ to be constructed by
/// adding one supervisor as a child of another.
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
    runtime_mode: RuntimeMode,
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
            runtime_mode: RuntimeMode::default(),
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
            entry.child.process_type(),
            entry.child.name(),
            entry.restart,
        );
        self.child_specs.push(entry);
    }

    fn get_child_spec(&self, child_spec_idx: usize) -> &SupervisedChild {
        match self.child_specs.get(child_spec_idx) {
            Some(entry) => &entry.child,
            None => unreachable!("child spec index should never be out of bounds"),
        }
    }

    fn get_restart_type(&self, child_spec_idx: usize) -> RestartType {
        match self.child_specs.get(child_spec_idx) {
            Some(entry) => entry.restart,
            None => unreachable!("child spec index should never be out of bounds"),
        }
    }

    fn spawn_child(&self, child_spec_idx: usize, worker_state: &mut WorkerState) -> Result<(), SupervisorError> {
        let child_spec = self.get_child_spec(child_spec_idx);
        debug!(supervisor_id = %self.supervisor_id, "Spawning static child process #{} ({}).", child_spec_idx, child_spec.name());
        worker_state.add_worker(child_spec_idx, child_spec)
    }

    fn spawn_all_children(&self, worker_state: &mut WorkerState) -> Result<(), SupervisorError> {
        debug!(supervisor_id = %self.supervisor_id, "Spawning all static child processes.");
        for child_spec_idx in 0..self.child_specs.len() {
            self.spawn_child(child_spec_idx, worker_state)?;
        }

        Ok(())
    }

    /// Respawns children after a one-for-all restart, honoring each child's [`RestartType`].
    ///
    /// Every child except [`RestartType::Temporary`] is restarted, matching Erlang/OTP: a group restart restarts all
    /// permanent and transient children -- regardless of how they last exited, including a transient child that had
    /// already exited cleanly -- but never temporary children, which are shut down with the group and not brought back.
    /// A transient child's "restart only on abnormal exit" rule governs its _own_ termination, not a group restart
    /// driven by a sibling.
    fn respawn_children_one_for_all(&self, worker_state: &mut WorkerState) -> Result<(), SupervisorError> {
        debug!(supervisor_id = %self.supervisor_id, "Restarting all eligible static child processes.");
        for child_spec_idx in 0..self.child_specs.len() {
            if self.get_restart_type(child_spec_idx) != RestartType::Temporary {
                self.spawn_child(child_spec_idx, worker_state)?;
            }
        }

        Ok(())
    }

    async fn run_inner(&self, process: Process, process_shutdown: ShutdownHandle) -> Result<(), SupervisorError> {
        if self.child_specs.is_empty() {
            return Err(SupervisorError::NoChildren);
        }

        let mut restart_state = RestartState::new(self.restart_strategy);
        let mut worker_state = WorkerState::new(process);

        // Spawn all child processes. Since initialization is folded into each worker's task, this returns immediately
        // after spawning -- children initialize concurrently in the background.
        self.spawn_all_children(&mut worker_state)?;

        // Now we supervise.
        pin!(process_shutdown);

        loop {
            select! {
                // Shutdown has been triggered.
                //
                // Propagate shutdown to all child processes and wait for them to exit.
                _ = &mut process_shutdown => {
                    debug!(supervisor_id = %self.supervisor_id, "Shutdown triggered, shutting down all child processes.");
                    worker_state.shutdown_workers().await;
                    break;
                },
                (child_spec_idx, worker_result) = worker_state.wait_for_next_worker() => {
                    let child_spec = self.get_child_spec(child_spec_idx);

                    // Initialization failures are not eligible for restart -- they propagate immediately.
                    if let Err(WorkerError::Initialization { child_name, source }) = worker_result {
                        // If the error came from a nested supervisor, include the original child name
                        // to make the error chain more informative (e.g., "ctrl-pln/privileged-api").
                        let full_name = match child_name {
                            Some(inner) => format!("{}/{}", child_spec.name(), inner),
                            None => child_spec.name().to_string(),
                        };

                        error!(supervisor_id = %self.supervisor_id, worker_name = full_name, "Child process failed to initialize: {}", source);
                        worker_state.shutdown_workers().await;
                        return Err(SupervisorError::FailedToInitialize {
                            child_name: full_name,
                            source,
                        });
                    }

                    // A worker exited abnormally if it returned an error, panicked, or was aborted; a clean exit is
                    // `Ok(())`. Together with the worker's restart policy, this determines whether we restart it.
                    let abnormal = worker_result.is_err();
                    let restart_type = self.get_restart_type(child_spec_idx);

                    // Convert the worker result to a process error for restart evaluation / logging.
                    let worker_result = worker_result.map_err(|e| match e {
                        WorkerError::Runtime(e) => ProcessError::Terminated { source: e },
                        WorkerError::Initialization { .. } => unreachable!("handled above"),
                    });

                    if !restart_type.should_restart(abnormal) {
                        // The worker isn't eligible for restart given how it exited. It has already been removed from the
                        // worker map by `wait_for_next_worker`, so we simply continue supervising the rest. Crucially, we
                        // do NOT consult `evaluate_restart` here: non-restarts must not consume the restart-intensity
                        // budget, otherwise a steady stream of terminating temporary/transient children would eventually
                        // trip the supervisor's restart limit and tear it (and its siblings) down.
                        debug!(supervisor_id = %self.supervisor_id, worker_name = child_spec.name(), ?restart_type, ?worker_result, "Child process exited and is not eligible for restart.");
                    } else {
                        match restart_state.evaluate_restart() {
                            RestartAction::Restart(mode) => match mode {
                                RestartMode::OneForOne => {
                                    warn!(supervisor_id = %self.supervisor_id, worker_name = child_spec.name(), ?worker_result, "Child process terminated, restarting.");
                                    self.spawn_child(child_spec_idx, &mut worker_state)?;
                                }
                                RestartMode::OneForAll => {
                                    warn!(supervisor_id = %self.supervisor_id, worker_name = child_spec.name(), ?worker_result, "Child process terminated, restarting all processes.");
                                    worker_state.shutdown_workers().await;
                                    self.respawn_children_one_for_all(&mut worker_state)?;
                                }
                            },
                            RestartAction::Shutdown => {
                                error!(supervisor_id = %self.supervisor_id, worker_name = child_spec.name(), ?worker_result, "Supervisor shutting down due to restart limits.");
                                worker_state.shutdown_workers().await;
                                return Err(SupervisorError::Shutdown);
                            }
                        }
                    }
                }
            }
        }

        Ok(())
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
            runtime_mode: self.runtime_mode.clone(),
        }
    }
}

struct ProcessState {
    worker_id: usize,
    shutdown_strategy: ShutdownStrategy,
    shutdown_coordinator: ShutdownCoordinator,
    abort_handle: AbortHandle,
}

struct WorkerState {
    process: Process,
    worker_tasks: JoinSet<Result<(), WorkerError>>,
    worker_map: FastIndexMap<Id, ProcessState>,
}

impl WorkerState {
    fn new(process: Process) -> Self {
        Self {
            process,
            worker_tasks: JoinSet::new(),
            worker_map: FastIndexMap::default(),
        }
    }

    fn add_worker(&mut self, worker_id: usize, child_spec: &SupervisedChild) -> Result<(), SupervisorError> {
        let (shutdown_coordinator, shutdown_handle) = ShutdownHandle::paired();
        let process = child_spec.create_process(&self.process)?;
        let worker_future = child_spec.create_worker_future(process.clone(), shutdown_handle)?;
        let shutdown_strategy = child_spec.shutdown_strategy();
        let abort_handle = self.worker_tasks.spawn(worker_future.into_process_future(process));
        self.worker_map.insert(
            abort_handle.id(),
            ProcessState {
                worker_id,
                shutdown_strategy,
                shutdown_coordinator,
                abort_handle,
            },
        );
        Ok(())
    }

    async fn wait_for_next_worker(&mut self) -> (usize, Result<(), WorkerError>) {
        debug!("Waiting for next process to complete.");

        // If there are no workers to wait on, park indefinitely so the supervisor's select loop only proceeds via its
        // other arms (shutdown, or -- for dynamic supervisors -- a newly-added worker). Without this guard,
        // `join_next_with_id` would return `None` immediately on an empty set and the supervisor would busy-loop. The
        // set legitimately empties when all children are non-restartable (e.g. `RestartType::Temporary`) and have
        // exited.
        if self.worker_tasks.is_empty() {
            std::future::pending::<()>().await;
        }

        match self.worker_tasks.join_next_with_id().await {
            Some(Ok((worker_task_id, worker_result))) => {
                let process_state = self
                    .worker_map
                    .swap_remove(&worker_task_id)
                    .expect("worker task ID not found");
                (process_state.worker_id, worker_result)
            }
            Some(Err(e)) => {
                let worker_task_id = e.id();
                let process_state = self
                    .worker_map
                    .swap_remove(&worker_task_id)
                    .expect("worker task ID not found");
                let e = if e.is_cancelled() {
                    ProcessError::Aborted
                } else {
                    ProcessError::Panicked
                };
                (process_state.worker_id, Err(WorkerError::Runtime(e.into())))
            }
            None => unreachable!(
                "join set is non-empty here: we park above while empty, and only this method removes workers"
            ),
        }
    }

    async fn shutdown_workers(&mut self) {
        debug!("Shutting down all processes.");

        // Pop entries from the worker map, which grabs us workers in the reverse order they were added. This lets us
        // ensure we're shutting down any _dependent_ processes (processes which depend on previously-started processes)
        // first.
        //
        // For each entry, we trigger shutdown in whatever way necessary, and then wait for the process to exit by
        // driving the `JoinSet`. If other workers complete while we're waiting, we'll simply remove them from the
        // worker map and continue waiting for the current worker we're shutting down.
        //
        // We do this until the worker map is empty, at which point we can be sure that all processes have exited.
        while let Some((current_worker_task_id, process_state)) = self.worker_map.pop() {
            let ProcessState {
                worker_id,
                shutdown_strategy,
                shutdown_coordinator,
                abort_handle,
            } = process_state;

            // Trigger the process to shutdown based on the configured shutdown strategy.
            let shutdown_deadline = match shutdown_strategy {
                ShutdownStrategy::Graceful(timeout) => {
                    debug!(worker_id, shutdown_timeout = ?timeout, "Gracefully shutting down process.");
                    shutdown_coordinator.shutdown();

                    tokio::time::sleep(timeout)
                }
                ShutdownStrategy::Brutal => {
                    debug!(worker_id, "Forcefully aborting process.");
                    abort_handle.abort();

                    // We have to return a future that never resolves, since we're already aborting it. This is a little
                    // hacky but it's also difficult to do an optional future, so this is what we're going with for now.
                    tokio::time::sleep(Duration::MAX)
                }
            };
            pin!(shutdown_deadline);

            // Wait for the process to exit by driving the `JoinSet`. If other workers complete while we're waiting,
            // we'll simply remove them from the worker map and continue waiting.
            loop {
                select! {
                    worker_result = self.worker_tasks.join_next_with_id() => {
                        match worker_result {
                            Some(Ok((worker_task_id, _))) => {
                                if worker_task_id == current_worker_task_id {
                                    debug!(?worker_task_id, "Target process exited successfully.");
                                    break;
                                } else {
                                    debug!(?worker_task_id, "Non-target process exited successfully. Continuing to wait.");
                                    self.worker_map.swap_remove(&worker_task_id);
                                }
                            },
                            Some(Err(e)) => {
                                let worker_task_id = e.id();
                                if worker_task_id == current_worker_task_id {
                                    debug!(?worker_task_id, "Target process exited with error.");
                                    break;
                                } else {
                                    debug!(?worker_task_id, "Non-target process exited with error. Continuing to wait.");
                                    self.worker_map.swap_remove(&worker_task_id);
                                }
                            }
                            None => unreachable!("worker task must exist in join set if we are waiting for it"),
                        }
                    },
                    // We've exceeded the shutdown timeout, so we need to abort the process.
                    _ = &mut shutdown_deadline => {
                        debug!(worker_id, "Shutdown timeout expired, forcefully aborting process.");
                        abort_handle.abort();
                    }
                }
            }
        }

        debug_assert!(self.worker_map.is_empty(), "worker map should be empty after shutdown");
        debug_assert!(
            self.worker_tasks.is_empty(),
            "worker tasks should be empty after shutdown"
        );
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

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
    }

    /// A configurable mock worker for testing supervisor behavior.
    struct MockWorker {
        name: &'static str,
        init_behavior: InitBehavior,
        run_behavior: RunBehavior,
        start_count: Arc<AtomicUsize>,
    }

    impl MockWorker {
        /// Creates a worker that runs until shutdown.
        fn long_running(name: &'static str) -> Self {
            Self {
                name,
                init_behavior: InitBehavior::Instant,
                run_behavior: RunBehavior::UntilShutdown,
                start_count: Arc::new(AtomicUsize::new(0)),
            }
        }

        /// Creates a worker that fails after the given delay.
        fn failing(name: &'static str, delay: Duration) -> Self {
            Self {
                name,
                init_behavior: InitBehavior::Instant,
                run_behavior: RunBehavior::FailAfter(delay, "worker failed"),
                start_count: Arc::new(AtomicUsize::new(0)),
            }
        }

        /// Creates a worker that completes successfully after the given delay.
        fn completing(name: &'static str, delay: Duration) -> Self {
            Self {
                name,
                init_behavior: InitBehavior::Instant,
                run_behavior: RunBehavior::CompleteAfter(delay),
                start_count: Arc::new(AtomicUsize::new(0)),
            }
        }

        /// Creates a worker that fails during initialization.
        fn init_failure(name: &'static str) -> Self {
            Self {
                name,
                init_behavior: InitBehavior::Fail("init failed"),
                run_behavior: RunBehavior::UntilShutdown,
                start_count: Arc::new(AtomicUsize::new(0)),
            }
        }

        /// Creates a worker with slow initialization.
        fn slow_init(name: &'static str, init_delay: Duration) -> Self {
            Self {
                name,
                init_behavior: InitBehavior::Slow(init_delay),
                run_behavior: RunBehavior::UntilShutdown,
                start_count: Arc::new(AtomicUsize::new(0)),
            }
        }

        /// Returns a shared handle to the start count for this worker.
        fn start_count(&self) -> Arc<AtomicUsize> {
            Arc::clone(&self.start_count)
        }
    }

    #[async_trait]
    impl Supervisable for MockWorker {
        fn name(&self) -> &str {
            self.name
        }

        fn shutdown_strategy(&self) -> ShutdownStrategy {
            ShutdownStrategy::Graceful(Duration::from_millis(500))
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
    async fn supervisor_with_no_children_returns_error() {
        let mut sup = Supervisor::new("empty-sup").unwrap();

        let (tx, rx) = oneshot::channel::<()>();
        let result = sup.run_with_shutdown(rx).await;
        drop(tx);

        assert!(matches!(result, Err(SupervisorError::NoChildren)));
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
}
