//! Ambient supervisor context.
//!
//! Every supervised process runs with a handle to the supervisor that supervises it installed as a task-local value.
//! That makes [`spawn`] possible: code anywhere inside a supervised process -- the worker's body, its
//! [`initialize`][crate::runtime::Supervisable::initialize], or any helper it calls -- can add a child to its own
//! supervisor without a handle being threaded to it.
//!
//! The value is installed per supervised task, so it doesn't leak into tasks started with [`tokio::spawn`]: those run
//! outside supervision, and [`spawn`] panics there rather than silently attaching the child somewhere unexpected. Use
//! [`SupervisorHandle::scope`] to deliberately establish an ambient supervisor for such a task.

use std::future::Future;

use tokio::task::futures::TaskLocalFuture;

use super::supervisor::{ChildId, ChildSpecification, ChildState, SupervisorHandle};

tokio::task_local! {
    /// Handle to the supervisor supervising the currently running process.
    pub(super) static CURRENT_SUPERVISOR: SupervisorHandle;
}

/// Spawns a child on the ambient supervisor.
///
/// The ambient supervisor is the one supervising the currently running process, so the child becomes a _sibling_ of
/// the caller rather than its descendant.
///
/// Accepts anything [`Supervisor::add_worker`][crate::runtime::Supervisor::add_worker] accepts: a bare
/// [`Supervisable`][crate::runtime::Supervisable], a [`Supervisor`][crate::runtime::Supervisor] to run as a nested
/// supervision subtree, or a [`ChildSpecification`] configured in detail. Unless the specification says otherwise, the
/// child is [`temporary`][crate::runtime::RestartType::Temporary]; see [`SupervisorHandle::spawn`] for what that
/// implies.
///
/// This mirrors [`tokio::spawn`] in both shape and guarantees: it always succeeds, and success means the child was
/// accepted, not that it will run. A supervisor that shuts down before it reaches the queued child never starts it.
///
/// # Panics
///
/// Panics if there is no ambient supervisor, which means the caller isn't running as (or within) a supervised process.
/// Establish one with [`SupervisorHandle::scope`], or spawn through a [`SupervisorHandle`] directly.
///
/// # Examples
///
/// ```no_run
/// # use saluki_core::runtime::{spawn, FnWorker};
/// # async fn refresh() {}
/// spawn(FnWorker::new("refresher", refresh()));
/// ```
pub fn spawn<S, T>(child: T) -> ChildId
where
    S: ChildState,
    T: Into<ChildSpecification<S>>,
{
    CURRENT_SUPERVISOR
        .try_with(|supervisor| supervisor.spawn(child))
        .unwrap_or_else(|_| {
            panic!(
                "`runtime::spawn` called outside of a supervised process: there is no ambient supervisor to spawn on. \
                 Spawn through a `SupervisorHandle` directly, or establish an ambient supervisor with \
                 `SupervisorHandle::scope`."
            )
        })
}

impl SupervisorHandle {
    /// Runs `fut` with this supervisor installed as the ambient supervisor.
    ///
    /// Anything `fut` spawns through [`spawn`] becomes a child of this supervisor. Supervised processes already have
    /// their own supervisor installed, so this is for code that runs outside supervision -- a test driving a component
    /// directly, or a task started with [`tokio::spawn`] that needs to attach children to a known supervisor.
    ///
    /// The ambient supervisor applies only for the duration of `fut`, and shadows any supervisor already installed.
    pub fn scope<F>(&self, fut: F) -> TaskLocalFuture<SupervisorHandle, F>
    where
        F: Future,
    {
        CURRENT_SUPERVISOR.scope(self.clone(), fut)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    use std::time::Duration;

    use async_trait::async_trait;
    use saluki_common::sync::shutdown::ShutdownHandle;
    use tokio::sync::oneshot;

    use super::*;
    use crate::runtime::{
        ChildSpecification, FnWorker, InitializationError, ShutdownStrategy, Supervisable, Supervisor, SupervisorError,
        SupervisorFuture,
    };
    use crate::test_support::wait_until;

    /// A worker that runs a caller-supplied action and then waits for shutdown.
    ///
    /// `when` decides whether the action runs during initialization or once the worker is running, which is the
    /// distinction these tests care about: the ambient supervisor has to be in place for both.
    struct ActionWorker {
        name: &'static str,
        during_init: bool,
        action: std::sync::Mutex<Option<Box<dyn FnOnce() + Send>>>,
    }

    impl ActionWorker {
        fn new<F>(name: &'static str, during_init: bool, action: F) -> Self
        where
            F: FnOnce() + Send + 'static,
        {
            Self {
                name,
                during_init,
                action: std::sync::Mutex::new(Some(Box::new(action))),
            }
        }

        fn take_action(&self) -> Box<dyn FnOnce() + Send> {
            self.action
                .lock()
                .expect("action mutex poisoned")
                .take()
                .expect("worker should only run once")
        }
    }

    #[async_trait]
    impl Supervisable for ActionWorker {
        fn name(&self) -> &str {
            self.name
        }

        async fn initialize(&self, process_shutdown: ShutdownHandle) -> Result<SupervisorFuture, InitializationError> {
            let action = self.take_action();
            if self.during_init {
                action();

                return Ok(Box::pin(async move {
                    process_shutdown.await;
                    Ok(())
                }));
            }

            Ok(Box::pin(async move {
                action();
                process_shutdown.await;
                Ok(())
            }))
        }
    }

    /// Wraps an endless worker so the supervisor aborts it at shutdown instead of waiting for a terminal condition
    /// it doesn't have.
    fn endless(worker: FnWorker) -> ChildSpecification {
        ChildSpecification::worker(worker).with_shutdown_strategy(ShutdownStrategy::Brutal)
    }

    /// Runs a supervisor holding a single worker that performs `action`, then shuts it down and returns the result.
    ///
    /// Waits for `action` to have actually run before signalling shutdown. A fixed sleep would work most of the time
    /// and fail on a loaded runner, which is the whole reason [`wait_until`] exists.
    async fn run_worker_that<F>(during_init: bool, action: F) -> Result<(), SupervisorError>
    where
        F: FnOnce() + Send + 'static,
    {
        let acted = Arc::new(AtomicUsize::new(0));
        let worker_acted = Arc::clone(&acted);

        let mut sup = Supervisor::new("ambient-sup").expect("supervisor name should be valid");
        sup.add_worker(ActionWorker::new("actor", during_init, move || {
            action();
            worker_acted.fetch_add(1, Ordering::SeqCst);
        }));

        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let handle = sup.handle();
        let run = tokio::spawn(async move { sup.run_with_shutdown(shutdown_rx).await });

        wait_until("supervisor is running", || handle.is_running()).await;
        wait_until("the worker has run its action", || acted.load(Ordering::SeqCst) == 1).await;

        let _ = shutdown_tx.send(());
        tokio::time::timeout(Duration::from_secs(5), run)
            .await
            .expect("supervisor should stop promptly")
            .expect("supervisor task should not panic")
    }

    #[tokio::test]
    async fn worker_spawns_onto_its_own_supervisor() {
        // The ambient supervisor of a running worker is the one supervising it, so what it spawns becomes its sibling
        // -- and is drained when that supervisor stops.
        let started = Arc::new(AtomicUsize::new(0));
        let child_started = Arc::clone(&started);

        let result = run_worker_that(false, move || {
            spawn(endless(FnWorker::new("sibling", async move {
                child_started.fetch_add(1, Ordering::SeqCst);
                std::future::pending::<()>().await;
            })));
        })
        .await;

        assert!(result.is_ok(), "supervisor should have stopped cleanly: {result:?}");
        assert_eq!(started.load(Ordering::SeqCst), 1, "the spawned sibling should have run");
    }

    #[tokio::test]
    async fn worker_can_spawn_during_initialization() {
        // Initialization runs inside the worker's own task, so the ambient supervisor is already in place there. A
        // worker that sets up helpers before it starts running shouldn't have to defer them until after.
        let started = Arc::new(AtomicUsize::new(0));
        let child_started = Arc::clone(&started);

        let result = run_worker_that(true, move || {
            spawn(endless(FnWorker::new("helper", async move {
                child_started.fetch_add(1, Ordering::SeqCst);
                std::future::pending::<()>().await;
            })));
        })
        .await;

        assert!(result.is_ok(), "supervisor should have stopped cleanly: {result:?}");
        assert_eq!(
            started.load(Ordering::SeqCst),
            1,
            "a child spawned during initialization should have run"
        );
    }

    #[tokio::test]
    #[should_panic(expected = "outside of a supervised process")]
    async fn spawning_without_an_ambient_supervisor_panics() {
        // Nothing sensible can be done with a child here, and silently dropping it would hide the mistake until
        // whatever was spawned turned out never to have run.
        spawn(endless(FnWorker::new("orphan", std::future::pending::<()>())));
    }

    #[tokio::test]
    async fn ambient_supervisor_is_not_inherited_by_tokio_spawn() {
        // A task started with `tokio::spawn` is outside supervision entirely, even when its parent was supervised.
        // Inheriting the ambient supervisor there would quietly attach children to a supervisor that has no
        // relationship to the task's actual lifetime.
        let escaped = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&escaped);

        let result = run_worker_that(false, move || {
            tokio::spawn(async move {
                if CURRENT_SUPERVISOR.try_with(|_| ()).is_err() {
                    observed.fetch_add(1, Ordering::SeqCst);
                }
            });
        })
        .await;

        assert!(result.is_ok(), "supervisor should have stopped cleanly: {result:?}");
        assert_eq!(
            escaped.load(Ordering::SeqCst),
            1,
            "a `tokio::spawn`ed task must not see an ambient supervisor"
        );
    }

    #[tokio::test]
    async fn scope_establishes_an_ambient_supervisor() {
        // The escape hatch for code that isn't running under supervision: tests, and tasks bridging back into a known
        // supervisor.
        let started = Arc::new(AtomicUsize::new(0));
        let child_started = Arc::clone(&started);

        let mut sup = Supervisor::new("scoped-sup").expect("supervisor name should be valid");
        // A supervisor with no children at all still idles until shutdown, so nothing else is needed here.
        let handle = sup.handle();
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let run = tokio::spawn(async move { sup.run_with_shutdown(shutdown_rx).await });
        wait_until("supervisor is running", || handle.is_running()).await;

        handle
            .scope(async {
                spawn(endless(FnWorker::new("scoped", async move {
                    child_started.fetch_add(1, Ordering::SeqCst);
                    std::future::pending::<()>().await;
                })));
            })
            .await;

        wait_until("the scoped child has started", || started.load(Ordering::SeqCst) == 1).await;

        let _ = shutdown_tx.send(());
        let result = tokio::time::timeout(Duration::from_secs(5), run)
            .await
            .expect("supervisor should stop promptly")
            .expect("supervisor task should not panic");
        assert!(result.is_ok(), "supervisor should have stopped cleanly: {result:?}");
    }
}
