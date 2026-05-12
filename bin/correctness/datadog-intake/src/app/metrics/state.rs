use std::sync::{Arc, Mutex};

use datadog_protos::metrics::{MetricPayload, SketchPayload};
use saluki_error::GenericError;
use stele::Metric;
use tokio::sync::watch;

#[derive(Clone)]
pub struct MetricsState {
    metrics: Arc<Mutex<Vec<Metric>>>,
    series_count_tx: Arc<watch::Sender<usize>>,
}

impl MetricsState {
    /// Creates a new `MetricsState`, returning a receiver that tracks how many
    /// `api/v2/series` payloads have been processed.
    pub fn new() -> (Self, watch::Receiver<usize>) {
        let (tx, rx) = watch::channel(0usize);
        let state = Self {
            metrics: Arc::new(Mutex::new(Vec::new())),
            series_count_tx: Arc::new(tx),
        };
        (state, rx)
    }

    /// Dumps the current metrics state.
    pub fn dump_metrics(&self) -> Vec<Metric> {
        let data = self.metrics.lock().unwrap();
        data.clone()
    }

    /// Merges the given series payload into the current metrics state.
    pub fn merge_series_payload(&self, payload: MetricPayload) -> Result<(), GenericError> {
        self.series_count_tx.send_modify(|n| *n += 1);

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
