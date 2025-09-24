use async_trait::async_trait;
use http::{uri::PathAndQuery, HeaderValue, Method, Uri};
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_common::iter::ReusableDeduplicator;
use saluki_config::GenericConfiguration;
use saluki_context::tags::Tag;
use saluki_core::{
    components::{encoders::*, ComponentContext},
    data_model::{
        event::{
            log::{Log, LogStatus},
            Event, EventType,
        },
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
use tracing::{error, info, warn};

use crate::common::datadog::{
    io::RB_BUFFER_CHUNK_SIZE,
    request_builder::{EndpointEncoder, RequestBuilder},
    telemetry::ComponentTelemetry,
    DEFAULT_INTAKE_COMPRESSED_SIZE_LIMIT, DEFAULT_INTAKE_UNCOMPRESSED_SIZE_LIMIT,
};

const DEFAULT_SERIALIZER_COMPRESSOR_KIND: &str = "zstd";
const MAX_LOGS_PER_PAYLOAD: usize = 1000;

static CONTENT_TYPE_JSON: HeaderValue = HeaderValue::from_static("application/json");

fn default_serializer_compressor_kind() -> String {
    DEFAULT_SERIALIZER_COMPRESSOR_KIND.to_owned()
}

const fn default_zstd_compressor_level() -> i32 {
    3
}

fn default_ddsource() -> String {
    "otel".to_string()
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

    /// Value for the `ddsource` field in Agent JSON envelope. Defaults to "otel".
    #[serde(default = "default_ddsource")]
    ddsource: String,
}

impl DatadogLogsConfiguration {
    /// WIP
    ///
    /// TODO: write the documentation
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

        let mut request_builder = RequestBuilder::new(
            LogsEndpointEncoder::new(self.ddsource.clone()),
            compression_scheme,
            RB_BUFFER_CHUNK_SIZE,
        )
        .await?;
        request_builder.with_max_inputs_per_payload(MAX_LOGS_PER_PAYLOAD);

        Ok(DatadogLogs {
            request_builder,
            telemetry,
        })
    }
}

impl MemoryBounds for DatadogLogsConfiguration {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        builder.minimum().with_single_value::<DatadogLogs>("component struct");
        builder
            .firm()
            .with_array::<Log>("logs split re-encode buffer", MAX_LOGS_PER_PAYLOAD);
    }
}

pub struct DatadogLogs {
    request_builder: RequestBuilder<LogsEndpointEncoder>,
    telemetry: ComponentTelemetry,
}

#[async_trait]
impl IncrementalEncoder for DatadogLogs {
    async fn process_event(&mut self, event: Event) -> Result<ProcessResult, GenericError> {
        info!("WACKTEST event received in encoder {:?} ", event);
        let log = match event {
            Event::Log(log) => log,
            _ => return Ok(ProcessResult::Continue),
        };

        match self.request_builder.encode(log).await {
            Ok(None) => Ok(ProcessResult::Continue),
            Ok(Some(log)) => Ok(ProcessResult::FlushRequired(Event::Log(log))),
            Err(e) => {
                if e.is_recoverable() {
                    warn!(error = %e, "Failed to encode Datadog log due to recoverable error. Continuing...");
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
    ddsource: String,
    tags_deduplicator: ReusableDeduplicator<Tag>,
}

impl LogsEndpointEncoder {
    fn new(ddsource: String) -> Self {
        Self {
            ddsource,
            tags_deduplicator: ReusableDeduplicator::new(),
        }
    }

    fn status_to_str(status: LogStatus) -> &'static str {
        match status {
            LogStatus::Trace => "trace",
            LogStatus::Emergency => "emerg",
            LogStatus::Alert => "alert",
            LogStatus::Fatal => "crit",
            LogStatus::Error => "err",
            LogStatus::Warning => "warn",
            LogStatus::Notice => "notice",
            LogStatus::Info => "info",
            LogStatus::Debug => "debug",
        }
    }

    // TODO: add source for logs

    fn build_agent_json(&mut self, log: &Log) -> JsonValue {
        let mut obj = JsonMap::new();

        // Required-ish envelope fields
        obj.insert("message".to_string(), JsonValue::String(log.message().to_string()));

        if let Some(status) = log.status() {
            obj.insert(
                "status".to_string(),
                JsonValue::String(Self::status_to_str(status).to_string()),
            );
        }

        if !log.hostname().is_empty() {
            obj.insert("hostname".to_string(), JsonValue::String(log.hostname().to_string()));
        }
        if !log.service().is_empty() {
            obj.insert("service".to_string(), JsonValue::String(log.service().to_string()));
        }

        obj.insert("ddsource".to_string(), JsonValue::String(self.ddsource.clone()));

        // ddtags: comma-separated, deduplicated
        let tags_iter = self.tags_deduplicator.deduplicated(log.tags().into_iter());
        let tags_vec: Vec<&str> = tags_iter.map(|t| t.as_str()).collect();
        if !tags_vec.is_empty() {
            obj.insert("ddtags".to_string(), JsonValue::String(tags_vec.join(",")));
        }

        // Copy additional properties flattened into the envelope
        for (k, v) in log.additional_properties() {
            obj.insert(k.clone(), v.clone());
        }

        // If we have "otel.timestamp" as a numeric string, add a numeric "timestamp" field (nanoseconds since epoch).
        if let Some(JsonValue::String(ns)) = obj.get("otel.timestamp") {
            if let Ok(parsed) = ns.parse::<i64>() {
                obj.insert("timestamp".to_string(), JsonValue::from(parsed));
            }
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
