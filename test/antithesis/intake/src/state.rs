//! Shared application state for the intake.
//!
//! Holds the ingested-metric store that backs `/metrics/dump` and the expected
//! Agent hostname that W17 resolves each series against.

use std::sync::{Arc, Mutex};

use datadog_protos::metrics::{MetricPayload, SketchPayload};
use saluki_error::GenericError;
use stele::Metric;

/// Application state passed to every handler.
#[derive(Clone)]
pub struct AppState {
    /// Ingested metrics, exposed at `/metrics/dump`.
    pub metrics: MetricsState,
    /// The Agent hostname W17 resolves each series' host resource against. Set
    /// from `DD_HOSTNAME` at startup to match the ADP `datadog.yaml`.
    pub expected_hostname: Arc<str>,
}

/// In-memory store of every metric the intake has ingested, in the simplified
/// `stele::Metric` form. Reused by `/metrics/dump`, which the
/// `finally_verify_delivery` test command polls to confirm end-to-end delivery.
#[derive(Clone)]
pub struct MetricsState {
    metrics: Arc<Mutex<Vec<Metric>>>,
}

impl MetricsState {
    /// Creates a new, empty `MetricsState`.
    #[must_use]
    pub fn new() -> Self {
        Self {
            metrics: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Dumps the current metrics state.
    #[must_use]
    pub fn dump_metrics(&self) -> Vec<Metric> {
        self.metrics.lock().unwrap().clone()
    }

    /// Merges the given series v2 payload into the current metrics state.
    ///
    /// # Errors
    ///
    /// Returns an error when a series carries an `UNSPECIFIED` type or an
    /// out-of-range timestamp that `stele` cannot represent.
    pub fn merge_series_v2_payload(&self, payload: MetricPayload) -> Result<(), GenericError> {
        let metrics = Metric::try_from_series_v2(payload)?;
        self.metrics.lock().unwrap().extend(metrics);
        Ok(())
    }

    /// Merges the given series v1 payload into the current metrics state.
    ///
    /// # Errors
    ///
    /// Returns an error when the bytes do not decode as a v1 series payload.
    pub fn merge_series_v1_payload(&self, bytes: &[u8]) -> Result<(), GenericError> {
        let metrics = Metric::try_from_series_v1(bytes)?;
        self.metrics.lock().unwrap().extend(metrics);
        Ok(())
    }

    /// Merges the given sketch payload into the current metrics state.
    ///
    /// # Errors
    ///
    /// Returns an error when a sketch carries an out-of-range timestamp that
    /// `stele` cannot represent.
    pub fn merge_sketch_payload(&self, payload: SketchPayload) -> Result<(), GenericError> {
        let metrics = Metric::try_from_sketch(payload)?;
        self.metrics.lock().unwrap().extend(metrics);
        Ok(())
    }
}
