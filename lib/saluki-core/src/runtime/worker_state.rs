//! Shared worker-tracking state for supervisors.
//!
//! `WorkerState` owns the set of running child tasks for a supervisor and provides the common operations that both the
//! static `Supervisor` and the `DynamicSupervisor` need: spawning a child, awaiting the next child to finish, and
//! shutting all children down. It is deliberately agnostic about restart policy -- callers decide what to do when a
//! worker exits.

use std::time::Duration;

use saluki_common::collections::FastIndexMap;
use saluki_common::sync::shutdown::{ShutdownCoordinator, ShutdownHandle};
use tokio::{
    pin, select,
    task::{AbortHandle, Id, JoinSet},
};
use tracing::debug;

use super::process::{Process, ProcessExt as _};
use super::supervisor::{ProcessError, ShutdownStrategy, SupervisedChild, SupervisorError, WorkerError};

/// Per-worker bookkeeping held by a [`WorkerState`].
struct ProcessState {
    /// Caller-assigned identifier for the worker.
    ///
    /// Opaque to `WorkerState`: the static supervisor uses the child-spec index, while the dynamic supervisor uses a
    /// monotonic counter. It is returned from [`WorkerState::wait_for_next_worker`] so the caller can correlate the
    /// exit with its own bookkeeping.
    worker_id: u64,
    shutdown_strategy: ShutdownStrategy,
    shutdown_coordinator: ShutdownCoordinator,
    abort_handle: AbortHandle,
}

/// Tracks the set of running child tasks for a supervisor.
pub(super) struct WorkerState {
    process: Process,
    worker_tasks: JoinSet<Result<(), WorkerError>>,
    worker_map: FastIndexMap<Id, ProcessState>,
}

impl WorkerState {
    pub(super) fn new(process: Process) -> Self {
        Self {
            process,
            worker_tasks: JoinSet::new(),
            worker_map: FastIndexMap::default(),
        }
    }

    /// Spawns the child described by `child_spec`, tracking it under the given `worker_id`.
    pub(super) fn add_worker(&mut self, worker_id: u64, child_spec: &SupervisedChild) -> Result<(), SupervisorError> {
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

    /// Awaits the next worker to finish, returning its `worker_id` and result.
    pub(super) async fn wait_for_next_worker(&mut self) -> (u64, Result<(), WorkerError>) {
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

    /// Shuts down all workers, in reverse order of insertion, honoring each worker's shutdown strategy.
    pub(super) async fn shutdown_workers(&mut self) {
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
