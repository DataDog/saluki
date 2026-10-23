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

use std::future::pending;
use std::sync::Arc;
use std::task::{Context, Poll};

use saluki_common::collections::FastIndexMap;
use saluki_common::sync::shutdown::{ShutdownCoordinator, ShutdownHandle};
use saluki_common::task::TaskInstrument as _;
use tokio::{
    select,
    task::{Id, JoinError, JoinSet},
    time::Instant,
};
use tracing::{debug, warn};

use super::halt::{self, Halt, HaltWatch, Halter};
use super::process::{Process, ProcessExt as _};
use super::scope::{drive, DriveContext, StopLink};
use super::spawn::CURRENT_SUPERVISOR;
use super::supervisable::ShutdownStrategy;
use super::supervisor::{ChildConfig, ProcessError, SupervisedChild, SupervisorError, SupervisorHandle, WorkerError};
use super::tree::{StartedChild, SupervisorNode, TreeParent, CURRENT_TREE_PARENT};

/// Per-worker bookkeeping held by a [`WorkerState`].
struct ProcessState {
    /// Caller-assigned identifier for the worker.
    ///
    /// `WorkerState` does not interpret the value. The owner gives each child a stable id from a monotonic counter.
    /// [`WorkerState::wait_for_next_worker`] returns it, so that the caller can match the exit to its own records.
    worker_id: u64,
    /// Fully qualified process name, retained so shutdown can name precisely which worker had to be halted.
    worker_name: Arc<str>,
    shutdown_strategy: ShutdownStrategy,
    /// Coordinator that signals this child.
    ///
    /// This coordinator is present also for a worker that reports
    /// [`wants_shutdown_signal`][super::Supervisable::wants_shutdown_signal] as false. That worker gets a
    /// [`ShutdownHandle::noop`] and never sees the signal that we fire. But the scope of the worker observes the
    /// signal, so that it can tell a requested exit of one of its children from an exit that nobody requested.
    ///
    /// Taken when it fires, which can be before shutdown for a child that stops with its owner.
    shutdown_coordinator: Option<ShutdownCoordinator>,
    /// Whether this child is told to stop as soon as the owner of its scope is.
    stops_with_owner: bool,
    /// See [`halt`].
    halter: Halter,
}

/// Tracks the set of running child tasks for a supervisor or scope.
pub(super) struct WorkerState {
    process: Process,
    /// Handle to the supervisor these workers belong to.
    ///
    /// This handle is installed as the ambient supervisor for every worker task. Thus, anything that a worker spawns
    /// through [`spawn_sibling`][crate::runtime::spawn_sibling] goes to this supervisor. It is not necessary to pass a
    /// handle to the worker for this. For a scope, this is the supervisor above the owner of the scope.
    handle: SupervisorHandle,
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
    /// Whether the owner of the scope was told to stop. See [`stop_with_owner`][Self::stop_with_owner].
    owner_stopping: bool,
}

impl WorkerState {
    pub(super) fn new(process: Process, handle: SupervisorHandle, node: Arc<SupervisorNode>) -> Self {
        Self {
            process,
            handle,
            node,
            ancestors: None,
            worker_tasks: JoinSet::new(),
            worker_map: FastIndexMap::default(),
            owner_stopping: false,
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
        let process = child_spec.create_process(&self.process, config.own_resource_group());
        let worker_name: Arc<str> = process.name().into();

        let started = StartedChild::new(&process, Arc::clone(&worker_name));

        // Every child gets a coordinator, whether or not it observes the signal itself. A worker that does not observe
        // it gets a no-op handle. But the scope of the worker still watches the signal, to know whether an exit of one
        // of its children was requested.
        let (mut shutdown_coordinator, shutdown_handle) = ShutdownHandle::paired();
        let shutdown_handle = if child_spec.wants_shutdown_signal() {
            shutdown_handle
        } else {
            drop(shutdown_handle);
            ShutdownHandle::noop()
        };

        // A worker runs inside `drive`, which owns the worker's scope and handles its halt. A nested supervisor has no
        // body or scope to drive, so it watches for the halt itself.
        let (halter, halt) = halt::pair();
        let worker_future = if child_spec.is_supervisor() {
            child_spec.create_worker_future(process.clone(), shutdown_handle, HaltWatch::new(halt))?
        } else {
            let body = child_spec.create_worker_future(process.clone(), shutdown_handle, HaltWatch::never())?;
            drive(
                body,
                DriveContext {
                    owner_name: Arc::clone(&worker_name),
                    process: process.clone(),
                    supervisor: self.handle.clone(),
                    owner_signal: shutdown_coordinator.register().view(),
                    owner_stop: child_spec
                        .wants_shutdown_signal()
                        .then(|| shutdown_coordinator.register()),
                    ancestors: self.ancestors.clone(),
                    halt,
                    adopt: config.adopt().cloned(),
                },
            )
        };

        // The observer must see the result after the scope closed, because the scope can replace a clean result of the
        // body with the failure of a child.
        #[cfg(any(test, feature = "test-util"))]
        let worker_future = match config.exit_observer() {
            Some(observer) => observer.observe(worker_future),
            None => worker_future,
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

        // Name the child slot this worker occupies for the same span, so a supervisor it builds and runs inside its
        // own future -- rather than handing it to us as a child -- can attach itself to the tree there.
        let task = CURRENT_TREE_PARENT.scope(TreeParent::new(Arc::clone(&self.node), worker_id), task);

        let abort_handle = match config.runtime() {
            Some(handle) => self.worker_tasks.spawn_on(task, handle),
            None => self.worker_tasks.spawn(task),
        };

        // A child that is spawned after its owner was told to stop is told to stop at once.
        let stops_with_owner = config.stops_with_owner();
        let shutdown_coordinator = if stops_with_owner && self.owner_stopping {
            shutdown_coordinator.shutdown();
            None
        } else {
            Some(shutdown_coordinator)
        };

        self.worker_map.insert(
            abort_handle.id(),
            ProcessState {
                worker_id,
                worker_name,
                shutdown_strategy: config.shutdown().resolve(child_spec.shutdown_strategy()),
                shutdown_coordinator,
                stops_with_owner,
                halter,
            },
        );
        Ok(started)
    }

    /// Tells every child that [stops with its owner][ChildConfig::stops_with_owner] to stop, because the owner of the
    /// scope was told to stop.
    ///
    /// Only a scope calls this, so the setting has no effect under a supervisor, which tells every child to stop at
    /// once anyway. A child that is added later is told to stop as soon as it starts.
    pub(super) fn stop_with_owner(&mut self) {
        self.owner_stopping = true;
        for process_state in self.worker_map.values_mut() {
            if !process_state.stops_with_owner {
                continue;
            }

            if let Some(shutdown_coordinator) = process_state.shutdown_coordinator.take() {
                debug!(worker_id = process_state.worker_id, worker_name = %process_state.worker_name, "Stopping process along with its owner.");
                shutdown_coordinator.shutdown();
            }
        }
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
    /// Every worker is signalled up front and then awaited concurrently under its **own** timeout, so total shutdown
    /// time is bounded by the slowest worker rather than the sum of all timeouts. [Brutal][ShutdownStrategy::Brutal]
    /// workers are halted immediately. If `halt` fires, every worker still running is halted, including those with no
    /// finite timeout, such as nested supervisors.
    ///
    /// Ordering between workers is deliberately not handled here: workers that must stop in a particular order arrange
    /// it themselves, for example through an input channel closing or one worker owning the other.
    ///
    /// Returns the number of processes halted for being overdue, across the whole subtree rooted here.
    pub(super) async fn shutdown_workers(&mut self, halt: &mut HaltWatch) -> usize {
        self.shutdown_workers_reporting(halt, |_, _| {}).await
    }

    /// Shuts down all workers, as [`shutdown_workers`][Self::shutdown_workers] does, and reports how each worker
    /// exited.
    ///
    /// `on_exit` receives the `worker_id` and the result of each worker when it exits. A scope uses these results, so
    /// that it does not lose the error of a child that exits while the scope closes. An overdue worker reports
    /// [`WorkerError::ShutdownTimedOut`], whose count is already included in the return value.
    pub(super) async fn shutdown_workers_reporting<F>(&mut self, halt: &mut HaltWatch, on_exit: F) -> usize
    where
        F: FnMut(u64, Result<(), WorkerError>),
    {
        debug!("Shutting down all processes.");

        let halted = self.shutdown_workers_inner(halt, on_exit).await;

        debug_assert!(self.worker_map.is_empty(), "worker map should be empty after shutdown");
        debug_assert!(
            self.worker_tasks.is_empty(),
            "worker tasks should be empty after shutdown"
        );

        halted
    }

    async fn shutdown_workers_inner<F>(&mut self, halt: &mut HaltWatch, mut on_exit: F) -> usize
    where
        F: FnMut(u64, Result<(), WorkerError>),
    {
        // Signal every graceful worker and record when it runs out of time, and halt brutal workers immediately.
        let now = Instant::now();
        let mut stopping: FastIndexMap<Id, Stopping> = FastIndexMap::default();
        for (task_id, process_state) in std::mem::take(&mut self.worker_map) {
            let ProcessState {
                worker_id,
                worker_name,
                shutdown_strategy,
                shutdown_coordinator,
                halter,
                ..
            } = process_state;

            let mut child = Stopping {
                worker_id,
                worker_name,
                halter: Some(halter),
                timeout_at: None,
                counted: true,
            };
            match shutdown_strategy {
                ShutdownStrategy::Graceful(timeout) => {
                    debug!(worker_id, shutdown_timeout = ?timeout, "Gracefully shutting down process.");
                    // A child that stopped with its owner was already signalled.
                    if let Some(shutdown_coordinator) = shutdown_coordinator {
                        shutdown_coordinator.shutdown();
                    }

                    // `Duration::MAX`, or anything near it, overflows an instant: treat it as no timeout.
                    child.timeout_at = now.checked_add(timeout);
                }
                ShutdownStrategy::Brutal => {
                    debug!(worker_id, "Halting process outright.");
                    child.halt(Halt::Immediate);
                }
            }
            stopping.insert(task_id, child);
        }

        // Our owner may already be halted, for example because it ignored shutdown or is brutal.
        if let Some(reason) = halt.halted() {
            for child in stopping.values_mut() {
                child.halt_with_owner(reason);
            }
        }

        // Reap every worker, halting each one whose own timeout passes, or all of them if our owner is halted.
        let mut halted_total = 0;
        while !self.worker_tasks.is_empty() {
            let next_timeout = stopping.values().filter_map(|child| child.timeout_at).min();
            let watching_owner = halt.halted().is_none();
            select! {
                biased;

                reason = halt.wait(), if watching_owner => {
                    for child in stopping.values_mut() {
                        child.halt_with_owner(reason);
                    }
                }

                joined = self.worker_tasks.join_next_with_id() => {
                    let (task_id, output) = match joined {
                        Some(Ok((task_id, output))) => (task_id, Some(output)),
                        // Report a panic as a failure. A cancellation has no result to report.
                        Some(Err(e)) if e.is_panic() => {
                            (e.id(), Some(Err(WorkerError::Runtime(ProcessError::Panicked.into()))))
                        }
                        Some(Err(e)) => (e.id(), None),
                        None => continue,
                    };
                    if let Some(child) = stopping.swap_remove(&task_id) {
                        // Don't count halts caused by a brutal strategy, the worker's own or its owner's.
                        if child.counted {
                            halted_total += output.as_ref().map_or(0, reported_abort_count);
                        }
                        if let Some(output) = output {
                            on_exit(child.worker_id, output);
                        }
                    }
                }

                _ = sleep_until(next_timeout) => {
                    let now = Instant::now();
                    for child in stopping.values_mut() {
                        if child.timeout_at.is_some_and(|timeout_at| timeout_at <= now) {
                            warn!(worker_id = child.worker_id, worker_name = %child.worker_name, "Worker didn't stop within its shutdown timeout; halting it.");
                            child.halt(Halt::Overdue);
                        }
                    }
                }
            }
        }

        halted_total
    }
}

/// A worker in shutdown that has not exited yet.
struct Stopping {
    worker_id: u64,
    worker_name: Arc<str>,
    /// Present until the worker is halted.
    halter: Option<Halter>,
    /// When to halt the worker, if it has a finite timeout.
    timeout_at: Option<Instant>,
    /// Whether the halts the worker reports count towards an unclean shutdown.
    counted: bool,
}

impl Stopping {
    /// Halts the worker, if it is not halted already.
    fn halt(&mut self, reason: Halt) {
        self.timeout_at = None;
        if let Some(halter) = self.halter.take() {
            self.counted = reason.is_counted();
            halter.halt(reason);
        }
    }

    /// Halts the worker because its owner was halted.
    fn halt_with_owner(&mut self, reason: Halt) {
        if self.halter.is_none() {
            return;
        }

        match reason {
            Halt::Overdue => {
                warn!(worker_id = self.worker_id, worker_name = %self.worker_name, "Halting worker: its owner didn't stop within its shutdown timeout.")
            }
            Halt::Immediate => {
                debug!(worker_id = self.worker_id, worker_name = %self.worker_name, "Halting worker along with its owner.")
            }
        }
        self.halt(reason);
    }
}

/// Sleeps until `instant`, or forever if there is no instant.
async fn sleep_until(instant: Option<Instant>) {
    match instant {
        Some(instant) => tokio::time::sleep_until(instant).await,
        None => pending().await,
    }
}

/// Extracts the number of halted processes that a reaped child reported.
///
/// A child that was halted, or that halted any of its own children, returns [`WorkerError::ShutdownTimedOut`] with a
/// count covering its whole subtree. Any other completion contributes nothing.
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
    use std::time::Duration;

    use super::*;
    use crate::runtime::supervisor::ChildShutdown;

    #[test]
    fn a_child_keeps_the_strategy_that_it_reports_for_itself() {
        let own = ShutdownStrategy::Graceful(Duration::from_secs(5));
        assert!(matches!(
            ChildShutdown::Worker.resolve(own),
            ShutdownStrategy::Graceful(timeout) if timeout == Duration::from_secs(5)
        ));
    }

    #[test]
    fn an_explicit_strategy_overrides_the_workers_own() {
        let own = ShutdownStrategy::Graceful(Duration::from_secs(5));
        assert!(matches!(
            ChildShutdown::Explicit(ShutdownStrategy::Brutal).resolve(own),
            ShutdownStrategy::Brutal
        ));
    }
}
