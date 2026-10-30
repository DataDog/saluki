use async_trait::async_trait;
use chrono::{SecondsFormat, Utc};
use http::{uri::PathAndQuery, HeaderValue, Method, Uri};
use saluki_common::iter::ReusableDeduplicator;
use saluki_core::{
    accounting::{MemoryBounds, MemoryBoundsBuilder},
    components::{encoders::*, BuildContext},
    data_model::{
        event::{log::Log, Event, EventType},
        payload::{HttpPayload, Payload, PayloadMetadata, PayloadType},
        tags::Tag,
    },
    observability::ComponentMetricsExt as _,
    topology::PayloadsDispatcher,
};
use saluki_error::{ErrorContext as _, GenericError};
use saluki_io::compression::CompressionScheme;
use saluki_metrics::MetricsBuilder;
use serde_json::{Map as JsonMap, Value as JsonValue};
use tracing::{error, warn};

use crate::common::datadog::{
    io::RB_BUFFER_CHUNK_SIZE,
    request_builder::{EndpointEncoder, RequestBuilder},
    telemetry::ComponentTelemetry,
    DEFAULT_INTAKE_COMPRESSED_SIZE_LIMIT, DEFAULT_INTAKE_UNCOMPRESSED_SIZE_LIMIT,
};

const MAX_LOGS_PER_PAYLOAD: usize = 1000;

static CONTENT_TYPE_JSON: HeaderValue = HeaderValue::from_static("application/json");

/// Datadog Logs incremental encoder.
#[derive(Debug)]
pub struct DatadogLogsConfiguration {
    /// Compression kind for Logs payloads.
    compressor_kind: String,

    /// The compression level to use when `zstd` is the algorithm.
    ///
    /// Ignored for algorithms other than `zstd`.
    zstd_level: i32,
}

impl DatadogLogsConfiguration {
    /// Creates a new `DatadogLogsConfiguration`.
    pub fn new(compressor_kind: impl Into<String>, zstd_level: i32) -> Self {
        Self {
            compressor_kind: compressor_kind.into(),
            zstd_level,
        }
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

    async fn build(&self, context: BuildContext) -> Result<Self::Output, GenericError> {
        let metrics_builder = MetricsBuilder::from_component_context(context.component_context());
        let telemetry = ComponentTelemetry::from_builder(&metrics_builder);
        let compression_scheme = CompressionScheme::new(&self.compressor_kind, self.zstd_level);

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
    use std::time::Duration;

    use saluki_core::data_model::{
        event::log::{Log, LogStatus},
        tags::{Tag, TagSet},
    };
    use saluki_core::{components::test_util::TestComponentDriver, data_model::event::Event};
    use serde_json::json;
    use stringtheory::MetaString;

    use super::{DatadogLogsConfiguration, JsonValue, LogsEndpointEncoder};
    use crate::common::datadog::test_util::{decompressed_bodies, incompressible_string};
    use crate::encoders::BufferedIncrementalConfiguration;

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

    /// Encodes `logs` with the encoder set up as the topology sets it up, and returns the decompressed request bodies.
    ///
    /// `logs` must fit in a single event buffer, so that the requests depend only on the logs and not on timing.
    async fn encode_logs(logs: Vec<Log>) -> Vec<(usize, Vec<u8>)> {
        assert!(logs.len() <= 1024, "logs must fit in a single event buffer");

        let config =
            BufferedIncrementalConfiguration::new(DatadogLogsConfiguration::new("zstd", 3), Duration::from_secs(2));
        let (control, mut payloads) = TestComponentDriver::encoder(config)
            .await
            .expect("logs encoder should build");
        control.send_events(logs.into_iter().map(Event::Log)).await;
        control.shutdown().await.expect("logs encoder should stop cleanly");

        decompressed_bodies(payloads.collect().await).await
    }

    /// Asserts that every request body is a JSON array holding as many logs as its metadata counts, and that the
    /// requests hold `expected_logs` logs in total.
    fn assert_valid_json_requests(requests: &[(usize, Vec<u8>)], expected_logs: usize) {
        assert!(requests.len() > 1, "the logs should be split across requests");
        for (index, (event_count, body)) in requests.iter().enumerate() {
            let tail = String::from_utf8_lossy(&body[body.len().saturating_sub(16)..]);
            let logs = serde_json::from_slice::<Vec<JsonValue>>(body).unwrap_or_else(|e| {
                panic!(
                    "request {} isn't a valid JSON array ({}), it ends with {:?}",
                    index, e, tail
                )
            });
            assert_eq!(
                *event_count,
                logs.len(),
                "request {} metadata should count its logs",
                index
            );
        }
        assert_eq!(requests.iter().map(|(count, _)| count).sum::<usize>(), expected_logs);
    }

    #[tokio::test]
    async fn requests_split_at_compressed_size_estimate_are_valid_json() {
        // About 6.5 MB of content that barely compresses, so the compressed size estimate rejects a log before the
        // 1000-log cap or the uncompressed limit is reached.
        let logs = (0..400)
            .map(|index| Log::new(incompressible_string(16 * 1024, index)))
            .collect::<Vec<_>>();

        let requests = encode_logs(logs).await;
        assert_valid_json_requests(&requests, 400);
    }

    #[tokio::test]
    async fn requests_split_at_uncompressed_size_limit_are_valid_json() {
        // About 70 MB of content that compresses to almost nothing, so the 60 MiB uncompressed limit rejects a log
        // before the 1000-log cap or the compressed size estimate is reached.
        let logs = (0..700)
            .map(|index| Log::new(format!("{:06}:{}", index, "a".repeat(100_000))))
            .collect::<Vec<_>>();

        let requests = encode_logs(logs).await;
        assert_valid_json_requests(&requests, 700);
    }
}
