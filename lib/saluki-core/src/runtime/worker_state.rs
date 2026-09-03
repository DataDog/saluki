//! Worker-tracking state for the supervisor.
//!
//! `WorkerState` owns the set of running child tasks for a [`Supervisor`](super::Supervisor) and provides the common
//! operations it needs: spawning a child, awaiting the next child to finish, and shutting all children down (either in
//! order or concurrently). It is deliberately agnostic about restart policy -- the supervisor decides what to do when a
//! worker exits.

use std::future::pending;
use std::sync::Arc;
use std::time::Duration;

use saluki_common::collections::FastIndexMap;
use saluki_common::sync::shutdown::{ShutdownCoordinator, ShutdownHandle};
use saluki_common::task::TaskInstrument as _;
use tokio::{
    select,
    task::{AbortHandle, Id, JoinSet},
};
use tracing::{debug, warn};

use super::process::{Process, ProcessExt as _};
use super::spawn::CURRENT_SUPERVISOR;
use super::supervisor::{
    ChildConfig, ChildShutdown, ProcessError, ShutdownStrategy, SupervisedChild, SupervisorError, SupervisorHandle,
    WorkerError,
};
use super::tree::{StartedChild, TreeSlot, CURRENT_TREE_SLOT};

/// Per-worker bookkeeping held by a [`WorkerState`].
struct ProcessState {
    /// Caller-assigned identifier for the worker.
    ///
    /// Opaque to `WorkerState`: the supervisor assigns each child a stable id from a monotonic counter. It is returned
    /// from [`WorkerState::wait_for_next_worker`] so the caller can correlate the exit with its own bookkeeping.
    worker_id: u64,
    /// Fully qualified process name, retained so shutdown can name precisely which worker had to be forcefully
    /// aborted.
    worker_name: Arc<str>,
    shutdown_strategy: ShutdownStrategy,
    /// Whether this child is subject to the supervisor's shutdown budget.
    ///
    /// False for a nested supervisor, which bounds itself through its own children. Aborting one would cut its subtree
    /// off mid-drain, and -- for a supervisor on a dedicated runtime, whose work lives on another OS thread -- would
    /// not even stop it, while still reporting it as stopped.
    budget_applies: bool,
    /// Coordinator for signalling this worker, if it observes the signal at all.
    ///
    /// `None` for a worker that reports [`wants_shutdown_signal`][super::Supervisable::wants_shutdown_signal] as
    /// false -- a closure-based worker, typically -- which was handed a [`ShutdownHandle::noop`] and would never see
    /// anything we fired. Only the `Graceful` shutdown paths read this, and only a worker that wanted the signal can
    /// reach them with anything to signal.
    shutdown_coordinator: Option<ShutdownCoordinator>,
    abort_handle: AbortHandle,
}

/// Tracks the set of running child tasks for a supervisor.
pub(super) struct WorkerState {
    process: Process,
    /// Handle to the supervisor these workers belong to.
    ///
    /// Installed as the ambient supervisor for every worker task, so anything a worker spawns through
    /// [`spawn`][crate::runtime::spawn] becomes a sibling of that worker rather than needing a handle threaded to it.
    handle: SupervisorHandle,
    /// Ceiling on the whole shutdown, if the supervisor configured one.
    ///
    /// Applied on top of each child's own strategy, so a child with no finite deadline of its own is still bounded,
    /// and one that has a shorter deadline still exits first.
    shutdown_budget: Option<Duration>,
    worker_tasks: JoinSet<Result<(), WorkerError>>,
    worker_map: FastIndexMap<Id, ProcessState>,
}

impl WorkerState {
    pub(super) fn new(process: Process, handle: SupervisorHandle, shutdown_budget: Option<Duration>) -> Self {
        Self {
            process,
            handle,
            shutdown_budget,
            worker_tasks: JoinSet::new(),
            worker_map: FastIndexMap::default(),
        }
    }

    /// Spawns the child described by `child_spec`, tracking it under the given `worker_id`.
    ///
    /// `config` supplies the per-child overrides chosen at registration time: which runtime to spawn the child's task
    /// on, and how the child's shutdown strategy is determined.
    ///
    /// Returns the identity of the process the child was started under, which is the only point at which that process
    /// exists and so the only point at which it can be recorded.
    pub(super) fn add_worker(
        &mut self, worker_id: u64, child_spec: &SupervisedChild, config: &ChildConfig,
    ) -> Result<StartedChild, SupervisorError> {
        let process = child_spec.create_process(&self.process);
        let worker_name: Arc<str> = process.name().into();

        // Every worker gets a slot through which a supervisor it drives internally -- built after initialization and
        // run inside its own future, rather than handed to us as a child -- can attach itself to the supervision
        // tree. Captured before the process is consumed below.
        let tree_slot = TreeSlot::default();
        let started = StartedChild::new(&process, Arc::clone(&worker_name), Arc::clone(&tree_slot));

        // Only create a coordinator for a child that actually observes the signal. Most workers don't: they run until
        // their own terminal condition and ignore whatever we fire at them, so a coordinator for them is an
        // allocation and a wake-up that buy nothing.
        let (shutdown_coordinator, shutdown_handle) = if child_spec.wants_shutdown_signal() {
            let (coordinator, handle) = ShutdownHandle::paired();
            (Some(coordinator), handle)
        } else {
            (None, ShutdownHandle::noop())
        };

        let worker_future = child_spec.create_worker_future(process.clone(), shutdown_handle)?;
        let shutdown_strategy = match config.shutdown() {
            ChildShutdown::Worker => child_spec.shutdown_strategy(),
            ChildShutdown::Explicit(strategy) => strategy,
            // The child has no deadline of its own, so the supervisor's budget is what bounds it. If nothing bounds
            // it after all, it would be free to stall the drain forever, so fall back to whatever the worker asks
            // for. Note that this asks whether the budget yields a *deadline*, not merely whether one was set: a
            // budget too large to represent as an instant bounds nothing, and is no better than having none.
            ChildShutdown::BudgetBounded => {
                match resolve_budget_deadline(tokio::time::Instant::now(), self.shutdown_budget) {
                    Some(_) => ShutdownStrategy::Graceful(Duration::MAX),
                    None => child_spec.shutdown_strategy(),
                }
            }
        };

        // Every worker's task is timed, keyed on its fully qualified process name -- the same name
        // `spawn_traced_named` would have recorded for an equivalent standalone task. A worker is a top-level task, so
        // it is polled once per wake-up rather than once per unit of work, which keeps the two clock reads per poll well
        // amortized against whatever the poll actually does.
        let task = worker_future
            .into_process_future(process)
            .with_task_instrumentation(worker_name.to_string());

        // Make ourselves the ambient supervisor for the worker's whole task, initialization included, so the worker
        // can spawn siblings without being handed a handle.
        let task = CURRENT_SUPERVISOR.scope(self.handle.clone(), task);

        // Put the worker's tree slot in scope for the same span, so a supervisor the worker starts can find it.
        let task = CURRENT_TREE_SLOT.scope(tree_slot, task);

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
        Ok(started)
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

    /// Shuts down all workers, honoring each worker's shutdown strategy.
    ///
    /// Every worker is signalled up front and then awaited concurrently under its **own** graceful deadline, so a
    /// worker that ignores shutdown is aborted at its configured timeout regardless of its siblings. Total shutdown
    /// time is therefore bounded by the slowest individual worker rather than the sum of all timeouts.
    ///
    /// Ordering between workers is deliberately not the supervisor's concern. A supervisor has no way to know what
    /// depends on what, so workers that must stop in a particular order arrange it themselves through the ordinary
    /// means -- an input channel closing, a barrier, a notification -- which is also what makes the order visible in
    /// the code that establishes the dependency rather than in a distant list of registrations.
    ///
    /// A worker whose graceful timeout is effectively unbounded (such as a nested supervisor, which uses
    /// `Duration::MAX` because it bounds itself via its own children's deadlines) is waited on indefinitely and is
    /// never aborted here.
    ///
    /// Returns the number of workers that had to be forcefully aborted because they exceeded their graceful shutdown
    /// timeout. The count includes aborts reported by nested child supervisors (which surface their own tally via
    /// [`WorkerError::ShutdownTimedOut`]), so the value reflects the entire supervision tree rooted at this
    /// supervisor.
    pub(super) async fn shutdown_workers(&mut self) -> usize {
        debug!("Shutting down all processes.");

        let aborted = self.shutdown_workers_inner().await;

        debug_assert!(self.worker_map.is_empty(), "worker map should be empty after shutdown");
        debug_assert!(
            self.worker_tasks.is_empty(),
            "worker tasks should be empty after shutdown"
        );

        aborted
    }

    async fn shutdown_workers_inner(&mut self) -> usize {
        // Take ownership of all worker bookkeeping so we can consume each worker's shutdown coordinator. Signal every
        // graceful worker and immediately abort brutal ones, recording a per-worker abort deadline so each is held to
        // its own timeout rather than a single shared one.
        let now = tokio::time::Instant::now();
        let budget_deadline = resolve_budget_deadline(now, self.shutdown_budget);
        let mut pending: FastIndexMap<Id, (u64, Arc<str>, AbortHandle, Option<tokio::time::Instant>)> =
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
                    // Absent for a worker that never observes the signal; we still wait for it to finish on its own.
                    if let Some(shutdown_coordinator) = shutdown_coordinator {
                        shutdown_coordinator.shutdown();
                    }
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
