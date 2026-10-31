//! Tracks explicitly registered background tasks for bounded shutdown before resource cleanup.

use std::future::Future;
use std::time::Duration;

use tokio::task::JoinHandle;
use tokio::time::{timeout_at, Instant};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

/// How long the runner waits for a case's tasks to stop. Taken out of [`RECLAIM_TIME`](crate::ledger::RECLAIM_TIME).
pub(crate) const TASK_STOP_TIME: Duration = Duration::from_secs(5);

/// Tracks tasks spawned through [`Self::spawn`], shared by the test body and runner.
///
/// Cancellation is cooperative: synchronous work can outlive [`Self::stop_all`]'s deadline. Register resources that
/// need asynchronous cleanup in the ledger.
#[derive(Clone, Debug)]
pub(crate) struct CaseTasks {
    tracker: TaskTracker,

    /// Child of the test token. Creation requests also watch it to mark interrupted resources before task shutdown.
    pub(crate) stop: CancellationToken,
}

impl CaseTasks {
    pub(crate) fn new(test_token: &CancellationToken) -> Self {
        Self {
            tracker: TaskTracker::new(),
            stop: test_token.child_token(),
        }
    }

    /// Spawns and tracks `task`; the handle yields `None` if cancellation drops it.
    ///
    /// If `stop` is already cancelled at the first poll, the wrapper drops `task` unpolled. Otherwise, each poll tries
    /// `task` first; cancellation drops it only on `Pending`. Creation requests can therefore observe `stop` before
    /// their task is dropped.
    pub(crate) fn spawn<F>(&self, task: F) -> JoinHandle<Option<F::Output>>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        self.tracker.spawn(self.stop.clone().run_until_cancelled_owned(task))
    }

    /// Requests cancellation and waits for tracked tasks to finish until `deadline`.
    ///
    /// Returns the number still tracked. Cancels only the child token, preserving the test token's state.
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

    struct DropFlag(Arc<AtomicBool>);

    impl Drop for DropFlag {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn stopping_drops_a_pending_task() {
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
