//! State that tracks workers for supervisors and scopes.
//!
//! `WorkerState` owns a set of child tasks that run for a [`Supervisor`](super::Supervisor) or a
//! [`Scope`](super::Scope). It gives the operations that both of them need:
//!
//! - spawn a child
//! - wait for the next child to finish
//! - shut down all children concurrently
//!
//! It is intentionally independent of the restart policy. The owner decides what to do when a worker exits.

use std::future::{pending, Future};
use std::pin::pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use saluki_common::collections::FastIndexMap;
use saluki_common::sync::shutdown::{ShutdownCoordinator, ShutdownHandle};
use saluki_common::task::TaskInstrument as _;
use tokio::{
    select,
    task::{AbortHandle, Id, JoinError, JoinSet},
};
use tracing::{debug, warn};

use super::process::{Process, ProcessExt as _};
use super::scope::{drive, DriveContext, StopLink};
use super::spawn::CURRENT_SUPERVISOR;
use super::supervisable::ShutdownStrategy;
use super::supervisor::{
    ChildConfig, ChildShutdown, ProcessError, SupervisedChild, SupervisorError, SupervisorHandle, WorkerError,
};
use super::tree::{StartedChild, SupervisorNode, TreeParent, CURRENT_TREE_PARENT};

/// Per-worker bookkeeping held by a [`WorkerState`].
struct ProcessState {
    /// Caller-assigned identifier for the worker.
    ///
    /// `WorkerState` does not interpret the value. The owner gives each child a stable id from a monotonic counter.
    /// [`WorkerState::wait_for_next_worker`] returns it, so that the caller can match the exit to its own records.
    worker_id: u64,
    /// Fully qualified process name, retained so shutdown can name precisely which worker had to be forcefully
    /// aborted.
    worker_name: Arc<str>,
    /// How the child's shutdown strategy is decided.
    ///
    /// The strategy is resolved only when shutdown starts. A child that uses the budget of its owner can get a deadline
    /// only after the owner knows the value of that budget. If the owner is a scope, it knows this value only after the
    /// owner of the scope was told to stop.
    shutdown: ChildShutdown,
    /// The strategy that the child reports for itself, which [`shutdown`][Self::shutdown] can use.
    worker_strategy: ShutdownStrategy,
    /// Whether the shutdown budget of the owner applies to this child.
    ///
    /// False for a nested supervisor, which bounds itself through its own children. Aborting one would cut its subtree
    /// off mid-drain, and -- for a supervisor on a dedicated runtime, whose work lives on another OS thread -- would
    /// not even stop it, while still reporting it as stopped.
    budget_applies: bool,
    /// Coordinator that signals this child.
    ///
    /// This coordinator is present also for a worker that reports
    /// [`wants_shutdown_signal`][super::Supervisable::wants_shutdown_signal] as false. That worker gets a
    /// [`ShutdownHandle::noop`] and never sees the signal that we fire. Instead, the scope of the worker observes the
    /// signal. Thus, the scope learns when to close and by what deadline, whether or not the worker itself listens.
    shutdown_coordinator: ShutdownCoordinator,
    abort_handle: AbortHandle,
}

/// Tracks the set of running child tasks for a supervisor or scope.
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
    /// Supervision-tree bookkeeping for the supervisor these workers belong to.
    ///
    /// Named as each worker's parent so that a supervisor a worker drives internally can attach itself to the tree.
    node: Arc<SupervisorNode>,
    /// The stop signals of the process that owns these workers and of the processes above it, if a scope owns them.
    ///
    /// Each worker gets this chain, so that the worker can see when a process above it is told to stop.
    ancestors: Option<Arc<StopLink>>,
    worker_tasks: JoinSet<Result<(), WorkerError>>,
    worker_map: FastIndexMap<Id, ProcessState>,
}

impl WorkerState {
    pub(super) fn new(
        process: Process, handle: SupervisorHandle, shutdown_budget: Option<Duration>, node: Arc<SupervisorNode>,
    ) -> Self {
        Self {
            process,
            handle,
            shutdown_budget,
            node,
            ancestors: None,
            worker_tasks: JoinSet::new(),
            worker_map: FastIndexMap::default(),
        }
    }

    /// Gives each worker `ancestors` as the stop signals of the processes above it.
    ///
    /// A scope uses this, so that its children can see when its owner, or a process above its owner, is told to stop.
    pub(super) fn with_ancestors(mut self, ancestors: Arc<StopLink>) -> Self {
        self.ancestors = Some(ancestors);
        self
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

        let started = StartedChild::new(&process, Arc::clone(&worker_name));

        // Every child gets a coordinator, whether or not it observes the signal itself. A worker that does not observe
        // it gets a no-op handle. But the scope of the worker still listens through its own handle. Thus, the scope
        // learns when to close, and by what time, but the worker itself never learns this.
        let (mut shutdown_coordinator, shutdown_handle) = ShutdownHandle::paired();
        let shutdown_handle = if child_spec.wants_shutdown_signal() {
            shutdown_handle
        } else {
            drop(shutdown_handle);
            ShutdownHandle::noop()
        };

        let mut worker_future = child_spec.create_worker_future(process.clone(), shutdown_handle)?;

        // A worker owns a scope for the children that it spawns, and it does not finish until they finish. A nested
        // supervisor already has its own children, and it has no body to drive with them.
        if !child_spec.is_supervisor() {
            worker_future = drive(
                worker_future,
                DriveContext {
                    owner_name: Arc::clone(&worker_name),
                    process: process.clone(),
                    supervisor: self.handle.clone(),
                    owner_signal: shutdown_coordinator.register(),
                    ancestors: self.ancestors.clone(),
                    adopt: config.adopt().cloned(),
                },
            );
        }

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

        // Name the child slot this worker occupies for the same span, so a supervisor it builds and runs inside its
        // own future -- rather than handing it to us as a child -- can attach itself to the tree there.
        let task = CURRENT_TREE_PARENT.scope(TreeParent::new(Arc::clone(&self.node), worker_id), task);

        let abort_handle = match config.runtime() {
            Some(handle) => self.worker_tasks.spawn_on(task, handle),
            None => self.worker_tasks.spawn(task),
        };
        self.worker_map.insert(
            abort_handle.id(),
            ProcessState {
                worker_id,
                worker_name,
                shutdown: config.shutdown(),
                worker_strategy: child_spec.shutdown_strategy(),
                budget_applies: !child_spec.is_supervisor(),
                shutdown_coordinator,
                abort_handle,
            },
        );
        Ok(started)
    }

    /// Returns `true` if no workers are running.
    pub(super) fn is_empty(&self) -> bool {
        self.worker_tasks.is_empty()
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
            Some(joined) => self.complete(joined),
            None => unreachable!(
                "join set is non-empty here: we park above while empty, and only this method removes workers"
            ),
        }
    }

    /// Polls for the next worker to finish, and returns its `worker_id` and result.
    ///
    /// Resolves to `None` if no worker runs, and does not park. The caller is a poll loop with other sources of
    /// wake-ups, and it decides what an empty set means.
    pub(super) fn poll_next_worker(&mut self, cx: &mut Context<'_>) -> Poll<Option<(u64, Result<(), WorkerError>)>> {
        match self.worker_tasks.poll_join_next_with_id(cx) {
            Poll::Ready(Some(joined)) => Poll::Ready(Some(self.complete(joined))),
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }

    /// Removes the records of a finished worker, and converts the end of its task into a worker result.
    fn complete(&mut self, joined: Result<(Id, Result<(), WorkerError>), JoinError>) -> (u64, Result<(), WorkerError>) {
        match joined {
            Ok((worker_task_id, worker_result)) => {
                let process_state = self
                    .worker_map
                    .shift_remove(&worker_task_id)
                    .expect("worker task ID not found");
                (process_state.worker_id, worker_result)
            }
            Err(e) => {
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
        let budget_deadline = resolve_budget_deadline(tokio::time::Instant::now(), self.shutdown_budget);
        self.shutdown_workers_until(budget_deadline, pending()).await
    }

    /// Shuts down all workers as [`shutdown_workers`][Self::shutdown_workers] does, but holds them to `budget_deadline`
    /// and not to a budget of this state.
    ///
    /// A scope has no budget of its own. The deadline that its owner got when the owner was told to stop bounds its
    /// children. That deadline exists only after the owner was told to stop.
    ///
    /// If `sooner` resolves with a deadline while the workers drain, that deadline also bounds each worker that the
    /// budget applies to. A scope uses this if its owner is told to stop only after the scope began to close. A worker
    /// keeps the deadline that it was told when it was signalled, because each worker is signalled only once.
    pub(super) async fn shutdown_workers_until<F>(
        &mut self, budget_deadline: Option<tokio::time::Instant>, sooner: F,
    ) -> usize
    where
        F: Future<Output = Option<tokio::time::Instant>>,
    {
        debug!("Shutting down all processes.");

        let aborted = self.shutdown_workers_inner(budget_deadline, sooner).await;

        debug_assert!(self.worker_map.is_empty(), "worker map should be empty after shutdown");
        debug_assert!(
            self.worker_tasks.is_empty(),
            "worker tasks should be empty after shutdown"
        );

        aborted
    }

    async fn shutdown_workers_inner<F>(&mut self, budget_deadline: Option<tokio::time::Instant>, sooner: F) -> usize
    where
        F: Future<Output = Option<tokio::time::Instant>>,
    {
        // Take ownership of all worker bookkeeping so we can consume each worker's shutdown coordinator. Signal every
        // graceful worker and immediately abort brutal ones, recording a per-worker abort deadline so each is held to
        // its own timeout rather than a single shared one.
        let now = tokio::time::Instant::now();
        let mut draining: FastIndexMap<Id, Draining> = FastIndexMap::default();
        for (task_id, process_state) in std::mem::take(&mut self.worker_map) {
            let ProcessState {
                worker_id,
                worker_name,
                shutdown,
                worker_strategy,
                budget_applies,
                shutdown_coordinator,
                abort_handle,
            } = process_state;

            let shutdown_strategy = match shutdown {
                ChildShutdown::Worker => worker_strategy,
                ChildShutdown::Explicit(strategy) => strategy,
                // The child has no deadline of its own, so the budget bounds it. If no budget bounds it, the child can
                // stall the drain forever. In that case, use the strategy that the worker asks for. This match checks
                // whether the budget gives a *deadline*, not only whether a budget was set. A budget too large to
                // represent as an instant bounds nothing, and is the same as no budget.
                ChildShutdown::BudgetBounded => match budget_deadline {
                    Some(_) => ShutdownStrategy::Graceful(Duration::MAX),
                    None => worker_strategy,
                },
            };

            match shutdown_strategy {
                ShutdownStrategy::Graceful(timeout) => {
                    debug!(worker_id, shutdown_timeout = ?timeout, "Gracefully shutting down process.");
                    let budget_deadline = budget_applies.then_some(budget_deadline).flatten();
                    let deadline = resolve_abort_deadline(now, timeout, budget_deadline);

                    // Tell the child the time of its abort, so that the child or its scope can finish before that time.
                    // Then the child is not aborted during its drain.
                    match deadline {
                        Some(deadline) => shutdown_coordinator.shutdown_with_deadline(deadline),
                        None => shutdown_coordinator.shutdown(),
                    }
                    draining.insert(
                        task_id,
                        Draining {
                            worker_id,
                            worker_name,
                            abort_handle,
                            deadline,
                            budget_applies,
                        },
                    );
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
        //
        // If only workers with no finite deadline remain (e.g. nested supervisors), we wait for them to exit on their
        // own. This is the path the topology takes -- each per-component supervisor is graceful-with-`MAX`, so its own
        // forced-abort tally is reported here and merged into ours.
        let mut sooner = pin!(sooner);
        let mut sooner_resolved = false;
        let mut aborted_total = 0;
        while !self.worker_tasks.is_empty() {
            let next_abort = draining.values().filter_map(|worker| worker.deadline).min();
            let abort_due = async move {
                match next_abort {
                    Some(deadline) => tokio::time::sleep_until(deadline).await,
                    None => pending::<()>().await,
                }
            };

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
                        draining.swap_remove(&task_id);
                    }
                }
                _ = abort_due => {
                    let now = tokio::time::Instant::now();
                    draining.retain(|_, worker| {
                        if worker.deadline.is_some_and(|deadline| deadline <= now) {
                            warn!(worker_id = worker.worker_id, worker_name = %worker.worker_name, "Worker ignored graceful shutdown; forcefully aborting after timeout.");
                            worker.abort_handle.abort();
                            aborted_total += 1;
                            false
                        } else {
                            true
                        }
                    });
                }
                sooner_deadline = &mut sooner, if !sooner_resolved => {
                    sooner_resolved = true;
                    if let Some(sooner_deadline) = sooner_deadline {
                        for worker in draining.values_mut().filter(|worker| worker.budget_applies) {
                            let deadline = worker.deadline.map_or(sooner_deadline, |deadline| deadline.min(sooner_deadline));
                            worker.deadline = Some(deadline);
                        }
                    }
                }
            }
        }

        aborted_total
    }
}

/// A worker that was told to stop, and that the shutdown still waits for.
struct Draining {
    worker_id: u64,
    worker_name: Arc<str>,
    abort_handle: AbortHandle,

    /// When the worker is forcefully aborted, if it has a deadline.
    deadline: Option<tokio::time::Instant>,

    /// Whether the budget of the owner applies to the worker. If so, a sooner deadline also applies to it.
    budget_applies: bool,
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
pub(super) fn reported_abort_count(output: &Result<(), WorkerError>) -> usize {
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
