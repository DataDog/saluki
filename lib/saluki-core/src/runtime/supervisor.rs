//! Asynchronous task supervision.

use std::{collections::HashMap, future::Future, pin::Pin, sync::Arc};

use saluki_error::GenericError;
use snafu::Snafu;
use tokio::task::{AbortHandle, Id, JoinSet};
use tracing::{debug, error, warn};

use super::restart::{RestartAction, RestartMode, RestartState, RestartStrategy};

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
    /// Initialize a `Future` that represents the execution of the child process.
    ///
    /// When `Some` is returned, the process is spawned and managed by the supervisor. When `None` is returned, the
    /// process is considered to be permanently failed. This can be useful for supervised tasks that are not expected to
    /// ever fail, or cannot support restart, but should still be managed within the same supervision hierarchy as other
    /// tasks.
    fn initialize(&self) -> Option<Pin<Box<dyn Future<Output = Result<(), GenericError>> + Send>>>;
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

/// A supervision tree.
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
    pub fn add_process<T: Supervisable + 'static>(&mut self, process: T) {
        debug!(supervisor_id = %self.supervisor_id, "Adding new static child process #{}.", self.child_specs.len());
        self.child_specs.push(Arc::new(process));
    }

    fn spawn_child(&self, child_spec_idx: usize, process_state: &mut ProcessState) -> Result<(), SupervisorError> {
        match self.child_specs.get(child_spec_idx) {
            Some(child_spec) => match child_spec.initialize() {
                Some(process) => {
                    debug!(supervisor_id = %self.supervisor_id, "Spawning static child process #{}.", child_spec_idx);
                    process_state.add_process(child_spec_idx, process)
                }
                None => return Err(SupervisorError::FailedToInitialize),
            },
            None => unreachable!("child spec index should never be out of bounds"),
        }

        Ok(())
    }

    fn spawn_all_children(&self, process_state: &mut ProcessState) -> Result<(), SupervisorError> {
        debug!(supervisor_id = %self.supervisor_id, "Spawning all static child processes.");
        for child_spec_idx in 0..self.child_specs.len() {
            self.spawn_child(child_spec_idx, process_state)?;
        }

        Ok(())
    }

    async fn run_inner(&self) -> Result<(), SupervisorError> {
        let mut restart_state = RestartState::new(self.restart_strategy);
        let mut process_state = ProcessState::new();

        // Do the initial spawn of all child processes and supervisors.
        self.spawn_all_children(&mut process_state)?;

        // Now we supervise.
        while let Some((child_spec_idx, process_result)) = process_state.wait_for_next_process().await {
            // TODO: Erlang/OTP defaults to always trying to restart a process, even if it doesn't terminate due to a
            // legitimate failure. It does allow configuring this behavior on a per-process basis, however. We don't
            // support dynamically adding child processes, which is the only real use case I can think of for having
            // non-long-lived child processes... so I think for now, we're OK just always try to restart.
            match restart_state.evaluate_restart() {
                RestartAction::Restart(mode) => match mode {
                    RestartMode::OneForOne => {
                        warn!(supervisor_id = %self.supervisor_id, ?process_result, "Child process terminated, restarting.");
                        self.spawn_child(child_spec_idx, &mut process_state)?;
                    }
                    RestartMode::OneForAll => {
                        warn!(supervisor_id = %self.supervisor_id, ?process_result, "Child process terminated, restarting all processes.");
                        process_state.abort_all_processes().await;
                        self.spawn_all_children(&mut process_state)?;
                    }
                },
                RestartAction::Shutdown => {
                    error!(supervisor_id = %self.supervisor_id, ?process_result, "Supervisor shutting down due to restart limits.");
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
    process_tasks: JoinSet<Result<(), GenericError>>,
    process_map: HashMap<Id, (usize, AbortHandle)>,
}

impl ProcessState {
    fn new() -> Self {
        Self {
            process_tasks: JoinSet::new(),
            process_map: HashMap::new(),
        }
    }

    fn add_process<F>(&mut self, process_id: usize, f: F)
    where
        F: Future<Output = Result<(), GenericError>> + Send + 'static,
    {
        let handle = self.process_tasks.spawn(f);
        self.process_map.insert(handle.id(), (process_id, handle));
    }

    async fn wait_for_next_process(&mut self) -> Option<(usize, Result<(), ProcessError>)> {
        debug!("Waiting for next process to complete.");

        match self.process_tasks.join_next_with_id().await {
            Some(Ok((process_task_id, process_result))) => {
                let (process_id, _) = self
                    .process_map
                    .remove(&process_task_id)
                    .expect("process task ID not found");
                Some((
                    process_id,
                    process_result.map_err(|e| ProcessError::Terminated { source: e }),
                ))
            }
            Some(Err(e)) => {
                let process_task_id = e.id();
                let (process_id, _) = self
                    .process_map
                    .remove(&process_task_id)
                    .expect("process task ID not found");
                let e = if e.is_cancelled() {
                    ProcessError::Aborted
                } else {
                    ProcessError::Panicked
                };
                Some((process_id, Err(e)))
            }
            None => None,
        }
    }

    async fn abort_all_processes(&mut self) {
        // TODO: Abort/shutdown the processes in reverse order of how they were started.
        debug!("Aborting all processes.");

        // Clear the process map to reset our process state.
        self.process_map.clear();

        // Abort all of the process tasks, waiting for them to complete.
        self.process_tasks.shutdown().await;
    }
}
