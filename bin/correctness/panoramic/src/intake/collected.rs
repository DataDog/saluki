use saluki_error::GenericError;
use stele::{ClientStatisticsAggregator, Event, Log, LogsRequest, Metric, ServiceCheck, Span};

use super::IntakeClient;

/// Collected data from a test target.
///
/// Holds all telemetry data sent by the test target to the `datadog-intake` server spawned for the test run.
#[cfg_attr(test, derive(Default))]
pub struct CollectedData {
    events: Vec<Event>,
    log_requests: Vec<LogsRequest>,
    metrics: Vec<Metric>,
    service_checks: Vec<ServiceCheck>,
    spans: Vec<Span>,
    trace_stats: ClientStatisticsAggregator,
    dogstatsd_forwarded_packets: Vec<String>,
}

impl CollectedData {
    /// Collects every telemetry kind from the `datadog-intake` server behind `client`.
    ///
    /// Endpoints are fetched one at a time, and the collection is all or nothing: the first failing endpoint discards
    /// whatever was already fetched.
    ///
    /// # Errors
    ///
    /// If any endpoint's data can't be retrieved from the `datadog-intake` server, an error is returned.
    pub async fn collect(client: &IntakeClient) -> Result<Self, GenericError> {
        let events = client.events().await?;
        let log_requests = client.logs().await?;
        let metrics = client.metrics().await?;
        let service_checks = client.service_checks().await?;
        let spans = client.spans().await?;
        let trace_stats = client.trace_stats().await?;
        let dogstatsd_forwarded_packets = client.dogstatsd_forwarded_packets().await?;

        Ok(Self {
            events,
            log_requests,
            metrics,
            service_checks,
            spans,
            trace_stats,
            dogstatsd_forwarded_packets,
        })
    }

    /// Creates collected data holding only the given logs requests.
    #[cfg(test)]
    pub fn with_logs(log_requests: Vec<LogsRequest>) -> Self {
        Self {
            log_requests,
            ..Self::default()
        }
    }

    /// Returns a reference to the collected events.
    pub fn events(&self) -> &[Event] {
        &self.events
    }

    /// Returns a reference to the collected logs requests.
    pub fn log_requests(&self) -> &[LogsRequest] {
        &self.log_requests
    }

    /// Returns the collected logs from every request, in the order they were received.
    pub fn logs(&self) -> impl Iterator<Item = &Log> {
        self.log_requests.iter().flat_map(LogsRequest::logs)
    }

    /// Returns a reference to the collected metrics.
    pub fn metrics(&self) -> &[Metric] {
        &self.metrics
    }

    /// Returns a reference to the collected service checks.
    pub fn service_checks(&self) -> &[ServiceCheck] {
        &self.service_checks
    }

    /// Returns a reference to the collected spans.
    pub fn spans(&self) -> &[Span] {
        &self.spans
    }

    /// Returns a reference to the collected trace statistics.
    pub fn trace_stats(&self) -> &ClientStatisticsAggregator {
        &self.trace_stats
    }

    /// Returns a reference to the forwarded DogStatsD packets captured by datadog-intake.
    pub fn dogstatsd_forwarded_packets(&self) -> &[String] {
        &self.dogstatsd_forwarded_packets
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

        let collected = tokio::time::timeout(
            Duration::from_secs(120),
            CollectedData::collect(&IntakeClient::new(intake.port())),
        )
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
