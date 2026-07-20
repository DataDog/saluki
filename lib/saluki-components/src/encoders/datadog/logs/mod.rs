use async_trait::async_trait;
use chrono::{SecondsFormat, Utc};
use facet::Facet;
use http::{uri::PathAndQuery, HeaderValue, Method, Uri};
use saluki_common::iter::ReusableDeduplicator;
use saluki_config::GenericConfiguration;
use saluki_context::tags::Tag;
use saluki_core::accounting::{MemoryBounds, MemoryBoundsBuilder};
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
    resolve_zstd_compressor_level,
    telemetry::ComponentTelemetry,
    DEFAULT_INTAKE_COMPRESSED_SIZE_LIMIT, DEFAULT_INTAKE_UNCOMPRESSED_SIZE_LIMIT,
};

const DEFAULT_SERIALIZER_COMPRESSOR_KIND: &str = "zstd";
const MAX_LOGS_PER_PAYLOAD: usize = 1000;

static CONTENT_TYPE_JSON: HeaderValue = HeaderValue::from_static("application/json");

fn default_serializer_compressor_kind() -> String {
    DEFAULT_SERIALIZER_COMPRESSOR_KIND.to_owned()
}

/// Datadog Logs incremental encoder.
#[derive(Deserialize, Debug, Facet)]
#[cfg_attr(test, derive(PartialEq, serde::Serialize))]
pub struct DatadogLogsConfiguration {
    /// Compression kind for Logs payloads. Defaults to `zstd`.
    #[serde(
        rename = "serializer_compressor_kind",
        default = "default_serializer_compressor_kind"
    )]
    compressor_kind: String,

    /// ADP-specific zstd compression level, taking precedence over `serializer_zstd_compressor_level`.
    /// See [`resolve_zstd_compressor_level`] for how the effective level is determined.
    #[serde(rename = "data_plane_serializer_zstd_compressor_level", default)]
    data_plane_zstd_compressor_level: Option<i32>,

    /// The Core Agent's zstd compression level, used only when set to a non-default value (not 1).
    /// See [`resolve_zstd_compressor_level`] for how the effective level is determined.
    #[serde(rename = "serializer_zstd_compressor_level", default)]
    serializer_zstd_compressor_level: Option<i32>,
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
        let zstd_compressor_level = resolve_zstd_compressor_level(
            self.data_plane_zstd_compressor_level,
            self.serializer_zstd_compressor_level,
        );
        let compression_scheme = CompressionScheme::new(&self.compressor_kind, zstd_compressor_level);

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
                Ok((events, _data_points, request)) => {
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

#[cfg(test)]
mod tests {
    use std::collections::{BTreeSet, HashMap};

    use saluki_context::tags::{Tag, TagSet};
    use saluki_core::data_model::event::log::{Log, LogStatus};
    use serde_json::json;
    use stringtheory::MetaString;

    use super::{JsonValue, LogsEndpointEncoder};

    fn tag_set<const N: usize>(tags: [&'static str; N]) -> TagSet {
        tags.into_iter().map(Tag::from_static).collect()
    }

    #[test]
    fn build_agent_json_enriches_documented_fields() {
        let mut encoder = LogsEndpointEncoder::new();
        let log = Log::new("hello world")
            .with_status(LogStatus::Error)
            .with_source(MetaString::from_static("nginx"))
            .with_hostname(MetaString::from_static("host-a"))
            .with_service(MetaString::from_static("web"))
            .with_tags(tag_set(["env:prod", "team:core"]));

        let json = encoder.build_agent_json(&log);
        let obj = json.as_object().expect("agent JSON should be an object");

        // `message` is itself a JSON string carrying a `{message, service}` object.
        let message = obj["message"].as_str().expect("message field should be a string");
        let message_inner: JsonValue = serde_json::from_str(message).expect("message field should itself be JSON");
        assert_eq!(json!("hello world"), message_inner["message"]);
        assert_eq!(json!("web"), message_inner["service"]);

        // Status renders via `LogStatus::as_str` (capitalized), and hostname/service/ddsource are lifted out.
        assert_eq!(json!("Error"), obj["status"]);
        assert_eq!(json!("host-a"), obj["hostname"]);
        assert_eq!(json!("web"), obj["service"]);
        assert_eq!(json!("nginx"), obj["ddsource"]);

        // `ddtags` is the comma-joined tag set.
        let ddtags = obj["ddtags"].as_str().expect("ddtags should be a string");
        let ddtags = ddtags.split(',').collect::<BTreeSet<_>>();
        assert_eq!(BTreeSet::from(["env:prod", "team:core"]), ddtags);
    }

    #[test]
    fn build_agent_json_omits_empty_optional_fields() {
        // A bare log has no status/hostname/service/ddsource/ddtags keys, and its `message` object carries only the
        // message (no `service`).
        let mut encoder = LogsEndpointEncoder::new();
        let json = encoder.build_agent_json(&Log::new("bare"));
        let obj = json.as_object().expect("agent JSON should be an object");

        assert!(!obj.contains_key("status"));
        assert!(!obj.contains_key("hostname"));
        assert!(!obj.contains_key("service"));
        assert!(!obj.contains_key("ddsource"));
        assert!(!obj.contains_key("ddtags"));

        let message = obj["message"].as_str().expect("message field should be a string");
        let message_inner: JsonValue = serde_json::from_str(message).expect("message field should itself be JSON");
        assert_eq!(json!("bare"), message_inner["message"]);
        assert!(message_inner.get("service").is_none());
    }

    #[test]
    fn build_agent_json_adds_default_timestamp_unless_user_supplied() {
        let mut encoder = LogsEndpointEncoder::new();

        // With no user timestamp, the encoder injects an RFC3339 `@timestamp` (milliseconds, UTC `Z`).
        let json = encoder.build_agent_json(&Log::new("no ts"));
        let ts = json["@timestamp"]
            .as_str()
            .expect("default @timestamp should be present");
        assert!(ts.ends_with('Z'), "default timestamp should be UTC-suffixed: {ts}");
        assert!(
            chrono::DateTime::parse_from_rfc3339(ts).is_ok(),
            "default timestamp should be RFC3339: {ts}"
        );

        // A user-provided `timestamp` suppresses the default `@timestamp`.
        let mut props = HashMap::new();
        props.insert(MetaString::from_static("timestamp"), json!(1_234_567));
        let json = encoder.build_agent_json(&Log::new("user ts").with_additional_properties(props));
        assert!(
            !json.as_object().unwrap().contains_key("@timestamp"),
            "a user-provided `timestamp` should suppress the default `@timestamp`"
        );
        assert_eq!(json!(1_234_567), json["timestamp"]);

        // A user-provided `@timestamp` is preserved as-is instead of being overwritten.
        let mut props = HashMap::new();
        props.insert(MetaString::from_static("@timestamp"), json!("2020-01-01T00:00:00Z"));
        let json = encoder.build_agent_json(&Log::new("user @ts").with_additional_properties(props));
        assert_eq!(json!("2020-01-01T00:00:00Z"), json["@timestamp"]);
    }

    #[test]
    fn build_agent_json_additional_properties_win_over_encoder_fields() {
        // AdditionalProperties are merged last, so they override the encoder-populated fields (last-write-wins).
        let mut encoder = LogsEndpointEncoder::new();
        let mut props = HashMap::new();
        props.insert(MetaString::from_static("hostname"), json!("override-host"));
        props.insert(MetaString::from_static("custom"), json!(42));
        let log = Log::new("msg")
            .with_hostname(MetaString::from_static("original-host"))
            .with_additional_properties(props);

        let json = encoder.build_agent_json(&log);
        assert_eq!(json!("override-host"), json["hostname"]);
        assert_eq!(json!(42), json["custom"]);
    }
}

#[cfg(test)]
mod config_smoke {
    use datadog_agent_config_testing::config_registry::structs;
    use datadog_agent_config_testing::run_config_smoke_tests;
    use serde_json::json;

    use super::DatadogLogsConfiguration;
    use crate::config::{DatadogRemapper, KEY_ALIASES};

    #[tokio::test]
    async fn smoke_test() {
        run_config_smoke_tests(
            structs::DATADOG_LOGS_CONFIGURATION,
            &[],
            json!({}),
            |cfg| {
                cfg.as_typed::<DatadogLogsConfiguration>()
                    .expect("DatadogLogsConfiguration should deserialize")
            },
            KEY_ALIASES,
            DatadogRemapper::new,
        )
        .await
    }
}
