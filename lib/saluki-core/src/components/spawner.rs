//! Utilities for spawning child tasks attached to specific components in a topology.
use std::{future::Future, time::Duration};

use saluki_common::sync::shutdown::ShutdownHandle;
use tokio::runtime::Handle;

use crate::runtime::{
    interruptible_worker, noninterruptible_worker, ChildId, ChildSpecification, FnWorker, IntoWorkerResult,
    ShutdownStrategy, SpawnError, Supervisable, SupervisorHandle,
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
/// # Child lifecycle
///
/// All child tasks are spawned as [`temporary`][crate::runtime::RestartType::Temporary] and non-significant, which
/// ensures that the component supervisor does not prematurely exit when they do. This means that, for example, a child
/// task handling a network error doesn't take down the component if it aborts, which would otherwise be the normal
/// behavior for a standard child task under supervision.
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

    /// Spawns a child that observes shutdown itself, on the component's own runtime.
    ///
    /// The function receives the supervisor's shutdown signal and decides when it's finished, which makes this the
    /// right choice for a child that has work to drain.
    ///
    /// To place the child on another runtime, or to bound it more tightly than the component as a whole, use
    /// [`noninterruptible`][Self::noninterruptible] instead.
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

    /// Spawns a child that runs until it completes or shutdown is signalled, on the component's own runtime.
    ///
    /// The future is dropped at whatever await point it's parked on when shutdown fires, so use this only for work
    /// that is safe to interrupt -- a server accept loop, a connection handler, a periodic refresher.
    ///
    /// To place the child on another runtime, or to bound it more tightly than the component as a whole, use
    /// [`interruptible`][Self::interruptible] instead.
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

    /// Describes a child that observes shutdown itself, for configuring before spawning.
    ///
    /// Same semantics as [`spawn_noninterruptible`][Self::spawn_noninterruptible]; use this form only when the child
    /// needs a non-default runtime or grace period. The returned builder does nothing until
    /// [`spawn`][ChildBuilder::spawn] is called.
    pub fn noninterruptible<N, F, Fut>(&self, name: N, f: F) -> ChildBuilder<'_>
    where
        N: Into<String>,
        F: FnOnce(ShutdownHandle) -> Fut + Send + 'static,
        Fut: Future + Send + 'static,
        Fut::Output: IntoWorkerResult,
    {
        ChildBuilder::new(self, noninterruptible_worker(name, f))
    }

    /// Describes a child that runs until it completes or shutdown is signalled, for configuring before spawning.
    ///
    /// Same semantics as [`spawn_interruptible`][Self::spawn_interruptible]; use this form only when the child needs a
    /// non-default runtime or grace period. The returned builder does nothing until [`spawn`][ChildBuilder::spawn] is
    /// called.
    pub fn interruptible<N, Fut>(&self, name: N, fut: Fut) -> ChildBuilder<'_>
    where
        N: Into<String>,
        Fut: Future + Send + 'static,
        Fut::Output: IntoWorkerResult,
    {
        ChildBuilder::new(self, interruptible_worker(name, fut))
    }

    /// Spawns a child from a hand-written [`Supervisable`].
    ///
    /// Use this when a child needs real asynchronous initialization, or state that can't be captured in a closure.
    ///
    /// Unlike the other spawn methods, the child's shutdown strategy comes from the worker itself, so a worker that
    /// reports a finite grace period is held to it in addition to the component's budget -- including the
    /// [`Supervisable`] default of five seconds, which a worker that doesn't override
    /// [`shutdown_strategy`][Supervisable::shutdown_strategy] silently inherits. Return
    /// `ShutdownStrategy::Graceful(Duration::MAX)` from the worker to be bounded by the budget alone, as the other
    /// spawn methods are.
    ///
    /// # Errors
    ///
    /// If the component's supervisor isn't running, or the child specification is invalid, an error is returned.
    pub async fn spawn_supervisable<T>(&self, worker: T) -> Result<ChildId, SpawnError>
    where
        T: Supervisable + 'static,
    {
        self.handle
            .spawn_with(ChildSpecification::one_shot_worker(worker))
            .await
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

/// A child task that has been described but not yet spawned.
///
/// Created by [`ComponentSpawner::noninterruptible`] or [`ComponentSpawner::interruptible`], and consumed by
/// [`spawn`][Self::spawn].
#[must_use = "a child is only started when `spawn` is called"]
pub struct ChildBuilder<'a> {
    spawner: &'a ComponentSpawner,
    worker: FnWorker,
    shutdown_timeout: Option<Duration>,
    runtime: Option<Handle>,
}

impl<'a> ChildBuilder<'a> {
    fn new(spawner: &'a ComponentSpawner, worker: FnWorker) -> Self {
        Self {
            spawner,
            worker,
            shutdown_timeout: None,
            runtime: None,
        }
    }

    /// Runs this child on the shared worker pool owned by the topology, instead of the component's runtime.
    ///
    /// Use this for compute-heavy work -- encoding, serialization, protocol servers -- that shouldn't contend with the
    /// runtime that drives the supervisors and I/O for the topology.
    pub fn on_worker_pool(mut self) -> Self {
        self.runtime = Some(self.spawner.worker_pool.clone());
        self
    }

    /// Runs this child on a specific runtime.
    ///
    /// Prefer [`on_worker_pool`][Self::on_worker_pool] unless the component owns a runtime of its own.
    pub fn on_runtime(mut self, handle: Handle) -> Self {
        self.runtime = Some(handle);
        self
    }

    /// Bounds this child more tightly than the component as a whole.
    ///
    /// By default a child has no deadline of its own and is bounded only by the component's shutdown budget. Set this
    /// when the component wants a particular child abandoned sooner than that -- a deadline it is deliberately
    /// imposing, rather than a guess at how long the child ought to take.
    pub fn with_shutdown_timeout(mut self, timeout: Duration) -> Self {
        self.shutdown_timeout = Some(timeout);
        self
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
        let Self {
            spawner,
            worker,
            shutdown_timeout,
            runtime,
        } = self;

        // `one_shot_worker` registers the child as temporary, which a function-based worker requires: restarting one
        // re-initializes a body that has already been consumed, failing the child and its supervisor.
        //
        // Unless the caller asked for a tighter bound, the child gets no deadline of its own and is bounded solely by
        // the component supervisor's shutdown budget.
        let strategy = ShutdownStrategy::Graceful(shutdown_timeout.unwrap_or(Duration::MAX));
        let mut spec = ChildSpecification::one_shot_worker(worker).with_shutdown_strategy(strategy);

        if let Some(runtime) = runtime {
            spec = spec.with_runtime(runtime);
        }

        spawner.handle.spawn_with(spec).await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    use saluki_metrics::test::TestRecorder;
    use tokio::sync::oneshot;
    use tokio::time::timeout;

    use super::*;
    use crate::components::test_util::TestComponentSupervisor;

    /// How long the drain-participating child in these tests takes to finish after observing shutdown.
    ///
    /// Long enough that a child given any short deadline of its own would be aborted before completing, so the tests
    /// fail if children stop being bounded by the supervisor's budget alone.
    const DRAIN_DURATION: Duration = Duration::from_millis(300);

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
