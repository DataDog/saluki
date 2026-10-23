//! Runs a worker's body together with the scope that the worker owns.

use std::{
    future::{poll_fn, Future as _},
    panic::{catch_unwind, resume_unwind, AssertUnwindSafe},
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use saluki_common::sync::shutdown::{ShutdownHandle, ShutdownView};
use tokio::sync::mpsc;
use tracing::{debug, error, warn};

use super::{Adoption, Edge, PendingScopeChild, Scope, ScopeError, ScopeSlot, CURRENT_SCOPE};
use crate::runtime::{
    halt::{Halt, HaltSignal, HaltWatch},
    process::Process,
    restart::{RestartAction, RestartState, RestartStrategy},
    supervisor::{ChildConfig, SupervisedChild, SupervisorHandle, WorkerError, WorkerFuture},
    tree::{ChildFacts, ChildKey, Roster, SupervisorNode, TreeParent, CURRENT_TREE_PARENT},
    worker_state::{reported_abort_count, WorkerState},
};

/// The number of queued children that a scope starts in one batch before it continues with the rest of its loop.
///
/// This is the same as the spawn batches of a supervisor. Many spawns at the same time cause only one wake-up. But the
/// loop soon returns to reap children and to watch for shutdown.
const SPAWN_DRAIN_BATCH: usize = 64;

/// The data that the supervisor or scope that starts a worker gives to the scope of that worker.
pub(in crate::runtime) struct DriveContext {
    /// The fully qualified process name of the worker, which is also the name of a scope that is created on demand.
    pub(in crate::runtime) owner_name: Arc<str>,

    /// The process of the worker, under which the scope starts its children.
    pub(in crate::runtime) process: Process,

    /// The supervisor of the worker, which is also the ambient supervisor for the children of the scope.
    pub(in crate::runtime) supervisor: SupervisorHandle,

    /// Reports whether the worker was told to stop, even if the worker does not listen for this signal.
    pub(in crate::runtime) owner_signal: ShutdownView,

    /// Resolves when the worker is told to stop, if the worker takes the signal, so that the scope can stop the
    /// children that [stop with their owner][crate::runtime::NestedSupervisorBuilder::stops_with_owner].
    pub(in crate::runtime) owner_stop: Option<ShutdownHandle>,

    /// The stop signals of the processes above the worker, if the worker runs in a scope.
    pub(in crate::runtime) ancestors: Option<Arc<StopLink>>,

    /// Resolves when the worker is halted.
    pub(in crate::runtime) halt: HaltSignal,

    /// A pre-created scope that the worker adopts, so that it does not create a scope on demand.
    pub(in crate::runtime) adopt: Option<Adoption>,
}

/// The stop signal of a process that owns a scope, linked to the stop signals of the processes above that process.
///
/// A scope gives this chain to each of its children. Thus, a child can see when its owner, or a process above its
/// owner, is told to stop. The child can see this before the scope of its owner closes and tells the child itself to
/// stop. The chain ends at the nearest supervisor, because a supervisor tells its children to stop as soon as it is told
/// to stop.
pub(in crate::runtime) struct StopLink {
    signal: ShutdownView,
    parent: Option<Arc<StopLink>>,
}

impl StopLink {
    /// Returns whether a process in this chain was told to stop.
    fn stop_requested(&self) -> bool {
        let mut link = Some(self);
        while let Some(current) = link {
            if current.signal.is_triggered() {
                return true;
            }
            link = current.parent.as_deref();
        }
        false
    }
}

/// Returns whether the worker that `owner_signal` belongs to, or a process above it, was told to stop.
fn requested_stop(owner_signal: &ShutdownView, ancestors: Option<&StopLink>) -> bool {
    owner_signal.is_triggered() || ancestors.is_some_and(StopLink::stop_requested)
}

/// Wraps a worker's future so that it runs the worker's scope together with the worker's body.
///
/// The returned future completes only after the body finishes and every child in the scope exits. A worker that never
/// spawns a child has no scope, and the wrapper adds almost no cost to its body.
///
/// When the worker is [halted][crate::runtime::halt], the body is dropped and every child in the scope is halted too.
pub(in crate::runtime) fn drive(body: WorkerFuture, ctx: DriveContext) -> WorkerFuture {
    Box::pin(Driver::new(body, ctx).run())
}

/// The reason that the run phase of a worker ended.
enum Stop {
    /// The body finished, or panicked.
    Body(std::thread::Result<Result<(), WorkerError>>),

    /// A child terminated and failed the worker with this error.
    ChildFailed(WorkerError),

    /// The worker was halted.
    Halted(Halt),
}

struct Driver {
    owner_name: Arc<str>,
    slot: ScopeSlot,

    /// The spawn queue of the pre-created scope that the worker adopted, until the worker starts its scope.
    ///
    /// The worker takes the queue when it is created, not when it first runs. Thus, if the worker never runs, the queue
    /// is dropped with the worker, and the scope closes.
    adopted: Option<mpsc::UnboundedReceiver<PendingScopeChild>>,

    body: Option<WorkerFuture>,
    process: Process,
    supervisor: SupervisorHandle,
    owner_signal: ShutdownView,

    /// Moves into the scope when the worker creates one.
    owner_stop: Option<ShutdownHandle>,
    ancestors: Option<Arc<StopLink>>,
    halt: HaltWatch,

    /// The worker's scope, when the worker has one.
    scope: Option<ScopeRuntime>,
}

impl Driver {
    fn new(body: WorkerFuture, ctx: DriveContext) -> Self {
        let DriveContext {
            owner_name,
            process,
            supervisor,
            owner_signal,
            owner_stop,
            ancestors,
            halt,
            adopt,
        } = ctx;

        let mut adopted = None;
        let slot = match adopt {
            Some(adoption) => match adoption.take() {
                Some(unadopted) => {
                    let (scope, rx) = unadopted.into_parts();
                    adopted = Some(rx);
                    ScopeSlot::prefilled(scope)
                }
                None => {
                    // Only one worker can adopt a scope. But a worker that adopts a scope can start more than once, for
                    // example when the supervisor that runs it restarts. The children of the scope belonged to the
                    // first instance, and they stopped with it. Thus, this instance gets a new scope, the same as a
                    // worker that adopts nothing.
                    debug!(
                        worker_name = %owner_name,
                        scope = adoption.name(),
                        "Scope was already adopted; the worker gets a new scope instead."
                    );
                    ScopeSlot::lazy(Arc::clone(&owner_name))
                }
            },
            None => ScopeSlot::lazy(Arc::clone(&owner_name)),
        };
        let body: WorkerFuture = Box::pin(CURRENT_SCOPE.scope(slot.clone(), body));

        Self {
            owner_name,
            slot,
            adopted,
            body: Some(body),
            process,
            supervisor,
            owner_signal,
            owner_stop,
            ancestors,
            halt: HaltWatch::new(halt),
            scope: None,
        }
    }

    async fn run(mut self) -> Result<(), WorkerError> {
        let stop = poll_fn(|cx| self.poll_running(cx)).await;

        // Drop the body before the scope closes, so that the body releases everything that it holds first. For example,
        // the body can hold the sender of a channel that its children drain. The children can finish only after the
        // body releases that sender.
        self.body = None;

        match stop {
            Stop::Body(Ok(result)) => {
                // A body that returned an error failed, so the children that it spawned but that did not start yet do
                // not start.
                let (halted, child_failure) = self.close(result.is_ok()).await;

                // A body can return because a child failed, for example when it sees the output channel of that child
                // close. Then the scope sees the failure only as it closes, and the failure is the result of the worker.
                // An error of the body itself has priority, because it is the first failure that the worker saw.
                let result = match (result, child_failure) {
                    (Ok(()), Some(failure)) => Err(failure),
                    (result, _) => result,
                };
                self.finish(result, halted)
            }
            Stop::Body(Err(panic)) => {
                self.close(false).await;
                resume_unwind(panic)
            }
            Stop::ChildFailed(error) => {
                self.close(false).await;
                Err(error)
            }
            // `close` sees the same halt, so it halts every child. Whoever halted the worker already logged it, but the
            // count includes it.
            Stop::Halted(reason) => {
                let (halted, _) = self.close(false).await;
                match reason {
                    Halt::Overdue => Err(WorkerError::ShutdownTimedOut { aborted: 1 + halted }),
                    Halt::Immediate => Ok(()),
                }
            }
        }
    }

    fn poll_running(&mut self, cx: &mut Context<'_>) -> Poll<Stop> {
        // Checked first, so that a halted body is never polled again.
        if let Poll::Ready(reason) = self.halt.poll_halted(cx) {
            return Poll::Ready(Stop::Halted(reason));
        }

        let body = self.body.as_mut().expect("body is present while the worker runs");
        match catch_unwind(AssertUnwindSafe(|| body.as_mut().poll(cx))) {
            Ok(Poll::Ready(result)) => return Poll::Ready(Stop::Body(Ok(result))),
            Err(panic) => return Poll::Ready(Stop::Body(Err(panic))),
            Ok(Poll::Pending) => {}
        }

        // The first spawn creates the scope of a worker. Thus, the scope can be new at this point.
        self.adopt_if_created();
        let Some(scope) = self.scope.as_mut() else {
            return Poll::Pending;
        };

        // Checked before the spawns below, so that a child that stops with its owner and is spawned after the owner was
        // told to stop is told to stop as soon as it starts.
        if let Some(owner_stop) = scope.owner_stop.as_mut() {
            if owner_stop.as_mut().poll(cx).is_ready() {
                scope.owner_stop = None;
                scope.worker_state.stop_with_owner();
            }
        }

        let mut started = 0;
        while started < SPAWN_DRAIN_BATCH {
            match scope.rx.poll_recv(cx) {
                Poll::Ready(Some(pending)) => {
                    scope.start(pending);
                    started += 1;
                }
                Poll::Ready(None) | Poll::Pending => break,
            }
        }
        if started == SPAWN_DRAIN_BATCH {
            // More children can be in the queue. Wake again to start them after the rest of the loop runs.
            cx.waker().wake_by_ref();
        }

        // Whether a stop was requested is checked only when a child exits, and only once for each poll.
        let mut requested = None;
        while let Poll::Ready(Some((child_id, result))) = scope.worker_state.poll_next_worker(cx) {
            let requested =
                *requested.get_or_insert_with(|| requested_stop(&self.owner_signal, self.ancestors.as_deref()));
            if let Some(error) = scope.on_exit(child_id, result, requested) {
                return Poll::Ready(Stop::ChildFailed(error));
            }
        }

        Poll::Pending
    }

    /// Takes ownership of the worker's scope, if the scope was created after the last check.
    fn adopt_if_created(&mut self) {
        if self.scope.is_some() {
            return;
        }

        let Some(scope) = self.slot.get().cloned() else {
            return;
        };

        // The spawn queue of the scope comes from the scope that the worker adopted, or from the slot that created the
        // scope on demand. Only this worker takes it, and only once, because it takes it when it first sees the scope.
        let rx = self
            .adopted
            .take()
            .or_else(|| self.slot.take_receiver())
            .expect("a worker's scope has a spawn queue that only the worker takes");

        // The supervision tree shows the children of the scope below the worker. This is possible because the worker
        // has its own slot in the tree. A worker that runs outside supervision, for example in a test, has no slot.
        let node = Arc::new(SupervisorNode::new(Arc::clone(&self.owner_name)));
        let attachment = CURRENT_TREE_PARENT
            .try_with(|parent| ScopeAttachment::attach(parent.clone(), Arc::clone(&node)))
            .ok();

        // The children of the scope see the stop signal of this worker and the signals of the processes above it.
        let ancestors = Arc::new(StopLink {
            signal: self.owner_signal.clone(),
            parent: self.ancestors.clone(),
        });

        self.scope = Some(ScopeRuntime {
            worker_state: WorkerState::new(self.process.clone(), self.supervisor.clone(), Arc::clone(&node))
                .with_ancestors(ancestors),
            restart_state: RestartState::new(RestartStrategy::default()),
            roster: Roster::new(node),
            scope,
            rx,
            owner_stop: self.owner_stop.take().map(Box::pin),
            aborted: 0,
            _attachment: attachment,
        });
    }

    /// Returns whether the worker, or a process above it, was told to stop.
    fn stop_requested(&self) -> bool {
        requested_stop(&self.owner_signal, self.ancestors.as_deref())
    }

    /// Closes the scope, stops its children, and waits for them to exit.
    ///
    /// If `start_queued` is set, this method first starts the children that were spawned but did not start yet. Thus,
    /// every child that the body spawned before it finished runs and is then told to stop. This is also true if the
    /// body returned before the scope took those spawns from its queue. If `start_queued` is not set, this method
    /// discards those children, because nothing must start after the worker fails or is halted.
    ///
    /// Each child gets as long as its own shutdown strategy allows, unless the worker is halted, in which case every
    /// child still running is halted.
    ///
    /// Returns the number of processes that were halted, and the first failure of a child that fails the worker. The
    /// count includes the processes that children reported as halted when they exited while the scope was open. See
    /// [`ScopeRuntime::on_close_exit`] for when a child fails the worker as the scope closes.
    async fn close(&mut self, start_queued: bool) -> (usize, Option<WorkerError>) {
        self.adopt_if_created();
        let Some(mut scope) = self.scope.take() else {
            return (0, None);
        };

        scope.rx.close();
        let mut discarded = 0;
        while let Ok(pending) = scope.rx.try_recv() {
            if start_queued {
                scope.start(pending);
            } else {
                discarded += 1;
            }
        }
        if discarded > 0 {
            debug!(
                scope = scope.scope.name(),
                discarded, "Discarded queued children as the scope closed."
            );
        }

        // Check whether a stop was requested when each child exits, not once for the whole close, because a stop can be
        // requested while the scope closes.
        //
        // Each child leaves the roster as soon as it exits, because its specification can hold the sender of a channel
        // that a sibling drains as the scope closes. A server's request handler is an example.
        let mut exits = Vec::new();
        let (owner_signal, ancestors) = (&self.owner_signal, self.ancestors.as_deref());
        let ScopeRuntime {
            worker_state, roster, ..
        } = &mut scope;
        let halted = worker_state
            .shutdown_workers_reporting(&mut self.halt, |child_id, result| {
                let ScopeEntry { spec, config } = roster
                    .remove(child_id)
                    .expect("exited child must be present in the roster");
                exits.push((
                    spec.name().to_string(),
                    config,
                    result,
                    requested_stop(owner_signal, ancestors),
                ));
            })
            .await;

        let mut child_failure = None;
        for (child_name, config, result, requested) in exits {
            if let Some(failure) = scope.on_close_exit(child_name, &config, result, requested) {
                child_failure.get_or_insert(failure);
            }
        }

        // The scope is dropped after this call, and that detaches it from the supervision tree.
        (scope.aborted + halted, child_failure)
    }

    /// Reports how the worker finished, and counts the children that were halted.
    fn finish(&self, result: Result<(), WorkerError>, aborted: usize) -> Result<(), WorkerError> {
        // As for a supervisor, halted children make the stop unclean only if the worker, or a process above it, was
        // told to stop. Each was already logged when it was halted.
        if aborted == 0 || !self.stop_requested() {
            return result;
        }

        match result {
            Ok(()) => Err(WorkerError::ShutdownTimedOut { aborted }),
            // The worker's own error is the root cause, so it has priority. But if that error is also an unclean stop,
            // the two counts are added together.
            Err(error) => {
                let error = Err(error);
                match reported_abort_count(&error) {
                    0 => error,
                    prior => Err(WorkerError::ShutdownTimedOut {
                        aborted: prior + aborted,
                    }),
                }
            }
        }
    }
}

/// A child registered in a scope.
struct ScopeEntry {
    spec: SupervisedChild,
    config: ChildConfig,
}

/// A worker's scope, when the worker has one.
struct ScopeRuntime {
    scope: Scope,
    rx: mpsc::UnboundedReceiver<PendingScopeChild>,
    worker_state: WorkerState,
    restart_state: RestartState,
    roster: Roster<ScopeEntry>,

    /// Resolves when the owner is told to stop. Boxed because a handle is not `Unpin`. Taken when it resolves.
    owner_stop: Option<Pin<Box<ShutdownHandle>>>,

    /// The number of processes that children reported as halted when they exited while the scope was open.
    aborted: usize,

    _attachment: Option<ScopeAttachment>,
}

impl ScopeRuntime {
    fn start(&mut self, pending: PendingScopeChild) {
        let PendingScopeChild { id, spec, config } = pending;

        if config.significant() {
            warn!(
                scope = self.scope.name(),
                child_name = spec.name(),
                "Child is marked significant, but it belongs to a scope rather than a supervisor, so the flag has no \
                 effect. Mark it needed instead."
            );
        }

        match self.worker_state.add_worker(id, &spec, &config) {
            Ok(started) => {
                let facts = ChildFacts {
                    key: ChildKey::Dynamic(id),
                    name: spec.name().into(),
                    node: spec.node(),
                    restart: config.restart(),
                    significant: config.can_stop_owner(),
                };
                self.roster.insert(id, ScopeEntry { spec, config }, facts, started);
            }
            Err(e) => {
                // This error occurs only when a nested supervisor on a dedicated runtime cannot create an OS thread. No
                // caller can receive the error, because a spawn always succeeds.
                error!(scope = self.scope.name(), child_name = spec.name(), error = %e, "Failed to start scope child.");
            }
        }
    }

    /// Handles a child exit while the scope is open, and returns an error if the owner must fail.
    ///
    /// `requested` is set if the owner, or a process above it, was told to stop before the child exited.
    fn on_exit(&mut self, child_id: u64, result: Result<(), WorkerError>, requested: bool) -> Option<WorkerError> {
        let (child_name, config) = {
            let entry = self
                .roster
                .get(child_id)
                .expect("exited child must be present in the roster");
            (entry.spec.name().to_string(), entry.config.clone())
        };
        let needed = config.edge() == Edge::Needed;
        let forwards_failures = config.forwards_failures();

        // A child that failed to initialize is not eligible for restart.
        if let Err(WorkerError::Initialization {
            child_name: inner,
            source,
        }) = result
        {
            self.roster.remove(child_id);
            let full_name = match inner {
                Some(inner) => format!("{}/{}", child_name, inner),
                None => child_name,
            };

            if (needed || forwards_failures) && !requested {
                return Some(WorkerError::Initialization {
                    child_name: Some(full_name),
                    source,
                });
            }

            warn!(scope = self.scope.name(), child_name = %full_name, error = %source, "Scope child failed to initialize.");
            return None;
        }

        // After the owner, or a process above it, was told to stop, every exit is a requested exit. This is the same as
        // for the children of a supervisor that was told to stop. The owner can still depend on its children while it
        // drains. A restart, or a failure of the owner, would only cut that drain short.
        let abnormal = result.is_err();
        if requested {
            self.roster.remove(child_id);

            // A child that halted its own children reports how many it halted. The runtime already logged each of them,
            // and they count towards the stop of the owner, the same as when the scope closes.
            let aborted = reported_abort_count(&result);
            self.aborted += aborted;
            if abnormal && aborted == 0 {
                warn!(scope = self.scope.name(), child_name = %child_name, ?result, "Scope child exited with an error while its owner was stopping.");
            } else {
                debug!(scope = self.scope.name(), child_name = %child_name, "Scope child exited while its owner was stopping.");
            }
            return None;
        }

        if config.restart().should_restart(abnormal) {
            match self.restart_state.evaluate_restart() {
                RestartAction::Restart(_) => {
                    warn!(scope = self.scope.name(), child_name = %child_name, ?result, "Scope child terminated, restarting.");
                    let spec = self.roster.get(child_id).expect("present for restart").spec.clone();
                    match self.worker_state.add_worker(child_id, &spec, &config) {
                        Ok(started) => {
                            self.roster.restart_in_place(child_id, started);
                            return None;
                        }
                        // If the scope must restart a child but the restart fails, the child fails the owner. The edge
                        // of the child has no effect on this. A child that reaches the restart limit also fails the
                        // owner in the same way.
                        Err(e) => {
                            self.roster.remove(child_id);
                            error!(scope = self.scope.name(), child_name = %child_name, error = %e, "Failed to restart scope child.");
                            return Some(child_terminated(child_name, e.to_string()));
                        }
                    }
                }
                // The edge of the child has no effect here. A child that continues to fail makes the owner fail, so
                // that the next level of the supervision tree can try again.
                RestartAction::Shutdown => {
                    self.roster.remove(child_id);
                    error!(scope = self.scope.name(), child_name = %child_name, ?result, "Scope child exceeded its scope's restart limit; failing the scope's owner.");
                    return Some(WorkerError::Runtime(
                        ScopeError::ChildRestartLimit { child_name }.into(),
                    ));
                }
            }
        }

        // The way that the child exited makes it not eligible for restart. The scope logs an abnormal exit at `warn`,
        // because a hosted child has no other path back to its owner.
        self.roster.remove(child_id);
        if abnormal {
            warn!(scope = self.scope.name(), child_name = %child_name, ?result, "Scope child exited with an error and is not eligible for restart.");
        } else {
            debug!(scope = self.scope.name(), child_name = %child_name, "Scope child exited and is not eligible for restart.");
        }

        (needed || (abnormal && forwards_failures)).then(|| child_terminated(child_name, describe_exit(&result)))
    }

    /// Handles a child exit while the scope closes, and returns an error if the owner must fail.
    ///
    /// `requested` is set if the owner, or a process above it, was told to stop before the child exited.
    ///
    /// The scope never restarts a child as it closes. A clean exit is how a child normally stops when its scope closes,
    /// so it never fails the owner. Neither does an exit after which processes were halted, because the scope counts
    /// those processes instead. The scope logs every other error.
    ///
    /// If nothing told the owner to stop, the error also fails the owner if the child is needed or forwards failures,
    /// the same as when the scope is open. This case is common: the body of an owner can return because a child failed,
    /// for example because it sees the output channel of that child close. The body returns before the scope can see
    /// the exit of the child, so the scope sees the error only now.
    fn on_close_exit(
        &self, child_name: String, config: &ChildConfig, result: Result<(), WorkerError>, requested: bool,
    ) -> Option<WorkerError> {
        if result.is_ok() || reported_abort_count(&result) > 0 {
            return None;
        }

        if requested {
            warn!(scope = self.scope.name(), child_name = %child_name, ?result, "Scope child exited with an error while its owner was stopping.");
            return None;
        }

        warn!(scope = self.scope.name(), child_name = %child_name, ?result, "Scope child exited with an error as its scope closed.");
        if config.edge() != Edge::Needed && !config.forwards_failures() {
            return None;
        }

        Some(match result {
            Err(WorkerError::Initialization {
                child_name: inner,
                source,
            }) => WorkerError::Initialization {
                child_name: Some(match inner {
                    Some(inner) => format!("{}/{}", child_name, inner),
                    None => child_name,
                }),
                source,
            },
            result => child_terminated(child_name, describe_exit(&result)),
        })
    }
}

fn child_terminated(child_name: String, reason: String) -> WorkerError {
    WorkerError::Runtime(ScopeError::ChildTerminated { child_name, reason }.into())
}

fn describe_exit(result: &Result<(), WorkerError>) -> String {
    match result {
        Ok(()) => String::from("exited without being asked to stop"),
        Err(WorkerError::Runtime(e)) => e.to_string(),
        Err(WorkerError::Initialization { source, .. }) => source.to_string(),
        Err(WorkerError::ShutdownTimedOut { aborted }) => {
            format!("stopped uncleanly after forcefully aborting {} worker(s)", aborted)
        }
    }
}

/// The position of a scope in the supervision tree, below the worker that owns it.
///
/// The attachment detaches itself when it is dropped. This covers each way that a scope can end: the scope closes, or
/// the runtime aborts the task of the worker.
struct ScopeAttachment {
    parent: TreeParent,
    node: Arc<SupervisorNode>,
}

impl ScopeAttachment {
    fn attach(parent: TreeParent, node: Arc<SupervisorNode>) -> Self {
        parent.attach_scope(Arc::clone(&node));
        Self { parent, node }
    }
}

impl Drop for ScopeAttachment {
    fn drop(&mut self) {
        self.parent.detach_scope(&self.node);
    }
}
