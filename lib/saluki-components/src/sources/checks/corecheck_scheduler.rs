use super::*;
pub struct CoreCheckScheduler {
    send_check_metrics: mpsc::Sender<CheckMetric>,
}

impl CoreCheckScheduler {
    pub fn new(send_check_metrics: mpsc::Sender<CheckMetric>) -> Result<Self, GenericError> {
        Ok(CoreCheckScheduler { send_check_metrics })
    }
}

impl CheckScheduler for CoreCheckScheduler {
    fn can_run_check(&self, check_request: &RunnableCheckRequest) -> bool {
        // todo
        info!(
            "Considered whether to run check as corecheck: {:?}",
            check_request.check_request
        );
        false
    }
    fn run_check(&mut self, check_request: &RunnableCheckRequest) -> Result<(), GenericError> {
        // todo

        Err(generic_error!("unimplemented"))
    }
    fn stop_check(&mut self, check_name: CheckRequest) {
        // todo
    }
}
