use std::sync::{Arc, Mutex};

use datadog_protos::traces::AgentPayload;
use saluki_error::GenericError;

#[derive(Clone)]
pub struct TracesState {
    traces: Arc<Mutex<Vec<()>>>,
}

impl TracesState {
    /// Creates a new `TracesState`.
    pub fn new() -> Self {
        Self {
            traces: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Dumps the current traces state.
    pub fn dump_traces(&self) -> Vec<()> {
        let data = self.traces.lock().unwrap();
        data.clone()
    }

    /// Merges the given agent payload into the current traces state.
    pub fn merge_agent_payload(&self, _payload: AgentPayload) -> Result<(), GenericError> {
        Ok(())
    }
}
