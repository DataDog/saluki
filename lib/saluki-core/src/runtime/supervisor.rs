//! Asynchronous task supervision.

use std::{collections::HashMap, future::Future, pin::Pin, sync::Arc};

use saluki_common::collections::FastIndexMap;
use saluki_error::GenericError;
use snafu::Snafu;
use tokio::task::{AbortHandle, Id, JoinSet};
use tracing::{debug, error, warn};

use super::{restart::{RestartAction, RestartMode, RestartState, RestartStrategy}, shutdown::{ProcessShutdown, ShutdownHandle}};

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

/// A supervisable process.
pub trait Supervisable: Send + Sync {
    /// Initialize a `Future` that represents the execution of the process.
    ///
    /// When `Some` is returned, the process is spawned and managed by the supervisor. When `None` is returned, the
    /// process is considered to be permanently failed. This can be useful for supervised tasks that are not expected to
    /// ever fail, or cannot support restart, but should still be managed within the same supervision hierarchy as other
    /// processes.
    fn initialize(&self, process_shutdown: ProcessShutdown) -> Option<Pin<Box<dyn Future<Output = Result<(), GenericError>> + Send>>>;
}

/// Supervisor errors.
#[derive(Debug, Snafu)]
pub enum SupervisorError {
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
        let (shutdown_handle, process_shutdown) = ProcessShutdown::new();
        match self.child_specs.get(child_spec_idx) {
            Some(child_spec) => match child_spec.initialize(process_shutdown) {
                Some(process) => {
                    debug!(supervisor_id = %self.supervisor_id, "Spawning static child process #{}.", child_spec_idx);
                    worker_state.add_worker(child_spec_idx, shutdown_handle, process);
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

    async fn run_inner(&self) -> Result<(), SupervisorError> {
        let mut restart_state = RestartState::new(self.restart_strategy);
        let mut worker_state = WorkerState::new();

        // Do the initial spawn of all child processes and supervisors.
        self.spawn_all_children(&mut worker_state)?;

        // Now we supervise.
        while let Some((child_spec_idx, worker_result)) = worker_state.wait_for_next_worker().await {
            // TODO: Erlang/OTP defaults to always trying to restart a process, even if it doesn't terminate due to a
            // legitimate failure. It does allow configuring this behavior on a per-process basis, however. We don't
            // support dynamically adding child processes, which is the only real use case I can think of for having
            // non-long-lived child processes... so I think for now, we're OK just always try to restart.
            match restart_state.evaluate_restart() {
                RestartAction::Restart(mode) => match mode {
                    RestartMode::OneForOne => {
                        warn!(supervisor_id = %self.supervisor_id, ?worker_result, "Child process terminated, restarting.");
                        self.spawn_child(child_spec_idx, &mut worker_state)?;
                    }
                    RestartMode::OneForAll => {
                        warn!(supervisor_id = %self.supervisor_id, ?worker_result, "Child process terminated, restarting all processes.");
                        worker_state.abort_all_workers().await;
                        self.spawn_all_children(&mut worker_state)?;
                    }
                },
                RestartAction::Shutdown => {
                    error!(supervisor_id = %self.supervisor_id, ?worker_result, "Supervisor shutting down due to restart limits.");
                    return Err(SupervisorError::Shutdown);
                }
            }
        }

        Ok(())
    }

    async fn run_nested(&self) -> Result<(), GenericError> {
        // Simple wrapper around `run_inner` to satisfy the return type signature needed when running the supervisor as
        // a nested child process in another supervisor.
        debug!(supervisor_id = %self.supervisor_id, "Nested supervisor starting.");
        self.run_inner().await?;
        Ok(())
    }

    /// Runs the supervisor.
    ///
    /// # Errors
    ///
    /// If the supervisor exceeds its restart limits, or fails to initialize a child process, an error is returned.
    pub async fn run(&mut self) -> Result<(), SupervisorError> {
        debug!(supervisor_id = %self.supervisor_id, "Supervisor starting.");
        self.run_inner().await
    }
}

impl Supervisable for Supervisor {
    fn initialize(&self) -> Option<Pin<Box<dyn Future<Output = Result<(), GenericError>> + Send>>> {
        // Clone ourselves so that we can move a copy into the resulting future, allowing us to be fulfill the
        // `Send` requirement of the worker future.
        let supervisor = Supervisor {
            supervisor_id: self.supervisor_id.clone(),
            child_specs: self.child_specs.clone(),
            restart_strategy: self.restart_strategy,
        };

        let f = async move { supervisor.run_nested().await };

        Some(Box::pin(f))
    }
}

struct ProcessState {
    worker_id: usize,
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
            worker_map: FastIndexMap::new(),
        }
    }

    fn add_worker<F>(&mut self, worker_id: usize, shutdown_handle: ShutdownHandle, f: F)
    where
        F: Future<Output = Result<(), GenericError>> + Send + 'static,
    {
        let abort_handle = self.worker_tasks.spawn(f);
        self.worker_map.insert(handle.id(), ProcessState {
            worker_id,
            shutdown_handle,
            abort_handle,
        });
    }

    async fn wait_for_next_worker(&mut self) -> Option<(usize, Result<(), ProcessError>)> {
        debug!("Waiting for next process to complete.");

        match self.worker_tasks.join_next_with_id().await {
            Some(Ok((worker_task_id, worker_result))) => {
                let process_state = self
                    .worker_map
                    .remove(&worker_task_id)
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
                    .remove(&worker_task_id)
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

    async fn abort_all_workers(&mut self) {
        debug!("Aborting all processes.");

        // TODO: Pop entries from the worker map, which gets us each process in the reverse order they were added.
        //
        // For each entry, we try to:
        // - trigger shutdown and then gracefully wait for the process to exit if configured to do so
        // - abort the process if "brutal kill" is configured or if we exceed the shutdown timeout
        //
        // When gracefully waiting for the process to exit, we have to drive the `JoinSet` to completion until we get
        // back the worker ID task for the process we just triggered to shutdown. If we get an unrelated process ID, we
        // simply remove it from the worker map and continue waiting.
        //
        // This is a bit ugly, but otherwise we'd have to go through the normal process once we reach that worker in our
        // iteration: trigger shutdown (or abort), and then wait for the process to exit by driving the `JoinSet`. We'd
        // be waiting for a process to finish that wasn't part of the `JoinSet` anymore, though, so... we gotta handle
        // it when it comes.
        //
        // This should leave us work a worker map and `JoinSet` that are empty by the time we break out of our loop.
        todo!()
    }
}
