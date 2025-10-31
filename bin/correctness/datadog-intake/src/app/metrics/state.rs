use std::sync::{Arc, Mutex};

use datadog_protos::metrics::{MetricPayload, SketchPayload};
use saluki_error::GenericError;
use stele::Metric;

#[derive(Clone)]
pub struct MetricsState {
    metrics: Arc<Mutex<Vec<Metric>>>,
}

impl MetricsState {
    /// Creates a new `MetricsState`.
    pub fn new() -> Self {
        Self {
            metrics: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Dumps the current metrics state.
    pub fn dump_metrics(&self) -> Vec<Metric> {
        let data = self.metrics.lock().unwrap();
        data.clone()
    }

    /// Merges the given series payload into the current metrics state.
    pub fn merge_series_payload(&self, payload: MetricPayload) -> Result<(), GenericError> {
        let metrics = Metric::try_from_series(payload)?;

        let mut data = self.metrics.lock().unwrap();
        data.extend(metrics);

        Ok(())
    }

    /// Merges the given sketch payload into the current metrics state.
    pub fn merge_sketch_payload(&self, payload: SketchPayload) -> Result<(), GenericError> {
        let metrics = Metric::try_from_sketch(payload)?;

        let mut data = self.metrics.lock().unwrap();
        data.extend(metrics);

        Ok(())
    }
}
