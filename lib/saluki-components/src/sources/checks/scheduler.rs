use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};

use rand::Rng;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time;
use tracing::{debug, error, info, warn};

use crate::sources::checks::Check;

/// Message type for worker communication
enum WorkerMessage {
    RunCheck(Arc<dyn Check + Send + Sync>),
    Shutdown,
}

/// A scheduler that manages the execution of checks.
/// It maintains a dynamic pool of workers and organizes checks by their intervals.
pub struct Scheduler {
    check_runners: usize,
    channels: Arc<Mutex<HashMap<u64, mpsc::Sender<WorkerMessage>>>>,
    channels_load: Arc<Mutex<HashMap<u64, usize>>>,
    worker_handles: Arc<Mutex<Vec<JoinHandle<()>>>>,
    checks: Arc<RwLock<HashMap<String, JoinHandle<()>>>>,
}

impl Scheduler {
    pub fn new(check_runners: usize) -> Self {
        let channels: Arc<Mutex<HashMap<u64, mpsc::Sender<WorkerMessage>>>> = Arc::new(Mutex::new(HashMap::new()));
        let channels_load: Arc<Mutex<HashMap<u64, usize>>> = Arc::new(Mutex::new(HashMap::new()));
        let worker_handles: Arc<Mutex<Vec<JoinHandle<()>>>> = Arc::new(Mutex::new(Vec::new()));
        let checks: Arc<RwLock<HashMap<String, JoinHandle<()>>>> = Arc::new(RwLock::new(HashMap::new()));

        let scheduler = Self {
            check_runners,
            channels,
            channels_load,
            worker_handles,
            checks,
        };

        scheduler.adjust_worker_count(scheduler.check_runners);

        scheduler
    }

    /// Schedule a check
    pub fn schedule(&self, check: Arc<dyn Check + Send + Sync>) {
        let interval_secs = check.interval().as_secs();

        if interval_secs == 0 {
            let check_id = check.id().to_string();
            // Check with no interval are push to a random worker immediately
            let channels_guard = self.channels.lock().unwrap();
            if channels_guard.is_empty() {
                warn!(check_id, "Failed to schedule one-time check: No workers available.");
                return;
            }

            let channel = {
                let mut loads = self.channels_load.lock().unwrap();
                // Find channel with minimum load
                let min_idx = loads
                    .iter()
                    .min_by_key(|(_, load)| **load)
                    .map(|(idx, _)| *idx)
                    .unwrap_or(rand::thread_rng().gen_range(0..channels_guard.len() as u64));

                // Increment the load counter
                let new_load = loads[&min_idx] + 1;
                loads.insert(min_idx, new_load);
                channels_guard[&min_idx].clone()
            };
            let check = Arc::clone(&check);

            tokio::spawn(async move {
                if let Err(e) = channel.send(WorkerMessage::RunCheck(check)).await {
                    error!(error = %e, check_id = %check_id.clone(), "Failed to enqueue a one-time check because channel is closed.");
                    return;
                }
                info!(check_id = %check_id.clone(), "Scheduled one-time check.");
            });
            return;
        };
        {
            let check_id = check.id();
            let checks = self.checks.read().unwrap();
            if checks.contains_key(check_id) {
                warn!(check_id, "Check already scheduled, skipping.");
                return;
            }
        }
        let check_id = check.id().to_string();
        {
            let mut checks = self.checks.write().unwrap();
            let mut ticker = time::interval(check.interval());
            let channels_load = Arc::clone(&self.channels_load);
            let channels = Arc::clone(&self.channels);
            let check_id = check.id().to_string();
            checks.insert(
                check_id.clone(),
                tokio::spawn(async move {
                    loop {
                        // Wait for the next interval tick
                        ticker.tick().await;

                        let channel = {
                            let channels_guard = channels.lock().unwrap();
                            if channels_guard.is_empty() {
                                warn!(check_id, "Failed to schedule one-time check: No workers available.");
                                return;
                            }
                            let mut loads = channels_load.lock().unwrap();
                            // Find channel with minimum load
                            let min_idx = loads
                                .iter()
                                .min_by_key(|(_, load)| **load)
                                .map(|(idx, _)| *idx)
                                .unwrap_or(rand::thread_rng().gen_range(0..channels_guard.len() as u64));
                            // Increment the load counter
                            let new_load = loads[&min_idx] + 1;
                            loads.insert(min_idx, new_load);
                            channels_guard[&min_idx].clone()
                        };
                        let check = Arc::clone(&check);
                        if let Err(e) = channel.send(WorkerMessage::RunCheck(check)).await {
                            error!(error = %e, check_id = %check_id, "Failed to enqueue a periodic check because channel is closed.");
                            break;
                        }
                    }
                }),
            );
        }

        info!(
            check_id,
            check_interval_secs = interval_secs,
            "Scheduled periodic check."
        );
    }

    /// Unschedule a check
    pub fn unschedule(&self, check_id: &str) {
        {
            let mut checks = self.checks.write().unwrap();
            if let Some(check) = checks.remove(check_id) {
                std::mem::drop(check)
            }
        }
        debug!(check_id, "Unscheduled check.");
    }

    /// Shutdown the scheduler and all its workers
    pub async fn shutdown(&self) {
        info!("Shutting down check scheduler.");

        let (channels, handles) = {
            let mut channels_guard = self.channels.lock().unwrap();
            let mut handles_guard = self.worker_handles.lock().unwrap();

            (
                std::mem::take(&mut *channels_guard),
                std::mem::take(&mut *handles_guard),
            )
        };

        // Send shutdown signal to all workers
        for (_chanel_id, channel) in channels {
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
    fn adjust_worker_count(&self, desired_count: usize) {
        let current_count = {
            let handles = self.worker_handles.lock().unwrap();
            handles.len()
        };

        if desired_count > current_count {
            for _ in 0..(desired_count - current_count) {
                self.add_worker(self.next_worker_id());
            }

            info!(worker_count = desired_count, "Check worker count updated.");
        }
    }

    /// Get the next worker ID
    fn next_worker_id(&self) -> u64 {
        let mut rng = rand::thread_rng();
        rng.gen_range(1..=u64::MAX)
    }

    /// Add a new worker
    fn add_worker(&self, worker_id: u64) {
        let (sender, mut receiver) = mpsc::channel::<WorkerMessage>(100);

        {
            let mut channels = self.channels.lock().unwrap();
            channels.insert(worker_id, sender);
            let mut loads = self.channels_load.lock().unwrap();
            loads.insert(worker_id, 0);
        }

        let channels_load = Arc::clone(&self.channels_load);
        let handle = tokio::spawn(async move {
            while let Some(msg) = receiver.recv().await {
                match msg {
                    WorkerMessage::RunCheck(check) => {
                        let check_id = check.id();
                        info!(check_id, "Running check");

                        match check.run() {
                            Ok(()) => debug!(check_id, "Check completed successfully."),
                            Err(e) => error!(error = %e, check_id, "Check failed."),
                        }

                        // Decrement load counter
                        let mut loads = channels_load.lock().unwrap();
                        let load = loads.get_mut(&worker_id);
                        if let Some(load) = load {
                            *load -= 1;
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
            let mut handles_guard = self.worker_handles.lock().unwrap();
            handles_guard.push(handle);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use saluki_error::{generic_error, GenericError};
    use tokio::time;

    use super::*;

    // Define a mock Check implementation for testing
    struct MockCheck {
        id: String,
        interval: Duration,
        run_count: Arc<Mutex<usize>>,
        should_fail: bool,
    }

    impl MockCheck {
        fn new(id: &str, interval_secs: u64) -> Self {
            Self {
                id: id.to_string(),
                interval: Duration::from_secs(interval_secs),
                run_count: Arc::new(Mutex::new(0)),
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

    impl Check for MockCheck {
        fn run(&self) -> Result<(), GenericError> {
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

    fn create_check(id: &str, interval_secs: u64) -> Arc<dyn Check + Send + Sync> {
        Arc::new(MockCheck::new(id, interval_secs))
    }

    #[tokio::test]
    async fn test_scheduler_creation() {
        let scheduler = Scheduler::new(2);

        let worker_count = {
            let handles = scheduler.worker_handles.lock().unwrap();
            handles.len()
        };

        assert_eq!(worker_count, 2, "Scheduler should start with 2 workers");
    }

    #[tokio::test]
    async fn test_scheduler_executes_checks() {
        let scheduler = Scheduler::new(2);

        let check1 = Arc::new(MockCheck::new("check-1s", 1));
        let check2 = Arc::new(MockCheck::new("check-2s", 2));

        scheduler.schedule(check1.clone() as Arc<dyn Check + Send + Sync>);
        scheduler.schedule(check2.clone() as Arc<dyn Check + Send + Sync>);

        {
            let checks = scheduler.checks.read().unwrap();
            assert_eq!(checks.len(), 2, "Two checks should be scheduled");
        }

        time::sleep(Duration::from_secs(3)).await;

        assert!(check1.get_run_count() > 0, "Check 1 should have run at least once");
        assert!(check2.get_run_count() > 0, "Check 2 should have run at least once");

        assert!(
            check1.get_run_count() >= check2.get_run_count(),
            "Check 1 should run more frequently than check 2"
        );

        scheduler.unschedule(check1.id());
        scheduler.unschedule(check2.id());
        scheduler.shutdown().await;
    }

    #[tokio::test]
    async fn test_scheduler_executes_one_time_check() {
        let scheduler = Scheduler::new(2);

        let check1 = Arc::new(MockCheck::new("one_time_check", 0));

        scheduler.schedule(check1.clone() as Arc<dyn Check + Send + Sync>);

        {
            let checks = scheduler.checks.read().unwrap();
            assert!(checks.len() == 0, "No checks handle should exist for one-time checks");
        }

        time::sleep(Duration::from_secs(1)).await;

        assert!(check1.get_run_count() == 1, "One time check should have run once");

        scheduler.shutdown().await;
    }

    #[tokio::test]
    async fn test_schedule_unschedule() {
        let scheduler = Scheduler::new(2);

        let check = create_check("test-check", 5);

        scheduler.schedule(Arc::clone(&check));

        {
            let checks = scheduler.checks.read().unwrap();
            assert!(checks.contains_key(check.id()), "Check should be in the registry");
        }

        scheduler.unschedule(check.id());

        {
            let checks = scheduler.checks.read().unwrap();
            assert!(
                !checks.contains_key("test-check"),
                "Check should be removed from registry"
            );
        }

        {
            let buckets = scheduler.checks.read().unwrap();
            assert!(
                !buckets.contains_key(check.id()),
                "Checks should be removed from the registry"
            );
        }
    }

    #[tokio::test]
    async fn test_failing_check() {
        let scheduler = Scheduler::new(1);

        let failing_check = Arc::new(MockCheck::with_failure("failing-check", 1));
        let failing_check_trait = failing_check.clone() as Arc<dyn Check + Send + Sync>;

        scheduler.schedule(Arc::clone(&failing_check_trait));

        time::sleep(Duration::from_secs(2)).await;

        assert!(
            failing_check.get_run_count() > 0,
            "Failing check should still be executed"
        );

        scheduler.unschedule(failing_check.id());
        scheduler.shutdown().await;
    }

    #[tokio::test]
    async fn test_shutdown() {
        let scheduler = Scheduler::new(3);

        let checks = (0..5)
            .map(|i| create_check(&format!("check-{}", i), 5))
            .collect::<Vec<_>>();

        for check in &checks {
            scheduler.schedule(Arc::clone(check));
        }

        scheduler.shutdown().await;

        {
            let handles = scheduler.worker_handles.lock().unwrap();
            assert_eq!(handles.len(), 0, "No worker handles should remain after shutdown");

            let channels = scheduler.channels.lock().unwrap();
            assert_eq!(channels.len(), 0, "No worker channels should remain after shutdown");
        }
    }
}
