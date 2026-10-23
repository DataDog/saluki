//! Test helpers for exercising components that spawn supervised children.
//!
//! Spawning only starts a child while its supervisor is actually running, and
//! [`runtime::spawn`][crate::runtime::spawn] needs an ambient supervisor at all. Neither is true of the obvious test
//! fixture -- `Supervisor::new("test").handle()` -- which looks right but silently drops every child the component
//! under test spawns, or panics outright if the component spawns on the ambient supervisor.
//!
//! [`TestComponentSupervisor`] runs a supervisor like the one topology components run under. Pass its
//! [`handle`][TestComponentSupervisor::handle] where a component wants one, and drive code that spawns on the ambient
//! supervisor inside [`scope`][TestComponentSupervisor::scope]. A component that spawns its own children into its scope
//! must run as a supervised worker instead, through [`spawn_component`][TestComponentSupervisor::spawn_component],
//! which also gives it a shutdown timeout the way the topology does.

use std::future::Future;
use std::time::Duration;

use saluki_common::sync::shutdown::{ShutdownCoordinator, ShutdownHandle};
use tokio::task::futures::TaskLocalFuture;
use tokio::task::JoinHandle;

use crate::runtime::{
    halt::HaltWatch, state::DataspaceRegistry, ChildId, ChildSpecification, IntoWorkerResult, NodeSnapshot, NodeState,
    ShutdownStrategy, SupervisionTreeHandle, Supervisor, SupervisorError, SupervisorHandle,
};

/// The default shutdown timeout for a component run with [`TestComponentSupervisor::spawn_component`] or with
/// [`TestComponentDriver`][super::TestComponentDriver].
///
/// Short enough that a test asserting on a halt doesn't stall, long enough that well-behaved children have ample time
/// to drain on a loaded CI machine.
pub(super) const TEST_COMPONENT_TIMEOUT: Duration = Duration::from_secs(5);

/// Interval between readiness polls.
const POLL_INTERVAL: Duration = Duration::from_millis(5);

/// Overall budget for readiness polling before panicking.
const POLL_TIMEOUT: Duration = Duration::from_secs(5);

/// A started supervisor for components under test.
///
/// The supervisor runs components as the topology supervisor does. Unlike the topology supervisor, it keeps the default
/// restart limit, so it restarts a child that it must restart once in each period before it stops. The topology
/// supervisor stops at the first restart instead. Started on its own, it does not include a component worker. The test
/// drives the component directly, or runs it with [`spawn_component`][Self::spawn_component].
/// [`TestComponentDriver`][super::TestComponentDriver] starts one with the worker of the component as its needed child,
/// the way the topology does.
pub struct TestComponentSupervisor {
    handle: SupervisorHandle,
    component_timeout: Duration,
    tree: SupervisionTreeHandle,
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
        Self::start_with_component_timeout(id, TEST_COMPONENT_TIMEOUT).await
    }

    /// Starts a supervisor named `id`, and gives a specific shutdown timeout to each component that
    /// [`spawn_component`][Self::spawn_component] runs.
    ///
    /// To assert that a component's timeout bounds its children, pick one shorter than the
    /// [`Supervisable`][crate::runtime::Supervisable] default of five seconds.
    ///
    /// # Panics
    ///
    /// Panics if `id` isn't a valid supervisor name, or if the supervisor doesn't start within a few seconds.
    pub async fn start_with_component_timeout(id: &str, component_timeout: Duration) -> Self {
        Self::start_inner(id, component_timeout, DataspaceRegistry::default(), None).await
    }

    /// Starts a supervisor named `id` with `worker` as its one static child, sharing `dataspace` with it.
    ///
    /// The worker is added before the supervisor runs, the way the topology adds a component's worker, so it isn't
    /// counted by [`active_children`][Self::active_children]. The worker keeps its own shutdown timeout:
    /// `component_timeout` applies only to [`spawn_component`][Self::spawn_component].
    ///
    /// # Panics
    ///
    /// Panics if `id` isn't a valid supervisor name, or if the supervisor doesn't start within a few seconds.
    pub(super) async fn start_with_worker(
        id: &str, component_timeout: Duration, dataspace: DataspaceRegistry, worker: ChildSpecification,
    ) -> Self {
        Self::start_inner(id, component_timeout, dataspace, Some(worker)).await
    }

    async fn start_inner(
        id: &str, component_timeout: Duration, dataspace: DataspaceRegistry, worker: Option<ChildSpecification>,
    ) -> Self {
        let mut supervisor = Supervisor::new(id).expect("test supervisor name should be valid");
        if let Some(worker) = worker {
            supervisor.add_worker(worker);
        }

        // Take the handle before moving the supervisor into its task; the handle is usable before the run starts, and
        // is how we observe that it has.
        let handle = supervisor.handle();
        let tree = supervisor.tree_handle();

        let task_dataspace = dataspace.clone();
        let (shutdown_coordinator, process_shutdown) = ShutdownHandle::paired();
        let task = tokio::spawn(async move {
            supervisor
                .run_with_shutdown_inner(process_shutdown, HaltWatch::never(), Some(task_dataspace))
                .await
        });

        let supervisor = Self {
            handle,
            component_timeout,
            tree,
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

    /// Runs a component as a supervised worker of this supervisor, in the same way as the topology.
    ///
    /// `f` receives the shutdown handle of the worker. In production, this handle is the shutdown handle of the
    /// component. Install it into the context of the component before you run the component. To make the component
    /// ignore its supervisor, install a handle that the test owns instead. The worker is needed, as a component is.
    /// Thus, if the worker terminates, the supervisor stops. Also like a component, it has a shutdown timeout covering
    /// everything it spawns.
    ///
    /// Unlike [`scope`][Self::scope], this function gives the component a scope of its own. A component that spawns
    /// its own children needs that scope.
    pub fn spawn_component<F, Fut>(&self, name: &str, f: F) -> ChildId
    where
        F: FnOnce(ShutdownHandle) -> Fut + Send + 'static,
        Fut: Future + Send + 'static,
        Fut::Output: IntoWorkerResult,
    {
        self.handle
            .worker_with_shutdown(name.to_string(), f)
            .with_shutdown_strategy(ShutdownStrategy::Graceful(self.component_timeout))
            .needed()
            .spawn()
    }

    /// Returns a read-only handle to this supervisor's supervision tree.
    pub fn tree_handle(&self) -> SupervisionTreeHandle {
        self.tree.clone()
    }

    /// Returns the number of processes that run anywhere under this supervisor.
    ///
    /// The count also includes the children of children. These are all the children that a component spawns into its
    /// scope when you run it with [`spawn_component`][Self::spawn_component] or
    /// [`TestComponentDriver`][super::TestComponentDriver], and the children of those children.
    /// [`active_children`][Self::active_children] does not count them.
    pub fn running_descendants(&self) -> usize {
        fn count(node: &NodeSnapshot) -> usize {
            node.children
                .iter()
                .map(|child| usize::from(child.state == NodeState::Running) + count(child))
                .sum()
        }

        count(&self.tree.snapshot().root)
    }

    /// Waits until exactly `count` processes run under this supervisor.
    ///
    /// # Panics
    ///
    /// Panics if the count does not reach `count` within a few seconds.
    pub async fn wait_for_running_descendants(&self, count: usize) {
        poll_until(
            POLL_TIMEOUT,
            || self.running_descendants() == count,
            || format!("the supervisor has {count} running descendants"),
        )
        .await;
    }

    /// Returns `true` if the supervisor run is complete.
    pub fn is_finished(&self) -> bool {
        self.task.is_finished()
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
    /// drain before the component starts stopping the children it spawned. A test that drives the two separately
    /// reverses that order. Thus, a needed child terminates while the supervisor still runs normally. This stops the
    /// supervisor, although the actual cause is an orderly shutdown.
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
    /// The result is the supervisor's own: `Err(SupervisorError::ShutdownTimedOut { .. })` means a child didn't stop in
    /// time and was halted, which is usually what a test wants to assert did *not* happen.
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
