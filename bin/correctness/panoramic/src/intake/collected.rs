use stele::{ClientStatisticsAggregator, Event, Log, LogsRequest, Metric, ServiceCheck, Span};

/// Collected data from a test target.
///
/// Holds all telemetry data sent by the test target to the `datadog-intake` server spawned for the test run.
#[cfg_attr(test, derive(Default))]
pub struct CollectedData {
    pub(crate) events: Vec<Event>,
    pub(crate) log_requests: Vec<LogsRequest>,
    pub(crate) metrics: Vec<Metric>,
    pub(crate) service_checks: Vec<ServiceCheck>,
    pub(crate) spans: Vec<Span>,
    pub(crate) trace_stats: ClientStatisticsAggregator,
    pub(crate) dogstatsd_forwarded_packets: Vec<String>,
}

impl CollectedData {
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
