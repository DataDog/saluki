//! Test helpers for exercising components that spawn supervised children.
//!
//! A [`ComponentSpawner`] is only useful while its supervisor is actually running: spawning against a supervisor that
//! was built but never run fails with [`SpawnError::SupervisorGone`][crate::runtime::SpawnError::SupervisorGone]. That
//! makes the obvious test fixture -- `Supervisor::new("test").handle()` -- a trap, because it looks right and fails
//! only once the component under test tries to spawn something.
//!
//! [`TestComponentSupervisor`] runs a supervisor configured the way the topology configures a component's supervisor,
//! and hands out a [`ComponentSpawner`] bound to it.

use std::time::Duration;

use saluki_common::sync::shutdown::{ShutdownCoordinator, ShutdownHandle};
use tokio::{runtime::Handle, task::JoinHandle};

use crate::components::ComponentSpawner;
use crate::runtime::{
    state::DataspaceRegistry, AutoShutdown, ShutdownMode, Supervisor, SupervisorError, SupervisorHandle,
};

/// Shutdown budget for the test supervisor.
///
/// Mirrors production, where a component supervisor -- not its children -- owns the deadline. Short enough that a test
/// asserting on forced-abort behavior doesn't stall, long enough that well-behaved children have ample time to drain
/// on a loaded CI machine.
const TEST_SHUTDOWN_BUDGET: Duration = Duration::from_secs(5);

/// Interval between readiness polls.
const POLL_INTERVAL: Duration = Duration::from_millis(5);

/// Overall budget for readiness polling before panicking.
const POLL_TIMEOUT: Duration = Duration::from_secs(5);

/// A running per-component supervisor for tests.
///
/// Configured like the supervisor the topology builds for each component ([`AutoShutdown::AnySignificant`],
/// [`ShutdownMode::Concurrent`], and a shutdown budget), minus the component worker itself -- the test drives the
/// component directly.
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
        let dataspace = DataspaceRegistry::default();
        let mut supervisor = Supervisor::new(id)
            .expect("test supervisor name should be valid")
            .with_auto_shutdown(AutoShutdown::AnySignificant)
            .with_shutdown_mode(ShutdownMode::Concurrent)
            .with_shutdown_budget(budget);

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
        supervisor
            .poll_until("the test supervisor is running", || supervisor.handle.is_running())
            .await;

        supervisor
    }

    /// Returns a spawner bound to this supervisor.
    ///
    /// The current runtime stands in for the shared worker pool.
    pub fn spawner(&self) -> ComponentSpawner {
        ComponentSpawner::new(self.handle.clone(), Handle::current())
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
        self.poll_until(&format!("the supervisor has {count} running children"), || {
            self.handle.active_children() == count
        })
        .await;
    }

    /// Signals shutdown and waits for the supervisor to finish draining its children.
    ///
    /// The result is the supervisor's own: `Err(SupervisorError::ShutdownTimedOut { .. })` means a child ignored
    /// shutdown and had to be aborted, which is usually what a test wants to assert did *not* happen.
    ///
    /// # Panics
    ///
    /// Panics if the supervisor task panicked.
    pub async fn shutdown(mut self) -> Result<(), SupervisorError> {
        if let Some(shutdown_coordinator) = self.shutdown_coordinator.take() {
            shutdown_coordinator.shutdown();
        }

        (&mut self.task).await.expect("test supervisor task should not panic")
    }

    async fn poll_until(&self, description: &str, mut condition: impl FnMut() -> bool) {
        let deadline = tokio::time::Instant::now() + POLL_TIMEOUT;
        loop {
            if condition() {
                return;
            }

            if tokio::time::Instant::now() >= deadline {
                panic!("timed out after {POLL_TIMEOUT:?} waiting until {description}");
            }

            tokio::time::sleep(POLL_INTERVAL).await;
        }
    }
}

impl Drop for TestComponentSupervisor {
    fn drop(&mut self) {
        // A test that returns (or panics) without calling `shutdown` shouldn't leak a supervisor and its children into
        // the rest of the run. Dropping the coordinator signals shutdown; the task tears itself down from there.
        self.shutdown_coordinator.take();
    }
}
