use std::sync::{Arc, Mutex};

use datadog_protos::traces::AgentPayload;
use saluki_error::GenericError;
use stele::Span;

#[derive(Clone)]
pub struct TracesState {
    spans: Arc<Mutex<Vec<Span>>>,
}

impl TracesState {
    /// Creates a new `TracesState`.
    pub fn new() -> Self {
        Self {
            spans: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Dumps the current traces state.
    pub fn dump(&self) -> Vec<Span> {
        let data = self.spans.lock().unwrap();
        data.clone()
    }

    /// Merges the given agent payload into the current traces state.
    pub fn merge_agent_payload(&self, payload: AgentPayload) -> Result<(), GenericError> {
        let new_spans = Span::get_spans_from_agent_payload(&payload);
        let mut data = self.spans.lock().unwrap();
        data.extend(new_spans);

        Ok(())
    }
}
