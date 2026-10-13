//! Structured concurrency scopes.
//!
//! A supervisor runs a set of children and restarts them when they fail. A _scope_ is the same mechanism, but a single
//! process owns it instead of a supervisor. The children that a worker spawns into its scope belong to that worker.
//! Each child runs as a separate task, but the worker does not finish until all of its children finish.
//!
//! When the worker stops, the scope tells its children to stop too. The worker stops for one of these reasons:
//!
//! - Its body returned.
//! - It failed.
//! - It was told to stop.
//!
//! Every supervised worker has a scope. The runtime creates the scope at the first spawn into it, so a worker that
//! never spawns a child uses no resources for a scope. The supervision tree shows the children of a worker below that
//! worker.
//!
//! # How a scope closes
//!
//! A scope closes when the body of its owner finishes. When the scope closes, it stops each child according to the
//! child's own shutdown strategy, the same as a supervisor stops its children. It tells each graceful child to stop,
//! waits for the child up to the child's own timeout, and then aborts the child if it still runs. It aborts each
//! brutal child at once. The scope adds no deadline of its own, and it does not pass the deadline of its owner to its
//! children. A supervisor's [shutdown budget][crate::runtime::Supervisor::with_shutdown_budget] does not reach into a
//! scope either. Thus, a closure-based child keeps the default strategy of a closure: it is graceful, with a timeout of
//! five seconds.
//!
//! The body can spawn children right before it returns, before they start. If the body returns normally, these
//! children still start, and then they are told to stop. If the body fails, they never start.
//!
//! The scope does not close only because the owner was told to stop. The owner decides when its children stop, because
//! its scope closes only when its body returns. Thus, the owner can stop its children in the order that it chooses. For
//! example, it can refuse new connections first, and then close the connections that it has. The signal that the
//! children get when the scope closes is the last step of that order. A child that already stopped, for example
//! because its input closed, has nothing left to do then.
//!
//! The timeouts of the children start only when the scope closes, after the body of the owner returns. Thus, the
//! shutdown strategy of the owner must cover the time for its own body to stop and the time for its children to stop.
//! If the owner does not stop in time, whatever supervises the owner aborts it. Its children are then aborted with it,
//! without a signal and without being counted, and the runtime logs their names. Along any path in the supervision
//! tree, the outermost finite timeout is the actual limit, and a child timeout that is longer than the timeout of its
//! owner has no effect.
//!
//! # Requested exits and other exits
//!
//! If a child exits while its scope closes, the exit is a requested exit. If a child exits after its owner, or a
//! process above its owner, was told to stop, the exit is also a requested exit, even if the scope is still open. The
//! scope never restarts a child after a requested exit, and a requested exit never fails the owner. Thus, a child that
//! exits while its owner drains cannot cut that drain short.
//!
//! If a child exits while its scope is open and no stop was requested, the child stopped by itself. Two things then
//! decide what occurs next:
//!
//! - Its restart policy decides if the scope restarts it, the same as under a supervisor.
//! - Its _edge_ decides what occurs if the scope does not restart it. The default is a _hosted_ child: the scope reaps
//!   it, and its owner continues. A _needed_ child is a child that its owner cannot work without, and it fails the
//!   owner. To mark a child as needed, use [`ChildBuilder::needed`][crate::runtime::ChildBuilder::needed].
//!
//! A child can stop again and again. If one more restart exceeds the restart limit of the scope, the child fails the
//! owner. The edge of the child has no effect on this, and under a supervisor, the same child stops the supervisor. A
//! restart is a reset. If a child continues to fail, the failure goes to the next level of the supervision tree, and
//! that level tries again.
//!
//! This also applies to the children of a [nested] scope: they fail the process that the scope belongs to.
//!
//! # How to spawn children
//!
//! [`ChildBuilder::spawn_child`][crate::runtime::ChildBuilder::spawn_child] spawns into the scope of the current
//! process. [`current`] returns a handle to that scope, and you can move that handle to other tasks. Some code can run
//! fully outside supervision, for example a primitive that a test constructs. That code uses
//! [`spawn_child_or_detached`][crate::runtime::ChildBuilder::spawn_child_or_detached] or [`nested_or_detached`]. If
//! there is no scope, these functions run the work as a detached task, the same as without scopes.
//!
//! # Scopes owned by values
//!
//! [`nested`] creates a scope that a value owns instead of a process. Its children run under the current process, but
//! the [`ScopeGuard`] that it returns stops them when the guard is dropped. For example, a cache can hold the guard for
//! its background tasks. The tasks then stop when the last copy of the cache is dropped or when the process that owns
//! the cache stops, whichever occurs first. In both cases, nothing restarts the tasks, because they were told to stop.
//! If the process stops before the nested scope can start, the nested scope closes at once, and its tasks never run.
//!
//! # Pre-created scopes
//!
//! [`UnadoptedScope::new`] creates a scope before the process that later owns it exists. The scope holds the children
//! that are spawned into it until a worker adopts it. Then the children start as children of that worker. This is how
//! the process that runs a component can own work that was spawned while the component was built. An example is the
//! background tasks of a cache that is constructed together with the component.
//!
//! The [`UnadoptedScope`] holds the children that wait. Handles to the scope do not. Thus, if the `UnadoptedScope` is
//! dropped before a worker adopts it, the children that wait are dropped too, even if one of them holds a handle to the
//! scope.
//!
//! Only one worker can adopt a scope. If that worker starts again, for example because its supervisor restarts, the
//! new instance gets a new scope of its own. The pre-created scope closed when the first instance stopped.

mod drive;
use std::{
    fmt,
    future::Future,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex, OnceLock,
    },
    time::Duration,
};

use async_trait::async_trait;
use saluki_common::{
    sync::shutdown::{ShutdownCoordinator, ShutdownHandle},
    task::spawn_traced_named,
};
use snafu::Snafu;
use tokio::{runtime::Handle, select, sync::mpsc};
use tracing::{debug, warn};

pub(super) use self::drive::{drive, DriveContext, StopLink};
use super::{
    restart::RestartType,
    supervisable::{InitializationError, ShutdownStrategy, Supervisable, SupervisorFuture},
    supervisor::{ChildConfig, ChildId, ChildSpecification, ChildState, SupervisedChild, WorkerSpec},
};

/// How the termination of a child affects the scope that owns it.
///
/// The scope uses the edge only if a child terminates while the scope is open and its restart policy does not restart
/// it. If the restart policy requires a restart but the child reached the restart limit of its scope, the child fails
/// the owner. The edge of the child has no effect on this.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Edge {
    /// The scope hosts the child: if the child terminates and is not restarted, it is reaped and the owner continues.
    #[default]
    Hosted,

    /// The owner cannot do its work without the child: if the child terminates and is not restarted, the owner fails.
    Needed,
}

/// Errors that make a worker fail because of a child in its scope.
#[derive(Debug, Snafu)]
#[snafu(context(suffix(false)))]
pub enum ScopeError {
    /// A child terminated without a request to stop and was not restarted, and its owner cannot continue without it.
    #[snafu(display("Child '{}' terminated: {}", child_name, reason))]
    ChildTerminated {
        /// The name of the child.
        child_name: String,

        /// How the child terminated.
        reason: String,
    },

    /// A child terminated repeatedly without a request to stop and exceeded the restart limit of its scope.
    #[snafu(display("Child '{}' exceeded its scope's restart limit.", child_name))]
    ChildRestartLimit {
        /// The name of the child.
        child_name: String,
    },
}

/// A child that is queued for a scope and waits for the owner of the scope to start it.
struct PendingScopeChild {
    id: u64,
    spec: SupervisedChild,
    config: ChildConfig,
}

/// The queue of children that wait for the owner of a scope to start them.
type SpawnQueue = mpsc::UnboundedReceiver<PendingScopeChild>;

/// A set of children owned by one process.
///
/// See the [module documentation][self] for a description of a scope. A `Scope` is a handle. A clone of it costs
/// little, and any task can use it. A spawn through a `Scope` always succeeds. But, as with [`tokio::spawn`], a
/// successful spawn does not guarantee that the child runs. If a child is spawned after its scope closed, the child is
/// dropped.
///
/// A handle does not hold the children that wait to start. The owner of the scope holds them, or the
/// [`UnadoptedScope`] before a worker adopts the scope. Thus, a child can hold a handle to its own scope without
/// keeping the scope alive.
#[derive(Clone)]
pub struct Scope {
    shared: Arc<ScopeShared>,
}

struct ScopeShared {
    name: Arc<str>,
    tx: mpsc::UnboundedSender<PendingScopeChild>,
    next_id: AtomicU64,
}

impl Scope {
    /// Creates a scope, and returns the handle to it together with its spawn queue.
    fn with_queue(name: String) -> (Self, SpawnQueue) {
        let (tx, rx) = mpsc::unbounded_channel();
        let scope = Self {
            shared: Arc::new(ScopeShared {
                name: Arc::from(name),
                tx,
                next_id: AtomicU64::new(0),
            }),
        };
        (scope, rx)
    }

    /// Returns the scope's name.
    pub fn name(&self) -> &str {
        &self.shared.name
    }

    /// Spawns a child into the scope.
    ///
    /// Accepts all values that [`SupervisorHandle::spawn`][crate::runtime::SupervisorHandle::spawn] accepts. By
    /// default, the child is [temporary][RestartType::Temporary] and hosted.
    ///
    /// The returned [`ChildId`] identifies the child in this scope.
    pub fn spawn<S, T>(&self, child: T) -> ChildId
    where
        S: ChildState,
        T: Into<ChildSpecification<S>>,
    {
        let (spec, config) = S::into_child_parts(child.into(), RestartType::Temporary).into_parts();
        self.enqueue(spec, config)
    }

    fn enqueue(&self, spec: SupervisedChild, config: ChildConfig) -> ChildId {
        let id = self.shared.next_id.fetch_add(1, Ordering::Relaxed);
        if let Err(e) = self.shared.tx.send(PendingScopeChild { id, spec, config }) {
            // A race with the shutdown of the owner is normal and not an exception, so this message stays at debug
            // level.
            debug!(
                scope = %self.shared.name,
                child_name = e.0.spec.name(),
                "Scope has closed; child will not be started."
            );
        }

        ChildId::from_raw(id)
    }

    /// Runs `fut` with this scope as the current one.
    ///
    /// If `fut` spawns into the current scope, the spawn goes into this scope instead. This applies to spawns through
    /// [`current`], [`ChildBuilder::spawn_child`][crate::runtime::ChildBuilder::spawn_child], or [`nested`]. While
    /// `fut` runs, this scope replaces the scope that was current before.
    pub fn enter<F: Future>(&self, fut: F) -> impl Future<Output = F::Output> {
        CURRENT_SCOPE.scope(ScopeSlot::prefilled(self.clone()), fut)
    }

    /// Runs `f` with this scope as the current one.
    ///
    /// This is the synchronous version of [`enter`][Self::enter].
    pub fn enter_sync<R>(&self, f: impl FnOnce() -> R) -> R {
        CURRENT_SCOPE.sync_scope(ScopeSlot::prefilled(self.clone()), f)
    }
}

impl fmt::Debug for Scope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Scope")
            .field("name", &self.shared.name)
            .finish_non_exhaustive()
    }
}

/// A scope that no process owns yet.
///
/// The scope holds the children that are spawned into it until a worker adopts it. Then the children start as
/// children of that worker. If the `UnadoptedScope` is dropped before a worker adopts it, the scope closes, and it
/// drops the children that wait and each child that is spawned into it later.
///
/// Most code does not use this type. Every supervised worker already has a scope, which is available through
/// [`current`] or [`ChildBuilder::spawn_child`][crate::runtime::ChildBuilder::spawn_child].
pub struct UnadoptedScope {
    scope: Scope,

    /// The children that wait for a worker to adopt the scope. Always present until the scope is adopted.
    rx: Option<SpawnQueue>,
}

impl UnadoptedScope {
    /// Creates a scope that no process owns yet.
    pub fn new<N: Into<String>>(name: N) -> Self {
        let (scope, rx) = Scope::with_queue(name.into());
        Self { scope, rx: Some(rx) }
    }

    /// Returns a handle to the scope, through which children can be spawned into it before a worker adopts it.
    pub fn scope(&self) -> &Scope {
        &self.scope
    }

    /// Describes a worker that owns this scope, for a supervisor to add as a child.
    ///
    /// The worker only holds the children of the scope. It runs until it is told to stop, and then the scope closes.
    /// Use this for work that is spawned outside supervised processes, such as work at startup before the supervision
    /// tree runs. The worker makes that work part of the tree when the tree runs. The worker fails if one of the
    /// children of the scope fails its owner. If the worker is itself spawned into a scope, it passes that failure to
    /// the owner of that scope.
    ///
    /// If the supervisor starts the worker again, for example because the supervisor restarts, the new worker holds
    /// nothing. The scope closed when the first worker stopped, and the scope drops each child that is spawned into it
    /// after that.
    pub fn into_host(self) -> ChildSpecification<WorkerSpec> {
        let name = self.scope.name().to_string();
        ChildSpecification::worker(ScopeHost::new(name, None))
            .with_restart_type(RestartType::Temporary)
            .with_budget_bounded_shutdown()
            .forwarding_failures()
            .adopting(self)
    }

    /// Splits the scope into its handle and its spawn queue, for the worker that adopts it.
    fn into_parts(mut self) -> (Scope, SpawnQueue) {
        let rx = self.rx.take().expect("an unadopted scope holds its spawn queue");
        (self.scope.clone(), rx)
    }
}

impl Drop for UnadoptedScope {
    fn drop(&mut self) {
        // If no worker adopted a scope, the scope drops all children that are in its queue. This is normal. For
        // example, a component that fails before it runs never adopts the scope that it was built in. Thus, this
        // message stays at debug level.
        let Some(mut rx) = self.rx.take() else {
            return;
        };

        rx.close();
        let mut dropped = 0;
        while rx.try_recv().is_ok() {
            dropped += 1;
        }
        if dropped > 0 {
            debug!(
                scope = self.scope.name(),
                dropped, "Scope was never adopted; dropping the children queued on it."
            );
        }
    }
}

impl fmt::Debug for UnadoptedScope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UnadoptedScope")
            .field("name", &self.scope.name())
            .finish_non_exhaustive()
    }
}

/// A pre-created scope for a worker to adopt, shared by every copy of the configuration of that worker.
///
/// The first instance of the worker takes the scope. A later instance finds nothing, and gets a new scope instead. If
/// the last copy of the configuration is dropped before any instance takes the scope, the scope closes and drops the
/// children that wait in it.
#[derive(Clone)]
pub(crate) struct Adoption {
    name: Arc<str>,
    scope: Arc<Mutex<Option<UnadoptedScope>>>,
}

impl Adoption {
    pub(crate) fn new(scope: UnadoptedScope) -> Self {
        Self {
            name: Arc::clone(&scope.scope.shared.name),
            scope: Arc::new(Mutex::new(Some(scope))),
        }
    }

    /// Returns the name of the scope.
    fn name(&self) -> &str {
        &self.name
    }

    /// Takes the scope, if no instance of the worker took it yet.
    fn take(&self) -> Option<UnadoptedScope> {
        self.scope.lock().expect("adoption lock poisoned").take()
    }
}

impl fmt::Debug for Adoption {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Adoption")
            .field("scope", &self.name)
            .finish_non_exhaustive()
    }
}

tokio::task_local! {
    /// The scope of the current process.
    static CURRENT_SCOPE: ScopeSlot;
}

/// Where the current process keeps its scope.
///
/// The runtime installs this slot around the body of each supervised worker. The runtime creates the scope of a worker
/// only at the first spawn into it. Thus, a worker that never spawns a child never has a scope.
#[derive(Clone)]
struct ScopeSlot {
    /// How the slot creates its scope on demand, or `None` if the slot received its scope at creation.
    lazy: Option<Arc<LazyScope>>,

    scope: Arc<OnceLock<Scope>>,
}

/// What a slot needs to create its scope on demand.
struct LazyScope {
    name: Arc<str>,

    /// The spawn queue of the scope after the slot created it, until the worker that owns the slot takes it.
    rx: Mutex<Option<SpawnQueue>>,
}

impl ScopeSlot {
    /// Creates a slot that creates its scope, with the name `name`, at the first use.
    fn lazy(name: Arc<str>) -> Self {
        Self {
            lazy: Some(Arc::new(LazyScope {
                name,
                rx: Mutex::new(None),
            })),
            scope: Arc::new(OnceLock::new()),
        }
    }

    /// Creates a slot that holds `scope`.
    fn prefilled(scope: Scope) -> Self {
        Self {
            lazy: None,
            scope: Arc::new(OnceLock::from(scope)),
        }
    }

    /// Returns the scope, if it exists.
    fn get(&self) -> Option<&Scope> {
        self.scope.get()
    }

    /// Returns the scope, and first creates it if this slot creates its scope on demand.
    fn get_or_create(&self) -> Option<Scope> {
        if let Some(scope) = self.scope.get() {
            return Some(scope.clone());
        }

        let lazy = self.lazy.as_ref()?;
        let scope = self.scope.get_or_init(|| {
            let (scope, rx) = Scope::with_queue(lazy.name.to_string());
            *lazy.rx.lock().expect("scope slot lock poisoned") = Some(rx);
            scope
        });
        Some(scope.clone())
    }

    /// Takes the spawn queue of the scope that this slot created on demand, if the slot created one.
    fn take_receiver(&self) -> Option<SpawnQueue> {
        self.lazy.as_ref()?.rx.lock().expect("scope slot lock poisoned").take()
    }
}

/// Returns the scope of the current process.
///
/// Returns `None` outside of a supervised process. You can move the returned handle to other tasks. Thus, code that
/// does not run as the process that owns the scope can spawn into that scope. A callback-driven executor is an example
/// of such code.
pub fn current() -> Option<Scope> {
    CURRENT_SCOPE.try_with(ScopeSlot::get_or_create).ok().flatten()
}

/// Creates a scope that a value owns instead of a process.
///
/// The children of the scope run under the current process. The supervision tree shows them below that process, under
/// `name`. They stop when the returned guard is dropped or when the current process stops, whichever occurs first. In
/// both cases, nothing restarts them. If the current process stops before the scope can start, the scope closes at
/// once, and its children never run.
///
/// # Panics
///
/// Panics if there is no current scope. This means that the caller does not run in a supervised process. Use
/// [`nested_or_detached`] for code that can run outside supervision.
pub fn nested<N: Into<String>>(name: N) -> ScopeGuard {
    let owner = current().unwrap_or_else(|| {
        panic!(
            "`scope::nested` called outside of a supervised process: there is no current scope to nest in. Use \
             `scope::nested_or_detached` for code that may run outside supervision."
        )
    });

    nested_under(&owner, name.into())
}

/// Creates a scope that a value owns, as [`nested`] does, or a detached guard that acts as a scope outside supervision.
///
/// Outside a supervised process, there is no scope to nest in. Thus, the returned guard runs its children as detached
/// tasks instead. When the guard is dropped, it tells these tasks to stop. This is the behavior that this work has
/// without scopes. For this reason, this function is the correct choice for primitives that code can construct
/// anywhere.
pub fn nested_or_detached<N: Into<String>>(name: N) -> ScopeGuard {
    let name = name.into();
    match current() {
        Some(owner) => nested_under(&owner, name),
        None => ScopeGuard {
            inner: GuardInner::Detached {
                name: Arc::from(name),
                coordinator: ShutdownCoordinator::default(),
            },
        },
    }
}

fn nested_under(owner: &Scope, name: String) -> ScopeGuard {
    let unadopted = UnadoptedScope::new(name.clone());
    let scope = unadopted.scope().clone();
    let (close, close_signal) = ShutdownHandle::paired();

    // The children of the nested scope must run under a process. Thus, a host worker adopts the scope and holds the
    // children until the guard is dropped or the host is told to stop. The failures of the children belong to the
    // owner, so the host passes its own failures to the owner.
    //
    // The specification of the host holds the scope until the host adopts it. If the host never starts, for example
    // because the scope of the owner closed first, the specification is dropped. The nested scope then closes, and it
    // drops the children that wait in it and each child that is spawned into it later.
    let host = ChildSpecification::worker(ScopeHost::new(name, Some(close_signal)))
        .with_restart_type(RestartType::Temporary)
        .with_budget_bounded_shutdown()
        .forwarding_failures()
        .adopting(unadopted);
    owner.spawn(host);

    ScopeGuard {
        inner: GuardInner::Scoped { scope, _close: close },
    }
}

/// A scope that a value owns, which closes when the guard is dropped.
///
/// [`nested`] and [`nested_or_detached`] return this guard. Keep the guard with the value that the scope's children
/// serve, so that they stop when that value is dropped.
#[must_use = "dropping the guard closes the scope, stopping everything spawned into it"]
pub struct ScopeGuard {
    inner: GuardInner,
}

enum GuardInner {
    /// A nested scope, which closes when a drop of the coordinator fires the signal of its host worker.
    Scoped { scope: Scope, _close: ShutdownCoordinator },

    /// Detached tasks that act as a scope outside supervision, where each task holds a handle to this coordinator.
    Detached {
        name: Arc<str>,
        coordinator: ShutdownCoordinator,
    },
}

impl ScopeGuard {
    /// Spawns a child into the scope.
    ///
    /// Returns the child's [`ChildId`], or `None` if the guard is detached and the child started as a detached task. A
    /// detached child receives a shutdown signal that fires when the guard is dropped. It has no other supervision:
    /// nothing restarts it, and nothing waits for it.
    pub fn spawn<T>(&mut self, child: T) -> Option<ChildId>
    where
        T: Into<ChildSpecification<WorkerSpec>>,
    {
        match &mut self.inner {
            GuardInner::Scoped { scope, .. } => Some(scope.spawn(child)),
            GuardInner::Detached { coordinator, .. } => {
                spawn_detached(child.into(), coordinator.register());
                None
            }
        }
    }

    /// Returns `true` if the guard acts as a scope outside supervision and runs its children as detached tasks.
    pub fn is_detached(&self) -> bool {
        matches!(self.inner, GuardInner::Detached { .. })
    }
}

impl fmt::Debug for ScopeGuard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.inner {
            GuardInner::Scoped { scope, .. } => f.debug_struct("ScopeGuard").field("scope", scope).finish(),
            GuardInner::Detached { name, .. } => f
                .debug_struct("ScopeGuard")
                .field("detached", name)
                .finish_non_exhaustive(),
        }
    }
}

/// Runs a worker as a detached task, outside supervision.
///
/// This function initializes the worker with `shutdown` and runs it to completion. It logs failures, because there is
/// no other place to report them. The worker runs on the runtime that it was placed on, if it was placed on one. No
/// other setting of the worker applies, because nothing supervises the worker.
pub(super) fn spawn_detached(child: ChildSpecification<WorkerSpec>, shutdown: ShutdownHandle) {
    let (spec, config) = WorkerSpec::into_child_parts(child, RestartType::Temporary).into_parts();
    let SupervisedChild::Worker(worker) = spec else {
        unreachable!("a worker specification always lowers to a worker");
    };

    let name = worker.name().to_string();
    let _runtime = config.runtime().map(Handle::enter);
    spawn_traced_named(name.clone(), async move {
        let run = match worker.initialize(shutdown).await {
            Ok(run) => run,
            Err(e) => {
                warn!(child_name = %name, error = %e, "Detached child failed to initialize.");
                return;
            }
        };

        if let Err(e) = run.await {
            warn!(child_name = %name, error = %e, "Detached child exited with an error.");
        }
    });
}

/// The worker that a nested or pre-created scope runs under.
///
/// Its body only waits until it is told to stop. Its supervisor can tell it to stop. For a nested scope, a drop of the
/// guard also tells it to stop. When the body returns, the scope that the host adopted closes. That scope contains the
/// actual work.
struct ScopeHost {
    name: String,
    close: Mutex<Option<ShutdownHandle>>,
}

impl ScopeHost {
    fn new(name: String, close: Option<ShutdownHandle>) -> Self {
        Self {
            name,
            close: Mutex::new(close),
        }
    }
}

#[async_trait]
impl Supervisable for ScopeHost {
    fn name(&self) -> &str {
        &self.name
    }

    fn shutdown_strategy(&self) -> ShutdownStrategy {
        // The host has no timeout of its own. Its body returns as soon as it is told to stop, and then its scope stops
        // each child according to the child's own shutdown strategy. The host waits for that, however long it takes.
        ShutdownStrategy::Graceful(Duration::MAX)
    }

    async fn initialize(&self, process_shutdown: ShutdownHandle) -> Result<SupervisorFuture, InitializationError> {
        let close = self.close.lock().expect("scope host lock poisoned").take();

        Ok(Box::pin(async move {
            match close {
                Some(close) => {
                    select! {
                        _ = process_shutdown => {},
                        _ = close => {},
                    }
                }
                None => process_shutdown.await,
            }

            Ok(())
        }))
    }
}

#[cfg(test)]
mod tests;
