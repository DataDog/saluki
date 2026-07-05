use agent_data_plane_config::shared::{Compression, MetricsEncoding};
use async_trait::async_trait;
use http::{uri::PathAndQuery, HeaderValue, Method, Uri};
use resource_accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_core::{
    components::{encoders::*, ComponentContext},
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
#[cfg_attr(test, derive(Debug, PartialEq))]
pub struct DatadogServiceChecksConfiguration {
    /// Maximum compressed size, in bytes, of a service check payload.
    ///
    /// This matches the Datadog Agent's generic payload limit for service checks. The effective value is
    /// clamped to the Agent's default intake-safe limit of 2,621,440 bytes, so larger configured values do not allow
    /// payloads that intake may reject. If set to `0`, every non-empty compressed payload exceeds the limit and is
    /// dropped during flush.
    ///
    max_payload_size: usize,

    /// Maximum uncompressed size, in bytes, of a service check payload.
    ///
    /// This matches the Datadog Agent's generic payload limit for service checks. The effective value is
    /// clamped to the Agent's default intake-safe limit of 4,194,304 bytes, so larger configured values do not allow
    /// payloads that intake may reject. Values smaller than the minimum endpoint framing size prevent the request
    /// builder from starting.
    ///
    max_uncompressed_payload_size: usize,

    /// Compression kind to use for the request payloads.
    compressor_kind: String,

    /// Compressor level to use when the compressor kind is `zstd`.
    zstd_compressor_level: i32,

    /// Whether to log service check payload contents before encoding.
    ///
    /// This logs decoded service check objects, not the encoded HTTP body.
    log_payloads: bool,
}

impl DatadogServiceChecksConfiguration {
    /// Creates a new `DatadogServiceChecksConfiguration` from the shared typed configuration.
    pub fn from_configuration(
        metrics_encoding: &MetricsEncoding, compression: &Compression,
    ) -> Result<Self, GenericError> {
        Ok(Self {
            max_payload_size: metrics_encoding.max_payload_size,
            max_uncompressed_payload_size: metrics_encoding.max_uncompressed_payload_size,
            compressor_kind: compression.compressor_kind.clone(),
            zstd_compressor_level: compression.zstd_compressor_level,
            log_payloads: metrics_encoding.log_payloads,
        })
    }
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

    async fn build(&self, context: ComponentContext) -> Result<Self::Output, GenericError> {
        let metrics_builder = MetricsBuilder::from_component_context(&context);
        let telemetry = ComponentTelemetry::from_builder(&metrics_builder);
        let compression_scheme = CompressionScheme::new(&self.compressor_kind, self.zstd_compressor_level);

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

    async fn flush(&mut self, dispatcher: &PayloadsDispatcher) -> Result<(), GenericError> {
        let maybe_requests = self.request_builder.flush().await;
        for maybe_request in maybe_requests {
            match maybe_request {
                Ok((events, _data_points, request)) => {
                    let payload_meta = PayloadMetadata::from_event_count(events);
                    let http_payload = HttpPayload::new(payload_meta, request);
                    let payload = Payload::Http(http_payload);

                    dispatcher.dispatch(payload).await?;
                }
                Err(e) => error!(error = %e, "Failed to build Datadog service checks payload. Continuing..."),
            }
        }

        Ok(())
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
    use agent_data_plane_config::shared::{Compression, MetricsEncoding};

    use super::DatadogServiceChecksConfiguration;

    #[test]
    fn from_configuration_reads_typed_shared_values() {
        let metrics_encoding = MetricsEncoding {
            max_payload_size: 123,
            max_uncompressed_payload_size: 456,
            log_payloads: true,
            ..Default::default()
        };

        let compression = Compression {
            compressor_kind: "gzip".to_owned(),
            zstd_compressor_level: 7,
        };

        let config = DatadogServiceChecksConfiguration::from_configuration(&metrics_encoding, &compression)
            .expect("typed configuration should be accepted");

        assert_eq!(config.max_payload_size, 123);
        assert_eq!(config.max_uncompressed_payload_size, 456);
        assert_eq!(config.compressor_kind, "gzip");
        assert_eq!(config.zstd_compressor_level, 7);
        assert!(config.log_payloads);
    }
}
