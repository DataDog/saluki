//! Builder for describing children.
//!
//! [`ChildSpecification`] is the complete description of a child, and this builder is the only way to configure one:
//! the specification itself carries no public methods. The builder turns an asynchronous function, or a
//! [`Supervisable`], into a child with sensible defaults, and uses the typestate pattern to expose only the settings
//! that make sense for the kind of worker being described.
//!
//! It comes in matching ambient and explicit forms, mirroring the two ways to spawn: the free functions here
//! ([`worker`], [`supervisable`]) target the ambient supervisor, and the identically named methods on
//! [`SupervisorHandle`] target that handle's supervisor. Either way, [`ChildBuilder::spawn`] starts the child on a
//! running supervisor, while [`ChildBuilder::build`] hands it to [`Supervisor::add_worker`][add_worker] to be started
//! when that supervisor runs.
//!
//! [add_worker]: super::Supervisor::add_worker
//!
//! # Child lifecycle, and one-shot vs supervisable
//!
//! We classify tasks as either _one-shot_ or _supervisable_: one-shot tasks are those based on a provided closure,
//! which cannot be reinitialized and so cannot be restarted, and supervisable tasks are those based on an
//! implementation of [`Supervisable`], which allows for (potentially) initializing the underlying task future multiple
//! times.
//!
//! One-shot tasks are always [`temporary`][RestartType::Temporary], since they cannot be reinitialized. Supervisable
//! tasks are [`permanent`][RestartType::Permanent] by default, since their structure exposes a way to initialize the
//! worker again; [`ChildBuilder::transient`] and [`ChildBuilder::temporary`] narrow that.
//!
//! All child tasks default to being marked as non-significant, so their termination -- clean or otherwise -- leaves
//! the supervisor running. This is usually the correct behavior, but a supervisor that cannot function without a
//! particular child may wish to mark it significant. See [`ChildBuilder::with_significant`], which is available only
//! for a child whose restart policy lets it terminate for good; a permanent child is always brought back, so there is
//! no termination for the supervisor to act on.
//!
//! # Shutdown
//!
//! Shutting a subtree down is a _trigger_, not an enforcement. A one-shot child is never handed the shutdown signal at
//! all: it runs until it reaches its own terminal condition -- an input channel closing, a loop finishing, a request
//! completing -- and the supervisor waits for it. That is what lets a set of tasks connected by channels drain in
//! dependency order without any of them having to know that order, and it is why the shutdown signal being invisible
//! here costs nothing.
//!
//! What bounds the wait is the supervisor's [shutdown budget][super::Supervisor::with_shutdown_budget], which covers
//! the whole set of children rather than each guessing at how long it ought to take. A supervisor without a budget has
//! nothing to bound such a child with, so it falls back to the worker's own strategy.
//!
//! Two cases need something other than the default:
//!
//! - Work with no terminal condition -- an endless background loop -- would hold the drain until the budget elapsed.
//!   Give it [`ShutdownStrategy::Brutal`] via [`ChildBuilder::with_shutdown_strategy`] so it is aborted at once.
//! - Work that must observe shutdown to know it should stop, or that needs to run cleanup, isn't a one-shot worker at
//!   all: implement [`Supervisable`] directly, which does receive the signal.
//!
//! [`ChildBuilder::with_shutdown_timeout`] imposes a deadline shorter than the budget when a particular child should
//! be abandoned sooner than its siblings.
//!
//! # Task naming
//!
//! Child task names should generally _not_ contain unique patterns/tokens -- such as monotonic IDs or high-cardinality
//! values -- as they are used for internal telemetry about the task. Generally, task names should be thought of as a
//! category label: if a supervisor manages tasks for handling connections, it should prefer to name them like
//! `conn_handler` instead of `conn_handler_<ID or IP>`.

use std::{future::Future, marker::PhantomData, time::Duration};

use tokio::runtime::Handle;

use super::{
    ChildId, ChildSpecification, FnWorker, IntoWorkerResult, RestartType, ShutdownStrategy, Supervisable,
    SupervisorHandle, WorkerSpec,
};

/// Creates a builder for a child task on the ambient supervisor.
///
/// The ambient counterpart to [`SupervisorHandle::worker`].
///
/// # Panics
///
/// [`ChildBuilder::spawn`] panics if there is no ambient supervisor. See [`spawn`][super::spawn].
pub fn worker<N, Fut>(name: N, fut: Fut) -> ChildBuilder<'static>
where
    N: Into<String>,
    Fut: Future + Send + 'static,
    Fut::Output: IntoWorkerResult,
{
    ChildBuilder::one_shot(BuilderTarget::Ambient, FnWorker::new(name, fut))
}

/// Creates a builder for a supervisable child task on the ambient supervisor.
///
/// The ambient counterpart to [`SupervisorHandle::supervisable`], which documents what a supervisable task is.
///
/// # Panics
///
/// [`ChildBuilder::spawn`] panics if there is no ambient supervisor. See [`spawn`][super::spawn].
pub fn supervisable<T>(worker: T) -> ChildBuilder<'static, Restartable>
where
    T: Supervisable + 'static,
{
    ChildBuilder::restartable(BuilderTarget::Ambient, worker)
}

impl SupervisorHandle {
    /// Creates a builder for a child task built from a plain future.
    ///
    /// The task runs until it reaches its own terminal condition; it is never handed the shutdown signal. See
    /// [`FnWorker`] for what that means at shutdown, and for the two cases that need something else.
    ///
    /// Use this method when advanced configuration of the underlying task is required. Otherwise, prefer
    /// [`spawn_worker`][Self::spawn_worker].
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use saluki_core::runtime::SupervisorHandle;
    /// # async fn encode() {}
    /// # fn example(supervisor: &SupervisorHandle, pool: tokio::runtime::Handle) {
    /// supervisor.worker("encoder", encode()).on_runtime(pool).spawn();
    /// # }
    /// ```
    pub fn worker<N, Fut>(&self, name: N, fut: Fut) -> ChildBuilder<'_>
    where
        N: Into<String>,
        Fut: Future + Send + 'static,
        Fut::Output: IntoWorkerResult,
    {
        ChildBuilder::one_shot(BuilderTarget::Handle(self), FnWorker::new(name, fut))
    }

    /// Creates a builder for a supervisable child task.
    ///
    /// Supervisable tasks are those where the worker already implements [`Supervisable`], which lets the builder serve
    /// as a consistent control surface for spawning both arbitrary asynchronous functions and more full-fledged
    /// workers.
    ///
    /// Supervisable tasks are set to permanently restart by default.
    ///
    /// Use this method when advanced configuration of the underlying task is required. Otherwise, prefer
    /// [`spawn_supervisable`][Self::spawn_supervisable].
    pub fn supervisable<T>(&self, worker: T) -> ChildBuilder<'_, Restartable>
    where
        T: Supervisable + 'static,
    {
        ChildBuilder::restartable(BuilderTarget::Handle(self), worker)
    }

    /// Spawns a child task built from a plain future.
    ///
    /// The task runs until it reaches its own terminal condition; it is never handed the shutdown signal. See
    /// [`FnWorker`] for what that means at shutdown, and for the two cases that need something else.
    ///
    /// Use [`worker`][Self::worker] when advanced configuration of the underlying task is required.
    pub fn spawn_worker<N, Fut>(&self, name: N, fut: Fut) -> ChildId
    where
        N: Into<String>,
        Fut: Future + Send + 'static,
        Fut::Output: IntoWorkerResult,
    {
        self.worker(name, fut).spawn()
    }

    /// Spawns a supervisable child task.
    ///
    /// Supervisable tasks are those where the worker already implements [`Supervisable`], which lets the builder serve
    /// as a consistent control surface for spawning both arbitrary asynchronous functions and more full-fledged
    /// workers.
    ///
    /// Use [`supervisable`][Self::supervisable] when advanced configuration of the underlying task is required.
    pub fn spawn_supervisable<T>(&self, worker: T) -> ChildId
    where
        T: Supervisable + 'static,
    {
        self.supervisable(worker).spawn()
    }
}

mod sealed {
    pub trait Sealed {}
}

/// The kind of worker a [`ChildBuilder`] is describing.
///
/// This trait is sealed, and exists only to mark which configuration a builder makes available: a worker that can be
/// initialized more than once accepts a restart policy, and one that can't doesn't. Implemented by [`OneShot`],
/// [`Restartable`] and [`Terminable`].
pub trait BuilderState: sealed::Sealed {}

/// Marks a builder whose child can terminate without being restarted.
///
/// This trait is sealed, and exists only to gate [`ChildBuilder::with_significant`]: significance asks what the
/// supervisor should do when a child terminates and isn't brought back, so it means nothing for a child that always
/// is. Implemented by [`OneShot`] and [`Terminable`].
pub trait CanTerminate: BuilderState {}

/// Marks a builder whose worker can only be initialized once.
///
/// Closure-based children ([`SupervisorHandle::worker`]) consume their body when they start, so they can never be
/// restarted: they are always [`RestartType::Temporary`], and no restart policy is offered.
pub struct OneShot;

/// Marks a builder whose worker can be initialized more than once.
///
/// A [`Supervisable`] builds its work in [`initialize`][Supervisable::initialize] each time it starts, so it can be
/// restarted, and it is [`RestartType::Permanent`] unless narrowed with [`ChildBuilder::transient`] or
/// [`ChildBuilder::temporary`].
pub struct Restartable;

/// Marks a builder whose worker can be initialized more than once but has been narrowed to a restart policy that lets
/// it terminate for good.
///
/// Reached from [`Restartable`] via [`ChildBuilder::transient`] or [`ChildBuilder::temporary`]. Narrowing is one-way:
/// there is no route back to [`Restartable`], which is what keeps a child from being marked significant and then
/// widened to [`RestartType::Permanent`] behind the flag's back.
pub struct Terminable;

impl sealed::Sealed for OneShot {}
impl BuilderState for OneShot {}
impl CanTerminate for OneShot {}
impl sealed::Sealed for Restartable {}
impl BuilderState for Restartable {}
impl sealed::Sealed for Terminable {}
impl BuilderState for Terminable {}
impl CanTerminate for Terminable {}

/// Builder for a yet-to-be-started child task.
///
/// This is the only way to configure a child: [`ChildSpecification`], which this produces, carries no settings of its
/// own. The builder uses the typestate pattern to expose only the properties that make sense for the child being
/// described, so an invalid combination doesn't need rejecting at runtime because it can't be written down. Two axes
/// govern that: whether the worker can be initialized more than once (which decides whether a restart policy is
/// offered), and whether its policy lets it terminate for good (which decides whether it can be marked significant).
///
/// See [`BuilderState`] and [`CanTerminate`] for the states themselves.
#[must_use = "a child is only described until `spawn` or `build` is called"]
pub struct ChildBuilder<'a, S = OneShot> {
    target: BuilderTarget<'a>,
    spec: ChildSpecification<WorkerSpec>,
    _state: PhantomData<S>,
}

/// Which supervisor a [`ChildBuilder`] spawns onto.
#[derive(Clone, Copy)]
enum BuilderTarget<'a> {
    /// Whichever supervisor is ambient when the child is spawned.
    Ambient,

    /// This specific supervisor.
    Handle(&'a SupervisorHandle),
}

impl<'a, S: BuilderState> ChildBuilder<'a, S> {
    fn new(target: BuilderTarget<'a>, spec: ChildSpecification<WorkerSpec>) -> Self {
        Self {
            target,
            spec,
            _state: PhantomData,
        }
    }

    fn map_spec<F>(self, f: F) -> Self
    where
        F: FnOnce(ChildSpecification<WorkerSpec>) -> ChildSpecification<WorkerSpec>,
    {
        self.map_spec_into(f)
    }

    /// As [`map_spec`][Self::map_spec], but for a transition that also moves the builder to a different state.
    fn map_spec_into<F, S2>(self, f: F) -> ChildBuilder<'a, S2>
    where
        F: FnOnce(ChildSpecification<WorkerSpec>) -> ChildSpecification<WorkerSpec>,
        S2: BuilderState,
    {
        let Self { target, spec, .. } = self;

        ChildBuilder::new(target, f(spec))
    }

    /// Runs this child task on a specific runtime.
    ///
    /// Use this for compute-heavy work -- encoding, serialization, protocol servers -- that shouldn't contend with the
    /// runtime driving the supervisor and its I/O. Only where the task runs changes: the child is still supervised
    /// here, and is still shut down and restarted by this supervisor.
    pub fn on_runtime(self, handle: Handle) -> Self {
        self.map_spec(|spec| spec.with_runtime(handle))
    }

    /// Sets an explicit shutdown timeout for this child task.
    ///
    /// By default a closure-based child has no deadline of its own and is bounded only by the supervisor's shutdown
    /// budget. Set this when the supervisor wants a particular child abandoned sooner than that -- a deadline it is
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

    /// Gives this child no shutdown deadline of its own, leaving it bounded solely by the supervisor's shutdown
    /// budget.
    ///
    /// This is already the default for a closure-based child. A [`Supervisable`] child reports its own strategy, so
    /// this is how one opts into being bounded as part of the group instead -- without which it silently keeps
    /// whatever [`Supervisable::shutdown_strategy`] returns, which may be far shorter than the budget.
    ///
    /// See [`Supervisor::with_shutdown_budget`][budget] for what the budget itself is.
    ///
    /// [budget]: super::Supervisor::with_shutdown_budget
    pub fn with_budget_bounded_shutdown(self) -> Self {
        self.map_spec(|spec| spec.with_budget_bounded_shutdown())
    }

    /// Finishes describing the child without starting it, for [`Supervisor::add_worker`].
    ///
    /// Use this to register a configured child on a supervisor that hasn't started yet; a child described this way is
    /// started when the supervisor runs, and restarted with it. [`spawn`][Self::spawn] is the counterpart for a
    /// supervisor that is already running.
    ///
    /// Whichever supervisor this builder was created against is irrelevant here -- the child belongs to whichever one
    /// it is handed to.
    ///
    /// [`Supervisor::add_worker`]: super::Supervisor::add_worker
    pub fn build(self) -> ChildSpecification<WorkerSpec> {
        self.spec
    }

    /// Spawns the child.
    ///
    /// Returns the child's [`ChildId`]. The child is queued for the supervisor rather than started synchronously, so
    /// it may not be running yet by the time this returns; if the supervisor isn't running, or shuts down before
    /// reaching the child, it never runs at all.
    ///
    /// # Panics
    ///
    /// If this builder targets the ambient supervisor and there isn't one, this panics. See [`spawn`][super::spawn].
    pub fn spawn(self) -> ChildId {
        let Self { target, spec, .. } = self;

        match target {
            BuilderTarget::Ambient => super::spawn(spec),
            BuilderTarget::Handle(supervisor) => supervisor.spawn(spec),
        }
    }
}

impl<'a, S: CanTerminate> ChildBuilder<'a, S> {
    /// Sets whether this child task's termination should stop the supervisor.
    ///
    /// Under [`AutoShutdown::AnySignificant`][auto_shutdown] -- which is what a topology component's supervisor uses
    /// -- a significant child terminating **shuts the supervisor down**: the child is not individually restarted. That
    /// happens even when the child exits cleanly, so this suits a child the supervisor cannot function without, and
    /// not one that is expected to finish on its own.
    ///
    /// For example, a component handling client connections generally shouldn't stop just because one connection
    /// failed, but a component forwarding work to a child task may become inoperable if that task dies and cannot be
    /// reattached to the necessary channels or state without recreating the component.
    ///
    /// Offered only for a child that can actually terminate for good -- a one-shot child, or a supervisable one
    /// narrowed with [`transient`][ChildBuilder::transient] or [`temporary`][ChildBuilder::temporary]. A permanent
    /// child is always restarted and so never reaches this path, which makes marking one significant a contradiction
    /// rather than a setting.
    ///
    /// Defaults to `false`.
    ///
    /// [auto_shutdown]: super::AutoShutdown::AnySignificant
    pub fn with_significant(self, significant: bool) -> Self {
        self.map_spec(|spec| spec.with_significant(significant))
    }
}

impl<'a> ChildBuilder<'a, OneShot> {
    fn one_shot<T>(target: BuilderTarget<'a>, worker: T) -> Self
    where
        T: Supervisable + 'static,
    {
        let spec = ChildSpecification::one_shot_worker(worker).with_budget_bounded_shutdown();

        Self::new(target, spec)
    }
}

impl<'a> ChildBuilder<'a, Restartable> {
    fn restartable<T>(target: BuilderTarget<'a>, worker: T) -> Self
    where
        T: Supervisable + 'static,
    {
        Self::new(
            target,
            ChildSpecification::worker(worker).with_restart_type(RestartType::Permanent),
        )
    }

    /// Restarts this child task only when it terminates abnormally.
    ///
    /// A clean exit is taken at face value and the child stays stopped; a failure is restarted. Use this for work that
    /// has a natural end but whose failure means it never got there.
    ///
    /// Narrows the restart policy to [`RestartType::Transient`], which makes
    /// [`with_significant`][ChildBuilder::with_significant] available: a child that can stop for good is one whose
    /// termination the supervisor may want to act on.
    pub fn transient(self) -> ChildBuilder<'a, Terminable> {
        self.map_spec_into(|spec| spec.with_restart_type(RestartType::Transient))
    }

    /// Never restarts this child task.
    ///
    /// However it terminates -- cleanly or by failing -- the child stays stopped. Use this for work that is meant to
    /// run once, where a retry would be wrong rather than merely redundant.
    ///
    /// Narrows the restart policy to [`RestartType::Temporary`], which makes
    /// [`with_significant`][ChildBuilder::with_significant] available: a child that can stop for good is one whose
    /// termination the supervisor may want to act on.
    pub fn temporary(self) -> ChildBuilder<'a, Terminable> {
        self.map_spec_into(|spec| spec.with_restart_type(RestartType::Temporary))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    };
    use std::thread::ThreadId;

    use async_trait::async_trait;
    use saluki_common::sync::shutdown::ShutdownHandle;
    use saluki_metrics::test::TestRecorder;
    use tokio::sync::oneshot;
    use tokio::time::timeout;

    use super::*;
    use crate::components::test_util::TestComponentSupervisor;
    use crate::runtime::{InitializationError, SupervisorError, SupervisorFuture};
    use crate::test_support::wait_until;

    /// How long the drain-participating child in these tests takes to finish after its input closes.
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

        async fn initialize(&self, process_shutdown: ShutdownHandle) -> Result<SupervisorFuture, InitializationError> {
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

        async fn initialize(&self, process_shutdown: ShutdownHandle) -> Result<SupervisorFuture, InitializationError> {
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

    #[tokio::test]
    async fn worker_drains_to_its_terminal_condition_before_the_supervisor_stops() {
        // The central promise of a one-shot worker: shutdown is a trigger, not an enforcement, so a worker that is
        // still finishing when the drain starts gets to finish. A clean supervisor result is the assertion that
        // matters -- `ShutdownTimedOut` would mean it was aborted instead.
        let supervisor = TestComponentSupervisor::start("test_component").await;

        // The child does real work *after* its input closes, so it would be cut short if children were given a
        // near-zero deadline rather than being bounded by the supervisor's budget.
        let drained = Arc::new(AtomicUsize::new(0));
        let child_drained = Arc::clone(&drained);
        let (input_tx, input_rx) = oneshot::channel::<()>();

        supervisor.handle().spawn_worker("drainer", async move {
            let _ = input_rx.await;
            tokio::time::sleep(DRAIN_DURATION).await;
            child_drained.fetch_add(1, Ordering::SeqCst);
        });

        supervisor.wait_for_children(1).await;

        // Close the child's input, then immediately shut down while it is still draining.
        drop(input_tx);
        let result = supervisor.shutdown().await;
        assert!(
            result.is_ok(),
            "child should have drained rather than been aborted: {result:?}"
        );
        assert_eq!(drained.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn worker_without_a_terminal_condition_is_aborted_when_marked_brutal() {
        // The counterpart: work that never ends has to say so, otherwise it holds the drain open until the budget
        // elapses. `Brutal` is how it says so, and the supervisor still reports a clean shutdown.
        let supervisor = TestComponentSupervisor::start_with_budget("test_component", Duration::from_secs(30)).await;

        supervisor
            .handle()
            .worker("endless", std::future::pending::<()>())
            .with_shutdown_strategy(ShutdownStrategy::Brutal)
            .spawn();
        supervisor.wait_for_children(1).await;

        let started = tokio::time::Instant::now();
        let result = supervisor.shutdown().await;
        let elapsed = started.elapsed();

        assert!(
            result.is_ok(),
            "an aborted brutal child is not an unclean shutdown: {result:?}"
        );
        assert!(
            elapsed < Duration::from_secs(5),
            "the child should have been aborted at once rather than held to the 30s budget; took {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn endless_worker_is_bounded_by_the_supervisors_budget() {
        // A one-shot child carries no deadline of its own, so the supervisor's budget is what bounds it and a stuck
        // child is aborted when the budget elapses.
        //
        // This deliberately does not try to distinguish that from a child having silently fallen back to the
        // `Supervisable` trait default of five seconds: deadlines resolve to whichever elapses first, so any budget
        // under five seconds produces an identical result.
        let supervisor = TestComponentSupervisor::start_with_budget("test_component", Duration::from_millis(200)).await;

        supervisor.handle().spawn_worker("stuck", std::future::pending::<()>());
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
    async fn on_runtime_places_the_child_on_that_runtime() {
        // `on_runtime` must actually change where the child's task runs, not just where it was spawned from.
        let pool = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .thread_name("builder-pool-test")
            .enable_all()
            .build()
            .expect("should build pool");

        let supervisor = TestComponentSupervisor::start("test_component").await;

        let (thread_tx, thread_rx) = oneshot::channel();
        supervisor
            .handle()
            .worker("pooled", async move {
                let _ = thread_tx.send(std::thread::current().name().unwrap_or_default().to_string());
                std::future::pending::<()>().await;
            })
            .on_runtime(pool.handle().clone())
            .with_shutdown_strategy(ShutdownStrategy::Brutal)
            .spawn();

        let thread_name = timeout(Duration::from_secs(5), thread_rx)
            .await
            .expect("child should report its thread promptly")
            .expect("child should not be dropped before reporting");
        assert!(
            thread_name.starts_with("builder-pool-test"),
            "child must run on the given runtime, but ran on thread {thread_name:?}"
        );

        assert!(supervisor.shutdown().await.is_ok());
        pool.shutdown_background();
    }

    #[tokio::test]
    async fn child_exiting_does_not_shut_the_supervisor_down() {
        // Children are non-significant, so one finishing -- the normal case during a drain -- must not trip the
        // supervisor's `AutoShutdown::AnySignificant` policy and tear the component down with it.
        let supervisor = TestComponentSupervisor::start("test_component").await;
        let handle = supervisor.handle();

        // Wait for the child to have actually run and exited. `wait_for_children(0)` would be vacuously true before
        // the supervisor ever picked it up, so the exit this test is about would never be observed.
        let exited = Arc::new(AtomicUsize::new(0));
        let child_exited = Arc::clone(&exited);
        handle.spawn_worker("brief", async move {
            child_exited.fetch_add(1, Ordering::SeqCst);
        });
        wait_until("the child has run and exited", || exited.load(Ordering::SeqCst) == 1).await;
        supervisor.wait_for_children(0).await;

        // The supervisor survived that exit: it is still running and still accepting work.
        assert!(handle.is_running());
        handle
            .worker("second", std::future::pending::<()>())
            .with_shutdown_strategy(ShutdownStrategy::Brutal)
            .spawn();

        assert!(supervisor.shutdown().await.is_ok());
    }

    #[tokio::test]
    async fn a_significant_child_exiting_stops_the_supervisor() {
        // The counterpart: a component supervisor uses `AutoShutdown::AnySignificant`, so a child marked significant
        // takes the component with it when it terminates -- here on a perfectly clean exit, which is the part that
        // surprises.
        let supervisor = TestComponentSupervisor::start("test_component").await;
        let handle = supervisor.handle();

        handle.worker("brief", async {}).with_significant(true).spawn();

        // Spawning only queues the child, so wait for the supervisor to actually stop itself rather than racing our
        // own shutdown against the child ever starting.
        wait_until("the supervisor has stopped", || !handle.is_running()).await;

        let result = supervisor.shutdown().await;
        assert!(
            matches!(result, Err(SupervisorError::SignificantChildExited)),
            "a significant child's exit should have stopped the supervisor, got {result:?}"
        );
    }

    #[tokio::test]
    async fn spawned_children_record_poll_metrics() {
        // Every supervised worker's task is timed, and a child spawned through the builder is no exception. The tag is
        // the child's fully qualified process name, which is what gives one series per name rather than per task.
        let recorder = TestRecorder::default();
        let _guard = metrics::set_default_local_recorder(&recorder);

        // The recorder must be installed before the child is spawned: metric handles are resolved once, at spawn.
        let supervisor = TestComponentSupervisor::start("metrics_component").await;

        // Spawning only queues the child, so wait for it to actually run before shutting down -- otherwise there's no
        // guarantee it was ever polled, and therefore none that it recorded anything.
        let (ran_tx, ran_rx) = oneshot::channel();
        supervisor.handle().spawn_worker("instrumented", async move {
            let _ = ran_tx.send(());
        });
        ran_rx.await.expect("child should have run");

        assert!(supervisor.shutdown().await.is_ok());

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
        // A `Supervisable` child goes through the same builder, including runtime placement.
        let worker_pool_thread_id = Arc::new(Mutex::new(None));
        let worker_pool_thread_id2 = Arc::clone(&worker_pool_thread_id);

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

        let (worker, initializations, worker_thread_id) = CountingWorker::new("counting");
        supervisor
            .handle()
            .supervisable(worker)
            .on_runtime(pool.handle().clone())
            .spawn();

        // `spawn` returns once the child is queued, and `initialize` runs inside the child's own task -- on the pool's
        // runtime here -- so wait for it rather than assuming it has been polled.
        supervisor.wait_for_children(1).await;
        wait_until("the worker has initialized once", || {
            initializations.load(Ordering::SeqCst) == 1
        })
        .await;

        // Assert where it actually ran, not just that it ran: without this, `on_runtime` could be a no-op and the test
        // would still pass.
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
            "child should have run on the given runtime"
        );

        assert!(supervisor.shutdown().await.is_ok());
        pool.shutdown_background();
    }

    #[tokio::test]
    async fn supervisable_children_are_restarted_by_default() {
        let supervisor = TestComponentSupervisor::start("test_component").await;

        let (worker, initializations) = FailingOnceWorker::new();
        supervisor.handle().spawn_supervisable(worker);

        // The worker fails its first run, but then runs forever after that, so we should observe two initializations
        // and no more after that.
        wait_until("the worker has initialized twice", || {
            initializations.load(Ordering::SeqCst) == 2
        })
        .await;
        assert!(supervisor.shutdown().await.is_ok());
    }

    #[tokio::test]
    async fn spawning_after_shutdown_never_starts_the_child() {
        let supervisor = TestComponentSupervisor::start("test_component").await;
        let handle = supervisor.handle();
        assert!(supervisor.shutdown().await.is_ok());

        let started = Arc::new(AtomicUsize::new(0));
        let child_started = Arc::clone(&started);
        handle.spawn_worker("late", async move {
            child_started.fetch_add(1, Ordering::SeqCst);
        });

        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            started.load(Ordering::SeqCst),
            0,
            "a child spawned against a stopped supervisor must never run"
        );
    }
}
