use saluki_error::{ErrorContext as _, GenericError};
use stele::{Metric, Span};
use tracing::debug;

/// Collected data from a test target.
///
/// Holds all telemetry data sent by the test target to the `datadog-intake` server spawned for the test run.
pub struct CollectedData {
    metrics: Vec<Metric>,
    spans: Vec<Span>,
}

impl CollectedData {
    /// Creates a new `CollectedData` instance, collecting data from the `datadog-intake` server at the given port.
    ///
    /// # Errors
    ///
    /// If the collected data cannot be retrieved from the `datadog-intake` server, an error is returned.
    pub async fn for_port(datadog_intake_port: u16) -> Result<Self, GenericError> {
        let metrics = get_captured_metrics(datadog_intake_port).await?;
        let spans = get_captured_spans(datadog_intake_port).await?;

        Ok(Self { metrics, spans })
    }

    /// Returns a reference to the collected metrics.
    pub fn metrics(&self) -> &[Metric] {
        &self.metrics
    }

    /// Returns a reference to the collected spans.
    pub fn spans(&self) -> &[Span] {
        &self.spans
    }
}

async fn get_captured_metrics(datadog_intake_port: u16) -> Result<Vec<Metric>, GenericError> {
    let client = reqwest::Client::new();
    let metrics = client
        .get(format!("http://localhost:{}/metrics/dump", datadog_intake_port))
        .send()
        .await
        .error_context("Failed to call metrics dump endpoint on datadog-intake server.")?
        .json::<Vec<Metric>>()
        .await
        .error_context("Failed to decode dumped metrics from datadog-intake response.")?;

    debug!("Metrics dumped successfully.");

    Ok(metrics)
}

async fn get_captured_spans(datadog_intake_port: u16) -> Result<Vec<Span>, GenericError> {
    let client = reqwest::Client::new();
    let spans = client
        .get(format!("http://localhost:{}/traces/dump", datadog_intake_port))
        .send()
        .await
        .error_context("Failed to call traces dump endpoint on datadog-intake server.")?
        .json::<Vec<Span>>()
        .await
        .error_context("Failed to decode dumped spans from datadog-intake response.")?;

    debug!("Spans dumped successfully.");

    Ok(spans)
}
