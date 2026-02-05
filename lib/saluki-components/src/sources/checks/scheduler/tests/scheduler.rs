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