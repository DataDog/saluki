use std::future::Future;

use panoramic_core::step::HttpResponse;
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use stele::{ClientStatisticsAggregator, Event, Metric, ServiceCheck, Span};
use tracing::{debug, error};

/// Client for the dump endpoints of one `datadog-intake` server.
///
/// The caller supplies the HTTP route; each method fetches one complete telemetry kind.
pub struct IntakeClient<F> {
    request: F,
}

impl<F, Fut> IntakeClient<F>
where
    F: Fn(&'static str) -> Fut,
    Fut: Future<Output = Result<HttpResponse, GenericError>>,
{
    /// Creates a client using the supplied HTTP GET callback.
    pub fn new(request: F) -> Self {
        Self { request }
    }

    async fn get(&self, path: &'static str) -> Result<HttpResponse, GenericError> {
        let response = (self.request)(path).await?;
        if !(200..300).contains(&response.status) {
            return Err(generic_error!(
                "intake dump at '{}' returned HTTP {}",
                path,
                response.status
            ));
        }
        Ok(response)
    }

    /// Fetches the captured events.
    ///
    /// # Errors
    ///
    /// If the endpoint can't be called, or its response can't be decoded, an error is returned.
    pub async fn events(&self) -> Result<Vec<Event>, GenericError> {
        let response = self
            .get("/events/dump")
            .await
            .error_context("Failed to call events dump endpoint on datadog-intake server.")?;
        let events = serde_json::from_slice::<Vec<Event>>(&response.body)
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
        let response = self
            .get("/metrics/dump")
            .await
            .error_context("Failed to call metrics dump endpoint on datadog-intake server.")?;
        let metrics = serde_json::from_slice::<Vec<Metric>>(&response.body)
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
        let response = self
            .get("/service_checks/dump")
            .await
            .error_context("Failed to call service checks dump endpoint on datadog-intake server.")?;
        let checks = serde_json::from_slice::<Vec<ServiceCheck>>(&response.body)
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
        let response = self
            .get("/traces/dump_spans")
            .await
            .error_context("Failed to call spans dump endpoint on datadog-intake server.")?;
        let spans = serde_json::from_slice::<Vec<Span>>(&response.body)
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
            .get("/traces/dump_stats")
            .await
            .error_context("Failed to call trace stats dump endpoint on datadog-intake server.")?;

        let body = response.body;

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
        let response = self
            .get("/dogstatsd-forwarding/dump")
            .await
            .error_context("Failed to call forwarded DogStatsD packet dump endpoint on datadog-intake server.")?;
        let packets = serde_json::from_slice::<Vec<String>>(&response.body)
            .error_context("Failed to decode dumped forwarded DogStatsD packets from datadog-intake response.")?;

        debug!("Forwarded DogStatsD packets dumped successfully.");

        Ok(packets)
    }
}
