//! Test helpers for exercising components that spawn supervised children.
//!
//! Spawning only starts a child while its supervisor is actually running, and
//! [`runtime::spawn`][crate::runtime::spawn] needs an ambient supervisor at all. Neither is true of the obvious test
//! fixture -- `Supervisor::new("test").handle()` -- which looks right but silently drops every child the component
//! under test spawns, or panics outright if the component spawns on the ambient supervisor.
//!
//! [`TestComponentSupervisor`] runs a supervisor configured the way the topology configures a component's supervisor.
//! Pass its [`handle`][TestComponentSupervisor::handle] where a component wants one, and drive code that spawns
//! on the ambient supervisor inside [`scope`][TestComponentSupervisor::scope].

use std::future::Future;
use std::time::Duration;

use saluki_common::sync::shutdown::{ShutdownCoordinator, ShutdownHandle};
use tokio::task::futures::TaskLocalFuture;
use tokio::task::JoinHandle;

use crate::runtime::{
    state::DataspaceRegistry, AutoShutdown, ChildSpecification, Supervisor, SupervisorError, SupervisorHandle,
};

/// Shutdown budget for the test supervisor.
///
/// Mirrors production, where a component supervisor -- not its children -- owns the deadline. Short enough that a test
/// asserting on forced-abort behavior doesn't stall, long enough that well-behaved children have ample time to drain
/// on a loaded CI machine.
pub(super) const TEST_SHUTDOWN_BUDGET: Duration = Duration::from_secs(5);

/// Interval between readiness polls.
const POLL_INTERVAL: Duration = Duration::from_millis(5);

/// Overall budget for readiness polling before panicking.
const POLL_TIMEOUT: Duration = Duration::from_secs(5);

/// A running per-component supervisor for tests.
///
/// Configured like the supervisor the topology builds for each component ([`AutoShutdown::AnySignificant`],
/// and a shutdown budget). Started on its own, it has no component worker -- the test drives the component directly.
/// [`TestComponentDriver`][super::TestComponentDriver] starts one with the component's worker as its significant
/// child, the way the topology does.
pub struct TestComponentSupervisor {
    handle: SupervisorHandle,
    dataspace: DataspaceRegistry,
    shutdown_coordinator: Option<ShutdownCoordinator>,
    task: JoinHandle<Result<(), SupervisorError>>,
}

impl TestComponentSupervisor {
    /// Starts a supervisor named `id`, returning once it is running and able to accept spawns.
    ///
    /// # Panics
    ///
    /// Panics if `id` isn't a valid supervisor name, or if the supervisor doesn't start within a few seconds.
    pub async fn start(id: &str) -> Self {
        Self::start_with_budget(id, TEST_SHUTDOWN_BUDGET).await
    }

    /// Starts a supervisor named `id` with a specific shutdown budget.
    ///
    /// Use this to assert that a child is bounded by the budget rather than by a deadline of its own: pick a budget
    /// shorter than the [`Supervisable`][crate::runtime::Supervisable] default of five seconds, and a child that had
    /// silently acquired its own deadline will miss it.
    ///
    /// # Panics
    ///
    /// Panics if `id` isn't a valid supervisor name, or if the supervisor doesn't start within a few seconds.
    pub async fn start_with_budget(id: &str, budget: Duration) -> Self {
        Self::start_inner(id, budget, DataspaceRegistry::default(), None).await
    }

    /// Starts a supervisor named `id` with `worker` as its one static child, sharing `dataspace` with it.
    ///
    /// The worker is added before the supervisor runs, the way the topology adds a component's worker, so it isn't
    /// counted by [`active_children`][Self::active_children].
    ///
    /// # Panics
    ///
    /// Panics if `id` isn't a valid supervisor name, or if the supervisor doesn't start within a few seconds.
    pub(super) async fn start_with_worker(
        id: &str, budget: Duration, dataspace: DataspaceRegistry, worker: ChildSpecification,
    ) -> Self {
        Self::start_inner(id, budget, dataspace, Some(worker)).await
    }

    async fn start_inner(
        id: &str, budget: Duration, dataspace: DataspaceRegistry, worker: Option<ChildSpecification>,
    ) -> Self {
        let mut supervisor = Supervisor::new(id)
            .expect("test supervisor name should be valid")
            .with_auto_shutdown(AutoShutdown::AnySignificant)
            .with_shutdown_budget(budget);
        if let Some(worker) = worker {
            supervisor.add_worker(worker);
        }

        // Take the handle before moving the supervisor into its task; the handle is usable before the run starts, and
        // is how we observe that it has.
        let handle = supervisor.handle();

        let task_dataspace = dataspace.clone();
        let (shutdown_coordinator, process_shutdown) = ShutdownHandle::paired();
        let task = tokio::spawn(async move {
            supervisor
                .run_with_shutdown_inner(process_shutdown, Some(task_dataspace))
                .await
        });

        let supervisor = Self {
            handle,
            dataspace,
            shutdown_coordinator: Some(shutdown_coordinator),
            task,
        };
        // A supervisor whose worker stops straight away stops with it, possibly before it's ever seen running.
        poll_until(
            POLL_TIMEOUT,
            || supervisor.handle.is_running() || supervisor.task.is_finished(),
            || "the test supervisor is running".to_string(),
        )
        .await;

        supervisor
    }

    /// Returns a handle to this supervisor.
    pub fn handle(&self) -> SupervisorHandle {
        self.handle.clone()
    }

    /// Runs `fut` with this supervisor installed as the ambient supervisor.
    ///
    /// Use this to drive code that spawns through [`runtime::spawn`][crate::runtime::spawn] (or the ambient builders
    /// alongside it), which is how a component spawns children when it isn't holding a handle. Outside a scope, that
    /// code would panic for want of an ambient supervisor.
    pub fn scope<F>(&self, fut: F) -> TaskLocalFuture<SupervisorHandle, F>
    where
        F: Future,
    {
        self.handle.scope(fut)
    }

    /// Returns the dataspace shared by the supervisor and its children.
    pub fn dataspace(&self) -> &DataspaceRegistry {
        &self.dataspace
    }

    /// Returns the number of dynamic children currently running.
    pub fn active_children(&self) -> usize {
        self.handle.active_children()
    }

    /// Waits until exactly `count` dynamic children are running.
    ///
    /// # Panics
    ///
    /// Panics if the count doesn't reach `count` within a few seconds.
    pub async fn wait_for_children(&self, count: usize) {
        poll_until(
            POLL_TIMEOUT,
            || self.handle.active_children() == count,
            || format!("the supervisor has {count} running children"),
        )
        .await;
    }

    /// Returns a shutdown handle that fires when this supervisor is asked to shut down.
    ///
    /// Install this into the context of the component under test, in place of a coordinator the test owns outright.
    /// In production a component's shutdown handle *is* its supervisor's, which is what puts the supervisor into its
    /// drain before the component starts stopping the children it spawned. A test driving the two separately inverts
    /// that order, so a child marked significant terminates while the supervisor is still running normally --
    /// tripping [`AutoShutdown`] over what is really an orderly shutdown.
    ///
    /// The supervisor's `select!` is biased towards shutdown, so a signal is always observed here before any child
    /// exit it causes.
    pub fn component_shutdown_handle(&mut self) -> ShutdownHandle {
        match self.shutdown_coordinator.as_mut() {
            Some(coordinator) => coordinator.register(),

            // Only reachable after shutdown has already been signalled, where a handle that never fires would hang
            // the caller. Hand back one that is already triggered instead.
            None => {
                let (coordinator, handle) = ShutdownHandle::paired();
                coordinator.shutdown();
                handle
            }
        }
    }

    /// Signals shutdown without waiting for the supervisor to finish draining.
    ///
    /// Use this where the test needs to observe something between the signal and the supervisor stopping -- that the
    /// component's own `run` returns first, say. Pair it with [`wait`][Self::wait].
    pub fn signal_shutdown(&mut self) {
        if let Some(shutdown_coordinator) = self.shutdown_coordinator.take() {
            shutdown_coordinator.shutdown();
        }
    }

    /// Waits for the supervisor to finish draining its children.
    ///
    /// The result is the supervisor's own: `Err(SupervisorError::ShutdownTimedOut { .. })` means a child ignored
    /// shutdown and had to be aborted, which is usually what a test wants to assert did *not* happen.
    ///
    /// # Panics
    ///
    /// Panics if the supervisor task panicked.
    pub async fn wait(mut self) -> Result<(), SupervisorError> {
        (&mut self.task).await.expect("test supervisor task should not panic")
    }

    /// Signals shutdown and waits for the supervisor to finish draining its children.
    ///
    /// See [`wait`][Self::wait] for how to read the result.
    ///
    /// # Panics
    ///
    /// Panics if the supervisor task panicked.
    pub async fn shutdown(mut self) -> Result<(), SupervisorError> {
        self.signal_shutdown();
        self.wait().await
    }
}

/// Polls `condition` until it holds.
///
/// `describe` says what is being waited for. It's only called if `timeout` elapses, so it can report the state at that
/// point.
///
/// # Panics
///
/// Panics if `condition` doesn't hold within `timeout`.
pub(super) async fn poll_until(
    timeout: Duration, mut condition: impl FnMut() -> bool, describe: impl FnOnce() -> String,
) {
    let poll = async {
        while !condition() {
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    };

    // `tokio::time::timeout` treats a timeout too large to add to the current time as no deadline, rather than
    // overflowing.
    if tokio::time::timeout(timeout, poll).await.is_err() {
        panic!("timed out after {timeout:?} waiting until {}", describe());
    }
}

impl Drop for TestComponentSupervisor {
    fn drop(&mut self) {
        // A test that returns (or panics) without calling `shutdown` shouldn't leak a supervisor and its children into
        // the rest of the run. Dropping the coordinator signals shutdown; the task tears itself down from there.
        self.shutdown_coordinator.take();
    }
}
