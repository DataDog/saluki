use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

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
    worker_channels: Arc<Mutex<Vec<mpsc::Sender<WorkerMessage>>>>,
    worker_handles: Arc<Mutex<Vec<JoinHandle<()>>>>,
    interval_buckets: Arc<RwLock<HashMap<u64, HashSet<String>>>>,
    checks: Arc<RwLock<HashMap<String, Arc<dyn Check + Send + Sync>>>>,
    interval_handles: Arc<Mutex<HashMap<u64, JoinHandle<()>>>>,
}

impl Scheduler {
    pub fn new(check_runners: usize) -> Self {
        let worker_channels: Arc<Mutex<Vec<mpsc::Sender<WorkerMessage>>>> = Arc::new(Mutex::new(Vec::new()));
        let worker_handles: Arc<Mutex<Vec<JoinHandle<()>>>> = Arc::new(Mutex::new(Vec::new()));
        let interval_buckets: Arc<RwLock<HashMap<u64, HashSet<String>>>> = Arc::new(RwLock::new(HashMap::new()));
        let checks: Arc<RwLock<HashMap<String, Arc<dyn Check + Send + Sync + 'static>>>> =
            Arc::new(RwLock::new(HashMap::new()));
        let interval_handles: Arc<Mutex<HashMap<u64, JoinHandle<()>>>> = Arc::new(Mutex::new(HashMap::new()));

        let scheduler = Self {
            check_runners,
            worker_channels,
            worker_handles,
            interval_buckets,
            checks,
            interval_handles,
        };

        scheduler.adjust_worker_count(scheduler.check_runners);

        scheduler
    }

    /// Schedule a check
    pub fn schedule(&self, check: Arc<dyn Check + Send + Sync>) {
        let check_id = check.id();
        let interval_secs = check.interval().as_secs();

        if interval_secs == 0 {
            // Check with no interval are push to a random worker immediately
            let channels_guard = self.worker_channels.lock().unwrap();
            if channels_guard.is_empty() {
                warn!(check_id, "Failed to schedule one-time check: No workers available.");
                return;
            }

            let channel_idx = rand::thread_rng().gen_range(0..channels_guard.len());
            let channel = channels_guard[channel_idx].clone();
            let check = Arc::clone(&check);
            let check_id_owned = check_id.to_string();

            tokio::spawn(async move {
                if let Err(e) = channel.send(WorkerMessage::RunCheck(check)).await {
                    error!(error = %e, check_id = %check_id_owned, "Failed to enqueue a one-time check because channel is closed.");
                    return;
                }
                info!(check_id = %check_id_owned, "Scheduled one-time check.");
            });
            return;
        };

        {
            let mut checks = self.checks.write().unwrap();
            checks.insert(check_id.to_string(), Arc::clone(&check));
        }

        let is_new_interval = {
            let mut buckets = self.interval_buckets.write().unwrap();
            let bucket = buckets.entry(interval_secs).or_default();

            let is_new = bucket.is_empty();
            bucket.insert(check_id.to_string());
            is_new
        };

        if is_new_interval {
            self.start_interval_ticker(interval_secs);
        }
        info!(
            check_id,
            check_interval_secs = interval_secs,
            "Scheduled periodic check."
        );
    }

    /// Unschedule a check
    pub fn unschedule(&self, check_id: &str) {
        let mut interval_secs = None;

        {
            let mut checks = self.checks.write().unwrap();
            if let Some(check) = checks.remove(check_id) {
                interval_secs = Some(check.interval().as_secs());
            }
        }

        if let Some(interval_secs) = interval_secs {
            let bucket_empty = {
                let mut buckets = self.interval_buckets.write().unwrap();
                if let Some(bucket) = buckets.get_mut(&interval_secs) {
                    bucket.remove(check_id);

                    let is_empty = bucket.is_empty();

                    if is_empty {
                        buckets.remove(&interval_secs);
                    }

                    is_empty
                } else {
                    false
                }
            };

            if bucket_empty {
                self.stop_interval_ticker(interval_secs);
            }
        }

        debug!(check_id, "Unscheduled check.");
    }

    /// Shutdown the scheduler and all its workers
    pub async fn shutdown(&self) {
        info!("Shutting down check scheduler.");

        // Stop all interval tickers
        let ticker_handles = {
            let mut tickers = self.interval_handles.lock().unwrap();
            std::mem::take(&mut *tickers)
        };

        for (interval, handle) in ticker_handles {
            handle.abort();
            debug!(check_interval_secs = interval, "Stopped ticker.");
        }

        let (channels, handles) = {
            let mut channels_guard = self.worker_channels.lock().unwrap();
            let mut handles_guard = self.worker_handles.lock().unwrap();

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

    /// Start a dedicated ticker for a specific interval
    fn start_interval_ticker(&self, interval_secs: u64) {
        let interval_duration = Duration::from_secs(interval_secs);
        let checks = Arc::clone(&self.checks);
        let interval_buckets = Arc::clone(&self.interval_buckets);
        let worker_channels = Arc::clone(&self.worker_channels);

        // Create and spawn the ticker task
        let handle = tokio::spawn(async move {
            let mut ticker = time::interval(interval_duration);

            loop {
                // Wait for the next interval tick
                ticker.tick().await;

                // Get all checks in this interval
                let check_ids = {
                    let buckets = interval_buckets.read().unwrap();
                    match buckets.get(&interval_secs) {
                        Some(bucket) => bucket.iter().cloned().collect::<Vec<_>>(),
                        None => {
                            // This bucket no longer exists, exit the ticker
                            debug!(
                                check_interval_secs = interval_secs,
                                "Interval no longer has any checks, stopping ticker."
                            );
                            break;
                        }
                    }
                };

                let channels = {
                    let channels_guard = worker_channels.lock().unwrap();
                    if channels_guard.is_empty() {
                        continue; // No workers available
                    }
                    channels_guard.clone()
                };

                // Queue each check for execution using round-robin distribution
                let channel_count = channels.len();
                for (i, check_id) in check_ids.into_iter().enumerate() {
                    let check = {
                        let checks_map = checks.read().unwrap();
                        match checks_map.get(&check_id) {
                            Some(check) => Arc::clone(check),
                            None => continue,
                        }
                    };

                    // Simple round-robin: select channel based on check index
                    let channel_idx = i % channel_count;
                    let channel = &channels[channel_idx];

                    // Send to worker
                    if let Err(e) = channel.send(WorkerMessage::RunCheck(check)).await {
                        error!(check_id, error = %e, "Failed to send check to worker because channel is closed. Stopping ticker.");
                        break;
                    }
                }
            }
        });

        let mut tickers = self.interval_handles.lock().unwrap();
        tickers.insert(interval_secs, handle);

        debug!(check_interval_secs = interval_secs, "Started ticker.");
    }

    /// Stop the ticker for a specific interval
    fn stop_interval_ticker(&self, interval_secs: u64) {
        let mut tickers = self.interval_handles.lock().unwrap();
        if let Some(handle) = tickers.remove(&interval_secs) {
            handle.abort();
            debug!(check_interval_secs = interval_secs, "Stopped ticker.");
        }
    }

    /// Adjust the number of workers
    fn adjust_worker_count(&self, desired_count: usize) {
        let current_count = {
            let handles = self.worker_handles.lock().unwrap();
            handles.len()
        };

        if desired_count > current_count {
            for _ in 0..(desired_count - current_count) {
                self.add_worker();
            }

            info!(worker_count = desired_count, "Check worker count updated.");
        }
    }

    /// Add a new worker
    fn add_worker(&self) {
        let (sender, mut receiver) = mpsc::channel::<WorkerMessage>(100);

        {
            let mut channels = self.worker_channels.lock().unwrap();
            channels.push(sender);
        }

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
                    }
                    WorkerMessage::Shutdown => {
                        debug!("Worker received shutdown signal.");
                        break;
                    }
                }
            }
            debug!("Worker shutting down.");
        });

        let mut handles = self.worker_handles.lock().unwrap();
        handles.push(handle);
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
            let buckets = scheduler.interval_buckets.read().unwrap();
            assert!(buckets.contains_key(&1), "Interval bucket should exist");
            let bucket = buckets.get(&1).unwrap();
            assert!(bucket.contains("check-1s"), "Check should be in the interval bucket");
            assert!(
                !bucket.contains("check-2s"),
                "Check should not be in the interval bucket"
            );
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
            let buckets = scheduler.interval_buckets.read().unwrap();
            assert!(
                buckets.len() == 0,
                "No interval buckets should exist for one-time checks"
            );
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
            assert!(checks.contains_key("test-check"), "Check should be in the registry");
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
            let buckets = scheduler.interval_buckets.read().unwrap();
            assert!(!buckets.contains_key(&5), "Interval bucket should be removed");
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
    async fn test_multiple_checks_same_interval() {
        let scheduler = Scheduler::new(2);

        let checks = (0..5)
            .map(|i| Arc::new(MockCheck::new(&format!("same-interval-{}", i), 1)))
            .collect::<Vec<_>>();

        for check in &checks {
            scheduler.schedule(Arc::clone(check) as Arc<dyn Check + Send + Sync>);
        }

        time::sleep(Duration::from_secs(2)).await;

        for (i, check) in checks.iter().enumerate() {
            assert!(check.get_run_count() > 0, "Check {} should have run at least once", i);
        }

        {
            let buckets = scheduler.interval_buckets.read().unwrap();
            assert_eq!(buckets.len(), 1, "Should have only one interval bucket");

            let bucket = buckets.get(&1).unwrap();
            assert_eq!(bucket.len(), 5, "Bucket should contain all 5 checks");
        }

        for check in &checks {
            scheduler.unschedule(check.id());
        }

        time::sleep(Duration::from_millis(100)).await;

        {
            let buckets = scheduler.interval_buckets.read().unwrap();
            assert_eq!(buckets.len(), 0, "All interval buckets should be removed");
        }

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

            let channels = scheduler.worker_channels.lock().unwrap();
            assert_eq!(channels.len(), 0, "No worker channels should remain after shutdown");

            let tickers = scheduler.interval_handles.lock().unwrap();
            assert_eq!(tickers.len(), 0, "No interval tickers should remain after shutdown");
        }
    }
}
