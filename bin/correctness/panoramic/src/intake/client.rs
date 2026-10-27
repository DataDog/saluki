use saluki_error::{generic_error, ErrorContext as _, GenericError};
use stele::{ClientStatisticsAggregator, Event, Metric, ServiceCheck, Span};
use tracing::{debug, error};

/// Client for the dump endpoints of one `datadog-intake` server.
///
/// Each method fetches the full set of one telemetry kind the server has received so far.
pub struct IntakeClient {
    client: reqwest::Client,
    base_url: String,
}

impl IntakeClient {
    /// Creates a client for the `datadog-intake` server published on the given local port.
    pub fn new(port: u16) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url: format!("http://localhost:{}", port),
        }
    }

    /// Fetches the captured events.
    ///
    /// # Errors
    ///
    /// If the endpoint can't be called, or its response can't be decoded, an error is returned.
    pub async fn events(&self) -> Result<Vec<Event>, GenericError> {
        let events = self
            .client
            .get(format!("{}/events/dump", self.base_url))
            .send()
            .await
            .error_context("Failed to call events dump endpoint on datadog-intake server.")?
            .json::<Vec<Event>>()
            .await
            .error_context("Failed to decode dumped events from datadog-intake response.")?;

        debug!("Events dumped successfully.");

        Ok(events)
    }

    /// Fetches the captured metrics, one entry per series per payload the server decoded.
    ///
    /// # Errors
    ///
    /// If the endpoint can't be called, or its response can't be decoded, an error is returned.
    pub async fn metrics(&self) -> Result<Vec<Metric>, GenericError> {
        let metrics = self
            .client
            .get(format!("{}/metrics/dump", self.base_url))
            .send()
            .await
            .error_context("Failed to call metrics dump endpoint on datadog-intake server.")?
            .json::<Vec<Metric>>()
            .await
            .error_context("Failed to decode dumped metrics from datadog-intake response.")?;

        debug!("Metrics dumped successfully.");

        Ok(metrics)
    }

    /// Fetches the captured service checks.
    ///
    /// # Errors
    ///
    /// If the endpoint can't be called, or its response can't be decoded, an error is returned.
    pub async fn service_checks(&self) -> Result<Vec<ServiceCheck>, GenericError> {
        let checks = self
            .client
            .get(format!("{}/service_checks/dump", self.base_url))
            .send()
            .await
            .error_context("Failed to call service checks dump endpoint on datadog-intake server.")?
            .json::<Vec<ServiceCheck>>()
            .await
            .error_context("Failed to decode dumped service checks from datadog-intake response.")?;

        debug!("Service checks dumped successfully.");

        Ok(checks)
    }

    /// Fetches the captured spans.
    ///
    /// # Errors
    ///
    /// If the endpoint can't be called, or its response can't be decoded, an error is returned.
    pub async fn spans(&self) -> Result<Vec<Span>, GenericError> {
        let spans = self
            .client
            .get(format!("{}/traces/dump_spans", self.base_url))
            .send()
            .await
            .error_context("Failed to call spans dump endpoint on datadog-intake server.")?
            .json::<Vec<Span>>()
            .await
            .error_context("Failed to decode dumped spans from datadog-intake response.")?;

        debug!("Spans dumped successfully.");

        Ok(spans)
    }

    /// Fetches the captured trace statistics.
    ///
    /// # Errors
    ///
    /// If the endpoint can't be called, or its response can't be decoded, an error is returned.
    pub async fn trace_stats(&self) -> Result<ClientStatisticsAggregator, GenericError> {
        let response = self
            .client
            .get(format!("{}/traces/dump_stats", self.base_url))
            .send()
            .await
            .error_context("Failed to call trace stats dump endpoint on datadog-intake server.")?;

        let body = response.bytes().await.error_context("Failed to read response body")?;

        let stats = match serde_json::from_slice(&body) {
            Ok(stats) => stats,
            Err(e) => {
                error!(body = %String::from_utf8_lossy(&body), "Failed to decode trace stats response.");
                return Err(generic_error!(
                    "Failed to decode dumped trace stats from datadog-intake response: {}",
                    e
                ));
            }
        };

        debug!("Trace stats dumped successfully.");

        Ok(stats)
    }

    /// Fetches the captured forwarded DogStatsD packets.
    ///
    /// # Errors
    ///
    /// If the endpoint can't be called, or its response can't be decoded, an error is returned.
    pub async fn dogstatsd_forwarded_packets(&self) -> Result<Vec<String>, GenericError> {
        let packets = self
            .client
            .get(format!("{}/dogstatsd-forwarding/dump", self.base_url))
            .send()
            .await
            .error_context("Failed to call forwarded DogStatsD packet dump endpoint on datadog-intake server.")?
            .json::<Vec<String>>()
            .await
            .error_context("Failed to decode dumped forwarded DogStatsD packets from datadog-intake response.")?;

        debug!("Forwarded DogStatsD packets dumped successfully.");

        Ok(packets)
    }
}
