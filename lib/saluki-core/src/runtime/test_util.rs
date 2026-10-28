//! Test helpers for exercising code that spawns supervised children.
//!
//! Spawning only starts a child while its supervisor is actually running, and
//! [`runtime::spawn`][crate::runtime::spawn] needs an ambient supervisor at all. Neither is true of the obvious test
//! fixture -- `Supervisor::new("test").handle()` -- which looks right but silently drops every child the code under test
//! spawns, or panics outright if that code spawns on the ambient supervisor.
//!
//! [`TestSupervisor`] runs a real supervisor. Pass its [`handle`][TestSupervisor::handle] where code wants one, and
//! drive code that spawns on the ambient supervisor inside [`scope`][TestSupervisor::scope]. Code that spawns its own
//! children into its scope must run as a supervised worker instead, through
//! [`spawn_worker`][TestSupervisor::spawn_worker], which also gives it a shutdown timeout covering those children.
//! Code that runs as a static worker, such as a server, is added with [`start_with`][TestSupervisor::start_with].

use std::future::Future;
use std::time::Duration;

use saluki_common::sync::shutdown::{ShutdownCoordinator, ShutdownHandle};
use tokio::task::futures::TaskLocalFuture;
use tokio::task::JoinHandle;

use crate::runtime::{
    halt::HaltWatch, state::DataspaceRegistry, ChildId, ChildSpecification, IntoWorkerResult, NodeSnapshot, NodeState,
    ShutdownStrategy, SupervisionTreeHandle, Supervisor, SupervisorError, SupervisorHandle,
};

/// The default shutdown timeout for a worker run with [`TestSupervisor::spawn_worker`].
///
/// Short enough that a test asserting on a halt doesn't stall, long enough that well-behaved children have ample time
/// to drain on a loaded CI machine.
pub(crate) const TEST_WORKER_TIMEOUT: Duration = Duration::from_secs(5);

/// Interval between readiness polls.
const POLL_INTERVAL: Duration = Duration::from_millis(5);

/// Overall budget for readiness polling before panicking.
const POLL_TIMEOUT: Duration = Duration::from_secs(5);

/// A started supervisor for code under test.
///
/// It keeps the default restart limit, so it restarts a child that it must restart once in each period before it
/// stops. Started on its own, it has no children. The test drives the code directly, runs it with
/// [`spawn_worker`][Self::spawn_worker], or adds it as a static worker with [`start_with`][Self::start_with].
pub struct TestSupervisor {
    handle: SupervisorHandle,
    worker_timeout: Duration,
    tree: SupervisionTreeHandle,
    dataspace: DataspaceRegistry,
    shutdown_coordinator: Option<ShutdownCoordinator>,
    task: JoinHandle<Result<(), SupervisorError>>,
}

impl TestSupervisor {
    /// Starts a supervisor named `id`, returning once it is running and able to accept spawns.
    ///
    /// # Panics
    ///
    /// Panics if `id` isn't a valid supervisor name, or if the supervisor doesn't start within a few seconds.
    pub async fn start(id: &str) -> Self {
        Self::start_with_worker_timeout(id, TEST_WORKER_TIMEOUT).await
    }

    /// Starts a supervisor named `id`, and gives a specific shutdown timeout to each worker that
    /// [`spawn_worker`][Self::spawn_worker] runs.
    ///
    /// To assert that a worker's timeout bounds its children, pick one shorter than the
    /// [`Supervisable`][crate::runtime::Supervisable] default of five seconds.
    ///
    /// # Panics
    ///
    /// Panics if `id` isn't a valid supervisor name, or if the supervisor doesn't start within a few seconds.
    pub async fn start_with_worker_timeout(id: &str, worker_timeout: Duration) -> Self {
        Self::start_inner(id, worker_timeout, DataspaceRegistry::default(), |_| {}).await
    }

    /// Starts a supervisor named `id` with the static workers that `configure` adds, returning once it is running.
    ///
    /// `configure` gets the supervisor before it runs, to add workers with
    /// [`add_worker`][Supervisor::add_worker], the way an application adds its own. The workers share
    /// [`dataspace`][Self::dataspace] with the supervisor, so the test can read what they assert, such as the address a
    /// server bound to. Static workers aren't counted by [`active_children`][Self::active_children], and keep their own
    /// shutdown timeouts.
    ///
    /// # Panics
    ///
    /// Panics if `id` isn't a valid supervisor name, or if the supervisor doesn't start within a few seconds.
    pub async fn start_with(id: &str, configure: impl FnOnce(&mut Supervisor)) -> Self {
        Self::start_inner(id, TEST_WORKER_TIMEOUT, DataspaceRegistry::default(), configure).await
    }

    /// Starts a supervisor named `id` with `worker` as its one static child, sharing `dataspace` with it.
    ///
    /// The worker is added before the supervisor runs, so it isn't counted by
    /// [`active_children`][Self::active_children]. The worker keeps its own shutdown timeout: `worker_timeout` applies
    /// only to [`spawn_worker`][Self::spawn_worker].
    ///
    /// # Panics
    ///
    /// Panics if `id` isn't a valid supervisor name, or if the supervisor doesn't start within a few seconds.
    pub(crate) async fn start_with_worker(
        id: &str, worker_timeout: Duration, dataspace: DataspaceRegistry, worker: ChildSpecification,
    ) -> Self {
        Self::start_inner(id, worker_timeout, dataspace, |supervisor| {
            supervisor.add_worker(worker)
        })
        .await
    }

    async fn start_inner(
        id: &str, worker_timeout: Duration, dataspace: DataspaceRegistry, configure: impl FnOnce(&mut Supervisor),
    ) -> Self {
        let mut supervisor = Supervisor::new(id).expect("test supervisor name should be valid");
        configure(&mut supervisor);

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
            worker_timeout,
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
    /// alongside it) when it isn't holding a handle. Outside a scope, that code would panic for want of an ambient
    /// supervisor.
    pub fn scope<F>(&self, fut: F) -> TaskLocalFuture<SupervisorHandle, F>
    where
        F: Future,
    {
        self.handle.scope(fut)
    }

    /// Runs `f` as a needed worker of this supervisor, with a shutdown timeout covering everything it spawns.
    ///
    /// `f` receives the shutdown handle of the worker. To make the code under test ignore its supervisor, hand it a
    /// handle that the test owns instead. The worker is needed, so if it terminates, the supervisor stops.
    ///
    /// Unlike [`scope`][Self::scope], this function gives the worker a scope of its own. Code that spawns its own
    /// children needs that scope.
    pub fn spawn_worker<F, Fut>(&self, name: &str, f: F) -> ChildId
    where
        F: FnOnce(ShutdownHandle) -> Fut + Send + 'static,
        Fut: Future + Send + 'static,
        Fut::Output: IntoWorkerResult,
    {
        self.handle
            .worker_with_shutdown(name.to_string(), f)
            .with_shutdown_strategy(ShutdownStrategy::Graceful(self.worker_timeout))
            .needed()
            .spawn()
    }

    /// Returns a read-only handle to this supervisor's supervision tree.
    pub fn tree_handle(&self) -> SupervisionTreeHandle {
        self.tree.clone()
    }

    /// Returns the number of processes that run anywhere under this supervisor.
    ///
    /// The count also includes the children of children, such as everything a worker run with
    /// [`spawn_worker`][Self::spawn_worker] spawns into its scope. [`active_children`][Self::active_children] does not
    /// count them.
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

    /// Signals shutdown without waiting for the supervisor to finish draining.
    ///
    /// Use this where the test needs to observe something between the signal and the supervisor stopping -- that a
    /// worker returns first, say. Pair it with [`wait`][Self::wait].
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
pub(crate) async fn poll_until(
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

impl Drop for TestSupervisor {
    fn drop(&mut self) {
        // A test that returns (or panics) without calling `shutdown` shouldn't leak a supervisor and its children into
        // the rest of the run. Dropping the coordinator signals shutdown; the task tears itself down from there.
        self.shutdown_coordinator.take();
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::TestSupervisor;
    use crate::runtime::state::{DataspaceRegistry, DataspaceUpdate, IdentifierFilter};
    use crate::runtime::FnWorker;

    /// A value that a static worker asserts into the dataspace.
    #[derive(Clone, Debug, PartialEq)]
    struct WorkerState(&'static str);

    #[tokio::test]
    async fn static_workers_share_the_supervisors_dataspace_and_stop_on_shutdown() {
        let supervisor = TestSupervisor::start_with("static", |supervisor| {
            supervisor.add_worker(FnWorker::with_shutdown("asserter", |shutdown| async move {
                DataspaceRegistry::try_current()
                    .expect("a static worker should run in the supervisor's dataspace")
                    .assert(WorkerState("running"), "state");
                shutdown.await;
            }));
            supervisor.add_worker(FnWorker::with_shutdown("idle", |shutdown| shutdown));
        })
        .await;

        let mut states = supervisor
            .dataspace()
            .subscribe::<WorkerState>(IdentifierFilter::exact("state"));
        match tokio::time::timeout(Duration::from_secs(5), states.recv()).await {
            Ok(Some(DataspaceUpdate::Asserted(_, state))) => assert_eq!(state, WorkerState("running")),
            other => panic!("expected the asserter's state, got {other:?}"),
        }

        supervisor.wait_for_running_descendants(2).await;
        assert_eq!(
            supervisor.active_children(),
            0,
            "static workers aren't dynamic children"
        );

        supervisor
            .shutdown()
            .await
            .expect("both static workers should stop on shutdown");
    }
}
