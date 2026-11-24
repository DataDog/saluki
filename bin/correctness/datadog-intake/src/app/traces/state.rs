use std::sync::{Arc, Mutex};

use datadog_protos::traces::{AgentPayload, StatsPayload};
use saluki_error::GenericError;
use stele::{ClientStatisticsAggregator, IntakeRequest, Span};
use tracing::debug;

use crate::app::RequestHead;

#[derive(Default)]
struct Inner {
    span_requests: Vec<IntakeRequest<Vec<Span>>>,
    stats: ClientStatisticsAggregator,
    stats_requests: Vec<IntakeRequest<ClientStatisticsAggregator>>,
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

    /// Dumps the spans of all requests, in the order that the requests arrived.
    pub fn dump_spans(&self) -> Vec<Span> {
        let inner = self.inner.lock().unwrap();
        inner
            .span_requests
            .iter()
            .flat_map(|request| request.items().iter().cloned())
            .collect()
    }

    /// Dumps the current stats.
    pub fn dump_stats(&self) -> ClientStatisticsAggregator {
        let inner = self.inner.lock().unwrap();
        inner.stats.clone()
    }

    /// Dumps the requests that carried spans.
    pub fn dump_span_requests(&self) -> Vec<IntakeRequest<Vec<Span>>> {
        self.inner.lock().unwrap().span_requests.clone()
    }

    /// Dumps the requests that carried stats, including those that the stats merge rejected.
    pub fn dump_stats_requests(&self) -> Vec<IntakeRequest<ClientStatisticsAggregator>> {
        self.inner.lock().unwrap().stats_requests.clone()
    }

    /// Merges the given agent payload into the current traces state.
    pub fn merge_agent_payload(&self, head: RequestHead, payload: AgentPayload) -> Result<(), GenericError> {
        let spans = Span::get_spans_from_agent_payload(&payload);
        let mut inner = self.inner.lock().unwrap();
        inner.span_requests.push(head.into_request(spans));

        Ok(())
    }

    /// Merges the given stats payload into the current traces state.
    ///
    /// The request is recorded before the merge, so a request that the merge rejects still appears in the dump.
    pub fn merge_stats_payload(&self, head: RequestHead, payload: StatsPayload) -> Result<(), GenericError> {
        // A payload that fails to merge into this empty aggregator fails in the shared one too. Its record then keeps
        // the groups merged before the failure.
        let mut groups = ClientStatisticsAggregator::default();
        if let Err(e) = groups.merge_payload(&payload) {
            debug!(error = %e, "Stats payload is inconsistent, so its record is partial.");
        }

        let mut inner = self.inner.lock().unwrap();
        inner.stats_requests.push(head.into_request(groups));
        inner.stats.merge_payload(&payload)
    }
}
