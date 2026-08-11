//! Worker-tracking state for the supervisor.
//!
//! `WorkerState` owns the set of running child tasks for a [`Supervisor`](super::Supervisor) and provides the common
//! operations it needs: spawning a child, awaiting the next child to finish, and shutting all children down (either in
//! order or concurrently). It is deliberately agnostic about restart policy -- the supervisor decides what to do when a
//! worker exits.

use std::future::pending;
use std::time::Duration;

use saluki_common::collections::FastIndexMap;
use saluki_common::sync::shutdown::{ShutdownCoordinator, ShutdownHandle};
use saluki_common::task::TaskInstrument as _;
use tokio::{
    pin, select,
    task::{AbortHandle, Id, JoinSet},
};
use tracing::{debug, warn};

use super::process::{Process, ProcessExt as _};
use super::supervisor::{
    ChildConfig, ProcessError, ShutdownMode, ShutdownStrategy, SupervisedChild, SupervisorError, WorkerError,
};

/// Per-worker bookkeeping held by a [`WorkerState`].
struct ProcessState {
    /// Caller-assigned identifier for the worker.
    ///
    /// Opaque to `WorkerState`: the supervisor assigns each child a stable id from a monotonic counter. It is returned
    /// from [`WorkerState::wait_for_next_worker`] so the caller can correlate the exit with its own bookkeeping.
    worker_id: u64,
    /// Fully qualified process name, retained so shutdown can name precisely which worker had to be forcefully
    /// aborted.
    worker_name: String,
    shutdown_strategy: ShutdownStrategy,
    shutdown_coordinator: ShutdownCoordinator,
    abort_handle: AbortHandle,
}

/// Tracks the set of running child tasks for a supervisor.
pub(super) struct WorkerState {
    process: Process,
    shutdown_mode: ShutdownMode,
    /// Ceiling on the whole shutdown, if the supervisor configured one.
    ///
    /// Applied on top of each child's own strategy, so a child with no finite deadline of its own is still bounded,
    /// and one that has a shorter deadline still exits first.
    shutdown_budget: Option<Duration>,
    worker_tasks: JoinSet<Result<(), WorkerError>>,
    worker_map: FastIndexMap<Id, ProcessState>,
}

impl WorkerState {
    pub(super) fn new(process: Process, shutdown_mode: ShutdownMode, shutdown_budget: Option<Duration>) -> Self {
        Self {
            process,
            shutdown_mode,
            shutdown_budget,
            worker_tasks: JoinSet::new(),
            worker_map: FastIndexMap::default(),
        }
    }

    /// Spawns the child described by `child_spec`, tracking it under the given `worker_id`.
    ///
    /// `config` supplies the per-child overrides chosen at registration time: which runtime to spawn the child's task
    /// on, and whether to override the shutdown strategy the child reports for itself.
    pub(super) fn add_worker(
        &mut self, worker_id: u64, child_spec: &SupervisedChild, config: &ChildConfig,
    ) -> Result<(), SupervisorError> {
        let (shutdown_coordinator, shutdown_handle) = ShutdownHandle::paired();
        let process = child_spec.create_process(&self.process)?;
        let worker_name = process.name().to_string();
        let worker_future = child_spec.create_worker_future(process.clone(), shutdown_handle)?;
        let shutdown_strategy = config
            .shutdown_strategy()
            .unwrap_or_else(|| child_spec.shutdown_strategy());

        // Task instrumentation is keyed on the fully qualified process name, matching what `spawn_traced_named` would
        // have recorded for an unsupervised task. Names are per-process-name rather than per-task, so a supervisor with
        // many identically-named children (one per connection, say) still reports a single series.
        let task = worker_future
            .into_process_future(process)
            .with_task_instrumentation(worker_name.clone());
        let abort_handle = match config.runtime() {
            Some(handle) => self.worker_tasks.spawn_on(task, handle),
            None => self.worker_tasks.spawn(task),
        };
        self.worker_map.insert(
            abort_handle.id(),
            ProcessState {
                worker_id,
                worker_name,
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
            pending::<()>().await;
        }

        match self.worker_tasks.join_next_with_id().await {
            Some(Ok((worker_task_id, worker_result))) => {
                let process_state = self
                    .worker_map
                    .shift_remove(&worker_task_id)
                    .expect("worker task ID not found");
                (process_state.worker_id, worker_result)
            }
            Some(Err(e)) => {
                let worker_task_id = e.id();
                let process_state = self
                    .worker_map
                    .shift_remove(&worker_task_id)
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
    ///
    /// Returns the number of workers that had to be forcefully aborted because they exceeded their graceful shutdown
    /// timeout. The count includes aborts reported by nested child supervisors (which surface their own tally via
    /// [`WorkerError::ShutdownTimedOut`]), so the value reflects the entire supervision tree rooted at this supervisor.
    pub(super) async fn shutdown_workers(&mut self) -> usize {
        debug!(shutdown_mode = ?self.shutdown_mode, "Shutting down all processes.");

        let aborted = match self.shutdown_mode {
            ShutdownMode::Ordered => self.shutdown_workers_ordered().await,
            ShutdownMode::Concurrent => self.shutdown_workers_concurrent().await,
        };

        debug_assert!(self.worker_map.is_empty(), "worker map should be empty after shutdown");
        debug_assert!(
            self.worker_tasks.is_empty(),
            "worker tasks should be empty after shutdown"
        );

        aborted
    }

    /// Shuts down all workers one at a time, in reverse order of insertion, honoring each worker's shutdown strategy.
    ///
    /// Returns the number of workers forcefully aborted after exceeding their graceful timeout, including counts
    /// merged from nested child supervisors that timed out.
    async fn shutdown_workers_ordered(&mut self) -> usize {
        // Pop entries from the worker map, which grabs us workers in the reverse order they were added. This lets us
        // ensure we're shutting down any _dependent_ processes (processes which depend on previously-started processes)
        // first.
        //
        // For each entry, we trigger shutdown in whatever way necessary, and then wait for the process to exit by
        // driving the `JoinSet`. If other workers complete while we're waiting, we'll simply remove them from the
        // worker map and continue waiting for the current worker we're shutting down.
        //
        // We do this until the worker map is empty, at which point we can be sure that all processes have exited.
        //
        // A shutdown budget, if set, bounds the sequence as a whole rather than each worker in turn: it is measured
        // from the start of the drain, so the workers shut down later in the order inherit whatever is left of it.
        let budget_deadline = self.shutdown_budget.map(|budget| tokio::time::Instant::now() + budget);

        let mut aborted_total = 0;
        while let Some((current_worker_task_id, process_state)) = self.worker_map.pop() {
            let ProcessState {
                worker_id,
                worker_name,
                shutdown_strategy,
                shutdown_coordinator,
                abort_handle,
            } = process_state;

            // Trigger the process to shutdown based on the configured shutdown strategy.
            let shutdown_deadline = match shutdown_strategy {
                ShutdownStrategy::Graceful(timeout) => {
                    debug!(worker_id, shutdown_timeout = ?timeout, "Gracefully shutting down process.");
                    shutdown_coordinator.shutdown();

                    match resolve_abort_deadline(tokio::time::Instant::now(), timeout, budget_deadline) {
                        Some(deadline) => tokio::time::sleep_until(deadline),
                        // Nothing bounds this worker, so wait for it to exit on its own. `sleep` clamps an
                        // effectively-infinite duration internally, where `sleep_until` would overflow the instant.
                        None => tokio::time::sleep(Duration::MAX),
                    }
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
            let mut aborted = false;
            loop {
                select! {
                    worker_result = self.worker_tasks.join_next_with_id() => {
                        match worker_result {
                            Some(Ok((worker_task_id, output))) => {
                                // A nested child supervisor that timed out reports its own abort tally here; merge it.
                                aborted_total += reported_abort_count(&output);
                                if worker_task_id == current_worker_task_id {
                                    debug!(?worker_task_id, "Target process exited successfully.");
                                    break;
                                } else {
                                    debug!(?worker_task_id, "Non-target process exited successfully. Continuing to wait.");
                                    let removed = self.worker_map.shift_remove(&worker_task_id);
                                    debug_assert!(removed.is_some(), "non-target worker must be in the worker map");
                                }
                            },
                            Some(Err(e)) => {
                                let worker_task_id = e.id();
                                if worker_task_id == current_worker_task_id {
                                    debug!(?worker_task_id, "Target process exited with error.");
                                    break;
                                } else {
                                    debug!(?worker_task_id, "Non-target process exited with error. Continuing to wait.");
                                    let removed = self.worker_map.shift_remove(&worker_task_id);
                                    debug_assert!(removed.is_some(), "non-target worker must be in the worker map");
                                }
                            }
                            None => unreachable!("worker task must exist in join set if we are waiting for it"),
                        }
                    },
                    // We've exceeded the shutdown timeout, so we abort the process. The `if !aborted` guard stops this
                    // arm from re-firing on every poll once the deadline has elapsed (an elapsed `Sleep` stays ready),
                    // which would otherwise spin re-aborting until the task is reaped.
                    _ = &mut shutdown_deadline, if !aborted => {
                        warn!(worker_id, worker_name = %worker_name, "Worker ignored graceful shutdown; forcefully aborting after timeout.");
                        abort_handle.abort();
                        aborted = true;
                        aborted_total += 1;
                    }
                }
            }
        }

        aborted_total
    }

    /// Shuts down all workers at once, waiting for them concurrently.
    ///
    /// Each worker is signalled up front, then awaited concurrently under its **own** graceful deadline, so a worker
    /// that ignores shutdown is aborted at its configured timeout regardless of its siblings. Total shutdown time is
    /// therefore bounded by the slowest individual worker rather than the sum of all timeouts, which suits large,
    /// independent worker sets (for example, one task per network connection).
    ///
    /// A worker whose graceful timeout is effectively unbounded (such as a nested supervisor, which uses
    /// `Duration::MAX` because it bounds itself via its own children's deadlines) is waited on indefinitely and is
    /// never aborted by this method.
    ///
    /// Returns the number of workers forcefully aborted after exceeding their graceful timeout, including counts
    /// merged from nested child supervisors that timed out.
    async fn shutdown_workers_concurrent(&mut self) -> usize {
        // Take ownership of all worker bookkeeping so we can consume each worker's shutdown coordinator. Signal every
        // graceful worker and immediately abort brutal ones, recording a per-worker abort deadline so each is held to
        // its own timeout rather than a single shared one.
        let now = tokio::time::Instant::now();
        let budget_deadline = self.shutdown_budget.map(|budget| now + budget);
        let mut pending: FastIndexMap<Id, (u64, String, AbortHandle, Option<tokio::time::Instant>)> =
            FastIndexMap::default();
        for (task_id, process_state) in std::mem::take(&mut self.worker_map) {
            let ProcessState {
                worker_id,
                worker_name,
                shutdown_strategy,
                shutdown_coordinator,
                abort_handle,
            } = process_state;

            match shutdown_strategy {
                ShutdownStrategy::Graceful(timeout) => {
                    debug!(worker_id, shutdown_timeout = ?timeout, "Gracefully shutting down process.");
                    shutdown_coordinator.shutdown();
                    let deadline = resolve_abort_deadline(now, timeout, budget_deadline);
                    pending.insert(task_id, (worker_id, worker_name, abort_handle, deadline));
                }
                ShutdownStrategy::Brutal => {
                    debug!(worker_id, "Forcefully aborting process.");
                    abort_handle.abort();
                }
            }
        }

        // Wait for every task to exit. Each iteration sleeps until the earliest still-pending abort deadline; when it
        // fires we abort exactly those workers whose own deadline has passed (their tasks are then reaped by a later
        // `join_next`). Brutal workers were aborted above and aren't tracked here.
        let mut aborted_total = 0;
        while !self.worker_tasks.is_empty() {
            match pending.values().filter_map(|(_, _, _, deadline)| *deadline).min() {
                Some(deadline) => {
                    select! {
                        joined = self.worker_tasks.join_next_with_id() => {
                            let task_id = match joined {
                                Some(Ok((task_id, output))) => {
                                    // A nested child supervisor that timed out reports its abort tally here; merge it.
                                    aborted_total += reported_abort_count(&output);
                                    Some(task_id)
                                }
                                Some(Err(e)) => Some(e.id()),
                                None => None,
                            };
                            if let Some(task_id) = task_id {
                                pending.swap_remove(&task_id);
                            }
                        }
                        _ = tokio::time::sleep_until(deadline) => {
                            let now = tokio::time::Instant::now();
                            pending.retain(|_, (worker_id, worker_name, abort_handle, deadline)| {
                                if deadline.is_some_and(|deadline| deadline <= now) {
                                    warn!(worker_id = *worker_id, worker_name = %worker_name, "Worker ignored graceful shutdown; forcefully aborting after timeout.");
                                    abort_handle.abort();
                                    aborted_total += 1;
                                    false
                                } else {
                                    true
                                }
                            });
                        }
                    }
                }
                // Only workers with no finite deadline remain (e.g. nested supervisors); wait for them to exit on their
                // own. This is the path the topology takes -- each per-component supervisor is graceful-with-`MAX`, so
                // its own forced-abort tally is reported here and merged into ours.
                None => {
                    if let Some(Ok((_, output))) = self.worker_tasks.join_next_with_id().await {
                        aborted_total += reported_abort_count(&output);
                    }
                }
            }
        }

        aborted_total
    }
}

/// Resolves the instant at which a worker must be forcefully aborted, if it must be at all.
///
/// A timeout of `Duration::MAX` means the worker carries no deadline of its own. That's correct for a nested
/// supervisor, which bounds itself through its own children, and for the children of a supervisor that holds a
/// shutdown budget on their behalf. When both a worker deadline and a budget apply, whichever elapses first wins, so
/// the budget acts as a ceiling rather than an override.
fn resolve_abort_deadline(
    now: tokio::time::Instant, timeout: Duration, budget_deadline: Option<tokio::time::Instant>,
) -> Option<tokio::time::Instant> {
    let own_deadline = (timeout != Duration::MAX).then(|| now + timeout);
    match (own_deadline, budget_deadline) {
        (Some(own), Some(budget)) => Some(own.min(budget)),
        (deadline, None) | (None, deadline) => deadline,
    }
}

/// Extracts the number of force-aborts a reaped child reported.
///
/// A nested child supervisor that completes a requested shutdown after forcefully aborting one or more of its own
/// workers returns [`WorkerError::ShutdownTimedOut`]; its tally is merged into the parent's so the count aggregates
/// across the whole supervision tree. Any other completion (clean exit, our own abort surfacing as a cancellation,
/// panic) contributes nothing here.
fn reported_abort_count(output: &Result<(), WorkerError>) -> usize {
    match output {
        Err(WorkerError::ShutdownTimedOut { aborted }) => *aborted,
        // A `Supervisable` worker that internally drives a supervisor (such as a topology blueprint) flattens that
        // supervisor's `SupervisorError` into a `GenericError` at its boundary, so it surfaces here as `Runtime`
        // rather than `ShutdownTimedOut`. Recover the structured count via downcast so it still aggregates upward; the
        // concrete error type is preserved because the boundary converts with a plain `Into` (no added context).
        Err(WorkerError::Runtime(e)) => match e.downcast_ref::<SupervisorError>() {
            Some(SupervisorError::ShutdownTimedOut { aborted }) => *aborted,
            _ => 0,
        },
        _ => 0,
    }
}
