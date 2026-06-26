use std::sync::{Arc, Mutex};

use datadog_protos::traces::{AgentPayload, StatsPayload};
use saluki_error::GenericError;
use stele::{ClientStatisticsAggregator, Span};

#[derive(Default)]
struct Inner {
    spans: Vec<Span>,
    stats: ClientStatisticsAggregator,
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

    /// Dumps the current set of spans.
    pub fn dump_spans(&self) -> Vec<Span> {
        let inner = self.inner.lock().unwrap();
        inner.spans.clone()
    }

    /// Dumps the current stats.
    pub fn dump_stats(&self) -> ClientStatisticsAggregator {
        let inner = self.inner.lock().unwrap();
        inner.stats.clone()
    }

    /// Merges the given agent payload into the current traces state.
    ///
    /// Both the classic `tracerPayloads` (field 5) and the efficient trace payload format `idxTracerPayloads` (field 11)
    /// paths are decoded. `raw_body` must be the original protobuf-encoded bytes of the
    /// `AgentPayload` and is used to decode field 11 directly, bypassing the incorrectly typed
    /// generated field in the `AgentPayload` struct.
    pub fn merge_agent_payload(&self, payload: AgentPayload, raw_body: &[u8]) -> Result<(), GenericError> {
        let mut new_spans = Span::get_spans_from_agent_payload(&payload);
        new_spans.extend(Span::get_spans_from_idx_bytes(&payload, raw_body));
        let mut inner = self.inner.lock().unwrap();
        inner.spans.extend(new_spans);

        Ok(())
    }

    /// Merges the given stats payload into the current traces state.
    pub fn merge_stats_payload(&self, payload: StatsPayload) -> Result<(), GenericError> {
        let mut inner = self.inner.lock().unwrap();
        inner.stats.merge_payload(&payload)
    }
}
