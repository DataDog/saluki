//! Background tasks a test case spawns, which the runner stops before reclaiming the case's resources.

use std::future::Future;
use std::time::Duration;

use tokio::task::JoinHandle;
use tokio::time::{timeout_at, Instant};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

/// How long the runner waits for a case's tasks to stop. Taken out of [`RECLAIM_TIME`](crate::ledger::RECLAIM_TIME).
pub(crate) const TASK_STOP_TIME: Duration = Duration::from_secs(5);

/// A case's background tasks, shared by its test body and the runner.
///
/// Each task is dropped at its next await once `stop` is cancelled. That happens when the test is cancelled, or when
/// the runner calls [`CaseTasks::stop_all`]. Tasks therefore own nothing that needs cleanup; the ledger owns resources.
#[derive(Clone, Debug)]
pub(crate) struct CaseTasks {
    tracker: TaskTracker,

    /// Child of the test token. A task's own creation requests watch it, so they see the stop before their task is
    /// dropped.
    pub(crate) stop: CancellationToken,
}

impl CaseTasks {
    pub(crate) fn new(test_token: &CancellationToken) -> Self {
        Self {
            tracker: TaskTracker::new(),
            stop: test_token.child_token(),
        }
    }

    /// Spawns `task`. The handle yields `None` if the task was stopped first.
    ///
    /// A task spawned after the stop is never polled. Otherwise the task is polled before the stop, so a request inside
    /// it that watches `stop` returns first.
    pub(crate) fn spawn<F>(&self, task: F) -> JoinHandle<Option<F::Output>>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        self.tracker.spawn(self.stop.clone().run_until_cancelled_owned(task))
    }

    /// Stops every task and waits for them until `deadline`.
    ///
    /// Returns how many are still running, which are all stuck in synchronous code. Leaves the test token alone.
    pub(crate) async fn stop_all(&self, deadline: Instant) -> usize {
        self.stop.cancel();
        self.tracker.close();
        let _ = timeout_at(deadline, self.tracker.wait()).await;
        self.tracker.len()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{mpsc, Arc};

    use tokio::time::timeout;

    use super::*;

    const BOUND: Duration = Duration::from_secs(5);

    /// Sets its flag when dropped.
    struct DropFlag(Arc<AtomicBool>);

    impl Drop for DropFlag {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn stopping_drops_each_task_at_its_next_await() {
        let test_token = CancellationToken::new();
        let tasks = CaseTasks::new(&test_token);
        let dropped = Arc::new(AtomicBool::new(false));
        let flag = DropFlag(dropped.clone());
        let handle = tasks.spawn(async move {
            let _flag = flag;
            std::future::pending::<()>().await;
        });
        tokio::task::yield_now().await;

        let started = Instant::now();
        let running = timeout(BOUND, tasks.stop_all(Instant::now() + TASK_STOP_TIME))
            .await
            .expect("stop_all should return within the bound");

        assert_eq!(running, 0);
        assert_eq!(started.elapsed(), Duration::ZERO);
        assert!(dropped.load(Ordering::SeqCst), "the task should have been dropped");
        assert_eq!(handle.await.expect("the task should not panic"), None);
        assert!(!test_token.is_cancelled(), "stopping tasks should not cancel the test");
    }

    #[tokio::test(start_paused = true)]
    async fn cancelling_the_test_stops_its_tasks() {
        let test_token = CancellationToken::new();
        let tasks = CaseTasks::new(&test_token);
        let handle = tasks.spawn(std::future::pending::<()>());

        test_token.cancel();

        let stopped = timeout(BOUND, handle)
            .await
            .expect("the task should stop within the bound")
            .expect("the task should not panic");
        assert_eq!(stopped, None);
    }

    #[tokio::test(start_paused = true)]
    async fn a_task_spawned_after_the_stop_is_never_polled() {
        let tasks = CaseTasks::new(&CancellationToken::new());
        tasks.stop_all(Instant::now() + TASK_STOP_TIME).await;

        for _ in 0..64 {
            let polled = Arc::new(AtomicBool::new(false));
            let task_polled = polled.clone();
            let handle = tasks.spawn(async move {
                task_polled.store(true, Ordering::SeqCst);
            });

            let stopped = timeout(BOUND, handle)
                .await
                .expect("the task should finish within the bound")
                .expect("the task should not panic");
            assert_eq!(stopped, None);
            assert!(
                !polled.load(Ordering::SeqCst),
                "a task spawned after the stop was polled"
            );
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stop_reports_a_task_stuck_in_synchronous_code() {
        let tasks = CaseTasks::new(&CancellationToken::new());
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        tasks.spawn(async move {
            let _ = started_tx.send(());
            // Blocks its worker thread, so the stop cannot drop the task until it is released.
            let _ = release_rx.recv();
            tokio::task::yield_now().await;
        });
        timeout(BOUND, started_rx)
            .await
            .expect("the task should start within the bound")
            .expect("the task should signal that it started");

        let running = timeout(BOUND, tasks.stop_all(Instant::now() + Duration::from_millis(100)))
            .await
            .expect("stop_all should return at its deadline");
        assert_eq!(running, 1);

        release_tx.send(()).expect("the task should still be blocked");
        let running = timeout(BOUND, tasks.stop_all(Instant::now() + BOUND))
            .await
            .expect("stop_all should return within the bound");
        assert_eq!(running, 0);
    }
}
