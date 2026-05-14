use std::sync::{Arc, Mutex};

use serde_json::Value;

/// Shared state for collected APM telemetry payloads.
#[derive(Clone)]
pub struct ApmTelemetryState {
    payloads: Arc<Mutex<Vec<Value>>>,
}

impl ApmTelemetryState {
    /// Creates a new `ApmTelemetryState`.
    pub fn new() -> Self {
        Self {
            payloads: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Adds a raw JSON payload to the collected set.
    pub fn add_payload(&self, payload: Value) {
        self.payloads.lock().unwrap().push(payload);
    }

    /// Returns all collected payloads.
    pub fn dump_payloads(&self) -> Vec<Value> {
        self.payloads.lock().unwrap().clone()
    }
}
