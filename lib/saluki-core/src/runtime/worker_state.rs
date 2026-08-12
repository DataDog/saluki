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
    /// Whether this child is subject to the supervisor's shutdown budget.
    ///
    /// False for a nested supervisor, which bounds itself through its own children. Aborting one would cut its subtree
    /// off mid-drain, and -- for a supervisor on a dedicated runtime, whose work lives on another OS thread -- would
    /// not even stop it, while still reporting it as stopped.
    budget_applies: bool,
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

        // Every worker's task is timed, keyed on its fully qualified process name -- the same name
        // `spawn_traced_named` would have recorded for an equivalent standalone task. A worker is a top-level task, so
        // it is polled once per wake-up rather than once per unit of work, which keeps the two clock reads per poll well
        // amortized against whatever the poll actually does.
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
                budget_applies: !child_spec.is_supervisor(),
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
        let budget_deadline = resolve_budget_deadline(tokio::time::Instant::now(), self.shutdown_budget);

        let mut aborted_total = 0;
        while let Some((current_worker_task_id, process_state)) = self.worker_map.pop() {
            let ProcessState {
                worker_id,
                worker_name,
                shutdown_strategy,
                budget_applies,
                shutdown_coordinator,
                abort_handle,
            } = process_state;

            let budget_deadline = budget_applies.then_some(budget_deadline).flatten();

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
        let budget_deadline = resolve_budget_deadline(now, self.shutdown_budget);
        let mut pending: FastIndexMap<Id, (u64, String, AbortHandle, Option<tokio::time::Instant>)> =
            FastIndexMap::default();
        for (task_id, process_state) in std::mem::take(&mut self.worker_map) {
            let ProcessState {
                worker_id,
                worker_name,
                shutdown_strategy,
                budget_applies,
                shutdown_coordinator,
                abort_handle,
            } = process_state;

            match shutdown_strategy {
                ShutdownStrategy::Graceful(timeout) => {
                    debug!(worker_id, shutdown_timeout = ?timeout, "Gracefully shutting down process.");
                    shutdown_coordinator.shutdown();
                    let budget_deadline = budget_applies.then_some(budget_deadline).flatten();
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
    // `checked_add` rather than `+`: adding a large duration to an instant panics on overflow, and both the sentinel
    // (`Duration::MAX`) and any duration near it are reachable from caller-supplied configuration. A deadline too far
    // out to represent is indistinguishable from no deadline at all, so both become `None`.
    let own_deadline = now.checked_add(timeout);
    match (own_deadline, budget_deadline) {
        (Some(own), Some(budget)) => Some(own.min(budget)),
        (deadline, None) | (None, deadline) => deadline,
    }
}

/// Resolves the instant at which a supervisor's whole shutdown must be cut off, if it configured a budget.
///
/// Overflows the same way as [`resolve_abort_deadline`]: a budget too large to represent is treated as no budget.
fn resolve_budget_deadline(now: tokio::time::Instant, budget: Option<Duration>) -> Option<tokio::time::Instant> {
    budget.and_then(|budget| now.checked_add(budget))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn instant() -> tokio::time::Instant {
        tokio::time::Instant::now()
    }

    #[tokio::test]
    async fn abort_deadline_uses_the_workers_own_timeout_when_unbudgeted() {
        let now = instant();
        assert_eq!(
            resolve_abort_deadline(now, Duration::from_secs(3), None),
            Some(now + Duration::from_secs(3))
        );
    }

    #[tokio::test]
    async fn abort_deadline_is_none_for_an_unbounded_worker_without_a_budget() {
        // `Duration::MAX` is the "no deadline of its own" sentinel: a nested supervisor, or a child whose supervisor
        // holds the budget. With no budget in play there is nothing to bound it.
        assert_eq!(resolve_abort_deadline(instant(), Duration::MAX, None), None);
    }

    #[tokio::test]
    async fn abort_deadline_falls_back_to_the_budget_for_an_unbounded_worker() {
        let now = instant();
        let budget = now + Duration::from_secs(10);
        assert_eq!(resolve_abort_deadline(now, Duration::MAX, Some(budget)), Some(budget));
    }

    #[tokio::test]
    async fn abort_deadline_takes_whichever_of_worker_and_budget_is_sooner() {
        let now = instant();
        let budget = now + Duration::from_secs(10);

        // Worker sooner than the budget.
        assert_eq!(
            resolve_abort_deadline(now, Duration::from_secs(2), Some(budget)),
            Some(now + Duration::from_secs(2))
        );

        // Budget sooner than the worker: a worker cannot buy itself more time than its supervisor allows.
        assert_eq!(
            resolve_abort_deadline(now, Duration::from_secs(30), Some(budget)),
            Some(budget)
        );
    }

    #[tokio::test]
    async fn abort_deadline_does_not_overflow_on_a_near_max_timeout() {
        // Only `Duration::MAX` exactly used to be special-cased, so anything just under it panicked when added to an
        // instant. A deadline too far out to represent is treated as no deadline.
        let now = instant();
        assert_eq!(
            resolve_abort_deadline(now, Duration::MAX - Duration::from_nanos(1), None),
            None
        );

        // Even then, a budget still bounds the worker.
        let budget = now + Duration::from_secs(5);
        assert_eq!(
            resolve_abort_deadline(now, Duration::MAX - Duration::from_nanos(1), Some(budget)),
            Some(budget)
        );
    }

    #[tokio::test]
    async fn budget_deadline_is_overflow_safe() {
        let now = instant();
        assert_eq!(resolve_budget_deadline(now, None), None);
        assert_eq!(
            resolve_budget_deadline(now, Some(Duration::from_secs(7))),
            Some(now + Duration::from_secs(7))
        );

        // `Duration::MAX` is the natural spelling of "no ceiling", and used to panic the supervisor task.
        assert_eq!(resolve_budget_deadline(now, Some(Duration::MAX)), None);
    }
}
