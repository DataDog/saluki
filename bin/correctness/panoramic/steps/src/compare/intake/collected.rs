use std::future::Future;

use base64::{engine::general_purpose::STANDARD, Engine as _};
use panoramic_core::step::{HandleError, HttpResponse};
use saluki_error::GenericError;
use serde::de::DeserializeOwned;
use serde_json::Value;
use stele::{ClientStatisticsAggregator, Event, Metric, ServiceCheck, Span};

use super::IntakeClient;

/// Collected data from a test target.
///
/// Holds all telemetry data sent by the test target to the `datadog-intake` server spawned for the test run.
#[derive(Default)]
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
    pub async fn collect<F, Fut>(client: &IntakeClient<F>) -> Result<Self, GenericError>
    where
        F: Fn(&'static str) -> Fut,
        Fut: Future<Output = Result<HttpResponse, GenericError>>,
    {
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

fn decode_dump<T: DeserializeOwned>(kind: &str, value: Value) -> Result<T, HandleError> {
    serde_json::from_value(value).map_err(|error| HandleError::Product {
        message: format!("Failed to decode dumped {kind} from intake: {error}"),
    })
}
