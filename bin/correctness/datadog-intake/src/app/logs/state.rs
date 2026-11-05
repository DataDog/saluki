use std::sync::{Arc, Mutex};

use stele::LogsRequest;

#[derive(Clone)]
pub struct LogsState {
    requests: Arc<Mutex<Vec<LogsRequest>>>,
}

impl LogsState {
    pub fn new() -> Self {
        Self {
            requests: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn dump_requests(&self) -> Vec<LogsRequest> {
        self.requests.lock().unwrap().clone()
    }

    pub fn merge_request(&self, request: LogsRequest) {
        self.requests.lock().unwrap().push(request);
    }
}
