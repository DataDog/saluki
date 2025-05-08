//! Asynchronous task supervision.

use std::{future::Future, pin::Pin, sync::Arc, time::Duration};

use saluki_common::collections::FastIndexMap;
use saluki_error::GenericError;
use snafu::Snafu;
use tokio::{
    pin, select,
    task::{AbortHandle, Id, JoinSet},
};
use tracing::{debug, error, warn};

use super::{
    restart::{RestartAction, RestartMode, RestartState, RestartStrategy},
    shutdown::{ProcessShutdown, ShutdownHandle},
};

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
pub enum SupervisorError {
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

/// Supervises a set of tasks called child processes.
///
/// `Supervisor` is the spiritual equivalent of `supervisor` in Erlang/OTP. To quote the Erlang documentation:
///
/// > The supervisor is responsible for starting, stopping, and monitoring its child processes. The basic idea
/// > of a supervisor is that it must keep its child processes alive by restarting them when necessary.
///
/// `Supervisor` supports a variety of configuration options, which allow for customizing the degree of failure that is
/// allowed, as well as how other child processes are affected by the failure of other child processes. `Supervisor` can
/// also be used to supervise other `Supervisor` instances, allowing the construction of a "supervision tree".
///
/// # Terminology
///
/// Erlang/OTP uses a number of specific terms that are either specific to supervisors or specific to the Erlang runtime
/// itself. Not all of these terms have directly equivalents in Rust or Tokio, so we've provided a mapping table below:
///
/// - **supervisor**: A `Supervisor` instance, which manages a set of child processes.
/// - **process**: An asynchronous task running on the Tokio runtime.
/// - **child process**: a worker process or a nested `Supervisor` instance being managed by a `Supervisor` instance.
/// - **worker**: Any process that is not itself a `Supervisor` instance.
/// - **termination**: The process of a child process exiting, either normally or abnormally.
///
/// # Behaviors and difference from Erlang/OTP
///
/// Supervisors in Erlang are heavily coupled to the Erlang runtime, which allows them to provide a vast amount of value
/// and capabilities without immense boilerplate on the part of the managed child processes. However, in Rust, and with
/// Tokio, we do not have this benefit and so we provide much of this functionality through explicit design patterns and
/// primitives.
pub struct Supervisor {
    supervisor_id: Arc<str>,
    child_specs: Vec<Arc<dyn Supervisable>>,
    restart_strategy: RestartStrategy,
}

impl Supervisor {
    /// Creates a new `Supervisor` with the default restart strategy and no children.
    pub fn new<S: AsRef<str>>(supervisor_id: S) -> Self {
        Self {
            supervisor_id: supervisor_id.as_ref().into(),
            child_specs: Vec::new(),
            restart_strategy: RestartStrategy::default(),
        }
    }

    /// Sets the restart strategy for the supervisor.
    pub fn with_restart_strategy(mut self, strategy: RestartStrategy) -> Self {
        self.restart_strategy = strategy;
        self
    }

    /// Adds a child process to the supervisor.
    pub fn add_worker<T: Supervisable + 'static>(&mut self, process: T) {
        debug!(supervisor_id = %self.supervisor_id, "Adding new static child process #{}.", self.child_specs.len());
        self.child_specs.push(Arc::new(process));
    }

    fn spawn_child(&self, child_spec_idx: usize, worker_state: &mut WorkerState) -> Result<(), SupervisorError> {
        let (process_shutdown, shutdown_handle) = ProcessShutdown::paired();
        match self.child_specs.get(child_spec_idx) {
            Some(child_spec) => match child_spec.initialize(process_shutdown) {
                Some(process) => {
                    debug!(supervisor_id = %self.supervisor_id, "Spawning static child process #{}.", child_spec_idx);
                    worker_state.add_worker(child_spec_idx, shutdown_handle, child_spec.shutdown_strategy(), process);
                }
                None => return Err(SupervisorError::FailedToInitialize),
            },
            None => unreachable!("child spec index should never be out of bounds"),
        }

        Ok(())
    }

    fn spawn_all_children(&self, worker_state: &mut WorkerState) -> Result<(), SupervisorError> {
        debug!(supervisor_id = %self.supervisor_id, "Spawning all static child processes.");
        for child_spec_idx in 0..self.child_specs.len() {
            self.spawn_child(child_spec_idx, worker_state)?;
        }

        Ok(())
    }

    async fn run_inner(&self, mut process_shutdown: ProcessShutdown) -> Result<(), SupervisorError> {
        if self.child_specs.is_empty() {
            return Err(SupervisorError::NoChildren);
        }

        let mut restart_state = RestartState::new(self.restart_strategy);
        let mut worker_state = WorkerState::new();

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
                    Some((child_spec_idx, worker_result)) =>  match restart_state.evaluate_restart() {
                        RestartAction::Restart(mode) => match mode {
                            RestartMode::OneForOne => {
                                warn!(supervisor_id = %self.supervisor_id, ?worker_result, "Child process terminated, restarting.");
                                self.spawn_child(child_spec_idx, &mut worker_state)?;
                            }
                            RestartMode::OneForAll => {
                                warn!(supervisor_id = %self.supervisor_id, ?worker_result, "Child process terminated, restarting all processes.");
                                worker_state.shutdown_workers().await;
                                self.spawn_all_children(&mut worker_state)?;
                            }
                        },
                        RestartAction::Shutdown => {
                            error!(supervisor_id = %self.supervisor_id, ?worker_result, "Supervisor shutting down due to restart limits.");
                            return Err(SupervisorError::Shutdown);
                        }
                    },
                    None => unreachable!("should not have empty worker joinset prior to shutdown"),
                }
            }
        }

        Ok(())
    }

    async fn run_nested(&self, process_shutdown: ProcessShutdown) -> Result<(), GenericError> {
        // Simple wrapper around `run_inner` to satisfy the return type signature needed when running the supervisor as
        // a nested child process in another supervisor.
        debug!(supervisor_id = %self.supervisor_id, "Nested supervisor starting.");
        self.run_inner(process_shutdown).await?;
        Ok(())
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

        debug!(supervisor_id = %self.supervisor_id, "Supervisor starting.");
        self.run_inner(process_shutdown).await
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

        debug!(supervisor_id = %self.supervisor_id, "Supervisor starting.");
        self.run_inner(process_shutdown).await
    }
}

impl Supervisable for Supervisor {
    fn shutdown_strategy(&self) -> ShutdownStrategy {
        // Supervisors should be given as much time as needed to shutdown.
        ShutdownStrategy::Graceful(Duration::MAX)
    }

    fn initialize(&self, process_shutdown: ProcessShutdown) -> Option<SupervisorFuture> {
        // Clone ourselves so that we can move a copy into the resulting future, allowing us to be fulfill the
        // `Send` requirement of the worker future.
        let supervisor = Supervisor {
            supervisor_id: self.supervisor_id.clone(),
            child_specs: self.child_specs.clone(),
            restart_strategy: self.restart_strategy,
        };

        let f = async move { supervisor.run_nested(process_shutdown).await };

        Some(Box::pin(f))
    }
}

struct ProcessState {
    worker_id: usize,
    shutdown_strategy: ShutdownStrategy,
    shutdown_handle: ShutdownHandle,
    abort_handle: AbortHandle,
}

struct WorkerState {
    worker_tasks: JoinSet<Result<(), GenericError>>,
    worker_map: FastIndexMap<Id, ProcessState>,
}

impl WorkerState {
    fn new() -> Self {
        Self {
            worker_tasks: JoinSet::new(),
            worker_map: FastIndexMap::default(),
        }
    }

    fn add_worker<F>(
        &mut self, worker_id: usize, shutdown_handle: ShutdownHandle, shutdown_strategy: ShutdownStrategy, f: F,
    ) where
        F: Future<Output = Result<(), GenericError>> + Send + 'static,
    {
        let abort_handle = self.worker_tasks.spawn(f);
        self.worker_map.insert(
            abort_handle.id(),
            ProcessState {
                worker_id,
                shutdown_strategy,
                shutdown_handle,
                abort_handle,
            },
        );
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
                    shutdown_handle.trigger().await;

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
