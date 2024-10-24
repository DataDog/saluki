use super::*;
use metrics::Counter;
use serde_json::json;

#[derive(Clone)]
pub struct ChecksTelemetry {
    pub check_requests_dispatched: Counter,
    pub check_instances_started: Counter,
}

#[cfg(test)]
impl ChecksTelemetry {
    pub fn noop() -> Self {
        Self {
            check_requests_dispatched: Counter::noop(),
            check_instances_started: Counter::noop(),
        }
    }
}
pub(crate) struct CheckDispatcher {
    tlm: ChecksTelemetry,
    health: Health,
    python_scheduler: python_scheduler::PythonCheckScheduler,
    check_metrics_rx: mpsc::Receiver<CheckMetric>,
    check_run_requests: mpsc::Receiver<CheckRequest>,
    check_stop_requests: mpsc::Receiver<CheckRequest>,
}

#[derive(Clone, Deserialize, Serialize, Default)]
struct DispatcherStatusDetail {
    // Each incoming check request should record if it was dispatched and to which scheduler
    check_request_state: HashMap<String, serde_json::Value>,
}

impl CheckDispatcher {
    pub(crate) fn new(
        check_run_requests: mpsc::Receiver<CheckRequest>, check_stop_requests: mpsc::Receiver<CheckRequest>,
        tlm: ChecksTelemetry, check_dispatcher_health: Option<Health>, python_scheduler_health: Option<Health>,
    ) -> Result<Self, GenericError> {
        let (check_metrics_tx, check_metrics_rx) = mpsc::channel(10_000_000);
        let python_scheduler_health = python_scheduler_health.expect("Health is not present");
        let check_dispatcher_health = check_dispatcher_health.expect("Health is not present");
        let python_scheduler = python_scheduler::PythonCheckScheduler::new(
            check_metrics_tx.clone(),
            tlm.clone(),
            python_scheduler_health,
        )?;
        Ok(Self {
            tlm,
            health: check_dispatcher_health,
            check_metrics_rx,
            check_run_requests,
            check_stop_requests,
            python_scheduler,
        })
    }

    /// Listens for check requests and dispatches each one to a check scheduler
    /// that can handle the requested 'check'.
    /// If multiple schedulers can handle a check, it is dispatched to the first one.
    pub(crate) fn run(self) -> mpsc::Receiver<CheckMetric> {
        info!("Check dispatcher started.");

        let CheckDispatcher {
            tlm,
            health,
            mut check_run_requests,
            mut check_stop_requests,
            mut python_scheduler,
            check_metrics_rx,
        } = self;
        health.set_status_detail(serde_json::to_value(DispatcherStatusDetail::default()).expect("Can serialize"));
        tokio::spawn(async move {
            loop {
                let mut details: DispatcherStatusDetail = serde_json::from_value(health.get_status_detail_copy())
                    .expect("Failed to deserialize status detail");
                select! {
                    Some(check_request) = check_run_requests.recv() => {
                        let decision = python_scheduler.can_run_check(&check_request);
                        match decision {
                            RunnableDecision::CanRun => {
                                match python_scheduler.run_check(&check_request) {
                                    Ok(_) => {
                                        tlm.check_requests_dispatched.increment(1);
                                        details.check_request_state.entry(check_request.name.clone()).or_insert(json!("dispatched to Python"));
                                        debug!("Check request dispatched: {}", check_request);
                                    }
                                    Err(e) => {
                                        details.check_request_state.entry(check_request.name.clone()).or_insert(json!(format!("Python runnable check failed due to error {e}")));
                                        error!("Error dispatching check request: {}", e);
                                    }
                                }
                            }
                            RunnableDecision::CannotRun(reason) => {
                                details.check_request_state.entry(check_request.name.clone()).or_insert(json!(format!("Check request {check_request} cannot be run in python due to {reason}")));
                                error!("Check request {check_request} cannot be run due to: {reason}");
                            }
                        };
                    },
                    Some(check_request) = check_stop_requests.recv() => {
                        info!("Stopping check request: {}", check_request);
                        // TODO check which one is running it and then stop it on that one
                        python_scheduler.stop_check(check_request);
                    },
                    else => {
                        error!("Check Dispatcher cannot recieve any more requests, shutting down.");
                        break;
                    }
                }
                health.set_status_detail(serde_json::to_value(details).expect("Can serialize"));
            }
        });

        check_metrics_rx
    }
}
