use async_trait::async_trait;
use http::{uri::PathAndQuery, HeaderValue, Method, Uri};
use saluki_core::accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_core::{
    components::{encoders::*, BuildContext},
    data_model::{
        event::{service_check::ServiceCheck, Event, EventType},
        payload::{HttpPayload, Payload, PayloadMetadata, PayloadType},
    },
    observability::ComponentMetricsExt as _,
    topology::PayloadsDispatcher,
};
use saluki_error::{ErrorContext as _, GenericError};
use saluki_io::compression::CompressionScheme;
use saluki_metrics::MetricsBuilder;
use tracing::{debug, error, warn};

use crate::common::datadog::{
    clamp_payload_limits,
    io::RB_BUFFER_CHUNK_SIZE,
    request_builder::{EndpointEncoder, RequestBuilder},
    telemetry::ComponentTelemetry,
    DEFAULT_SERIALIZER_COMPRESSED_SIZE_LIMIT, DEFAULT_SERIALIZER_UNCOMPRESSED_SIZE_LIMIT,
};

const MAX_SERVICE_CHECKS_PER_PAYLOAD: usize = 100;

static CONTENT_TYPE_JSON: HeaderValue = HeaderValue::from_static("application/json");

/// Datadog Service Checks incremental encoder.
///
/// Generates Datadog Service Checks payloads for the Datadog platform.
#[derive(Debug)]
pub struct DatadogServiceChecksConfiguration {
    /// Maximum compressed size, in bytes, of a service check payload.
    ///
    /// This matches the Datadog Agent's generic payload limit for service checks. The effective value is
    /// clamped to the Agent's default intake-safe limit of 2,621,440 bytes, so larger configured values do not allow
    /// payloads that intake may reject. If set to `0`, every non-empty compressed payload exceeds the limit and is
    /// dropped during flush.
    ///
    /// Defaults to 2,621,440 bytes.
    pub max_payload_size: usize,

    /// Maximum uncompressed size, in bytes, of a service check payload.
    ///
    /// This matches the Datadog Agent's generic payload limit for service checks. The effective value is
    /// clamped to the Agent's default intake-safe limit of 4,194,304 bytes, so larger configured values do not allow
    /// payloads that intake may reject. Values smaller than the minimum endpoint framing size prevent the request
    /// builder from starting.
    ///
    /// Defaults to 4,194,304 bytes.
    pub max_uncompressed_payload_size: usize,

    /// Compression kind to use for the request payloads.
    ///
    /// Defaults to `zstd`.
    pub compressor_kind: String,

    /// The compression level to use when `zstd` is the algorithm.
    ///
    /// Ignored for algorithms other than `zstd`.
    pub zstd_level: i32,

    /// Whether to log service check payload contents before encoding.
    ///
    /// This logs decoded service check objects, not the encoded HTTP body.
    ///
    /// Defaults to `false`.
    pub log_payloads: bool,
}

#[async_trait]
impl IncrementalEncoderBuilder for DatadogServiceChecksConfiguration {
    type Output = DatadogServiceChecks;

    fn input_event_type(&self) -> EventType {
        EventType::ServiceCheck
    }

    fn output_payload_type(&self) -> PayloadType {
        PayloadType::Http
    }

    async fn build(&self, context: BuildContext) -> Result<Self::Output, GenericError> {
        let metrics_builder = MetricsBuilder::from_component_context(context.component_context());
        let telemetry = ComponentTelemetry::from_builder(&metrics_builder);
        let compression_scheme = CompressionScheme::new(&self.compressor_kind, self.zstd_level);

        // Create our request builder.
        let mut request_builder =
            RequestBuilder::new(ServiceChecksEndpointEncoder, compression_scheme, RB_BUFFER_CHUNK_SIZE).await?;
        let (uncompressed_limit, compressed_limit) = clamp_payload_limits(
            self.max_uncompressed_payload_size,
            self.max_payload_size,
            DEFAULT_SERIALIZER_UNCOMPRESSED_SIZE_LIMIT,
            DEFAULT_SERIALIZER_COMPRESSED_SIZE_LIMIT,
        );
        request_builder.with_len_limits(uncompressed_limit, compressed_limit)?;
        request_builder.with_max_inputs_per_payload(MAX_SERVICE_CHECKS_PER_PAYLOAD);

        Ok(DatadogServiceChecks {
            request_builder,
            telemetry,
            log_payloads: self.log_payloads,
        })
    }
}

impl MemoryBounds for DatadogServiceChecksConfiguration {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        // TODO: How do we properly represent the requests we can generate that may be sitting around in-flight?
        //
        // Theoretically, we'll end up being limited by the size of the downstream forwarder's interconnect, and however
        // many payloads it will buffer internally... so realistically the firm limit boils down to the forwarder itself
        // but we'll have a hard time in the forwarder knowing the maximum size of any given payload being sent in, which
        // then makes it hard to calculate a proper firm bound even though we know the rest of the values required to
        // calculate the firm bound.
        builder
            .minimum()
            .with_single_value::<DatadogServiceChecks>("component struct");

        builder
            .firm()
            // Capture the size of the "split re-encode" buffer in the request builder, which is where we keep owned
            // versions of events that we encode in case we need to actually re-encode them during a split operation.
            .with_array::<ServiceCheck>("service checks split re-encode buffer", MAX_SERVICE_CHECKS_PER_PAYLOAD);
    }
}

pub struct DatadogServiceChecks {
    request_builder: RequestBuilder<ServiceChecksEndpointEncoder>,
    telemetry: ComponentTelemetry,
    log_payloads: bool,
}

#[async_trait]
impl IncrementalEncoder for DatadogServiceChecks {
    async fn process_event(&mut self, event: Event) -> Result<ProcessResult, GenericError> {
        let service_check = match event.try_into_service_check() {
            Some(eventd) => eventd,
            None => return Ok(ProcessResult::Continue),
        };

        if self.log_payloads {
            debug!(?service_check, "Flushing service check.");
        }

        match self.request_builder.encode(service_check).await {
            Ok(None) => Ok(ProcessResult::Continue),
            Ok(Some(service_check)) => Ok(ProcessResult::FlushRequired(Event::ServiceCheck(service_check))),
            Err(e) => {
                if e.is_recoverable() {
                    warn!(error = %e, "Failed to encode Datadog service check due to recoverable error. Continuing...");

                    // TODO: Get the actual number of events dropped from the error itself.
                    self.telemetry.events_dropped_encoder().increment(1);

                    Ok(ProcessResult::Continue)
                } else {
                    Err(e).error_context("Failed to encode Datadog service check due to unrecoverable error.")
                }
            }
        }
    }

    async fn flush(&mut self, dispatcher: &PayloadsDispatcher) -> Result<usize, FlushError> {
        let mut dispatched_events = 0;
        let pending_events = self.request_builder.pending_inputs();

        let maybe_requests = self.request_builder.flush().await;
        for maybe_request in maybe_requests {
            match maybe_request {
                Ok((events, _data_points, _uncompressed_len, request)) => {
                    let payload_meta = PayloadMetadata::from_event_count(events);
                    let http_payload = HttpPayload::new(payload_meta, request);
                    let payload = Payload::Http(http_payload);

                    dispatcher
                        .dispatch(payload)
                        .await
                        .map_err(|source| FlushError::new(dispatched_events, source))?;
                    dispatched_events += events;
                }
                Err(e) => error!(error = %e, "Failed to build Datadog service checks payload. Continuing..."),
            }
        }

        // Events that were queued but not dispatched were in a payload that couldn't be built, such as one that exceeded
        // the size limit and couldn't be split. They're gone, so report them as dropped.
        let dropped_events = pending_events.saturating_sub(dispatched_events);
        if dropped_events > 0 {
            self.telemetry.events_dropped_encoder().increment(dropped_events as u64);
        }

        Ok(dispatched_events)
    }
}

#[derive(Debug)]
struct ServiceChecksEndpointEncoder;

impl EndpointEncoder for ServiceChecksEndpointEncoder {
    type Input = ServiceCheck;
    type EncodeError = serde_json::Error;

    fn encoder_name() -> &'static str {
        "service_check"
    }

    fn compressed_size_limit(&self) -> usize {
        DEFAULT_SERIALIZER_COMPRESSED_SIZE_LIMIT
    }

    fn uncompressed_size_limit(&self) -> usize {
        DEFAULT_SERIALIZER_UNCOMPRESSED_SIZE_LIMIT
    }

    fn encode(&mut self, input: &Self::Input, buffer: &mut Vec<u8>) -> Result<(), Self::EncodeError> {
        serde_json::to_writer(buffer, input)
    }

    fn get_payload_prefix(&self) -> Option<&'static [u8]> {
        Some(b"[")
    }

    fn get_payload_suffix(&self) -> Option<&'static [u8]> {
        Some(b"]")
    }

    fn get_input_separator(&self) -> Option<&'static [u8]> {
        Some(b",")
    }

    fn endpoint_uri(&self) -> Uri {
        PathAndQuery::from_static("/api/v1/check_run").into()
    }

    fn endpoint_method(&self) -> Method {
        Method::POST
    }

    fn content_type(&self) -> HeaderValue {
        CONTENT_TYPE_JSON.clone()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use saluki_core::{
        components::test_util::TestComponentDriver,
        data_model::event::service_check::{CheckStatus, ServiceCheck},
    };
    use serde_json::Value as JsonValue;

    use super::*;
    use crate::common::datadog::test_util::{decompressed_bodies, incompressible_string};
    use crate::encoders::BufferedIncrementalConfiguration;

    /// Returns the default service checks encoder settings.
    fn service_checks_configuration() -> DatadogServiceChecksConfiguration {
        DatadogServiceChecksConfiguration {
            max_payload_size: DEFAULT_SERIALIZER_COMPRESSED_SIZE_LIMIT,
            max_uncompressed_payload_size: DEFAULT_SERIALIZER_UNCOMPRESSED_SIZE_LIMIT,
            compressor_kind: "zstd".to_string(),
            zstd_level: 3,
            log_payloads: false,
        }
    }

    /// Encodes `checks` with the encoder set up as the topology sets it up, and returns the decompressed request
    /// bodies.
    ///
    /// `checks` must fit in a single event buffer, so that the requests depend only on the checks and not on timing.
    async fn encode_service_checks(checks: Vec<ServiceCheck>) -> Vec<(usize, Vec<u8>)> {
        assert!(checks.len() <= 1024, "service checks must fit in a single event buffer");

        // The default encoder flush timeout.
        let config = BufferedIncrementalConfiguration::new(service_checks_configuration(), Duration::from_secs(2));
        let (control, mut payloads) = TestComponentDriver::encoder(config)
            .await
            .expect("service checks encoder should build");
        control.send_events(checks.into_iter().map(Event::ServiceCheck)).await;
        control
            .shutdown()
            .await
            .expect("service checks encoder should stop cleanly");

        decompressed_bodies(payloads.collect().await).await
    }

    fn service_check(index: usize, message: String) -> ServiceCheck {
        ServiceCheck::new(format!("check.{:03}", index), CheckStatus::Ok).with_message(Some(message.into()))
    }

    /// Asserts that every request body is a JSON array holding as many service checks as its metadata counts, and
    /// that the requests hold `expected_checks` service checks in total.
    fn assert_valid_json_requests(requests: &[(usize, Vec<u8>)], expected_checks: usize) {
        for (index, (event_count, body)) in requests.iter().enumerate() {
            let tail = String::from_utf8_lossy(&body[body.len().saturating_sub(16)..]);
            let checks = serde_json::from_slice::<Vec<JsonValue>>(body).unwrap_or_else(|e| {
                panic!(
                    "request {} isn't a valid JSON array ({}), it ends with {:?}",
                    index, e, tail
                )
            });
            assert_eq!(
                *event_count,
                checks.len(),
                "request {} metadata should count its service checks",
                index
            );
        }
        assert_eq!(requests.iter().map(|(count, _)| count).sum::<usize>(), expected_checks);
    }

    #[tokio::test]
    async fn requests_split_at_compressed_size_estimate_are_valid_json() {
        // About 3.8 MB of content that barely compresses, so the compressed size estimate rejects a service check
        // before the 100-check cap or the uncompressed limit is reached.
        let checks = (0..95)
            .map(|index| service_check(index, incompressible_string(40_000, index as u64)))
            .collect::<Vec<_>>();

        let requests = encode_service_checks(checks).await;
        assert!(requests.len() > 1, "the service checks should be split across requests");
        assert_valid_json_requests(&requests, 95);
    }

    #[tokio::test]
    async fn requests_split_at_uncompressed_size_limit_are_valid_json() {
        // About 4.75 MB of content that compresses to almost nothing, so the 4 MiB uncompressed limit rejects a
        // service check before the 100-check cap or the compressed size estimate is reached.
        let checks = (0..95)
            .map(|index| service_check(index, "a".repeat(50_000)))
            .collect::<Vec<_>>();

        let requests = encode_service_checks(checks).await;
        assert!(requests.len() > 1, "the service checks should be split across requests");
        assert_valid_json_requests(&requests, 95);
    }

    #[tokio::test]
    async fn request_after_a_check_that_cannot_fit_with_the_framing_is_valid_json() {
        // A service check whose encoding fits under the uncompressed limit by itself, but not once the `[` and `]`
        // around it are added, is rejected even by an empty payload, and dropped after the retry.
        let limit = service_checks_configuration().max_uncompressed_payload_size;
        let base_len = serde_json::to_vec(&service_check(0, "a".to_string())).unwrap().len();
        let oversized = service_check(0, "a".repeat(limit - base_len));
        assert_eq!(serde_json::to_vec(&oversized).unwrap().len(), limit - 1);

        let requests = encode_service_checks(vec![oversized, service_check(1, "after".to_string())]).await;
        assert_valid_json_requests(&requests, 1);
    }
}
