use std::{future::Future, pin::Pin, sync::Arc, time::Duration};

use saluki_common::collections::FastIndexMap;
use saluki_error::{ErrorContext as _, GenericError};
use snafu::{OptionExt as _, Snafu};
use tokio::{
    pin, select,
    task::{AbortHandle, Id, JoinSet},
};
use tracing::{debug, error, warn};

use super::{
    restart::{RestartAction, RestartMode, RestartState, RestartStrategy},
    shutdown::{ProcessShutdown, ShutdownHandle},
};
use crate::runtime::process::{Process, ProcessExt as _};

/// A `Future` that represents the execution of a supervised process.
pub type SupervisorFuture = Pin<Box<dyn Future<Output = Result<(), GenericError>> + Send>>;

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

/// Strategy for shutting down a process.
pub enum ShutdownStrategy {
    /// Waits for the configured duration for the process to exit, and then forcefully aborts it otherwise.
    Graceful(Duration),

    /// Forcefully aborts the process without waiting.
    Brutal,
}

/// A supervisable process.
pub trait Supervisable: Send + Sync {
    /// Returns the name of the process.
    fn name(&self) -> &str;

    /// Defines the shutdown strategy for the process.
    fn shutdown_strategy(&self) -> ShutdownStrategy {
        ShutdownStrategy::Graceful(Duration::from_secs(5))
    }

    /// Initialize a `Future` that represents the execution of the process.
    ///
    /// When `Some` is returned, the process is spawned and managed by the supervisor. When `None` is returned, the
    /// process is considered to be permanently failed. This can be useful for supervised tasks that are not expected to
    /// ever fail, or cannot support restart, but should still be managed within the same supervision hierarchy as other
    /// processes.
    fn initialize(&self, process_shutdown: ProcessShutdown) -> Option<SupervisorFuture>;
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
    #[snafu(display("Child process failed to initialize."))]
    FailedToInitialize,

    /// The supervisor exceeded its restart limits and was forced to shutdown.
    #[snafu(display("Supervisor has exceeded restart limits and was forced to shutdown."))]
    Shutdown,
}

/// A child process specification.
///
/// All workers added to a [`Supervisor`] must be specified as a `ChildSpecification`. This acts a template for how the
/// supervisor should create the underlying future that represents the process, as well as information about the
/// process, such as its name, shutdown strategy, and more.
///
/// A child process specification can be created implicitly from an existing [`Supervisor`], or any type that implements
/// [`Supervisable`].
pub enum ChildSpecification {
    Worker(Arc<dyn Supervisable>),
    Supervisor(Supervisor),
}

impl ChildSpecification {
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

    fn initialize(
        &self, parent_process: &Process, process_shutdown: ProcessShutdown,
    ) -> Result<Option<(Process, SupervisorFuture)>, SupervisorError> {
        match self {
            Self::Worker(worker) => {
                let process = Process::worker(worker.name(), parent_process).context(InvalidName {
                    name: worker.name().to_string(),
                })?;
                Ok(worker.initialize(process_shutdown).map(|future| (process, future)))
            }
            Self::Supervisor(sup) => {
                let process = Process::supervisor(&sup.supervisor_id, Some(parent_process)).context(InvalidName {
                    name: sup.supervisor_id.to_string(),
                })?;
                Ok(Some((
                    process.clone(),
                    sup.as_nested_process(process, process_shutdown),
                )))
            }
        }
    }
}

impl Clone for ChildSpecification {
    fn clone(&self) -> Self {
        match self {
            Self::Worker(worker) => Self::Worker(Arc::clone(worker)),
            Self::Supervisor(supervisor) => Self::Supervisor(supervisor.inner_clone()),
        }
    }
}

impl From<Supervisor> for ChildSpecification {
    fn from(supervisor: Supervisor) -> Self {
        Self::Supervisor(supervisor)
    }
}

impl<T> From<T> for ChildSpecification
where
    T: Supervisable + 'static,
{
    fn from(worker: T) -> Self {
        Self::Worker(Arc::new(worker))
    }
}

/// Supervises a set of workers.
///
/// # Workers
///
/// All workers are defined through implementation of the [`Supervisable`] trait, which provides the logic for both
/// creating the underlying worker future that is spawned, as well as other metadata, such as the worker's name, how the
/// worker should be shutdown, and so on.
///
/// Supervisors also (indirectly) implement the [`Supervisable`] trait, allowing them to be supervised by other
/// supervisors in order to construct _supervision trees_.
///
/// # Instrumentation
///
/// Supervisors automatically create their own allocation group
/// ([`TrackingAllocator`][memory_accounting::allocator::TrackingAllocator]), which is used to track both the memory usage of the
/// supervisor itself and its children. Additionally, individual worker processes are wrapped in a dedicated
/// [`tracing::Span`] to allow tracing the casual relationship between arbitrary code and the worker executing it.
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
    child_specs: Vec<ChildSpecification>,
    restart_strategy: RestartStrategy,
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
        })
    }

    /// Sets the restart strategy for the supervisor.
    pub fn with_restart_strategy(mut self, strategy: RestartStrategy) -> Self {
        self.restart_strategy = strategy;
        self
    }

    /// Adds a worker to the supervisor.
    ///
    /// A worker can be anything that implements the [`Supervisable`] trait. A [`Supervisor`] can also be added as a
    /// worker and managed in a nested fashion, known as a supervision tree.
    pub fn add_worker<T: Into<ChildSpecification>>(&mut self, process: T) {
        let child_spec = process.into();
        debug!(
            supervisor_id = %self.supervisor_id,
            "Adding new static child process #{}. ({}, {})",
            self.child_specs.len(),
            child_spec.process_type(),
            child_spec.name(),
        );
        self.child_specs.push(child_spec);
    }

    fn get_child_spec(&self, child_spec_idx: usize) -> &ChildSpecification {
        match self.child_specs.get(child_spec_idx) {
            Some(child_spec) => child_spec,
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

    async fn run_inner(&self, process: Process, mut process_shutdown: ProcessShutdown) -> Result<(), SupervisorError> {
        if self.child_specs.is_empty() {
            return Err(SupervisorError::NoChildren);
        }

        let mut restart_state = RestartState::new(self.restart_strategy);
        let mut worker_state = WorkerState::new(process);

        // Do the initial spawn of all child processes and supervisors.
        self.spawn_all_children(&mut worker_state)?;

        // Now we supervise.
        let shutdown = process_shutdown.wait_for_shutdown();
        pin!(shutdown);

        loop {
            select! {
                // Shutdown has been triggered.
                //
                // Propagate shutdown to all child processes and wait for them to exit.
                _ = &mut shutdown => {
                    debug!(supervisor_id = %self.supervisor_id, "Shutdown triggered, shutting down all child processes.");
                    worker_state.shutdown_workers().await;
                    break;
                },
                worker_task_result = worker_state.wait_for_next_worker() => match worker_task_result {
                    // TODO: Erlang/OTP defaults to always trying to restart a process, even if it doesn't terminate due to a
                    // legitimate failure. It does allow configuring this behavior on a per-process basis, however. We don't
                    // support dynamically adding child processes, which is the only real use case I can think of for having
                    // non-long-lived child processes... so I think for now, we're OK just always try to restart.
                    Some((child_spec_idx, worker_result)) =>  {
                        let child_spec = self.get_child_spec(child_spec_idx);
                        match restart_state.evaluate_restart() {
                            RestartAction::Restart(mode) => match mode {
                                RestartMode::OneForOne => {
                                    warn!(supervisor_id = %self.supervisor_id, worker_name = child_spec.name(), ?worker_result, "Child process terminated, restarting.");
                                    self.spawn_child(child_spec_idx, &mut worker_state)?;
                                }
                                RestartMode::OneForAll => {
                                    warn!(supervisor_id = %self.supervisor_id, worker_name = child_spec.name(), ?worker_result, "Child process terminated, restarting all processes.");
                                    worker_state.shutdown_workers().await;
                                    self.spawn_all_children(&mut worker_state)?;
                                }
                            },
                            RestartAction::Shutdown => {
                                error!(supervisor_id = %self.supervisor_id, worker_name = child_spec.name(), ?worker_result, "Supervisor shutting down due to restart limits.");
                                worker_state.shutdown_workers().await;
                                return Err(SupervisorError::Shutdown);
                            }
                        }
                    },
                    None => unreachable!("should not have empty worker joinset prior to shutdown"),
                }
            }
        }

        Ok(())
    }

    fn as_nested_process(&self, process: Process, process_shutdown: ProcessShutdown) -> SupervisorFuture {
        // Simple wrapper around `run_inner` to satisfy the return type signature needed when running the supervisor as
        // a nested child process in another supervisor.
        debug!(supervisor_id = %self.supervisor_id, "Nested supervisor starting.");

        // Create a standalone clone of ourselves so we can fulfill the future signature.
        let sup = self.inner_clone();

        Box::pin(async move {
            sup.run_inner(process, process_shutdown)
                .await
                .error_context("Nested supervisor failed to exit cleanly.")
        })
    }

    /// Runs the supervisor forever.
    ///
    /// # Errors
    ///
    /// If the supervisor exceeds its restart limits, or fails to initialize a child process, an error is returned.
    pub async fn run(&mut self) -> Result<(), SupervisorError> {
        // Create a no-op `ProcessShutdown` to satisfy the `run_inner` function. This is never used since we want to
        // run forever, but we need to satisfy the signature.
        let process_shutdown = ProcessShutdown::noop();
        let process = Process::supervisor(&self.supervisor_id, None).context(InvalidName {
            name: self.supervisor_id.to_string(),
        })?;

        debug!(supervisor_id = %self.supervisor_id, "Supervisor starting.");
        self.run_inner(process.clone(), process_shutdown)
            .into_instrumented(process)
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
        let process_shutdown = ProcessShutdown::wrapped(shutdown);
        let process = Process::supervisor(&self.supervisor_id, None).context(InvalidName {
            name: self.supervisor_id.to_string(),
        })?;

        debug!(supervisor_id = %self.supervisor_id, "Supervisor starting.");
        self.run_inner(process.clone(), process_shutdown)
            .into_instrumented(process)
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
        }
    }
}

struct ProcessState {
    worker_id: usize,
    shutdown_strategy: ShutdownStrategy,
    shutdown_handle: ShutdownHandle,
    abort_handle: AbortHandle,
}

struct WorkerState {
    process: Process,
    worker_tasks: JoinSet<Result<(), GenericError>>,
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

    fn add_worker(&mut self, worker_id: usize, child_spec: &ChildSpecification) -> Result<(), SupervisorError> {
        let (process_shutdown, shutdown_handle) = ProcessShutdown::paired();
        match child_spec.initialize(&self.process, process_shutdown)? {
            Some((process, worker)) => {
                let shutdown_strategy = child_spec.shutdown_strategy();

                let abort_handle = self.worker_tasks.spawn(worker.into_instrumented(process));
                self.worker_map.insert(
                    abort_handle.id(),
                    ProcessState {
                        worker_id,
                        shutdown_strategy,
                        shutdown_handle,
                        abort_handle,
                    },
                );

                Ok(())
            }
            None => Err(SupervisorError::FailedToInitialize),
        }
    }

    async fn wait_for_next_worker(&mut self) -> Option<(usize, Result<(), ProcessError>)> {
        debug!("Waiting for next process to complete.");

        match self.worker_tasks.join_next_with_id().await {
            Some(Ok((worker_task_id, worker_result))) => {
                let process_state = self
                    .worker_map
                    .swap_remove(&worker_task_id)
                    .expect("worker task ID not found");
                Some((
                    process_state.worker_id,
                    worker_result.map_err(|e| ProcessError::Terminated { source: e }),
                ))
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
                Some((process_state.worker_id, Err(e)))
            }
            None => None,
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
                shutdown_handle,
                abort_handle,
            } = process_state;

            // Trigger the process to shutdown based on the configured shutdown strategy.
            let shutdown_deadline = match shutdown_strategy {
                ShutdownStrategy::Graceful(timeout) => {
                    debug!(worker_id, shutdown_timeout = ?timeout, "Gracefully shutting down process.");
                    shutdown_handle.trigger();

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
