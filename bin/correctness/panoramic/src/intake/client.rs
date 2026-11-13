use std::time::Duration;

use saluki_error::{generic_error, ErrorContext as _, GenericError};
use stele::{ClientStatisticsAggregator, Event, LogsRequest, Metric, ServiceCheck, Span};
use tracing::{debug, error};

use crate::steps::intake::CollectedData;

/// Maximum wait for response headers or between body reads.
const INTAKE_READ_TIMEOUT: Duration = Duration::from_secs(30);

/// Client for the dump endpoints of one `datadog-intake` server.
///
/// Each method fetches the full set of one telemetry kind the server has received so far.
pub struct IntakeClient {
    client: reqwest::Client,
    base_url: String,
}

impl IntakeClient {
    /// Creates a client for the `datadog-intake` server published on the given local port.
    ///
    /// Requests fail after [`INTAKE_READ_TIMEOUT`] without data. Each read resets the timeout, allowing large responses
    /// that keep making progress.
    ///
    /// # Panics
    ///
    /// Panics if the TLS backend can't be initialized, as [`reqwest::Client::new`] does.
    pub fn new(port: u16) -> Self {
        Self {
            client: reqwest::Client::builder()
                .read_timeout(INTAKE_READ_TIMEOUT)
                .build()
                .expect("the intake HTTP client should build"),
            // `localhost` can try IPv6 first, which delays each connection on Windows.
            base_url: format!("http://127.0.0.1:{}", port),
        }
    }

    /// Collects every telemetry kind from the server.
    ///
    /// Endpoints are fetched one at a time, and the collection is all or nothing: the first failing endpoint discards
    /// whatever was already fetched.
    ///
    /// # Errors
    ///
    /// If any endpoint's data can't be retrieved from the `datadog-intake` server, an error is returned.
    pub async fn collect(&self) -> Result<CollectedData, GenericError> {
        Ok(CollectedData {
            events: self.events().await?,
            log_requests: self.logs().await?,
            metrics: self.metrics().await?,
            service_checks: self.service_checks().await?,
            spans: self.spans().await?,
            trace_stats: self.trace_stats().await?,
            dogstatsd_forwarded_packets: self.dogstatsd_forwarded_packets().await?,
        })
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

    /// Fetches the captured logs requests.
    ///
    /// # Errors
    ///
    /// If the endpoint can't be called, or its response can't be decoded, an error is returned.
    pub async fn logs(&self) -> Result<Vec<LogsRequest>, GenericError> {
        let requests = self
            .client
            .get(format!("{}/logs/dump", self.base_url))
            .send()
            .await
            .error_context("Failed to call logs dump endpoint on datadog-intake server.")?
            .json::<Vec<LogsRequest>>()
            .await
            .error_context("Failed to decode dumped logs from datadog-intake response.")?;

        debug!("Logs dumped successfully.");

        Ok(requests)
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

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::time::Instant;

    use super::*;
    use crate::intake::test_server::MockIntake;

    #[tokio::test(start_paused = true)]
    async fn collection_from_a_stalled_intake_fails_within_the_read_timeout() {
        let intake = MockIntake::unresponsive().await;
        let started = Instant::now();

        let collected = tokio::time::timeout(Duration::from_secs(120), IntakeClient::new(intake.port()).collect())
            .await
            .expect("collection should give up on a stalled intake within the bound");

        let Err(error) = collected else {
            panic!("collection from a stalled intake should fail");
        };
        assert_eq!(started.elapsed(), Duration::from_secs(30));
        assert!(
            format!("{error:#}").starts_with("Failed to call events dump endpoint on datadog-intake server.: "),
            "{error:#}"
        );
        assert!(
            error.chain().any(|cause| cause
                .downcast_ref::<reqwest::Error>()
                .is_some_and(reqwest::Error::is_timeout)),
            "{error:#}"
        );
    }
}
