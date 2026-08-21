//! Utilities for spawning child tasks attached to specific components in a topology.
use std::{future::Future, marker::PhantomData, time::Duration};

use saluki_common::sync::shutdown::ShutdownHandle;
use tokio::runtime::Handle;

use crate::runtime::{
    interruptible_worker, noninterruptible_worker, ChildId, ChildSpecification, IntoWorkerResult, RestartType,
    ShutdownStrategy, SpawnError, Supervisable, SupervisorHandle, WorkerSpec,
};

/// Component-scoped spawner for child tasks.
///
/// Every component in a topology consists of a primary task which runs the "core loop" of the component, and optionally
/// a number of child tasks that range from handling network connections to processing compute-heavy work in a separate
/// thread pool.
///
/// [`ComponentSpawner`] provides a component-scoped mechanism for spawning those child tasks under supervision. It is
/// tied specifically to the dedicated per-component supervisor that each component gets, which ensures that child tasks
/// spawned through this mechanism are properly attributed to the component, and also that their lifecycle is one-to-one
/// with the component itself.
///
/// # Child lifecycle, and one-shot vs supervisable
///
/// We classify tasks as either _one-shot_ or _supervisable_: one-shot tasks are those based on a provided closure,
/// which cannot be reinitialized and so cannot be restarted, and supervisable tasks are those based on an implementation
/// of [`Supervisable`], which allows for (potentially) initializing the underlying task future multiple times.
///
/// One-shot tasks are always [`temporary`][crate::runtime::RestartType::Temporary], since they cannot be
/// reinitialized. Supervisable tasks default to the same, and opt into being restarted via
/// [`ChildBuilder::with_restart_type`] when the worker is built to be initialized more than once.
///
/// All child tasks default to being marked as non-significant, so their termination -- clean or otherwise -- leaves the
/// component running. This is usually the correct behavior, but a component that cannot function without a particular
/// child may wish to mark it significant, which stops the component when that child terminates.
///
/// See [`ChildBuilder::with_significant`] for more information.
///
/// # Interruptible vs non-interruptible
///
/// [`ComponentSpawner`] allows spawning two styles of child task: "interruptible" and "non-interruptible."
/// Interruptible tasks are wrapped such that when the supervisor signals shutdown, the shutdown signal is
/// honored/polled despite whatever the logic is in the task itself does. Non-interruptible tasks still received a
/// shutdown handle, but the task logic itself is responsible for honoring shutdown signals.
///
/// Non-interruptible tasks aren't _truly_ uninterrupible: following the normal behavior of async Rust and the behavior
/// of futures, the future associated with a task can simply be no longer polled or dropped, _effectively_ interrupting
/// it when considered at the level of "will this task run to completion?"
///
/// # Worker pool
///
/// [`ComponentSpawner`] is topology-aware, which means callers have the ability to specify a child task runs on the
/// shared "global" thread pool attached to a given topology. This should be used for compute-heavy tasks, which
/// otherwise can affect the scheduling latency of I/O-heavy tasks.
///
/// # Task naming
///
/// Child task names should generally _not_ contain unique patterns/tokens -- such as monotonic IDs or high-cardinality
/// values -- as they are used for internal telemetry about the task. Generally, task names should be thought of as a
/// category label: if a component spawns tasks for handling connections, it should prefer to name them like
/// `conn_handler` instead of `conn_handler_<ID or IP>`.
///
/// A child task that must finish draining before the component stops:
///
/// ```no_run
/// # use saluki_core::components::ComponentSpawner;
/// # async fn drain(shutdown: saluki_common::sync::shutdown::ShutdownHandle) {}
/// # async fn example(spawner: &ComponentSpawner) -> Result<(), Box<dyn std::error::Error>> {
/// spawner.spawn_noninterruptible("queue_drainer", |shutdown| drain(shutdown)).await?;
/// # Ok(())
/// # }
/// ```
///
/// A compute-heavy task that belongs on the shared worker pool, which needs the builder to say so:
///
/// ```no_run
/// # use saluki_core::components::ComponentSpawner;
/// # async fn encode() {}
/// # async fn example(spawner: ComponentSpawner) -> Result<(), Box<dyn std::error::Error>> {
/// spawner.interruptible("encoder", encode()).on_worker_pool().spawn().await?;
/// # Ok(())
/// # }
/// ```
#[derive(Clone)]
pub struct ComponentSpawner {
    handle: SupervisorHandle,
    worker_pool: Handle,
}

impl ComponentSpawner {
    /// Creates a new `ComponentSpawner`.
    ///
    /// `worker_pool` is the shared worker pool owned by the topology, used by children that opt in via
    /// [`ChildBuilder::on_worker_pool`].
    ///
    /// The supervisor behind `handle` **MUST** carry a shutdown budget
    /// ([`Supervisor::with_shutdown_budget`][crate::runtime::Supervisor::with_shutdown_budget]). Children spawned here
    /// have no deadline of their own, so without one a child that ignores shutdown stalls the drain indefinitely.
    pub fn new(handle: SupervisorHandle, worker_pool: Handle) -> Self {
        Self { handle, worker_pool }
    }

    /// Creates a builder for an interruptible child task.
    ///
    /// Interruptible tasks are implicitly wrapped such that shutdown is polled alongside the underlying task future,
    /// ensuring that shutdown is observed at the earliest possible moment. They are best used for work which has no
    /// requirements on orderly shutdown, draining of remaining work, and so on.
    ///
    /// Use this method when advanced configuration of the underlying task is required. Otherwise, prefer
    /// [`spawn_interruptible`][Self::spawn_interruptible].
    pub fn interruptible<N, Fut>(&self, name: N, fut: Fut) -> ChildBuilder<'_>
    where
        N: Into<String>,
        Fut: Future + Send + 'static,
        Fut::Output: IntoWorkerResult,
    {
        ChildBuilder::one_shot(self, interruptible_worker(name, fut))
    }

    /// Creates a builder for a non-interruptible child task.
    ///
    /// Non-interruptible tasks are those which handle shutdown signals directly in order to precisely control when the
    /// task completes. They are best used for tasks which must perform some operation, or operations, between the
    /// receiving of a shutdown signal and completion.
    ///
    /// Non-interruptible tasks are not necessarily blocking: running a non-interruptible does not mean that it is guaranteed
    /// to complete, only that it won't be wrapped in a way that tries to shutdown at the earliest possible moment.
    ///
    /// Use this method when advanced configuration of the underlying task is required. Otherwise, prefer
    /// [`spawn_noninterruptible`][Self::spawn_noninterruptible].
    pub fn noninterruptible<N, F, Fut>(&self, name: N, f: F) -> ChildBuilder<'_>
    where
        N: Into<String>,
        F: FnOnce(ShutdownHandle) -> Fut + Send + 'static,
        Fut: Future + Send + 'static,
        Fut::Output: IntoWorkerResult,
    {
        ChildBuilder::one_shot(self, noninterruptible_worker(name, f))
    }

    /// Creates a builder for a supervisable child task.
    ///
    /// Supervisable tasks are those where the worker already implements [`Supervisable`], which lets
    /// `ComponentSpawner` serve as a consistent control surface for spawning both arbitrary asynchronous functions
    /// and more full-fledged workers.
    ///
    /// Supervisable tasks are set to permanently restart by default.
    ///
    /// Use this method when advanced configuration of the underlying task is required. Otherwise, prefer
    /// [`spawn_supervisable`][Self::spawn_supervisable].
    pub fn supervisable<T>(&self, worker: T) -> ChildBuilder<'_, Restartable>
    where
        T: Supervisable + 'static,
    {
        ChildBuilder::restartable(self, worker)
    }

    /// Spawns an interruptible child task.
    ///
    /// Interruptible tasks are implicitly wrapped such that shutdown is polled alongside the underlying task future,
    /// ensuring that shutdown is observed at the earliest possible moment. They are best used for work which has no
    /// requirements on orderly shutdown, draining of remaining work, and so on.
    ///
    /// Use [`interruptible`][Self::interruptible] when advanced configuration of the underlying task is required.
    ///
    /// # Errors
    ///
    /// If the component's supervisor isn't running, or the child specification is invalid, an error is returned.
    pub async fn spawn_interruptible<N, Fut>(&self, name: N, fut: Fut) -> Result<ChildId, SpawnError>
    where
        N: Into<String>,
        Fut: Future + Send + 'static,
        Fut::Output: IntoWorkerResult,
    {
        self.interruptible(name, fut).spawn().await
    }

    /// Spawns a non-interruptible child task.
    ///
    /// Non-interruptible tasks are those which handle shutdown signals directly in order to precisely control when the
    /// task completes. They are best used for tasks which must perform some operation, or operations, between the
    /// receiving of a shutdown signal and completion.
    ///
    /// Non-interruptible tasks are not necessarily blocking: running a non-interruptible does not mean that it is guaranteed
    /// to complete, only that it won't be wrapped in a way that tries to shutdown at the earliest possible moment.
    ///
    /// Use [`noninterruptible`][Self::noninterruptible] when advanced configuration of the underlying task is required.
    ///
    /// # Errors
    ///
    /// If the component's supervisor isn't running, or the child specification is invalid, an error is returned.
    pub async fn spawn_noninterruptible<N, F, Fut>(&self, name: N, f: F) -> Result<ChildId, SpawnError>
    where
        N: Into<String>,
        F: FnOnce(ShutdownHandle) -> Fut + Send + 'static,
        Fut: Future + Send + 'static,
        Fut::Output: IntoWorkerResult,
    {
        self.noninterruptible(name, f).spawn().await
    }

    /// Spawns a supervisable child task.
    ///
    /// Supervisable tasks are those where the worker already implements [`Supervisable`], which lets
    /// `ComponentSpawner` serve as a consistent control surface for spawning both arbitrary asynchronous functions
    /// and more full-fledged workers.
    ///
    /// Use [`supervisable`][Self::supervisable] when advanced configuration of the underlying task is required.
    ///
    /// # Errors
    ///
    /// If the component's supervisor isn't running, or the child specification is invalid, an error is returned.
    pub async fn spawn_supervisable<T>(&self, worker: T) -> Result<ChildId, SpawnError>
    where
        T: Supervisable + 'static,
    {
        self.supervisable(worker).spawn().await
    }

    /// Returns a handle to the shared worker pool owned by the topology.
    pub fn worker_pool(&self) -> &Handle {
        &self.worker_pool
    }

    /// Returns the underlying supervisor handle.
    pub fn handle(&self) -> &SupervisorHandle {
        &self.handle
    }

    /// Returns the number of children currently running that were spawned through a spawner.
    ///
    /// Statically registered children -- the component itself, in a topology -- are not counted.
    pub fn active_children(&self) -> usize {
        self.handle.active_children()
    }
}

mod sealed {
    pub trait Sealed {}
}

/// The kind of worker a [`ChildBuilder`] is describing.
///
/// This trait is sealed, and exists only to mark which configuration a builder makes available: a worker that can be
/// initialized more than once accepts a restart policy, and one that can't doesn't.
pub trait BuilderState: sealed::Sealed {}

/// Marks a builder whose worker can only be initialized once.
///
/// Closure-based children ([`ComponentSpawner::noninterruptible`], [`ComponentSpawner::interruptible`]) consume their
/// body when they start, so they can never be restarted, and [`ChildBuilder::with_restart_type`] is not available.
pub struct OneShot;

/// Marks a builder whose worker can be initialized more than once.
///
/// A [`Supervisable`] builds its work in [`initialize`][Supervisable::initialize] each time it starts, so it can be
/// restarted and [`ChildBuilder::with_restart_type`] is available.
pub struct Restartable;

impl sealed::Sealed for OneShot {}
impl BuilderState for OneShot {}
impl sealed::Sealed for Restartable {}
impl BuilderState for Restartable {}

/// Builder for a yet-to-be-spawned child task.
///
/// Advanced properties of a task can be configured with this builder prior to spawning. This builder uses the
/// typestate pattern to control which properties of the child task that can be configured (by controlling which
/// configuration methods are exposed) based on whether the worker is supervisable or not.
///
/// See [`BuilderState`] for more information on worker types.
#[must_use = "a child is only started when `spawn` is called"]
pub struct ChildBuilder<'a, S = OneShot> {
    spawner: &'a ComponentSpawner,
    spec: ChildSpecification<WorkerSpec>,
    _state: PhantomData<S>,
}

impl<'a, S: BuilderState> ChildBuilder<'a, S> {
    fn new(spawner: &'a ComponentSpawner, spec: ChildSpecification<WorkerSpec>) -> Self {
        Self {
            spawner,
            spec,
            _state: PhantomData,
        }
    }

    fn map_spec<F>(self, f: F) -> Self
    where
        F: FnOnce(ChildSpecification<WorkerSpec>) -> ChildSpecification<WorkerSpec>,
    {
        let Self { spawner, spec, .. } = self;

        Self::new(spawner, f(spec))
    }

    /// Runs this child task on the shared worker pool owned by the topology, instead of the component's runtime.
    ///
    /// Use this for compute-heavy work -- encoding, serialization, protocol servers -- that shouldn't contend with the
    /// runtime that drives the supervisors and I/O for the topology.
    pub fn on_worker_pool(self) -> Self {
        let worker_pool = self.spawner.worker_pool.clone();
        self.on_runtime(worker_pool)
    }

    /// Runs this child task on a specific runtime.
    ///
    /// Prefer [`on_worker_pool`][Self::on_worker_pool] unless the component owns a runtime of its own.
    pub fn on_runtime(self, handle: Handle) -> Self {
        self.map_spec(|spec| spec.with_runtime(handle))
    }

    /// Sets an explicit shutdown timeout for this child task.
    ///
    /// By default a closure-based child has no deadline of its own and is bounded only by the component's shutdown
    /// budget. Set this when the component wants a particular child abandoned sooner than that -- a deadline it is
    /// deliberately imposing, rather than a guess at how long the child ought to take. A value longer than the budget
    /// has no effect, since the two are resolved to whichever elapses first.
    ///
    /// For a [`Supervisable`] child, this overrides the strategy the task reports for itself.
    pub fn with_shutdown_timeout(self, timeout: Duration) -> Self {
        self.with_shutdown_strategy(ShutdownStrategy::Graceful(timeout))
    }

    /// Sets the explicit shutdown strategy used for this child task.
    pub fn with_shutdown_strategy(self, strategy: ShutdownStrategy) -> Self {
        self.map_spec(|spec| spec.with_shutdown_strategy(strategy))
    }

    /// Sets whether this child task's termination should stop the component.
    ///
    /// A component's supervisor uses [`AutoShutdown::AnySignificant`][auto_shutdown], so a significant child
    /// terminating **shuts the component down**: the child is not individually restarted. That happens even when the
    /// child exits cleanly, so this suits a child the component cannot function without, and not one that is expected
    /// to finish on its own.
    ///
    /// For example, a component handling client connections generally shouldn't stop just because one connection
    /// failed, but a component forwarding work to a child task may become inoperable if that task dies and cannot be
    /// reattached to the necessary channels or state without recreating the component.
    ///
    /// Only meaningful for a child that can terminate without being restarted, so setting it alongside
    /// [`RestartType::Permanent`] has no effect: such a child is always restarted and so never reaches this path.
    ///
    /// Defaults to `false`.
    ///
    /// [auto_shutdown]: crate::runtime::AutoShutdown::AnySignificant
    pub fn with_significant(self, significant: bool) -> Self {
        self.map_spec(|spec| spec.with_significant(significant))
    }

    /// Spawns the child, returning once the supervisor has started it.
    ///
    /// # Errors
    ///
    /// Returns [`SpawnError::SupervisorGone`] if the component's supervisor isn't running, or [`SpawnError::Rejected`]
    /// if it rejected the child (for example, an invalid name). A component's supervisor is running for the whole of
    /// the component's `run`, so `SupervisorGone` in a component indicates that the topology is already being torn
    /// down.
    pub async fn spawn(self) -> Result<ChildId, SpawnError> {
        let Self { spawner, spec, .. } = self;

        spawner.handle.spawn_with(spec).await
    }
}

impl<'a> ChildBuilder<'a, OneShot> {
    fn one_shot<T>(spawner: &'a ComponentSpawner, worker: T) -> Self
    where
        T: Supervisable + 'static,
    {
        let spec = ChildSpecification::one_shot_worker(worker)
            .with_restart_type(RestartType::Temporary)
            .with_shutdown_strategy(ShutdownStrategy::Graceful(Duration::MAX));

        Self::new(spawner, spec)
    }
}

impl<'a> ChildBuilder<'a, Restartable> {
    fn restartable<T>(spawner: &'a ComponentSpawner, worker: T) -> Self
    where
        T: Supervisable + 'static,
    {
        Self::new(spawner, ChildSpecification::worker(worker))
    }

    /// Sets the restart type for this child task.
    ///
    /// Supervised tasks that are defined through [`Supervisable`] default to being restarted permanently, since their
    /// structure naturally exposes a mechanism to allow initializing a worker more than once. However, in some cases,
    /// it may be desirable to disallow restarting a specific worker and instead treat their exit differently: only
    /// restart when the worker exits abnormally, or never restart the worker, and so on.
    ///
    /// Defaults to [`RestartType::Permanent`].
    pub fn with_restart_type(self, restart_type: RestartType) -> Self {
        self.map_spec(|spec| spec.with_restart_type(restart_type))
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc, Mutex,
        },
        thread::ThreadId,
    };

    use async_trait::async_trait;
    use saluki_metrics::test::TestRecorder;
    use tokio::sync::oneshot;
    use tokio::time::timeout;

    use super::*;
    use crate::components::test_util::TestComponentSupervisor;
    use crate::runtime::SupervisorError;

    /// How long the drain-participating child in these tests takes to finish after observing shutdown.
    ///
    /// Long enough that a child given any short deadline of its own would be aborted before completing, so the tests
    /// fail if children stop being bounded by the supervisor's budget alone.
    const DRAIN_DURATION: Duration = Duration::from_millis(300);

    /// A hand-written [`Supervisable`] that records where and how often it was initialized, then runs until shutdown.
    struct CountingWorker {
        name: &'static str,
        initializations: Arc<AtomicUsize>,
        thread_id: Arc<Mutex<Option<ThreadId>>>,
    }

    impl CountingWorker {
        fn new(name: &'static str) -> (Self, Arc<AtomicUsize>, Arc<Mutex<Option<ThreadId>>>) {
            let thread_id = Arc::new(Mutex::new(None));
            let initializations = Arc::new(AtomicUsize::new(0));

            (
                Self {
                    name,
                    initializations: Arc::clone(&initializations),
                    thread_id: Arc::clone(&thread_id),
                },
                initializations,
                thread_id,
            )
        }
    }

    #[async_trait]
    impl Supervisable for CountingWorker {
        fn name(&self) -> &str {
            self.name
        }

        fn shutdown_strategy(&self) -> ShutdownStrategy {
            ShutdownStrategy::Graceful(Duration::MAX)
        }

        async fn initialize(
            &self, process_shutdown: ShutdownHandle,
        ) -> Result<crate::runtime::SupervisorFuture, crate::runtime::InitializationError> {
            self.initializations.fetch_add(1, Ordering::SeqCst);
            *self.thread_id.lock().unwrap() = Some(std::thread::current().id());

            Ok(Box::pin(async move {
                process_shutdown.await;
                Ok(())
            }))
        }
    }

    /// A [`Supervisable`] that fails its first run and waits for shutdown on every run after.
    struct FailingOnceWorker {
        initializations: Arc<AtomicUsize>,
    }

    impl FailingOnceWorker {
        fn new() -> (Self, Arc<AtomicUsize>) {
            let initializations = Arc::new(AtomicUsize::new(0));
            (
                Self {
                    initializations: Arc::clone(&initializations),
                },
                initializations,
            )
        }
    }

    #[async_trait]
    impl Supervisable for FailingOnceWorker {
        fn name(&self) -> &str {
            "failing_once"
        }

        fn shutdown_strategy(&self) -> ShutdownStrategy {
            ShutdownStrategy::Graceful(Duration::MAX)
        }

        async fn initialize(
            &self, process_shutdown: ShutdownHandle,
        ) -> Result<crate::runtime::SupervisorFuture, crate::runtime::InitializationError> {
            let first_run = self.initializations.fetch_add(1, Ordering::SeqCst) == 0;

            Ok(Box::pin(async move {
                if first_run {
                    return Err(saluki_error::generic_error!("first run always fails"));
                }

                process_shutdown.await;
                Ok(())
            }))
        }
    }

    /// Polls `condition` until it holds, panicking after a few seconds.
    async fn wait_for(mut condition: impl FnMut() -> bool) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while !condition() {
            assert!(tokio::time::Instant::now() < deadline, "condition never became true");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    #[tokio::test]
    async fn noninterruptible_child_observes_shutdown_and_supervisor_exits_cleanly() {
        // The child holds the component's grace period, observes shutdown, and finishes. A clean supervisor result is
        // the assertion that matters: `ShutdownTimedOut` would mean the child was aborted rather than draining.
        let supervisor = TestComponentSupervisor::start("test_component").await;
        let spawner = supervisor.spawner();

        // The child does real work *after* observing shutdown. Without that, it finishes within any grace period at
        // all and the test would pass even if children were given a near-zero deadline instead of none.
        let drained = Arc::new(AtomicUsize::new(0));
        let child_drained = Arc::clone(&drained);

        spawner
            .spawn_noninterruptible("drainer", move |shutdown| async move {
                shutdown.await;
                tokio::time::sleep(DRAIN_DURATION).await;
                child_drained.fetch_add(1, Ordering::SeqCst);
            })
            .await
            .expect("should spawn");

        supervisor.wait_for_children(1).await;

        let result = supervisor.shutdown().await;
        assert!(
            result.is_ok(),
            "child should have drained rather than been aborted: {result:?}"
        );
        assert_eq!(drained.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn interruptible_child_is_torn_down_with_the_supervisor() {
        // An interruptible child that would otherwise run forever must be dropped when the supervisor shuts down.
        let supervisor = TestComponentSupervisor::start("test_component").await;

        supervisor
            .spawner()
            .spawn_interruptible("forever", std::future::pending::<()>())
            .await
            .expect("should spawn");

        supervisor.wait_for_children(1).await;
        assert!(supervisor.shutdown().await.is_ok());
    }

    #[tokio::test]
    async fn on_worker_pool_places_child_on_the_worker_pool() {
        // `on_worker_pool` must actually change where the child's task runs. The spawner is built with an explicit
        // pool handle here so the child's thread name is unambiguous.
        let pool = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .thread_name("spawner-pool-test")
            .enable_all()
            .build()
            .expect("should build pool");

        let supervisor = TestComponentSupervisor::start("test_component").await;
        let spawner = ComponentSpawner::new(supervisor.spawner().handle().clone(), pool.handle().clone());

        let (thread_tx, thread_rx) = oneshot::channel();
        spawner
            .noninterruptible("pooled", move |shutdown| async move {
                let _ = thread_tx.send(std::thread::current().name().unwrap_or_default().to_string());
                shutdown.await;
            })
            .on_worker_pool()
            .spawn()
            .await
            .expect("should spawn");

        let thread_name = timeout(Duration::from_secs(5), thread_rx)
            .await
            .expect("child should report its thread promptly")
            .expect("child should not be dropped before reporting");
        assert!(
            thread_name.starts_with("spawner-pool-test"),
            "child must run on the worker pool, but ran on thread {thread_name:?}"
        );

        assert!(supervisor.shutdown().await.is_ok());
        pool.shutdown_background();
    }

    #[tokio::test]
    async fn child_exiting_does_not_shut_the_component_down() {
        // Children are non-significant, so one finishing -- the normal case during a drain -- must not trip the
        // supervisor's `AutoShutdown::AnySignificant` policy and tear the component down with it.
        let supervisor = TestComponentSupervisor::start("test_component").await;

        supervisor
            .spawner()
            .spawn_noninterruptible("brief", |_shutdown| async {})
            .await
            .expect("should spawn");

        supervisor.wait_for_children(0).await;

        // Still accepting work, so the supervisor is still running.
        supervisor
            .spawner()
            .spawn_interruptible("second", std::future::pending::<()>())
            .await
            .expect("supervisor should still be running after a child exited");

        assert!(supervisor.shutdown().await.is_ok());
    }

    #[tokio::test]
    async fn spawned_children_record_poll_metrics() {
        // Every supervised worker's task is timed, and a dynamically spawned child is no exception. The tag is the
        // child's fully qualified process name, which is what gives one series per name rather than per task.
        let recorder = TestRecorder::default();
        let _guard = metrics::set_default_local_recorder(&recorder);

        // The recorder must be installed before the child is spawned: metric handles are resolved once, at spawn.
        let supervisor = TestComponentSupervisor::start("metrics_component").await;
        supervisor
            .spawner()
            .spawn_noninterruptible("instrumented", |shutdown| shutdown)
            .await
            .expect("should spawn");
        assert!(supervisor.shutdown().await.is_ok());

        // Tagged with the child's fully qualified process name, matching what `spawn_traced_named` recorded.
        let polls = recorder.counter((
            "runtime_task_poll_count",
            &[("task_name", "metrics_component.instrumented")],
        ));
        assert!(
            polls.is_some_and(|polls| polls > 0),
            "spawned child should have recorded poll metrics, got {polls:?}"
        );
    }

    #[tokio::test]
    async fn supervisable_child_can_be_configured_before_spawning() {
        let worker_pool_thread_id = Arc::new(Mutex::new(None));
        let worker_pool_thread_id2 = Arc::clone(&worker_pool_thread_id);

        // Create a dedicated worker pool that we'll set as the worker pool on our spawner.
        //
        // This pool tracks the thread ID of the single worker thread we create, such that we can take the thread ID in
        // our running task, and compare it to ensure the worker ran on the worker pool as intended.
        let pool = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .on_thread_start(move || {
                worker_pool_thread_id2
                    .lock()
                    .unwrap()
                    .replace(std::thread::current().id());
            })
            .enable_all()
            .build()
            .expect("should build pool");

        let supervisor = TestComponentSupervisor::start("test_component").await;
        let spawner = ComponentSpawner::new(supervisor.spawner().handle().clone(), pool.handle().clone());

        let (worker, initializations, worker_thread_id) = CountingWorker::new("counting");
        spawner
            .supervisable(worker)
            .on_worker_pool()
            .spawn()
            .await
            .expect("should spawn");

        // `spawn` returns once the supervisor has registered the child, but `initialize` runs inside the child's own
        // task -- on the pool's runtime here -- so wait for it rather than assuming it has been polled.
        supervisor.wait_for_children(1).await;
        wait_for(|| initializations.load(Ordering::SeqCst) == 1).await;

        // Assert where it actually ran, not just that it ran: without this, `on_worker_pool` could be a no-op and the
        // test would still pass.
        let worker_pool_thread_id = worker_pool_thread_id
            .lock()
            .unwrap()
            .expect("worker pool thread should have recorded its thread ID");
        let worker_thread_id = worker_thread_id
            .lock()
            .unwrap()
            .expect("worker should have recorded its thread ID");
        assert_eq!(
            worker_pool_thread_id, worker_thread_id,
            "child should have run on the worker pool"
        );

        assert!(supervisor.shutdown().await.is_ok());
        pool.shutdown_background();
    }

    #[tokio::test]
    async fn supervisable_children_are_restarted_by_default() {
        let supervisor = TestComponentSupervisor::start("test_component").await;

        let (worker, initializations) = FailingOnceWorker::new();
        supervisor
            .spawner()
            .supervisable(worker)
            .spawn()
            .await
            .expect("should spawn");

        // The worker fails its first run, but then runs forever after that, so we should observe two initializations
        // and no more after that.
        wait_for(|| initializations.load(Ordering::SeqCst) == 2).await;
        assert!(supervisor.shutdown().await.is_ok());
    }

    #[tokio::test]
    async fn one_shot_children_are_bounded_by_the_supervisors_budget() {
        // A one-shot child carries no deadline of its own, so the supervisor's budget is what bounds it and a stuck
        // child is aborted when the budget elapses.
        //
        // This deliberately does not try to distinguish that from a child having silently fallen back to the
        // `Supervisable` trait default of five seconds: deadlines resolve to whichever elapses first, so any budget
        // under five seconds produces an identical result, and telling them apart would need a test that runs for
        // longer than five seconds. The distinction is unobservable in practice too -- a component supervisor's budget
        // comes from the topology shutdown timeout, four seconds by default in ADP.
        let supervisor = TestComponentSupervisor::start_with_budget("test_component", Duration::from_millis(200)).await;

        supervisor
            .spawner()
            .spawn_noninterruptible("stuck", |_shutdown| std::future::pending::<()>())
            .await
            .expect("should spawn");
        supervisor.wait_for_children(1).await;

        let started = tokio::time::Instant::now();
        let result = supervisor.shutdown().await;
        let elapsed = started.elapsed();

        assert!(
            matches!(result, Err(SupervisorError::ShutdownTimedOut { aborted: 1 })),
            "the budget should have aborted the stuck child, got {result:?}"
        );
        assert!(
            elapsed < Duration::from_secs(1),
            "the child should have been bounded by the 200ms budget rather than a deadline of its own; took {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn a_significant_child_exiting_stops_the_component() {
        // The counterpart to `child_exiting_does_not_shut_the_component_down`: a component supervisor uses
        // `AutoShutdown::AnySignificant`, so a child marked significant takes the component with it when it terminates
        // -- here on a perfectly clean exit, which is the part that surprises.
        let supervisor = TestComponentSupervisor::start("test_component").await;

        supervisor
            .spawner()
            .noninterruptible("brief", |_shutdown| async {})
            .with_significant(true)
            .spawn()
            .await
            .expect("should spawn");

        let result = supervisor.shutdown().await;
        assert!(
            matches!(result, Err(SupervisorError::SignificantChildExited)),
            "a significant child's exit should have stopped the supervisor, got {result:?}"
        );
    }

    #[tokio::test]
    async fn spawning_after_shutdown_reports_supervisor_gone() {
        let supervisor = TestComponentSupervisor::start("test_component").await;
        let spawner = supervisor.spawner();
        assert!(supervisor.shutdown().await.is_ok());

        let result = spawner.spawn_interruptible("late", std::future::pending::<()>()).await;
        assert!(
            matches!(result, Err(SpawnError::SupervisorGone)),
            "spawning against a stopped supervisor should report `SupervisorGone`, got {result:?}"
        );
    }
}
