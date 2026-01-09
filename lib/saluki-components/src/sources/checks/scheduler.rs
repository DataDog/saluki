use std::collections::HashMap;
use std::sync::Arc;

use rand::Rng;
use tokio::sync::{mpsc, Mutex};
use tokio::task::JoinHandle;
use tokio::time;
use tracing::{debug, error, info, warn};

use crate::sources::checks::Check;

/// Message type for worker communication
enum WorkerMessage {
    RunCheck(Arc<Mutex<dyn Check + Send + Sync>>),
    Shutdown,
}

/// A scheduler that manages the execution of checks.
/// It:
/// - Schedules checks based on their intervals.
/// - Supports one-time checks.
///
/// Checks are distributed accross multiple workers base on the load of each worker.
pub struct Scheduler {
    check_runners: usize,
    channels: Arc<Mutex<Vec<mpsc::Sender<WorkerMessage>>>>,
    worker_handles: Arc<Mutex<Vec<JoinHandle<()>>>>,
    checks: Arc<Mutex<HashMap<String, JoinHandle<()>>>>,
}

impl Scheduler {
    pub async fn new(check_runners: usize) -> Self {
        let channels: Arc<Mutex<Vec<mpsc::Sender<WorkerMessage>>>> = Arc::new(Mutex::new(vec![]));
        let worker_handles: Arc<Mutex<Vec<JoinHandle<()>>>> = Arc::new(Mutex::new(Vec::new()));
        let checks: Arc<Mutex<HashMap<String, JoinHandle<()>>>> = Arc::new(Mutex::new(HashMap::new()));

        let scheduler = Self {
            check_runners,
            channels,
            worker_handles,
            checks,
        };

        scheduler.adjust_worker_count(scheduler.check_runners).await;

        scheduler
    }

    /// Schedule a check
    pub async fn schedule(&self, check: Arc<Mutex<dyn Check + Send + Sync>>) {
        let interval_secs: u64;
        let check_id: String;

        {
            let check = check.lock().await;
            interval_secs = check.interval().as_secs();
            check_id = check.id().to_string();
        }

        if interval_secs == 0 {
            self.spawn_check_task(check);
            return;
        }

        {
            let checks = self.checks.lock().await;
            if checks.contains_key(&check_id) {
                warn!(check_id, "Check already scheduled, skipping.");
                return;
            }
        }

        {
            let mut checks = self.checks.lock().await;
            checks.insert(check_id.clone(), self.spawn_check_task(check));
        }

        info!(
            check_id,
            check_interval_secs = interval_secs,
            "Scheduled periodic check."
        );
    }

    fn spawn_check_task(&self, check: Arc<Mutex<dyn Check + Send + Sync>>) -> JoinHandle<()> {
        let check_arc = Arc::clone(&check);
        let channels = Arc::clone(&self.channels);

        tokio::spawn(async move {
            let (check_interval, check_id) = {
                let check = check_arc.lock().await;
                (check.interval(), check.id().to_string())
            };

            let one_time_check = check_interval.is_zero();

            let execute_check = || async {
                let channel = {
                    let channels_guard = channels.lock().await;
                    if channels_guard.is_empty() {
                        warn!(check_id, "Failed to schedule check: No workers available.");

                        return false;
                    }

                    channels_guard
                        .iter()
                        .max_by_key(|&channel| channel.capacity())
                        .unwrap()
                        .clone()
                };

                let check_clone = Arc::clone(&check_arc);
                if let Err(e) = channel.send(WorkerMessage::RunCheck(check_clone)).await {
                    error!(error = %e, check_id = %check_id,"Failed to enqueue check because channel is closed.");
                    return false;
                }

                true
            };

            if one_time_check {
                if execute_check().await {
                    info!(check_id, "One-time check enqueued successfully.");
                }
            } else {
                let mut ticker = time::interval(check_interval);
                loop {
                    ticker.tick().await;

                    if !execute_check().await {
                        break;
                    }
                }
            }
        })
    }

    /// Unschedule a check
    pub async fn unschedule(&self, check_id: &str) {
        {
            let mut checks = self.checks.lock().await;
            if let Some(check) = checks.remove(check_id) {
                check.abort();
            }
        }
        debug!(check_id, "Unscheduled check.");
    }

    /// Shutdown the scheduler and all its workers
    pub async fn shutdown(&self) {
        info!("Shutting down check scheduler.");

        let (channels, handles) = {
            let mut channels_guard = self.channels.lock().await;
            let mut handles_guard = self.worker_handles.lock().await;

            (
                std::mem::take(&mut *channels_guard),
                std::mem::take(&mut *handles_guard),
            )
        };

        // Send shutdown signal to all workers
        for channel in channels {
            let _ = channel.send(WorkerMessage::Shutdown).await;
        }

        // Wait for all workers to finish
        for handle in handles {
            if let Err(e) = handle.await {
                error!(error = %e, "Error when shutting down worker.");
            }
        }

        info!("Check scheduler shutdown complete.");
    }

    /// Adjust the number of workers
    async fn adjust_worker_count(&self, desired_count: usize) {
        let current_count = {
            let handles = self.worker_handles.lock().await;
            handles.len()
        };

        if desired_count > current_count {
            for _ in 0..(desired_count - current_count) {
                self.add_worker(self.next_worker_id()).await;
            }

            info!(worker_count = desired_count, "Check worker count updated.");
        }
    }

    /// Get the next worker ID
    fn next_worker_id(&self) -> u64 {
        let mut rng = rand::rng();
        rng.random_range(1..=u64::MAX)
    }

    /// Add a new worker
    async fn add_worker(&self, worker_id: u64) {
        let (sender, mut receiver) = mpsc::channel::<WorkerMessage>(100);

        {
            let mut channels_guard = self.channels.lock().await;
            channels_guard.push(sender);
        }

        let handle = tokio::spawn(async move {
            while let Some(msg) = receiver.recv().await {
                match msg {
                    WorkerMessage::RunCheck(check) => {
                        let check_id = {
                            let check = check.lock().await;
                            check.id().to_string()
                        };
                        info!(check_id, "Running check");

                        let result = {
                            let mut check = check.lock().await;
                            check.run().await
                        };

                        match result {
                            Ok(()) => debug!(worker_id, check_id, "Check completed successfully."),
                            Err(e) => error!(error = %e, check_id, "Check failed."),
                        }
                    }
                    WorkerMessage::Shutdown => {
                        debug!("Worker received shutdown signal.");
                        break;
                    }
                }
            }
            debug!("Worker shutting down.");
        });

        {
            let mut handles_guard = self.worker_handles.lock().await;
            handles_guard.push(handle);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use async_trait::async_trait;
    use saluki_error::{generic_error, GenericError};
    use tokio::sync::Mutex;
    use tokio::time;

    use super::*;

    // Define a mock Check implementation for testing
    struct MockCheck {
        id: String,
        interval: Duration,
        run_count: Arc<std::sync::Mutex<usize>>,
        should_fail: bool,
    }

    impl MockCheck {
        fn new(id: &str, interval_secs: u64) -> Self {
            Self {
                id: id.to_string(),
                interval: Duration::from_secs(interval_secs),
                run_count: Arc::new(std::sync::Mutex::new(0)),
                should_fail: false,
            }
        }

        fn with_failure(id: &str, interval_secs: u64) -> Self {
            let mut check = Self::new(id, interval_secs);
            check.should_fail = true;
            check
        }

        fn get_run_count(&self) -> usize {
            *self.run_count.lock().unwrap()
        }
    }

    #[async_trait]
    impl Check for MockCheck {
        async fn run(&mut self) -> Result<(), GenericError> {
            let mut count = self.run_count.lock().unwrap();
            *count += 1;

            if self.should_fail {
                Err(generic_error!("Mock check failure."))
            } else {
                Ok(())
            }
        }

        fn interval(&self) -> Duration {
            self.interval
        }

        fn id(&self) -> &str {
            &self.id
        }

        fn version(&self) -> &str {
            "1.0"
        }

        fn source(&self) -> &str {
            "mock"
        }
    }

    fn create_check(id: &str, interval_secs: u64) -> Arc<Mutex<dyn Check + Send + Sync>> {
        Arc::new(Mutex::new(MockCheck::new(id, interval_secs)))
    }

    #[tokio::test]
    async fn test_scheduler_creation() {
        let scheduler = Scheduler::new(2).await;

        let worker_count = {
            let handles = scheduler.worker_handles.lock().await;
            handles.len()
        };

        assert_eq!(worker_count, 2, "Scheduler should start with 2 workers");
    }

    #[tokio::test]
    async fn test_scheduler_executes_checks() {
        let scheduler = Scheduler::new(2).await;

        let check1 = Arc::new(Mutex::new(MockCheck::new("check-1s", 1)));
        let check2 = Arc::new(Mutex::new(MockCheck::new("check-2s", 2)));

        scheduler.schedule(check1.clone()).await;
        scheduler.schedule(check2.clone()).await;

        {
            let checks = scheduler.checks.lock().await;
            assert_eq!(checks.len(), 2, "Two checks should be scheduled");
        }

        time::sleep(Duration::from_secs(3)).await;

        let check1_count = check1.lock().await.get_run_count();
        let check2_count = check2.lock().await.get_run_count();
        let check1_id = check1.lock().await.id().to_string();
        let check2_id = check2.lock().await.id().to_string();

        assert!(check1_count > 0, "Check 1 should have run at least once");
        assert!(check2_count > 0, "Check 2 should have run at least once");

        assert!(
            check1_count >= check2_count,
            "Check 1 should run more frequently than check 2"
        );

        scheduler.unschedule(&check1_id).await;
        scheduler.unschedule(&check2_id).await;
        scheduler.shutdown().await;
    }

    #[tokio::test]
    async fn test_scheduler_executes_one_time_check() {
        let scheduler = Scheduler::new(2).await;

        let check1 = Arc::new(Mutex::new(MockCheck::new("one_time_check", 0)));

        scheduler.schedule(check1.clone()).await;

        {
            let checks = scheduler.checks.lock().await;
            assert!(checks.is_empty(), "No checks handle should exist for one-time checks");
        }

        time::sleep(Duration::from_secs(1)).await;

        let check1_count = check1.lock().await.get_run_count();
        assert!(check1_count == 1, "One time check should have run once");

        scheduler.shutdown().await;
    }

    #[tokio::test]
    async fn test_schedule_unschedule() {
        let scheduler = Scheduler::new(2).await;

        let check = create_check("test-check", 5);

        scheduler.schedule(Arc::clone(&check)).await;

        let check_id = check.lock().await.id().to_string();

        {
            let checks = scheduler.checks.lock().await;
            assert!(checks.contains_key(&check_id), "Check should be in the registry");
        }

        scheduler.unschedule(&check_id).await;

        {
            let checks = scheduler.checks.lock().await;
            assert!(
                !checks.contains_key("test-check"),
                "Check should be removed from registry"
            );
        }

        {
            let buckets = scheduler.checks.lock().await;
            assert!(
                !buckets.contains_key(&check_id),
                "Checks should be removed from the registry"
            );
        }
    }

    #[tokio::test]
    async fn test_failing_check() {
        let scheduler = Scheduler::new(1).await;

        let failing_check = Arc::new(Mutex::new(MockCheck::with_failure("failing-check", 1)));

        scheduler
            .schedule(Arc::clone(&failing_check) as Arc<Mutex<dyn Check + Send + Sync>>)
            .await;

        time::sleep(Duration::from_secs(2)).await;

        let failing_check_count = failing_check.lock().await.get_run_count();
        let failing_check_id = failing_check.lock().await.id().to_string();

        assert!(failing_check_count > 0, "Failing check should still be executed");

        scheduler.unschedule(&failing_check_id).await;
        scheduler.shutdown().await;
    }

    #[tokio::test]
    async fn test_shutdown() {
        let scheduler = Scheduler::new(3).await;

        let checks = (0..5)
            .map(|i| create_check(&format!("check-{}", i), 5))
            .collect::<Vec<_>>();

        for check in &checks {
            scheduler
                .schedule(Arc::clone(check) as Arc<Mutex<dyn Check + Send + Sync>>)
                .await;
        }

        scheduler.shutdown().await;

        {
            let handles = scheduler.worker_handles.lock().await;
            assert_eq!(handles.len(), 0, "No worker handles should remain after shutdown");

            let channels = scheduler.channels.lock().await;
            assert_eq!(channels.len(), 0, "No worker channels should remain after shutdown");
        }
    }
}
