use std::sync::{Arc, Mutex};

use datadog_protos::traces::{AgentPayload, StatsPayload};
use saluki_error::GenericError;
use stele::{ClientStatisticsAggregator, Span};

#[derive(Default)]
struct Inner {
    spans: Vec<Span>,
    stats_aggregator: ClientStatisticsAggregator,
}

#[derive(Clone)]
pub struct TracesState {
    inner: Arc<Mutex<Inner>>,
}

impl TracesState {
    /// Creates a new `TracesState`.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner::default())),
        }
    }

    /// Dumps the current traces state.
    pub fn dump(&self) -> Vec<Span> {
        let inner = self.inner.lock().unwrap();
        inner.spans.clone()
    }

    /// Merges the given agent payload into the current traces state.
    pub fn merge_agent_payload(&self, payload: AgentPayload) -> Result<(), GenericError> {
        let new_spans = Span::get_spans_from_agent_payload(&payload);
        let mut inner = self.inner.lock().unwrap();
        inner.spans.extend(new_spans);

        Ok(())
    }

    /// Merges the given stats payload into the current traces state.
    pub fn merge_stats_payload(&self, payload: StatsPayload) -> Result<(), GenericError> {
        let mut inner = self.inner.lock().unwrap();
        inner.stats_aggregator.merge_payload(&payload)
    }
}
