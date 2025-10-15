use async_trait::async_trait;
use chrono::{SecondsFormat, Utc};
use http::{uri::PathAndQuery, HeaderValue, Method, Uri};
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_common::iter::ReusableDeduplicator;
use saluki_config::GenericConfiguration;
use saluki_context::tags::Tag;
use saluki_core::{
    components::{encoders::*, ComponentContext},
    data_model::{
        event::{log::Log, Event, EventType},
        payload::{HttpPayload, Payload, PayloadMetadata, PayloadType},
    },
    observability::ComponentMetricsExt as _,
    topology::PayloadsDispatcher,
};
use saluki_error::{ErrorContext as _, GenericError};
use saluki_io::compression::CompressionScheme;
use saluki_metrics::MetricsBuilder;
use serde::Deserialize;
use serde_json::{Map as JsonMap, Value as JsonValue};
use tracing::{error, warn};

use crate::common::datadog::{
    io::RB_BUFFER_CHUNK_SIZE,
    request_builder::{EndpointEncoder, RequestBuilder},
    telemetry::ComponentTelemetry,
    DEFAULT_INTAKE_COMPRESSED_SIZE_LIMIT, DEFAULT_INTAKE_UNCOMPRESSED_SIZE_LIMIT,
};

const DEFAULT_SERIALIZER_COMPRESSOR_KIND: &str = "zstd";
const MAX_LOGS_PER_PAYLOAD: usize = 1;

static CONTENT_TYPE_JSON: HeaderValue = HeaderValue::from_static("application/json");

fn default_serializer_compressor_kind() -> String {
    DEFAULT_SERIALIZER_COMPRESSOR_KIND.to_owned()
}

const fn default_zstd_compressor_level() -> i32 {
    3
}

/// Datadog Logs incremental encoder.
#[derive(Deserialize, Debug)]
pub struct DatadogLogsConfiguration {
    /// Compression kind for Logs payloads. Defaults to `zstd`.
    #[serde(
        rename = "serializer_compressor_kind",
        default = "default_serializer_compressor_kind"
    )]
    compressor_kind: String,

    /// Compressor level to use when the compressor kind is `zstd`. Defaults to 3.
    #[serde(
        rename = "serializer_zstd_compressor_level",
        default = "default_zstd_compressor_level"
    )]
    zstd_compressor_level: i32,
}

impl DatadogLogsConfiguration {
    /// Creates a new `DatadogLogsConfiguration` from the given configuration.
    pub fn from_configuration(config: &GenericConfiguration) -> Result<Self, GenericError> {
        Ok(config.as_typed()?)
    }
}

#[async_trait]
impl IncrementalEncoderBuilder for DatadogLogsConfiguration {
    type Output = DatadogLogs;

    fn input_event_type(&self) -> EventType {
        EventType::Log
    }

    fn output_payload_type(&self) -> PayloadType {
        PayloadType::Http
    }

    async fn build(&self, context: ComponentContext) -> Result<Self::Output, GenericError> {
        let metrics_builder = MetricsBuilder::from_component_context(&context);
        let telemetry = ComponentTelemetry::from_builder(&metrics_builder);
        let compression_scheme = CompressionScheme::new(&self.compressor_kind, self.zstd_compressor_level);

        let mut request_builder =
            RequestBuilder::new(LogsEndpointEncoder::new(), compression_scheme, RB_BUFFER_CHUNK_SIZE).await?;
        request_builder.with_max_inputs_per_payload(MAX_LOGS_PER_PAYLOAD);

        Ok(DatadogLogs {
            request_builder,
            telemetry,
        })
    }
}

impl MemoryBounds for DatadogLogsConfiguration {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        // TODO: How do we properly represent the requests we can generate that may be sitting around in-flight?

        builder.minimum().with_single_value::<DatadogLogs>("component struct");
        builder.firm().with_array::<Log>("logs buffer", MAX_LOGS_PER_PAYLOAD);
    }
}

pub struct DatadogLogs {
    request_builder: RequestBuilder<LogsEndpointEncoder>,
    telemetry: ComponentTelemetry,
}

#[async_trait]
impl IncrementalEncoder for DatadogLogs {
    async fn process_event(&mut self, event: Event) -> Result<ProcessResult, GenericError> {
        let log: Log = match event {
            Event::Log(log) => log,
            _ => return Ok(ProcessResult::Continue),
        };
        match self.request_builder.encode(log).await {
            Ok(None) => Ok(ProcessResult::Continue),
            Ok(Some(log)) => Ok(ProcessResult::FlushRequired(Event::Log(log))),
            Err(e) => {
                if e.is_recoverable() {
                    warn!(error = %e, "Failed to encode Datadog log due to recoverable error. Continuing...");

                    // TODO: Get the actual number of events dropped from the error itself.
                    self.telemetry.events_dropped_encoder().increment(1);
                    Ok(ProcessResult::Continue)
                } else {
                    Err(e).error_context("Failed to encode Datadog log due to unrecoverable error.")
                }
            }
        }
    }

    async fn flush(&mut self, dispatcher: &PayloadsDispatcher) -> Result<(), GenericError> {
        let maybe_requests = self.request_builder.flush().await;
        for maybe_request in maybe_requests {
            match maybe_request {
                Ok((events, request)) => {
                    let payload_meta = PayloadMetadata::from_event_count(events);
                    let http_payload = HttpPayload::new(payload_meta, request);
                    let payload = Payload::Http(http_payload);
                    dispatcher.dispatch(payload).await?;
                }
                Err(e) => error!(error = %e, "Failed to build Datadog logs payload. Continuing..."),
            }
        }

        Ok(())
    }
}

#[derive(Debug)]
struct LogsEndpointEncoder {
    tags_deduplicator: ReusableDeduplicator<Tag>,
}

impl LogsEndpointEncoder {
    fn new() -> Self {
        Self {
            tags_deduplicator: ReusableDeduplicator::new(),
        }
    }

    fn build_agent_json(&mut self, log: &Log) -> JsonValue {
        let mut obj = JsonMap::new();

        // Encode a structured message object as a JSON string in the `message` field.
        let mut message_inner = JsonMap::new();
        message_inner.insert("message".to_string(), JsonValue::String(log.message().to_string()));
        if !log.service().is_empty() {
            message_inner.insert("service".to_string(), JsonValue::String(log.service().to_string()));
        }
        let message_str =
            serde_json::to_string(&JsonValue::Object(message_inner)).unwrap_or_else(|_| log.message().to_string());
        obj.insert("message".to_string(), JsonValue::String(message_str));

        if let Some(status) = log.status() {
            obj.insert("status".to_string(), JsonValue::String(status.as_str().to_string()));
        }
        if !log.hostname().is_empty() {
            obj.insert("hostname".to_string(), JsonValue::String(log.hostname().to_string()));
        }
        if !log.service().is_empty() {
            obj.insert("service".to_string(), JsonValue::String(log.service().to_string()));
        }

        if let Some(ddsource) = log.source().clone() {
            obj.insert("ddsource".to_string(), JsonValue::String(ddsource.to_string()));
        }

        // ddtags: comma-separated, deduplicated
        let tags_iter = self.tags_deduplicator.deduplicated(log.tags().into_iter());
        let tags_vec: Vec<&str> = tags_iter.map(|t| t.as_str()).collect();
        if !tags_vec.is_empty() {
            obj.insert("ddtags".to_string(), JsonValue::String(tags_vec.join(",")));
        }

        // Default timestamp (RFC3339 with milliseconds, Z) unless user provided `timestamp` or `@timestamp`.
        let user_provided_timestamp = log.additional_properties().contains_key("timestamp")
            || log.additional_properties().contains_key("@timestamp");
        if !user_provided_timestamp {
            let now_rfc3339 = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
            obj.insert("@timestamp".to_string(), JsonValue::String(now_rfc3339));
        }

        // Last-write-wins: merge AdditionalProperties last
        for (k, v) in log.additional_properties() {
            obj.insert(k.to_string(), v.clone());
        }

        JsonValue::Object(obj)
    }
}

impl EndpointEncoder for LogsEndpointEncoder {
    type Input = Log;
    type EncodeError = serde_json::Error;

    fn encoder_name() -> &'static str {
        "logs"
    }

    fn compressed_size_limit(&self) -> usize {
        DEFAULT_INTAKE_COMPRESSED_SIZE_LIMIT
    }

    fn uncompressed_size_limit(&self) -> usize {
        DEFAULT_INTAKE_UNCOMPRESSED_SIZE_LIMIT
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

    fn encode(&mut self, input: &Self::Input, buffer: &mut Vec<u8>) -> Result<(), Self::EncodeError> {
        let json = self.build_agent_json(input);
        serde_json::to_writer(buffer, &json)
    }

    fn endpoint_uri(&self) -> Uri {
        PathAndQuery::from_static("/api/v2/logs").into()
    }

    fn endpoint_method(&self) -> Method {
        Method::POST
    }

    fn content_type(&self) -> HeaderValue {
        CONTENT_TYPE_JSON.clone()
    }
}
