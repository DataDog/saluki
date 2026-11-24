use std::sync::{Arc, Mutex};

use datadog_protos::metrics::{v3::Payload as V3Payload, MetricPayload, SketchPayload};
use saluki_error::GenericError;
use stele::{IntakeRequest, Metric};

use crate::app::RequestHead;

#[derive(Clone)]
pub struct MetricsState {
    requests: Arc<Mutex<Vec<IntakeRequest<Vec<Metric>>>>>,
}

impl MetricsState {
    /// Creates a new `MetricsState`.
    pub fn new() -> Self {
        Self {
            requests: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Dumps the metrics of all requests, in the order that the requests arrived.
    pub fn dump_metrics(&self) -> Vec<Metric> {
        let requests = self.requests.lock().unwrap();
        requests
            .iter()
            .flat_map(|request| request.items().iter().cloned())
            .collect()
    }

    /// Dumps the requests that carried metrics.
    pub fn dump_requests(&self) -> Vec<IntakeRequest<Vec<Metric>>> {
        self.requests.lock().unwrap().clone()
    }

    /// Merges the given series v2 payload into the current metrics state.
    pub fn merge_series_v2_payload(&self, head: RequestHead, payload: MetricPayload) -> Result<(), GenericError> {
        let metrics = Metric::try_from_series_v2(payload)?;
        self.record_request(head, metrics);

        Ok(())
    }

    /// Merges the given sketch payload into the current metrics state.
    pub fn merge_sketch_payload(&self, head: RequestHead, payload: SketchPayload) -> Result<(), GenericError> {
        let metrics = Metric::try_from_sketch(payload)?;
        self.record_request(head, metrics);

        Ok(())
    }

    /// Merges the given metrics v3 payload into the current metrics state.
    pub fn merge_v3_payload(&self, head: RequestHead, payload: V3Payload) -> Result<(), GenericError> {
        let metrics = Metric::try_from_v3(payload)?;
        self.record_request(head, metrics);

        Ok(())
    }

    /// Merges the given series v1 payload into the current metrics state.
    pub fn merge_series_v1_payload(&self, head: RequestHead, bytes: &[u8]) -> Result<(), GenericError> {
        let metrics = Metric::try_from_series_v1(bytes)?;
        self.record_request(head, metrics);

        Ok(())
    }

    fn record_request(&self, head: RequestHead, metrics: Vec<Metric>) {
        self.requests.lock().unwrap().push(head.into_request(metrics));
    }
}
