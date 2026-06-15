//! Worker-tracking state for the supervisor.
//!
//! `WorkerState` owns the set of running child tasks for a [`Supervisor`](super::Supervisor) and provides the common
//! operations it needs: spawning a child, awaiting the next child to finish, and shutting all children down (either in
//! order or concurrently). It is deliberately agnostic about restart policy -- the supervisor decides what to do when a
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
use super::supervisor::{ProcessError, ShutdownMode, ShutdownStrategy, SupervisedChild, SupervisorError, WorkerError};

/// Per-worker bookkeeping held by a [`WorkerState`].
struct ProcessState {
    /// Caller-assigned identifier for the worker.
    ///
    /// Opaque to `WorkerState`: the supervisor assigns each child a stable id from a monotonic counter. It is returned
    /// from [`WorkerState::wait_for_next_worker`] so the caller can correlate the exit with its own bookkeeping.
    worker_id: u64,
    shutdown_strategy: ShutdownStrategy,
    shutdown_coordinator: ShutdownCoordinator,
    abort_handle: AbortHandle,
}

/// Tracks the set of running child tasks for a supervisor.
pub(super) struct WorkerState {
    process: Process,
    shutdown_mode: ShutdownMode,
    worker_tasks: JoinSet<Result<(), WorkerError>>,
    worker_map: FastIndexMap<Id, ProcessState>,
}

impl WorkerState {
    pub(super) fn new(process: Process, shutdown_mode: ShutdownMode) -> Self {
        Self {
            process,
            shutdown_mode,
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
        // other arms (shutdown, or a newly-added dynamic child). Without this guard, `join_next_with_id` would return
        // `None` immediately on an empty set and the supervisor would busy-loop. The set legitimately empties when all
        // children are non-restartable (e.g. `RestartType::Temporary`) and have exited.
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

    /// Shuts down all workers, honoring each worker's shutdown strategy and the configured [`ShutdownMode`].
    pub(super) async fn shutdown_workers(&mut self) {
        debug!(shutdown_mode = ?self.shutdown_mode, "Shutting down all processes.");

        match self.shutdown_mode {
            ShutdownMode::Ordered => self.shutdown_workers_ordered().await,
            ShutdownMode::Concurrent => self.shutdown_workers_concurrent().await,
        }

        debug_assert!(self.worker_map.is_empty(), "worker map should be empty after shutdown");
        debug_assert!(
            self.worker_tasks.is_empty(),
            "worker tasks should be empty after shutdown"
        );
    }

    /// Shuts down all workers one at a time, in reverse order of insertion, honoring each worker's shutdown strategy.
    async fn shutdown_workers_ordered(&mut self) {
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
    }

    /// Shuts down all workers at once, waiting for them concurrently.
    ///
    /// Each worker is signalled up front, then we wait for them all under a single deadline -- the longest graceful
    /// timeout among them -- so total shutdown time is bounded by the slowest worker rather than the sum of all
    /// timeouts. This suits large, independent worker sets (for example, one task per network connection).
    async fn shutdown_workers_concurrent(&mut self) {
        // Take ownership of all worker bookkeeping so we can consume each worker's shutdown coordinator. Signal every
        // worker up front, tracking the longest graceful timeout as a single shared deadline.
        let mut deadline = Duration::ZERO;
        let mut has_graceful = false;
        for (_, process_state) in std::mem::take(&mut self.worker_map) {
            let ProcessState {
                worker_id,
                shutdown_strategy,
                shutdown_coordinator,
                abort_handle,
            } = process_state;

            match shutdown_strategy {
                ShutdownStrategy::Graceful(timeout) => {
                    debug!(worker_id, shutdown_timeout = ?timeout, "Gracefully shutting down process.");
                    shutdown_coordinator.shutdown();
                    has_graceful = true;
                    deadline = deadline.max(timeout);
                }
                ShutdownStrategy::Brutal => {
                    debug!(worker_id, "Forcefully aborting process.");
                    abort_handle.abort();
                }
            }
        }

        // Wait for every task to exit. If the deadline expires first, abort whatever remains and reap it. If nothing
        // asked for a graceful period (everything was aborted), there's no timeout to honor.
        let shutdown_deadline = tokio::time::sleep(if has_graceful { deadline } else { Duration::MAX });
        pin!(shutdown_deadline);

        let mut aborted = false;
        while !self.worker_tasks.is_empty() {
            if aborted {
                // The deadline has already passed and everything is aborted; just reap the remaining tasks.
                let _ = self.worker_tasks.join_next_with_id().await;
                continue;
            }

            select! {
                _ = self.worker_tasks.join_next_with_id() => {}
                _ = &mut shutdown_deadline => {
                    debug!("Shutdown timeout expired, forcefully aborting all remaining processes.");
                    self.worker_tasks.abort_all();
                    aborted = true;
                }
            }
        }
    }
}
