use async_trait::async_trait;
use facet::Facet;
use http::{uri::PathAndQuery, HeaderValue, Method, Uri};
use resource_accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_config::GenericConfiguration;
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
use serde::Deserialize;
use tracing::{debug, error, warn};

use crate::common::datadog::{
    clamp_payload_limits,
    io::RB_BUFFER_CHUNK_SIZE,
    request_builder::{EndpointEncoder, RequestBuilder},
    telemetry::ComponentTelemetry,
    DEFAULT_SERIALIZER_COMPRESSED_SIZE_LIMIT, DEFAULT_SERIALIZER_UNCOMPRESSED_SIZE_LIMIT,
};

const DEFAULT_SERIALIZER_COMPRESSOR_KIND: &str = "zstd";
const MAX_SERVICE_CHECKS_PER_PAYLOAD: usize = 100;

static CONTENT_TYPE_JSON: HeaderValue = HeaderValue::from_static("application/json");

fn default_serializer_compressor_kind() -> String {
    DEFAULT_SERIALIZER_COMPRESSOR_KIND.to_owned()
}

const fn default_zstd_compressor_level() -> i32 {
    3
}

const fn default_max_payload_size() -> usize {
    DEFAULT_SERIALIZER_COMPRESSED_SIZE_LIMIT
}

const fn default_max_uncompressed_payload_size() -> usize {
    DEFAULT_SERIALIZER_UNCOMPRESSED_SIZE_LIMIT
}

const fn default_log_payloads() -> bool {
    false
}

/// Datadog Service Checks incremental encoder.
///
/// Generates Datadog Service Checks payloads for the Datadog platform.
#[derive(Deserialize, Facet)]
#[cfg_attr(test, derive(Debug, PartialEq, serde::Serialize))]
pub struct DatadogServiceChecksConfiguration {
    /// Maximum compressed size, in bytes, of a service check payload.
    ///
    /// This matches the Datadog Agent's generic payload limit for service checks. The effective value is
    /// clamped to the Agent's default intake-safe limit of 2,621,440 bytes, so larger configured values do not allow
    /// payloads that intake may reject. If set to `0`, every non-empty compressed payload exceeds the limit and is
    /// dropped during flush.
    ///
    /// Defaults to 2,621,440 bytes.
    #[serde(rename = "serializer_max_payload_size", default = "default_max_payload_size")]
    max_payload_size: usize,

    /// Maximum uncompressed size, in bytes, of a service check payload.
    ///
    /// This matches the Datadog Agent's generic payload limit for service checks. The effective value is
    /// clamped to the Agent's default intake-safe limit of 4,194,304 bytes, so larger configured values do not allow
    /// payloads that intake may reject. Values smaller than the minimum endpoint framing size prevent the request
    /// builder from starting.
    ///
    /// Defaults to 4,194,304 bytes.
    #[serde(
        rename = "serializer_max_uncompressed_payload_size",
        default = "default_max_uncompressed_payload_size"
    )]
    max_uncompressed_payload_size: usize,

    /// Compression kind to use for the request payloads.
    ///
    /// Defaults to `zstd`.
    #[serde(
        rename = "serializer_compressor_kind",
        default = "default_serializer_compressor_kind"
    )]
    compressor_kind: String,

    /// Compressor level to use when the compressor kind is `zstd`.
    ///
    /// Defaults to 3.
    #[serde(
        rename = "serializer_zstd_compressor_level",
        default = "default_zstd_compressor_level"
    )]
    zstd_compressor_level: i32,

    /// Whether to log service check payload contents before encoding.
    ///
    /// This logs decoded service check objects, not the encoded HTTP body.
    ///
    /// Defaults to `false`.
    #[serde(default = "default_log_payloads")]
    log_payloads: bool,
}

impl DatadogServiceChecksConfiguration {
    /// Creates a new `DatadogServiceChecksConfiguration` from the given configuration.
    pub fn from_configuration(config: &GenericConfiguration) -> Result<Self, GenericError> {
        Ok(config.as_typed()?)
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
mod config_smoke {
    use datadog_agent_config_testsupport::config_registry::structs;
    use datadog_agent_config_testsupport::run_config_smoke_tests;
    use serde_json::json;

    use super::DatadogServiceChecksConfiguration;
    use crate::config::{DatadogRemapper, KEY_ALIASES};

    #[tokio::test]
    async fn smoke_test() {
        run_config_smoke_tests(
            structs::DATADOG_SERVICE_CHECKS_CONFIGURATION,
            &[],
            json!({}),
            |cfg| {
                cfg.as_typed::<DatadogServiceChecksConfiguration>()
                    .expect("DatadogServiceChecksConfiguration should deserialize")
            },
            KEY_ALIASES,
            DatadogRemapper::new,
        )
        .await
    }
}
