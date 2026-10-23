//! Builder for describing children.
//!
//! [`ChildSpecification`] is the complete description of a child, and this builder is the only way to configure one:
//! the specification itself carries no public methods. The builder turns an asynchronous function, or a
//! [`Supervisable`], into a child with sensible defaults, and uses the typestate pattern to expose only the settings
//! that make sense for the kind of worker being described.
//!
//! The builder has an ambient form and an explicit form, one for each of the two ways to spawn. The free functions in
//! this module ([`worker`], [`supervisable`]) spawn into the current process. The methods with the same names on
//! [`SupervisorHandle`] spawn onto the supervisor of that handle. In both forms, [`ChildBuilder::spawn`] starts the
//! child. [`ChildBuilder::build`] gives the child to [`Supervisor::add_worker`][add_worker], and the supervisor starts
//! the child when it runs.
//!
//! [add_worker]: super::Supervisor::add_worker
//!
//! # Restart policy
//!
//! You do not choose the restart policy of a child directly. The policy follows from two facts: whether the child can
//! be started again, and whether its owner can work without it. See the [runtime documentation][restart] for how the
//! policy is derived, and for the defaults.
//!
//! [restart]: super#restart-policy
//!
//! # Shutdown
//!
//! A subtree shutdown is a _trigger_, not an enforcement. A one-shot child built with [`worker`] never receives the
//! shutdown signal. It runs until it reaches its own terminal condition, and its owner waits for it. Examples of a
//! terminal condition are an input channel that closes, a loop that finishes, and a request that completes.
//!
//! This behavior lets a set of tasks that are connected by channels drain in dependency order. The tasks do not have to
//! know that order. Because of this behavior, it is not a problem that a one-shot child cannot see the shutdown signal.
//!
//! Each child is held to its own shutdown strategy, whether its owner is a supervisor or a scope. A closure-based child
//! is graceful, with a timeout of five seconds. When a timeout expires, the process is halted along with everything in
//! its scope.
//!
//! Two cases need something other than the default:
//!
//! - Work with no terminal condition holds the drain open until its timeout expires. An endless background loop is an
//!   example. Give it [`ShutdownStrategy::Brutal`] with [`ChildBuilder::with_shutdown_strategy`] so that it is halted
//!   at once.
//! - If work must observe shutdown to know that it must stop, or must run cleanup, build it with
//!   [`worker_with_shutdown`]. The body of that worker receives the shutdown signal. You can also implement
//!   [`Supervisable`] directly.
//!
//! [`ChildBuilder::with_shutdown_timeout`] sets a timeout other than the default. Use it if a particular child needs
//! more time to drain, or must be halted sooner than its siblings.
//!
//! # Children of the current process
//!
//! [`ChildBuilder::spawn`] makes a child a child of the current process, and the [scope][super::scope] of that process
//! holds it. The process does not finish until its children finish. When the body of the process returns, the scope
//! stops the children, each according to its own shutdown strategy. [`ChildBuilder::spawn_sibling`] gives the child
//! to the nearest supervisor above the current process instead.
//!
//! # Task naming
//!
//! Child task names should generally _not_ contain unique patterns/tokens -- such as monotonic IDs or high-cardinality
//! values -- as they are used for internal telemetry about the task. Generally, task names should be thought of as a
//! category label: if a supervisor manages tasks for handling connections, it should prefer to name them like
//! `conn_handler` instead of `conn_handler_<ID or IP>`.

use std::{future::Future, marker::PhantomData, time::Duration};

use saluki_common::sync::shutdown::ShutdownHandle;
use tokio::runtime::Handle;

#[cfg(any(test, feature = "test-util"))]
use super::ExitObserver;
use super::{
    scope::{self, Edge, Scope, UnadoptedScope},
    ChildId, ChildSpecification, FnWorker, IntoWorkerResult, RestartType, ShutdownStrategy, Supervisable, Supervisor,
    SupervisorHandle, SupervisorSpec, WorkerSpec,
};

/// Creates a builder for a child task of the current process.
///
/// The ambient counterpart to [`SupervisorHandle::worker`].
///
/// # Panics
///
/// [`ChildBuilder::spawn`] panics if it is called outside supervision. See [`spawn`][super::spawn].
pub fn worker<N, Fut>(name: N, fut: Fut) -> ChildBuilder<'static>
where
    N: Into<String>,
    Fut: Future + Send + 'static,
    Fut::Output: IntoWorkerResult,
{
    ChildBuilder::new_one_shot(BuilderTarget::Ambient, FnWorker::new(name, fut))
}

/// Creates a builder for a child task of the current process, with a body that receives the shutdown signal.
///
/// The ambient counterpart to [`SupervisorHandle::worker_with_shutdown`].
///
/// # Panics
///
/// [`ChildBuilder::spawn`] panics if it is called outside supervision. See [`spawn`][super::spawn].
pub fn worker_with_shutdown<N, F, Fut>(name: N, f: F) -> ChildBuilder<'static>
where
    N: Into<String>,
    F: FnOnce(ShutdownHandle) -> Fut + Send + 'static,
    Fut: Future + Send + 'static,
    Fut::Output: IntoWorkerResult,
{
    ChildBuilder::new_one_shot(BuilderTarget::Ambient, FnWorker::with_shutdown(name, f))
}

/// Creates a builder for a supervisable child task of the current process.
///
/// The ambient counterpart to [`SupervisorHandle::supervisable`], which documents what a supervisable task is.
///
/// # Panics
///
/// [`ChildBuilder::spawn`] panics if it is called outside supervision. See [`spawn`][super::spawn].
pub fn supervisable<T>(worker: T) -> ChildBuilder<'static, Restartable>
where
    T: Supervisable + 'static,
{
    ChildBuilder::new_restartable(BuilderTarget::Ambient, worker)
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
        ChildBuilder::new_one_shot(BuilderTarget::Handle(self), FnWorker::new(name, fut))
    }

    /// Creates a builder for a child task with a body that receives the shutdown signal.
    ///
    /// `f` receives the [`ShutdownHandle`] of the task when the task starts. Use this method for work that must observe
    /// shutdown to stop in time. Such work stops when it observes shutdown, not when it reaches its own terminal
    /// condition. An example is a long-lived connection that must close gracefully. See [`FnWorker::with_shutdown`].
    pub fn worker_with_shutdown<N, F, Fut>(&self, name: N, f: F) -> ChildBuilder<'_>
    where
        N: Into<String>,
        F: FnOnce(ShutdownHandle) -> Fut + Send + 'static,
        Fut: Future + Send + 'static,
        Fut::Output: IntoWorkerResult,
    {
        ChildBuilder::new_one_shot(BuilderTarget::Handle(self), FnWorker::with_shutdown(name, f))
    }

    /// Creates a builder for a supervisable child task.
    ///
    /// Supervisable tasks are those where the worker already implements [`Supervisable`], which lets the builder serve
    /// as a consistent control surface for spawning both arbitrary asynchronous functions and more full-fledged
    /// workers.
    ///
    /// A supervisable task can usually be started again, so its owner restarts it when it fails. A task that reports
    /// through [`can_restart`][Supervisable::can_restart] that it cannot be started again is never restarted. See the
    /// [runtime documentation][super#restart-policy] for how its restart policy is decided.
    ///
    /// Use this method when advanced configuration of the underlying task is required. Otherwise, prefer
    /// [`spawn_supervisable`][Self::spawn_supervisable].
    pub fn supervisable<T>(&self, worker: T) -> ChildBuilder<'_, Restartable>
    where
        T: Supervisable + 'static,
    {
        ChildBuilder::new_restartable(BuilderTarget::Handle(self), worker)
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

    /// Creates a builder for a nested supervisor.
    ///
    /// You can give a [`Supervisor`] directly to [`spawn`][Self::spawn] or [`Supervisor::add_worker`]. Most callers
    /// need only that. This builder is for the settings that are not available in that way: whether the subtree is
    /// needed or hosted, and whether it [stops with its owner][NestedSupervisorBuilder::stops_with_owner] in a scope.
    ///
    /// Unlike [`supervisable`][Self::supervisable], there is no placement or shutdown setting. A nested supervisor runs
    /// wherever its parent does and its children carry their own placement, and its drain is bounded by its children's
    /// timeouts and by its owner.
    pub fn nested_supervisor(&self, supervisor: Supervisor) -> NestedSupervisorBuilder<'_> {
        NestedSupervisorBuilder::new(BuilderTarget::Handle(self), supervisor)
    }
}

/// Creates a builder for a nested supervisor spawned into the current process.
///
/// The ambient counterpart to [`SupervisorHandle::nested_supervisor`], which documents what the builder is for.
///
/// # Panics
///
/// [`NestedSupervisorBuilder::spawn`] panics if it is called outside supervision. See [`spawn`][super::spawn].
pub fn nested_supervisor(supervisor: Supervisor) -> NestedSupervisorBuilder<'static> {
    NestedSupervisorBuilder::new(BuilderTarget::Ambient, supervisor)
}

mod sealed {
    pub trait Sealed {}
}

/// The kind of worker a [`ChildBuilder`] is describing.
///
/// This trait is sealed. It exists only to mark which configuration a builder makes available. [`OneShot`] and
/// [`Restartable`] implement it.
pub trait BuilderState: sealed::Sealed {}

/// Marks a builder whose worker can only be started once.
///
/// Closure-based children ([`SupervisorHandle::worker`]) consume their body when they start, so they can never be
/// restarted.
pub struct OneShot;

/// Marks a builder whose worker can usually be started more than once.
///
/// A [`Supervisable`] builds its work in [`initialize`][Supervisable::initialize] each time it starts, so it can be
/// restarted. A [`Supervisable`] that consumes something when it starts says so through
/// [`can_restart`][Supervisable::can_restart], and its owner never restarts it.
pub struct Restartable;

impl sealed::Sealed for OneShot {}
impl BuilderState for OneShot {}
impl sealed::Sealed for Restartable {}
impl BuilderState for Restartable {}

/// Builder for a yet-to-be-started child task.
///
/// This is the only way to configure a child: [`ChildSpecification`], which this produces, carries no settings of its
/// own. The builder uses the typestate pattern to expose only the properties that apply to the child that it describes.
/// See [`BuilderState`] for the states.
#[must_use = "a child is only described until `spawn` or `build` is called"]
pub struct ChildBuilder<'a, S = OneShot> {
    target: BuilderTarget<'a>,
    spec: ChildSpecification<WorkerSpec>,
    _state: PhantomData<S>,
}

/// The place where a [`ChildBuilder`] spawns its child.
#[derive(Clone, Copy)]
enum BuilderTarget<'a> {
    /// The scope of the current process, or the ambient supervisor if there is no current scope.
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
        let Self { target, spec, .. } = self;

        Self::new(target, f(spec))
    }

    /// Runs this child task on a specific runtime.
    ///
    /// Use this method for compute-heavy work that must not compete with the runtime that drives its owner and its I/O.
    /// Examples are encoding, serialization, and protocol servers. This method changes only where the task runs. The
    /// owner of the child still supervises, shuts down, and restarts the child.
    pub fn on_runtime(self, handle: Handle) -> Self {
        self.map_spec(|spec| spec.with_runtime(handle))
    }

    /// Sets an explicit shutdown timeout for this child task.
    ///
    /// A closure-based child defaults to five seconds, after which it is halted. Set this if a particular child needs
    /// more time to drain, or must be halted sooner. A timeout longer than its owner's has no effect, since the child
    /// is halted along with its owner.
    ///
    /// For a [`Supervisable`] child, this overrides the strategy the task reports for itself.
    pub fn with_shutdown_timeout(self, timeout: Duration) -> Self {
        self.with_shutdown_strategy(ShutdownStrategy::Graceful(timeout))
    }

    /// Sets the explicit shutdown strategy used for this child task.
    pub fn with_shutdown_strategy(self, strategy: ShutdownStrategy) -> Self {
        self.map_spec(|spec| spec.with_shutdown_strategy(strategy))
    }

    /// Marks this child as one that its owner cannot work without.
    ///
    /// If the child stops without a request to stop and can only be started once, its owner also stops and fails. This
    /// occurs even when the child exits cleanly. Thus, use this method for a child that its owner cannot work without.
    /// Do not use it for a child that you expect to finish by itself. If the child can be started again, its owner
    /// restarts it after any such stop, not only after a failure.
    ///
    /// For example, one failed connection is usually not a reason to stop a component that handles client connections.
    /// But a component that forwards work to a child task can become inoperable if that task stops unexpectedly. This
    /// occurs if the task cannot be attached again to the necessary channels or state unless the component is created
    /// again.
    ///
    /// See the [runtime documentation][super#restart-policy] for the default.
    pub fn needed(self) -> Self {
        self.map_spec(|spec| spec.with_edge(Edge::Needed))
    }

    /// Marks this child as one that its owner runs but does not need.
    ///
    /// If the child stops without a request to stop, and its owner does not restart it, the owner reaps the child and
    /// continues. The owner does not restart a child that can only be started once, or a child that exited cleanly. If
    /// the child can be started again, its owner restarts it only if it failed, and accepts a clean exit as final. But
    /// if the child fails more often than the restart limit of its owner allows, the owner also fails, as for a needed
    /// child.
    ///
    /// See the [runtime documentation][super#restart-policy] for the default.
    pub fn hosted(self) -> Self {
        self.map_spec(|spec| spec.with_edge(Edge::Hosted))
    }

    /// Sets whether the supervisor stops when this child task terminates.
    ///
    /// This setting has an effect only under a supervisor that uses the deprecated
    /// [`with_auto_shutdown`][super::Supervisor::with_auto_shutdown]. It also has no effect on a child that its
    /// supervisor always restarts, such as a child that can start again and that you declare with
    /// [`Supervisor::add_worker`] without marking it [hosted][Self::hosted]. The supervisor logs a warning for each
    /// child where the setting has no effect.
    #[deprecated(note = "mark the child `needed` instead, which stops its owner when it terminates for good")]
    pub fn with_significant(self, significant: bool) -> Self {
        self.map_spec(|spec| spec.with_significant(significant))
    }

    /// Makes this child adopt `scope` as its own and start what was spawned into it before the child started.
    pub(crate) fn adopting(self, scope: UnadoptedScope) -> Self {
        self.map_spec(|spec| spec.adopting(scope))
    }

    /// Gives this child a resource group of its own, with the same name as the child.
    ///
    /// The child then does not count against the resource group of its owner. Use this for a worker that represents
    /// something with its own resource accounting, such as a topology component.
    pub(crate) fn with_own_resource_group(self) -> Self {
        self.map_spec(|spec| spec.with_own_resource_group())
    }

    /// Makes `observer` receive how this child ended, after its scope closed. See [`ExitObserver`].
    #[cfg(any(test, feature = "test-util"))]
    pub(crate) fn observing_exit(self, observer: ExitObserver) -> Self {
        self.map_spec(|spec| spec.observing_exit(observer))
    }

    /// Finishes describing the child without starting it, for [`Supervisor::add_worker`].
    ///
    /// Use this method to register a configured child on a supervisor that has not started yet. The supervisor starts
    /// this child when it runs. When the supervisor restarts, the child restarts with it. To start a child now, use
    /// [`spawn`][Self::spawn].
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
    /// A builder that was created from a [`SupervisorHandle`] spawns the child onto that supervisor. Other builders
    /// make the child a child of the current process, and the [scope][super::scope] of that process holds it. See
    /// [`spawn`][super::spawn].
    ///
    /// Returns the [`ChildId`] of the child. This method queues the child and does not start it synchronously. Thus,
    /// when this method returns, the child can still be in the queue. If its owner does not run, or stops before it
    /// processes the child, the child never runs.
    ///
    /// # Panics
    ///
    /// Panics if this builder does not target a specific supervisor and code outside supervision calls this method. See
    /// [`spawn`][super::spawn].
    pub fn spawn(self) -> ChildId {
        let Self { target, spec, .. } = self;

        match target {
            BuilderTarget::Ambient => super::spawn(spec),
            BuilderTarget::Handle(supervisor) => supervisor.spawn(spec),
        }
    }

    /// Spawns the child onto the ambient supervisor, the nearest supervisor above the current process.
    ///
    /// The child is a sibling of the current process only if a supervisor runs that process directly. For a process
    /// that runs in a scope, the child goes to the nearest supervisor above it instead. See
    /// [`spawn_sibling`][super::spawn_sibling].
    ///
    /// A builder that was created from a [`SupervisorHandle`] spawns the child onto that supervisor, exactly as
    /// [`spawn`][Self::spawn] does.
    ///
    /// # Panics
    ///
    /// Panics if this builder does not target a specific supervisor and there is no ambient supervisor.
    pub fn spawn_sibling(self) -> ChildId {
        let Self { target, spec, .. } = self;

        match target {
            BuilderTarget::Ambient => super::spawn_sibling(spec),
            BuilderTarget::Handle(supervisor) => supervisor.spawn(spec),
        }
    }

    /// Spawns the child into `scope`.
    pub fn spawn_in(self, scope: &Scope) -> ChildId {
        let Self { spec, .. } = self;

        scope.spawn(spec)
    }

    /// Spawns the child into the scope of the current process, or as a detached task outside supervision.
    ///
    /// Returns the [`ChildId`] of the child, or `None` if there was no current scope and the child started as a
    /// detached task instead. A detached child never receives a shutdown signal, and nothing restarts it or waits for
    /// it. This is how the same work runs without supervision at all. Thus, this method is the correct choice for code
    /// that can be called from any location, such as a primitive that a test can construct.
    pub fn spawn_or_detached(self) -> Option<ChildId> {
        let Self { spec, .. } = self;

        match scope::current() {
            Some(scope) => Some(scope.spawn(spec)),
            None => {
                scope::spawn_detached(spec, ShutdownHandle::noop());
                None
            }
        }
    }
}

impl<'a> ChildBuilder<'a, OneShot> {
    fn new_one_shot<T>(target: BuilderTarget<'a>, worker: T) -> Self
    where
        T: Supervisable + 'static,
    {
        let spec = ChildSpecification::worker(worker);

        Self::new(target, spec)
    }
}

impl<'a> ChildBuilder<'a, Restartable> {
    fn new_restartable<T>(target: BuilderTarget<'a>, worker: T) -> Self
    where
        T: Supervisable + 'static,
    {
        Self::new(target, ChildSpecification::worker(worker))
    }

    /// Restarts this child task only when it terminates abnormally.
    #[deprecated(note = "mark the child `hosted` instead, which restarts it only if it fails")]
    pub fn transient(self) -> Self {
        self.map_spec(|spec| spec.with_restart_type(RestartType::Transient))
    }

    /// Never restarts this child task.
    #[deprecated(note = "implement `Supervisable::can_restart` instead, if the child can only be started once")]
    pub fn temporary(self) -> Self {
        self.map_spec(|spec| spec.with_restart_type(RestartType::Temporary))
    }
}

/// Builder for a yet-to-be-started nested supervisor.
///
/// The counterpart to [`ChildBuilder`] for a child that is itself a [`Supervisor`], and deliberately a much smaller
/// surface. A nested supervisor has no placement of its own, because it runs where its parent runs and its children
/// carry their own placement. It also has no shutdown deadline of its own, because it limits its own drain through its
/// children. Thus, the settings that remain are whether it is needed or hosted, and whether it stops with its owner.
#[must_use = "a child is only described until `spawn` or `build` is called"]
pub struct NestedSupervisorBuilder<'a> {
    target: BuilderTarget<'a>,
    spec: ChildSpecification<SupervisorSpec>,
}

impl<'a> NestedSupervisorBuilder<'a> {
    fn new(target: BuilderTarget<'a>, supervisor: Supervisor) -> Self {
        Self {
            target,
            spec: ChildSpecification::from(supervisor),
        }
    }

    fn map_spec<F>(self, f: F) -> Self
    where
        F: FnOnce(ChildSpecification<SupervisorSpec>) -> ChildSpecification<SupervisorSpec>,
    {
        let Self { target, spec } = self;

        Self { target, spec: f(spec) }
    }

    /// Marks this subtree as one that its owner cannot work without.
    ///
    /// See [`ChildBuilder::needed`], which this method mirrors.
    pub fn needed(self) -> Self {
        self.map_spec(|spec| spec.with_edge(Edge::Needed))
    }

    /// Marks this subtree as one that its owner runs but does not need.
    ///
    /// See [`ChildBuilder::hosted`], which this method mirrors.
    pub fn hosted(self) -> Self {
        self.map_spec(|spec| spec.with_edge(Edge::Hosted))
    }

    /// Tells this subtree to stop as soon as its owner is told to stop, instead of when the owner's scope closes.
    ///
    /// Use this for ingress whose output the owner's body consumes, such as a server whose handlers feed a channel that
    /// the body drains. The server stops accepting work and finishes what it accepted while the body keeps draining.
    ///
    /// This takes effect only in the scope of a worker that takes the shutdown signal (see
    /// [`Supervisable::wants_shutdown_signal`]). A supervisor tells all of its children to stop at once anyway, and a
    /// worker that takes no signal has nothing to stop the subtree with.
    ///
    /// The scope keeps the subtree's specification until the subtree exits, and the specification holds whatever the
    /// subtree was built with, such as the sender in a request handler. Thus, the channel closes once the subtree exits.
    /// The body **MUST NOT** hold a sender of that channel itself, or the channel never closes. See the
    /// [scope documentation][super::scope#nested-supervisors-that-stop-with-their-owner].
    pub fn stops_with_owner(self) -> Self {
        self.map_spec(|spec| spec.stopping_with_owner())
    }

    /// Restarts this subtree only when it terminates abnormally.
    #[deprecated(note = "mark the subtree `hosted` instead, which restarts it only if it fails")]
    pub fn transient(self) -> Self {
        self.map_spec(|spec| spec.with_restart_type(RestartType::Transient))
    }

    /// Never restarts this subtree.
    #[deprecated(note = "a supervisor can always be started again; mark it `hosted` or `needed` instead")]
    pub fn temporary(self) -> Self {
        self.map_spec(|spec| spec.with_restart_type(RestartType::Temporary))
    }

    /// Sets whether the parent supervisor stops when this subtree terminates.
    #[deprecated(note = "mark the subtree `needed` instead, which stops its owner when it terminates for good")]
    pub fn with_significant(self, significant: bool) -> Self {
        self.map_spec(|spec| spec.with_significant(significant))
    }

    /// Finishes describing the child without starting it, for [`Supervisor::add_worker`].
    ///
    /// Use this method to register a configured subtree on a supervisor that has not started yet. To start the subtree
    /// now, use [`spawn`][Self::spawn].
    ///
    /// Whichever supervisor this builder was created against is irrelevant here -- the child belongs to whichever one
    /// it is handed to.
    pub fn build(self) -> ChildSpecification<SupervisorSpec> {
        self.spec
    }

    /// Spawns the nested supervisor.
    ///
    /// This method works as [`ChildBuilder::spawn`] does. It spawns onto the target supervisor if the builder has one.
    /// If not, it spawns the nested supervisor as a child of the current process.
    ///
    /// # Panics
    ///
    /// Panics if this builder does not target a specific supervisor and code outside supervision calls this method. See
    /// [`spawn`][super::spawn].
    pub fn spawn(self) -> ChildId {
        let Self { target, spec } = self;

        match target {
            BuilderTarget::Ambient => super::spawn(spec),
            BuilderTarget::Handle(supervisor) => supervisor.spawn(spec),
        }
    }

    /// Spawns the nested supervisor onto the ambient supervisor, the nearest supervisor above the current process.
    ///
    /// This method works as [`ChildBuilder::spawn_sibling`] does.
    ///
    /// # Panics
    ///
    /// Panics if this builder does not target a specific supervisor and there is no ambient supervisor.
    pub fn spawn_sibling(self) -> ChildId {
        let Self { target, spec } = self;

        match target {
            BuilderTarget::Ambient => super::spawn_sibling(spec),
            BuilderTarget::Handle(supervisor) => supervisor.spawn(spec),
        }
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
    /// Long enough to tell a completed drain apart from a halt.
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
        restartable: bool,
    }

    impl FailingOnceWorker {
        fn new() -> (Self, Arc<AtomicUsize>) {
            let initializations = Arc::new(AtomicUsize::new(0));
            (
                Self {
                    initializations: Arc::clone(&initializations),
                    restartable: true,
                },
                initializations,
            )
        }

        /// Makes this worker report that it cannot be started again, as a worker that consumes its state does.
        fn one_shot(mut self) -> Self {
            self.restartable = false;
            self
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

        fn can_restart(&self) -> bool {
            self.restartable
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

        // The child does real work *after* its input closes, which a premature halt would cut short.
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
    async fn worker_without_a_terminal_condition_is_halted_at_once_when_marked_brutal() {
        // The counterpart: work that never ends has to say so, otherwise it holds the drain open until its owner's
        // timeout. `Brutal` is how it says so, and the shutdown is still clean.
        let supervisor =
            TestComponentSupervisor::start_with_component_timeout("test_component", Duration::from_secs(30)).await;

        supervisor.spawn_component("component", |shutdown| async move {
            crate::runtime::worker("endless", std::future::pending::<()>())
                .with_shutdown_strategy(ShutdownStrategy::Brutal)
                .spawn();
            shutdown.await;
        });
        supervisor.wait_for_running_descendants(2).await;

        let started = tokio::time::Instant::now();
        let result = supervisor.shutdown().await;
        let elapsed = started.elapsed();

        assert!(
            result.is_ok(),
            "halting a brutal child is not an unclean shutdown: {result:?}"
        );
        assert!(
            elapsed < Duration::from_secs(5),
            "the child should have been halted at once rather than held to the component's 30s timeout; took {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn endless_worker_is_halted_with_the_component_it_belongs_to() {
        // The child keeps its default five-second timeout, but the component's shorter timeout halts it first.
        let supervisor =
            TestComponentSupervisor::start_with_component_timeout("test_component", Duration::from_millis(200)).await;

        supervisor.spawn_component("component", |shutdown| async move {
            crate::runtime::worker("stuck", std::future::pending::<()>()).spawn();
            shutdown.await;
        });
        supervisor.wait_for_running_descendants(2).await;

        let started = tokio::time::Instant::now();
        let result = supervisor.shutdown().await;
        let elapsed = started.elapsed();

        assert!(
            matches!(result, Err(SupervisorError::ShutdownTimedOut { aborted: 1 })),
            "the stuck child should have been halted with the component, and counted, got {result:?}"
        );
        assert!(
            elapsed < Duration::from_secs(1),
            "the child should have been halted with the component at 200ms rather than at its own timeout; took {elapsed:?}"
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
        // Spawned children are hosted. Thus, a child that finishes must not stop the supervisor, because that also
        // stops the component. During a drain, a child that finishes is the normal case.
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
    async fn a_needed_child_exiting_stops_the_supervisor() {
        // The counterpart: when a child that is marked needed terminates permanently, its supervisor also stops. Here
        // the child exits cleanly. It is not obvious that a clean exit also stops the supervisor.
        let supervisor = TestComponentSupervisor::start("test_component").await;
        let handle = supervisor.handle();

        handle.worker("brief", async {}).needed().spawn();

        // Spawning only queues the child, so wait for the supervisor to actually stop itself rather than racing our
        // own shutdown against the child ever starting.
        wait_until("the supervisor has stopped", || !handle.is_running()).await;

        let result = supervisor.shutdown().await;
        assert!(
            matches!(result, Err(SupervisorError::SignificantChildExited)),
            "a needed child's exit should have stopped the supervisor, got {result:?}"
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
    #[allow(deprecated)]
    async fn a_worker_that_cannot_restart_overrides_an_explicit_restart_policy() {
        // A worker that reports that it cannot start again is never restarted by its owner. This is also true if the
        // deprecated `transient` gave it an explicit policy that restarts it after a failure.
        let supervisor = TestComponentSupervisor::start("test_component").await;

        let (worker, initializations) = FailingOnceWorker::new();
        supervisor.handle().supervisable(worker.one_shot()).transient().spawn();

        // The child starts and fails. If the supervisor restarted it, the child would stay registered, because each
        // later run waits for shutdown.
        wait_until("the worker has initialized once", || {
            initializations.load(Ordering::SeqCst) == 1
        })
        .await;
        supervisor.wait_for_children(0).await;
        assert_eq!(initializations.load(Ordering::SeqCst), 1);

        assert!(supervisor.shutdown().await.is_ok());
    }

    #[tokio::test]
    #[allow(deprecated)]
    async fn a_significant_child_that_is_always_restarted_is_accepted() {
        // The deprecated `with_significant` is available on the builder of a child that can start again. Declared in
        // advance, such a child is needed and permanent, so the flag has no effect. The supervisor warns about that,
        // but must accept the child and run it.
        let (worker, initializations, _) = CountingWorker::new("counting");
        let mut sup = Supervisor::new("test_sup").expect("supervisor name should be valid");
        sup.add_worker(supervisable(worker).with_significant(true).build());

        let (tx, rx) = oneshot::channel::<()>();
        let run = tokio::spawn(async move {
            sup.run_with_shutdown(async move {
                let _ = rx.await;
            })
            .await
        });
        wait_until("the worker has initialized once", || {
            initializations.load(Ordering::SeqCst) == 1
        })
        .await;

        tx.send(()).expect("supervisor should still be running");
        let result = run.await.expect("supervisor task should not panic");
        assert!(result.is_ok(), "supervisor should stop cleanly: {result:?}");
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
