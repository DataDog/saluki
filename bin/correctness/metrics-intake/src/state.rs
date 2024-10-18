use std::sync::{Arc, Mutex};

use datadog_protos::metrics::{MetricPayload, SketchPayload};
use saluki_error::GenericError;
use stele::Metric;
use tokio::sync::mpsc;

#[derive(Clone)]
pub struct IntakeState {
    shutdown_tx: mpsc::Sender<()>,
    metrics: Arc<Mutex<Vec<Metric>>>,
}

impl IntakeState {
    /// Creates a new `IntakeState` with the given shutdown trigger.
    pub fn new(shutdown_tx: mpsc::Sender<()>) -> Self {
        Self {
            shutdown_tx,
            metrics: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Dumps the current metrics state.
    pub fn dump_metrics(&self) -> Vec<Metric> {
        let data = self.metrics.lock().unwrap();
        data.clone()
    }

    /// Triggers shutdown of the intake server.
    pub fn trigger_shutdown(&self) {
        self.shutdown_tx.try_send(()).unwrap();
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
