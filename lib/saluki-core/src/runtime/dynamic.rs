//! Dynamic supervision.
//!
//! A [`DynamicSupervisor`] supervises children that are created on demand, after the supervisor is already running,
//! rather than being fixed up front like a [`Supervisor`](super::Supervisor). This suits workloads where the number and
//! identity of children can't be known ahead of time -- most notably network servers that spawn one task per
//! connection.
//!
//! Children are started through a [`DynamicSupervisorHandle`], which is cheap to clone and can be handed to other
//! workers (for example, a connection-accepting worker). All children are treated as temporary: they're never
//! restarted, since their termination is a normal part of operation. An optional maximum-children limit provides
//! admission control.

use std::sync::{
    atomic::{AtomicU64, AtomicUsize, Ordering},
    Arc, Mutex,
};
use std::time::Duration;

use saluki_common::sync::shutdown::{ShutdownCoordinator, ShutdownHandle};
use snafu::Snafu;
use tokio::{
    pin, select,
    sync::{
        mpsc::{self, UnboundedSender},
        oneshot, OwnedSemaphorePermit, Semaphore,
    },
    task::JoinSet,
};
use tracing::{debug, error};

use super::process::{Process, ProcessExt as _};
use super::supervisor::{ProcessError, Supervisable, SupervisedChild, SupervisorError, WorkerError, WorkerFuture};

/// Default grace period a [`DynamicSupervisor`] gives its children to exit on shutdown before aborting them.
const DEFAULT_CHILD_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

/// Identifier for a child started by a [`DynamicSupervisor`].
///
/// This is an opaque token returned by [`DynamicSupervisorHandle::spawn`]. It is unique within a single process for the
/// lifetime of the supervisor.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct DynChildId(u64);

impl DynChildId {
    /// Returns the raw numeric value of this identifier.
    pub fn as_u64(self) -> u64 {
        self.0
    }
}

/// Error returned when starting a child on a [`DynamicSupervisor`] fails.
#[derive(Debug, Snafu)]
pub enum SpawnError {
    /// The supervisor is already running its configured maximum number of children.
    #[snafu(display("dynamic supervisor is at capacity ({} children)", max))]
    AtCapacity {
        /// The configured maximum-children limit.
        max: usize,
    },

    /// The supervisor isn't currently running: it either hasn't started yet, or has shut down.
    #[snafu(display("dynamic supervisor is not running"))]
    SupervisorGone,
}

/// A command sent from a [`DynamicSupervisorHandle`] to a running [`DynamicSupervisor`].
enum DynCommand {
    /// Start a new child.
    Spawn {
        spec: SupervisedChild,
        permit: Option<OwnedSemaphorePermit>,
        ack: Option<oneshot::Sender<()>>,
    },
}

/// A handle to a [`DynamicSupervisor`], used to start children on demand.
///
/// Handles are cheap to clone and can be shared across tasks. Starting a child is non-blocking. A handle keeps working
/// across supervisor restarts: it always routes to the currently running supervisor, and returns
/// [`SpawnError::SupervisorGone`] when none is running.
#[derive(Clone)]
pub struct DynamicSupervisorHandle {
    name: Arc<str>,
    current_tx: Arc<Mutex<Option<UnboundedSender<DynCommand>>>>,
    id_counter: Arc<AtomicU64>,
    active: Arc<AtomicUsize>,
    permits: Option<Arc<Semaphore>>,
    max_children: Option<usize>,
}

impl DynamicSupervisorHandle {
    /// Returns the name of the supervisor this handle refers to.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Starts a new child, returning once it has been handed off to the supervisor.
    ///
    /// The child begins initializing in its own task; this method does not wait for that to complete. Use
    /// [`spawn_confirmed`](Self::spawn_confirmed) to wait until the child has been registered.
    ///
    /// # Errors
    ///
    /// Returns [`SpawnError::AtCapacity`] if a maximum-children limit is configured and currently reached, or
    /// [`SpawnError::SupervisorGone`] if the supervisor isn't running.
    pub fn spawn<T: Supervisable + 'static>(&self, worker: T) -> Result<DynChildId, SpawnError> {
        let permit = self.acquire_permit()?;
        let id = self.id_counter.fetch_add(1, Ordering::Relaxed);
        self.send(DynCommand::Spawn {
            spec: SupervisedChild::Worker(Arc::new(worker)),
            permit,
            ack: None,
        })?;
        Ok(DynChildId(id))
    }

    /// Starts a new child and waits until the supervisor has registered it.
    ///
    /// # Errors
    ///
    /// As [`spawn`](Self::spawn), plus [`SpawnError::SupervisorGone`] if the supervisor stops before confirming.
    pub async fn spawn_confirmed<T: Supervisable + 'static>(&self, worker: T) -> Result<DynChildId, SpawnError> {
        let permit = self.acquire_permit()?;
        let id = self.id_counter.fetch_add(1, Ordering::Relaxed);
        let (ack_tx, ack_rx) = oneshot::channel();
        self.send(DynCommand::Spawn {
            spec: SupervisedChild::Worker(Arc::new(worker)),
            permit,
            ack: Some(ack_tx),
        })?;
        ack_rx.await.map_err(|_| SpawnError::SupervisorGone)?;
        Ok(DynChildId(id))
    }

    /// Returns whether the supervisor is currently running.
    pub fn is_running(&self) -> bool {
        self.current_tx.lock().unwrap().is_some()
    }

    /// Returns the number of children currently running under the supervisor.
    pub fn active_children(&self) -> usize {
        self.active.load(Ordering::Relaxed)
    }

    fn acquire_permit(&self) -> Result<Option<OwnedSemaphorePermit>, SpawnError> {
        match &self.permits {
            Some(semaphore) => {
                Arc::clone(semaphore)
                    .try_acquire_owned()
                    .map(Some)
                    .map_err(|_| SpawnError::AtCapacity {
                        max: self.max_children.unwrap_or(0),
                    })
            }
            None => Ok(None),
        }
    }

    fn send(&self, command: DynCommand) -> Result<(), SpawnError> {
        // Holding the lock across the (synchronous, non-blocking) send is fine: the unbounded sender never awaits, and
        // the lock is uncontended in the common case. If `send` fails, the command -- and any permit it carries -- is
        // dropped, freeing the slot.
        let guard = self.current_tx.lock().unwrap();
        match guard.as_ref() {
            Some(tx) => tx.send(command).map_err(|_| SpawnError::SupervisorGone),
            None => Err(SpawnError::SupervisorGone),
        }
    }
}

/// Supervises children created on demand at runtime.
///
/// Unlike [`Supervisor`](super::Supervisor), a `DynamicSupervisor` starts with no children; they're added while it runs
/// via a [`DynamicSupervisorHandle`] (see [`handle`](Self::handle)). All children are temporary -- they're never
/// restarted. Add a `DynamicSupervisor` to a parent supervisor with
/// [`Supervisor::add_worker`](super::Supervisor::add_worker), the same way you'd add any other worker; it then
/// participates in the supervision tree, including coordinated shutdown.
///
/// # Restarts
///
/// If the `DynamicSupervisor` is itself restarted by its parent, its in-flight children are lost (matching Erlang/OTP
/// semantics for dynamically added children) and any outstanding handles transparently re-point at the new run. To keep
/// a connection-accepting worker and its `DynamicSupervisor` consistent across failures, place them as siblings under a
/// parent that restarts them together (a [`RestartMode::OneForAll`](super::RestartMode::OneForAll) supervisor).
pub struct DynamicSupervisor {
    name: Arc<str>,
    max_children: Option<usize>,
    shutdown_timeout: Duration,
    // Shared across clones (the supervisor is cloned each time it runs as a nested child) and across all handles. The
    // currently running run loop publishes its command sender here; handles read it to reach the live loop, and it's
    // cleared to `None` when no loop is active.
    current_tx: Arc<Mutex<Option<UnboundedSender<DynCommand>>>>,
    id_counter: Arc<AtomicU64>,
    // Number of children currently running, shared with handles so it can be surfaced as a gauge.
    active: Arc<AtomicUsize>,
    permits: Option<Arc<Semaphore>>,
}

impl DynamicSupervisor {
    /// Creates a new `DynamicSupervisor` with the given name and no child limit.
    pub fn new<S: AsRef<str>>(name: S) -> Self {
        Self {
            name: name.as_ref().into(),
            max_children: None,
            shutdown_timeout: DEFAULT_CHILD_SHUTDOWN_TIMEOUT,
            current_tx: Arc::new(Mutex::new(None)),
            id_counter: Arc::new(AtomicU64::new(0)),
            active: Arc::new(AtomicUsize::new(0)),
            permits: None,
        }
    }

    /// Sets the maximum number of children allowed to run at once.
    ///
    /// Once `max` children are running, [`DynamicSupervisorHandle::spawn`] returns [`SpawnError::AtCapacity`] until a
    /// child exits and frees a slot. By default there is no limit.
    pub fn with_max_children(mut self, max: usize) -> Self {
        self.max_children = Some(max);
        self.permits = Some(Arc::new(Semaphore::new(max)));
        self
    }

    /// Sets how long children are given to exit on shutdown before being aborted.
    ///
    /// All children are signalled at once and shut down concurrently, so this bounds the total time spent waiting for
    /// them rather than applying per child. Defaults to five seconds.
    pub fn with_child_shutdown_timeout(mut self, timeout: Duration) -> Self {
        self.shutdown_timeout = timeout;
        self
    }

    /// Returns a handle for starting children on this supervisor.
    ///
    /// The handle can be created before the supervisor starts running and cloned freely. Spawns made before the
    /// supervisor is running (or after it stops) return [`SpawnError::SupervisorGone`].
    pub fn handle(&self) -> DynamicSupervisorHandle {
        DynamicSupervisorHandle {
            name: Arc::clone(&self.name),
            current_tx: Arc::clone(&self.current_tx),
            id_counter: Arc::clone(&self.id_counter),
            active: Arc::clone(&self.active),
            permits: self.permits.clone(),
            max_children: self.max_children,
        }
    }

    pub(super) fn name(&self) -> &str {
        &self.name
    }

    /// Wraps the run loop as a [`WorkerFuture`] for embedding in a parent supervisor.
    ///
    /// This mirrors `Supervisor::as_nested_process`: the parent creates the process for this child and hands it in here.
    pub(super) fn as_nested_dynamic(&self, process: Process, process_shutdown: ShutdownHandle) -> WorkerFuture {
        let this = self.clone();
        Box::pin(async move {
            this.run_dynamic(process, process_shutdown)
                .await
                .map_err(WorkerError::from)
        })
    }

    async fn run_dynamic(&self, process: Process, process_shutdown: ShutdownHandle) -> Result<(), SupervisorError> {
        // Build a fresh command channel for this run and publish the sender so handles route here. Rebuilding the
        // channel per run means a restart transparently re-points existing handles at the new run loop.
        let (tx, mut rx) = mpsc::unbounded_channel();
        *self.current_tx.lock().unwrap() = Some(tx);

        let mut state = LightWorkerState::new(process, Arc::clone(&self.active));

        pin!(process_shutdown);
        loop {
            select! {
                // Shutdown takes priority so a flood of spawns can't starve it.
                biased;

                _ = &mut process_shutdown => {
                    debug!(supervisor = %self.name, "Shutdown triggered, shutting down all dynamic children.");
                    state.shutdown_all(self.shutdown_timeout).await;
                    break;
                }

                // A new child to start. The run loop keeps a sender alive in `current_tx` for its whole lifetime, so
                // `recv` never returns `None` here.
                command = rx.recv() => match command {
                    Some(DynCommand::Spawn { spec, permit, ack }) => {
                        match state.add_worker(&spec, permit) {
                            Ok(()) => {
                                if let Some(ack) = ack {
                                    let _ = ack.send(());
                                }
                            }
                            Err(e) => {
                                // Registration failed (for example, an invalid name). The permit was already dropped
                                // inside `add_worker`, and dropping `ack` lets a `spawn_confirmed` caller observe it.
                                error!(supervisor = %self.name, error = %e, "Failed to start dynamic child.");
                            }
                        }
                    }
                    None => unreachable!("the run loop holds a sender, so the command channel cannot close"),
                },

                // A child finished. Children are temporary and are never restarted; we just note the outcome.
                result = state.reap_next() => {
                    if let Err(error) = result {
                        debug!(supervisor = %self.name, ?error, "Dynamic child exited with an error.");
                    }
                }
            }
        }

        // The loop is done; stop routing handles to it.
        *self.current_tx.lock().unwrap() = None;
        Ok(())
    }
}

impl Clone for DynamicSupervisor {
    fn clone(&self) -> Self {
        Self {
            name: Arc::clone(&self.name),
            max_children: self.max_children,
            shutdown_timeout: self.shutdown_timeout,
            current_tx: Arc::clone(&self.current_tx),
            id_counter: Arc::clone(&self.id_counter),
            active: Arc::clone(&self.active),
            permits: self.permits.clone(),
        }
    }
}

/// Lightweight worker-tracking for the dynamic supervisor.
///
/// Unlike the static supervisor's `WorkerState`, this is built for large, homogeneous, temporary child sets: there's no
/// per-child coordinator or index map -- just a `JoinSet`, one shared `ShutdownCoordinator`, and an atomic count -- and
/// shutdown is concurrent rather than ordered one-at-a-time.
struct LightWorkerState {
    process: Process,
    tasks: JoinSet<Result<(), WorkerError>>,
    // A single coordinator shared by every child: one signal shuts them all down at once. Taken at shutdown.
    shutdown: Option<ShutdownCoordinator>,
    active: Arc<AtomicUsize>,
}

impl LightWorkerState {
    fn new(process: Process, active: Arc<AtomicUsize>) -> Self {
        Self {
            process,
            tasks: JoinSet::new(),
            shutdown: Some(ShutdownCoordinator::default()),
            active,
        }
    }

    /// Spawns a child, holding `permit` for the child's lifetime so its `max_children` slot frees when it exits.
    fn add_worker(
        &mut self, child_spec: &SupervisedChild, permit: Option<OwnedSemaphorePermit>,
    ) -> Result<(), SupervisorError> {
        let shutdown_handle = self
            .shutdown
            .as_mut()
            .expect("coordinator is present until shutdown")
            .register();
        let process = child_spec.create_process(&self.process)?;
        let worker_future = child_spec.create_worker_future(process.clone(), shutdown_handle)?;
        let task = worker_future.into_process_future(process);

        self.active.fetch_add(1, Ordering::Relaxed);
        self.tasks.spawn(async move {
            // The permit rides with the task, so it's released exactly when the child ends (completes, fails, or is
            // aborted), freeing a slot.
            let _permit = permit;
            task.await
        });
        Ok(())
    }

    /// Awaits the next child to finish and returns its result. Parks indefinitely if there are no children.
    async fn reap_next(&mut self) -> Result<(), WorkerError> {
        if self.tasks.is_empty() {
            std::future::pending::<()>().await;
        }

        let outcome = match self.tasks.join_next().await {
            Some(Ok(result)) => result,
            Some(Err(join_error)) => {
                let error = if join_error.is_cancelled() {
                    ProcessError::Aborted
                } else {
                    ProcessError::Panicked
                };
                Err(WorkerError::Runtime(error.into()))
            }
            None => unreachable!("we park above while empty, and only this method drains the set"),
        };

        self.active.fetch_sub(1, Ordering::Relaxed);
        outcome
    }

    /// Shuts down all children concurrently, bounded by a single grace period.
    ///
    /// Every child is signalled at once through the shared coordinator; we then wait up to `grace` for them all to
    /// exit, and abort whatever remains. Total shutdown time is therefore bounded by `grace`, not by the sum of
    /// per-child timeouts.
    async fn shutdown_all(&mut self, grace: Duration) {
        if let Some(coordinator) = self.shutdown.take() {
            let _ = tokio::time::timeout(grace, coordinator.shutdown_and_wait()).await;
        }

        // Whether or not every child exited within the grace period, abort anything still running and drain the set so
        // the active count returns to zero.
        self.tasks.abort_all();
        while self.tasks.join_next().await.is_some() {
            self.active.fetch_sub(1, Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use async_trait::async_trait;
    use saluki_common::sync::shutdown::ShutdownHandle;
    use saluki_error::GenericError;
    use tokio::{
        select,
        sync::oneshot,
        task::JoinHandle,
        time::{sleep, timeout},
    };

    use super::*;
    use crate::runtime::{
        InitializationError, ShutdownStrategy, Supervisable, Supervisor, SupervisorError, SupervisorFuture,
    };

    /// Decrements the active-child counter when dropped, so tests can observe when a child future actually ends.
    struct ActiveGuard(Arc<AtomicUsize>);

    impl Drop for ActiveGuard {
        fn drop(&mut self) {
            self.0.fetch_sub(1, SeqCst);
        }
    }

    #[derive(Clone)]
    enum ChildBehavior {
        /// Runs until shutdown is signalled.
        UntilShutdown,
        /// Fails with an error after the given delay.
        FailAfter(Duration),
        /// On shutdown, sleeps for the given duration before exiting (to exercise concurrent draining).
        SlowShutdown(Duration),
        /// Ignores shutdown entirely and runs forever (to exercise abort-at-deadline).
        IgnoreShutdown,
        /// Panics after the given delay, unless shutdown arrives first.
        PanicAfter(Duration),
    }

    /// A configurable dynamic child that records when it starts and how many are currently running.
    struct TestChild {
        name: String,
        started: Arc<AtomicUsize>,
        active: Arc<AtomicUsize>,
        behavior: ChildBehavior,
    }

    impl TestChild {
        fn until_shutdown(name: &str, started: &Arc<AtomicUsize>, active: &Arc<AtomicUsize>) -> Self {
            Self {
                name: name.to_string(),
                started: Arc::clone(started),
                active: Arc::clone(active),
                behavior: ChildBehavior::UntilShutdown,
            }
        }

        fn fail_after(name: &str, delay: Duration, started: &Arc<AtomicUsize>, active: &Arc<AtomicUsize>) -> Self {
            Self {
                name: name.to_string(),
                started: Arc::clone(started),
                active: Arc::clone(active),
                behavior: ChildBehavior::FailAfter(delay),
            }
        }

        fn slow_shutdown(name: &str, delay: Duration, started: &Arc<AtomicUsize>, active: &Arc<AtomicUsize>) -> Self {
            Self {
                name: name.to_string(),
                started: Arc::clone(started),
                active: Arc::clone(active),
                behavior: ChildBehavior::SlowShutdown(delay),
            }
        }

        fn ignore_shutdown(name: &str, started: &Arc<AtomicUsize>, active: &Arc<AtomicUsize>) -> Self {
            Self {
                name: name.to_string(),
                started: Arc::clone(started),
                active: Arc::clone(active),
                behavior: ChildBehavior::IgnoreShutdown,
            }
        }

        fn panic_after(name: &str, delay: Duration, started: &Arc<AtomicUsize>, active: &Arc<AtomicUsize>) -> Self {
            Self {
                name: name.to_string(),
                started: Arc::clone(started),
                active: Arc::clone(active),
                behavior: ChildBehavior::PanicAfter(delay),
            }
        }
    }

    #[async_trait]
    impl Supervisable for TestChild {
        fn name(&self) -> &str {
            &self.name
        }

        fn shutdown_strategy(&self) -> ShutdownStrategy {
            ShutdownStrategy::Graceful(Duration::from_millis(500))
        }

        async fn initialize(&self, process_shutdown: ShutdownHandle) -> Result<SupervisorFuture, InitializationError> {
            let started = Arc::clone(&self.started);
            let active = Arc::clone(&self.active);
            let behavior = self.behavior.clone();

            Ok(Box::pin(async move {
                started.fetch_add(1, SeqCst);
                active.fetch_add(1, SeqCst);
                let _guard = ActiveGuard(active);

                match behavior {
                    ChildBehavior::UntilShutdown => {
                        process_shutdown.await;
                        Ok(())
                    }
                    ChildBehavior::FailAfter(delay) => {
                        select! {
                            _ = sleep(delay) => Err(GenericError::msg("child failed")),
                            _ = process_shutdown => Ok(()),
                        }
                    }
                    ChildBehavior::SlowShutdown(delay) => {
                        process_shutdown.await;
                        sleep(delay).await;
                        Ok(())
                    }
                    ChildBehavior::IgnoreShutdown => {
                        // Hold the handle (so the supervisor counts us as outstanding) but never react to it.
                        let _hold = process_shutdown;
                        std::future::pending::<Result<(), GenericError>>().await
                    }
                    ChildBehavior::PanicAfter(delay) => {
                        select! {
                            _ = sleep(delay) => panic!("child panicked"),
                            _ = process_shutdown => Ok(()),
                        }
                    }
                }
            }))
        }
    }

    /// Runs a `DynamicSupervisor` as the sole child of a parent supervisor, returning a shutdown trigger and the join
    /// handle for the parent's run.
    async fn run_parent(dynamic: DynamicSupervisor) -> (oneshot::Sender<()>, JoinHandle<Result<(), SupervisorError>>) {
        let mut parent = Supervisor::new("parent").unwrap();
        parent.add_worker(dynamic);
        let (tx, rx) = oneshot::channel();
        let handle = tokio::spawn(async move { parent.run_with_shutdown(rx).await });
        (tx, handle)
    }

    async fn wait_running(handle: &DynamicSupervisorHandle) {
        for _ in 0..200 {
            if handle.is_running() {
                return;
            }
            sleep(Duration::from_millis(5)).await;
        }
        panic!("dynamic supervisor did not start in time");
    }

    async fn wait_until(condition: impl Fn() -> bool) {
        for _ in 0..200 {
            if condition() {
                return;
            }
            sleep(Duration::from_millis(5)).await;
        }
        panic!("condition not met in time");
    }

    #[tokio::test]
    async fn spawns_children_after_start() {
        let dynamic = DynamicSupervisor::new("dyn");
        let handle = dynamic.handle();
        let (tx, parent) = run_parent(dynamic).await;
        wait_running(&handle).await;

        let started = Arc::new(AtomicUsize::new(0));
        let active = Arc::new(AtomicUsize::new(0));
        handle
            .spawn(TestChild::until_shutdown("c1", &started, &active))
            .unwrap();
        handle
            .spawn(TestChild::until_shutdown("c2", &started, &active))
            .unwrap();

        wait_until(|| started.load(SeqCst) == 2).await;
        assert_eq!(active.load(SeqCst), 2);

        tx.send(()).unwrap();
        let result = timeout(Duration::from_secs(2), parent).await.unwrap().unwrap();
        assert!(result.is_ok());
        assert_eq!(active.load(SeqCst), 0, "all children must be drained on shutdown");
    }

    #[tokio::test]
    async fn rejects_spawns_at_capacity() {
        let dynamic = DynamicSupervisor::new("dyn").with_max_children(2);
        let handle = dynamic.handle();
        let (tx, parent) = run_parent(dynamic).await;
        wait_running(&handle).await;

        let started = Arc::new(AtomicUsize::new(0));
        let active = Arc::new(AtomicUsize::new(0));
        handle
            .spawn(TestChild::until_shutdown("c1", &started, &active))
            .unwrap();
        handle
            .spawn(TestChild::until_shutdown("c2", &started, &active))
            .unwrap();
        let err = handle
            .spawn(TestChild::until_shutdown("c3", &started, &active))
            .unwrap_err();
        assert!(matches!(err, SpawnError::AtCapacity { max: 2 }));

        tx.send(()).unwrap();
        let result = timeout(Duration::from_secs(2), parent).await.unwrap().unwrap();
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn capacity_frees_when_child_exits() {
        let dynamic = DynamicSupervisor::new("dyn").with_max_children(1);
        let handle = dynamic.handle();
        let (tx, parent) = run_parent(dynamic).await;
        wait_running(&handle).await;

        let started = Arc::new(AtomicUsize::new(0));
        let active = Arc::new(AtomicUsize::new(0));
        // The first child fails quickly, freeing its slot once it's reaped.
        handle
            .spawn(TestChild::fail_after(
                "c1",
                Duration::from_millis(20),
                &started,
                &active,
            ))
            .unwrap();
        wait_until(|| started.load(SeqCst) == 1 && active.load(SeqCst) == 0).await;

        // The slot frees when the exited child is reaped, which happens shortly after it exits. Retry until it does.
        let mut spawned = false;
        for _ in 0..200 {
            match handle.spawn(TestChild::until_shutdown("c2", &started, &active)) {
                Ok(_) => {
                    spawned = true;
                    break;
                }
                Err(SpawnError::AtCapacity { .. }) => sleep(Duration::from_millis(5)).await,
                Err(other) => panic!("unexpected spawn error: {other}"),
            }
        }
        assert!(spawned, "capacity must free up after the first child exits");

        tx.send(()).unwrap();
        let result = timeout(Duration::from_secs(2), parent).await.unwrap().unwrap();
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn children_are_not_restarted() {
        let dynamic = DynamicSupervisor::new("dyn");
        let handle = dynamic.handle();
        let (tx, parent) = run_parent(dynamic).await;
        wait_running(&handle).await;

        let started = Arc::new(AtomicUsize::new(0));
        let active = Arc::new(AtomicUsize::new(0));
        handle
            .spawn(TestChild::fail_after("c", Duration::from_millis(30), &started, &active))
            .unwrap();

        // Wait well past the failure; a restart would push `started` above one.
        sleep(Duration::from_millis(300)).await;
        assert_eq!(started.load(SeqCst), 1, "a temporary child must not be restarted");
        assert_eq!(active.load(SeqCst), 0);
        assert!(handle.is_running(), "supervisor stays up after a child fails");

        tx.send(()).unwrap();
        let result = timeout(Duration::from_secs(2), parent).await.unwrap().unwrap();
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn spawn_fails_before_start_and_after_shutdown() {
        let dynamic = DynamicSupervisor::new("dyn");
        let handle = dynamic.handle();
        let started = Arc::new(AtomicUsize::new(0));
        let active = Arc::new(AtomicUsize::new(0));

        // Before the supervisor is running.
        assert!(!handle.is_running());
        let err = handle
            .spawn(TestChild::until_shutdown("c", &started, &active))
            .unwrap_err();
        assert!(matches!(err, SpawnError::SupervisorGone));

        let (tx, parent) = run_parent(dynamic).await;
        wait_running(&handle).await;
        tx.send(()).unwrap();
        let result = timeout(Duration::from_secs(2), parent).await.unwrap().unwrap();
        assert!(result.is_ok());

        // After shutdown, the handle is invalidated.
        wait_until(|| !handle.is_running()).await;
        let err = handle
            .spawn(TestChild::until_shutdown("c", &started, &active))
            .unwrap_err();
        assert!(matches!(err, SpawnError::SupervisorGone));
    }

    #[tokio::test]
    async fn spawn_confirmed_waits_for_registration() {
        let dynamic = DynamicSupervisor::new("dyn");
        let handle = dynamic.handle();
        let (tx, parent) = run_parent(dynamic).await;
        wait_running(&handle).await;

        let started = Arc::new(AtomicUsize::new(0));
        let active = Arc::new(AtomicUsize::new(0));
        let id = handle
            .spawn_confirmed(TestChild::until_shutdown("c", &started, &active))
            .await
            .unwrap();
        assert_eq!(id.as_u64(), 0);

        // The child is registered on confirmation and starts running shortly after.
        wait_until(|| started.load(SeqCst) == 1).await;

        tx.send(()).unwrap();
        let result = timeout(Duration::from_secs(2), parent).await.unwrap().unwrap();
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn shutdown_drains_many_children_concurrently() {
        const CHILDREN: usize = 500;
        const CHILD_SHUTDOWN_DELAY: Duration = Duration::from_millis(50);

        let dynamic = DynamicSupervisor::new("dyn");
        let handle = dynamic.handle();
        let (tx, parent) = run_parent(dynamic).await;
        wait_running(&handle).await;

        let started = Arc::new(AtomicUsize::new(0));
        let active = Arc::new(AtomicUsize::new(0));
        for i in 0..CHILDREN {
            handle
                .spawn(TestChild::slow_shutdown(
                    &format!("c{i}"),
                    CHILD_SHUTDOWN_DELAY,
                    &started,
                    &active,
                ))
                .unwrap();
        }
        wait_until(|| handle.active_children() == CHILDREN).await;

        // Each child sleeps after observing shutdown. Concurrent shutdown drains them all in roughly one delay; a
        // sequential shutdown would take CHILDREN * delay (25s here). Assert it finishes well under that.
        let start = Instant::now();
        tx.send(()).unwrap();
        let result = timeout(Duration::from_secs(5), parent).await.unwrap().unwrap();
        let elapsed = start.elapsed();

        assert!(result.is_ok());
        assert_eq!(handle.active_children(), 0, "active count must return to zero");
        assert_eq!(active.load(SeqCst), 0);
        assert!(
            elapsed < Duration::from_secs(2),
            "shutdown must be concurrent (took {elapsed:?})"
        );
    }

    #[tokio::test]
    async fn unresponsive_children_are_aborted_at_deadline() {
        let dynamic = DynamicSupervisor::new("dyn").with_child_shutdown_timeout(Duration::from_millis(100));
        let handle = dynamic.handle();
        let (tx, parent) = run_parent(dynamic).await;
        wait_running(&handle).await;

        let started = Arc::new(AtomicUsize::new(0));
        let active = Arc::new(AtomicUsize::new(0));
        handle
            .spawn(TestChild::ignore_shutdown("stuck", &started, &active))
            .unwrap();
        wait_until(|| handle.active_children() == 1).await;

        // The child never reacts to shutdown, so it must be aborted once the grace period elapses rather than hanging.
        let start = Instant::now();
        tx.send(()).unwrap();
        let result = timeout(Duration::from_secs(2), parent).await.unwrap().unwrap();
        let elapsed = start.elapsed();

        assert!(result.is_ok());
        assert_eq!(handle.active_children(), 0);
        assert!(
            elapsed < Duration::from_secs(1),
            "stuck child must be aborted at the deadline (took {elapsed:?})"
        );
    }

    #[tokio::test]
    async fn panicking_child_is_reaped() {
        let dynamic = DynamicSupervisor::new("dyn");
        let handle = dynamic.handle();
        let (tx, parent) = run_parent(dynamic).await;
        wait_running(&handle).await;

        let started = Arc::new(AtomicUsize::new(0));
        let active = Arc::new(AtomicUsize::new(0));
        handle
            .spawn(TestChild::panic_after(
                "boom",
                Duration::from_millis(30),
                &started,
                &active,
            ))
            .unwrap();

        // The child panics; it must be reaped (active returns to zero) without restarting or taking down the supervisor.
        wait_until(|| handle.active_children() == 0).await;
        sleep(Duration::from_millis(100)).await;
        assert_eq!(started.load(SeqCst), 1, "panicked child must not be restarted");
        assert!(handle.is_running(), "supervisor survives a child panic");

        // The supervisor still accepts new children after a panic.
        handle
            .spawn(TestChild::until_shutdown("ok", &started, &active))
            .unwrap();
        wait_until(|| started.load(SeqCst) == 2).await;

        tx.send(()).unwrap();
        let result = timeout(Duration::from_secs(2), parent).await.unwrap().unwrap();
        assert!(result.is_ok());
    }
}
