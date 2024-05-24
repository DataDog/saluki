use tokio::task::JoinHandle;

use super::*;

struct CheckHandle(JoinHandle<Result<(), GenericError>>);

#[derive(Debug, Clone, Copy)]
enum CoreChecks {
    Smoke(SmokeCheck),
}

#[derive(Debug, Clone, Copy)]
struct SmokeCheck {}
impl SmokeCheck {
    const NAME: &'static str = "smoke";

    fn run(
        &self, send_metrics: mpsc::Sender<CheckMetric>, config: CheckInstanceConfiguration,
    ) -> JoinHandle<Result<(), GenericError>> {
        debug!("Beginning execution of Smoke Check Instance {:p}", self);
        tokio::spawn(async move {
            loop {
                send_metrics
                    .send(CheckMetric {
                        name: "smoke_metric".to_string(),
                        metric_type: PyMetricType::Gauge,
                        value: 1.0,
                        tags: vec!["hello:world".to_string()],
                    })
                    .await
                    .map_err(|e| generic_error!("Failed to send metric: {}", e))?;

                tokio::time::sleep(Duration::from_millis(config.min_collection_interval_ms().into())).await;
            }
        })
    }
}

impl CoreChecks {
    fn from_name(name: &str) -> Option<Self> {
        match name {
            SmokeCheck::NAME => Some(CoreChecks::Smoke(SmokeCheck {})),
            _ => None,
        }
    }

    fn name(&self) -> &'static str {
        match self {
            CoreChecks::Smoke(_) => SmokeCheck::NAME,
        }
    }

    fn run(
        self, send_metrics: mpsc::Sender<CheckMetric>, config: CheckInstanceConfiguration,
    ) -> JoinHandle<Result<(), GenericError>> {
        match self {
            CoreChecks::Smoke(check) => check.run(send_metrics, config),
        }
    }
}

pub struct CoreCheckScheduler {
    send_check_metrics: mpsc::Sender<CheckMetric>,
    running: HashMap<CheckSource, Vec<CheckHandle>>,
}

impl CoreCheckScheduler {
    pub fn new(send_check_metrics: mpsc::Sender<CheckMetric>) -> Result<Self, GenericError> {
        Ok(CoreCheckScheduler {
            send_check_metrics,
            running: HashMap::new(),
        })
    }
}

impl CheckScheduler for CoreCheckScheduler {
    fn can_run_check(&self, runnable: &RunnableCheckRequest) -> bool {
        info!(
            "Considered whether to run check as corecheck: {:?}",
            runnable.check_request
        );
        CoreChecks::from_name(&runnable.check_request.name).is_some()
    }
    fn run_check(&mut self, runnable: &RunnableCheckRequest) -> Result<(), GenericError> {
        let check = match CoreChecks::from_name(&runnable.check_request.name) {
            Some(check) => check,
            None => return Err(generic_error!("Check is not supported, can't run it.")),
        };
        info!(
            "Running {} instances of check {}",
            runnable.check_request.instances.len(),
            check.name()
        );
        for instance in &runnable.check_request.instances {
            let handle = check.run(self.send_check_metrics.clone(), instance.clone());
            self.running
                .entry(runnable.check_request.source.clone())
                .or_default()
                .push(CheckHandle(handle));
        }

        Ok(())
    }
    fn stop_check(&mut self, check_name: CheckRequest) {
        let removed = self.running.remove(&check_name.source);
        if let Some(removed) = removed {
            for handle in removed {
                handle.0.abort();
            }
        } else {
            warn!("Tried to stop a check that wasn't running")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::*;
    use super::*;
    use tokio::sync::mpsc;

    #[tokio::test]
    async fn test_new_check_scheduler() {
        let (sender, _) = mpsc::channel(10);
        let scheduler = CoreCheckScheduler::new(sender).unwrap();
        assert!(scheduler.running.is_empty());
    }

    #[tokio::test]
    async fn test_can_run_with_no_checks() {
        let (sender, _) = mpsc::channel(10);
        let scheduler = CoreCheckScheduler::new(sender).unwrap();
        let source = CheckSource::Yaml(YamlCheck::new("my_check", "instances: [{}]", None));
        let check_request = source.to_check_request().unwrap();

        let runnable_check_request: RunnableCheckRequest = check_request.into();
        assert!(!scheduler.can_run_check(&runnable_check_request));
    }

    #[tokio::test]
    async fn test_can_run_with_valid_check() {
        let (sender, _) = mpsc::channel(10);
        let scheduler = CoreCheckScheduler::new(sender).unwrap();
        let source = CheckSource::Yaml(YamlCheck::new("smoke", "instances: [{}]", None));
        let check_request = source.to_check_request().unwrap();

        let runnable_check_request: RunnableCheckRequest = check_request.into();
        assert!(scheduler.can_run_check(&runnable_check_request));
    }

    #[tokio::test]
    async fn test_execution_with_single_instance() {
        let (sender, mut receiver) = mpsc::channel(10);
        let mut scheduler = CoreCheckScheduler::new(sender).unwrap();
        let source = CheckSource::Yaml(YamlCheck::new("smoke", "instances: [{}]", None));
        let check_request = source.to_check_request().unwrap();

        let runnable_check_request: RunnableCheckRequest = check_request.into();
        scheduler
            .run_check(&runnable_check_request)
            .expect("Failed to run check");

        assert_eq!(scheduler.running.len(), 1);

        let check_metric = CheckMetric {
            name: "smoke_metric".to_string(),
            metric_type: PyMetricType::Gauge,
            value: 1.0,
            tags: vec!["hello:world".to_string()],
        };
        let check_from_channel = receiver.recv().await.unwrap();
        assert_eq!(check_from_channel, check_metric);
    }
}
