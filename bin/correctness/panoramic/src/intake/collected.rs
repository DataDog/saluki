use saluki_error::GenericError;
use stele::{ClientStatisticsAggregator, Event, Metric, ServiceCheck, Span};

use super::IntakeClient;

/// Collected data from a test target.
///
/// Holds all telemetry data sent by the test target to the `datadog-intake` server spawned for the test run.
#[cfg_attr(test, derive(Default))]
pub struct CollectedData {
    events: Vec<Event>,
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
        let metrics = client.metrics().await?;
        let service_checks = client.service_checks().await?;
        let spans = client.spans().await?;
        let trace_stats = client.trace_stats().await?;
        let dogstatsd_forwarded_packets = client.dogstatsd_forwarded_packets().await?;

        Ok(Self {
            events,
            metrics,
            service_checks,
            spans,
            trace_stats,
            dogstatsd_forwarded_packets,
        })
    }

    /// Returns a reference to the collected events.
    pub fn events(&self) -> &[Event] {
        &self.events
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
