use std::future::Future;

use base64::{engine::general_purpose::STANDARD, Engine as _};
use panoramic_core::step::HandleError;
use serde::de::DeserializeOwned;
use serde_json::Value;
use stele::{ClientStatisticsAggregator, Event, Metric, ServiceCheck, Span};

/// Collected data from a test target.
///
/// Holds all telemetry data sent by the test target to the `datadog-intake` server spawned for the test run.
#[derive(Default)]
pub(crate) struct CollectedData {
    events: Vec<Event>,
    metrics: Vec<Metric>,
    service_checks: Vec<ServiceCheck>,
    spans: Vec<Span>,
    trace_stats: ClientStatisticsAggregator,
    dogstatsd_forwarded_packets: Vec<String>,
}

impl CollectedData {
    /// Collects each telemetry kind once through a participant's `dump` operation.
    ///
    /// # Errors
    ///
    /// Preserves operation errors; an invalid telemetry shape is a product error.
    pub(crate) async fn collect_dumps<F, Fut>(dump: F) -> Result<Self, HandleError>
    where
        F: Fn(&'static str) -> Fut,
        Fut: Future<Output = Result<Value, HandleError>>,
    {
        let events = decode_dump("events", dump("events").await?)?;
        let metrics = decode_dump("metrics", dump("metrics").await?)?;
        let service_checks = decode_dump("service_checks", dump("service_checks").await?)?;
        let spans = decode_dump("spans", dump("spans").await?)?;
        let trace_stats = decode_dump("trace_stats", dump("trace_stats").await?)?;
        let packets: Vec<Vec<u8>> = decode_dump("dogstatsd_forwarded", dump("dogstatsd_forwarded").await?)?;
        Ok(Self {
            events,
            metrics,
            service_checks,
            spans,
            trace_stats,
            dogstatsd_forwarded_packets: packets.into_iter().map(|packet| STANDARD.encode(packet)).collect(),
        })
    }

    /// Returns a reference to the collected events.
    pub(crate) fn events(&self) -> &[Event] {
        &self.events
    }

    /// Returns a reference to the collected metrics.
    pub(crate) fn metrics(&self) -> &[Metric] {
        &self.metrics
    }

    /// Returns a reference to the collected service checks.
    pub(crate) fn service_checks(&self) -> &[ServiceCheck] {
        &self.service_checks
    }

    /// Returns a reference to the collected spans.
    pub(crate) fn spans(&self) -> &[Span] {
        &self.spans
    }

    /// Returns a reference to the collected trace statistics.
    pub(crate) fn trace_stats(&self) -> &ClientStatisticsAggregator {
        &self.trace_stats
    }

    /// Returns a reference to the forwarded DogStatsD packets captured by datadog-intake.
    pub(crate) fn dogstatsd_forwarded_packets(&self) -> &[String] {
        &self.dogstatsd_forwarded_packets
    }
}

fn decode_dump<T: DeserializeOwned>(kind: &str, value: Value) -> Result<T, HandleError> {
    serde_json::from_value(value).map_err(|error| HandleError::Product {
        message: format!("Failed to decode dumped {kind} from intake: {error}"),
    })
}
