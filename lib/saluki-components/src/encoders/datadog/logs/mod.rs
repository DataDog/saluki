use async_trait::async_trait;
use chrono::{DateTime, Utc};
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
use stringtheory::MetaString;
use tracing::{error, warn};

use crate::common::datadog::{
    io::RB_BUFFER_CHUNK_SIZE,
    request_builder::{EndpointEncoder, RequestBuilder},
    telemetry::ComponentTelemetry,
    DEFAULT_INTAKE_COMPRESSED_SIZE_LIMIT, DEFAULT_INTAKE_UNCOMPRESSED_SIZE_LIMIT,
};

const MAX_LOGS_PER_PAYLOAD: usize = 1000;

/// The most tags buffer capacity that the encoder keeps between logs.
///
/// The encoder keeps the capacity of the longest tags it has written, so one log with enormous tags would otherwise
/// stay held for the encoder's lifetime. The limit sits well above the tags of an ordinary log: such a log keeps its
/// reuse, and a log past it has the excess released before the next log is encoded.
const TAGS_BUFFER_RETENTION_LIMIT_BYTES: usize = 64 * 1024;

/// The envelope status of a log that has none.
const DEFAULT_STATUS: &str = "info";

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

    /// Hostname for the envelope of a log that has none.
    default_hostname: MetaString,

    /// Source of the current time, for the envelope `timestamp`.
    clock: fn() -> DateTime<Utc>,
}

impl DatadogLogsConfiguration {
    /// Creates a new `DatadogLogsConfiguration`.
    pub fn new(compressor_kind: impl Into<String>, zstd_level: i32) -> Self {
        Self {
            compressor_kind: compressor_kind.into(),
            zstd_level,
            default_hostname: MetaString::empty(),
            clock: Utc::now,
        }
    }

    /// Sets the hostname for the envelope of a log that has none.
    ///
    /// Only the envelope uses it: a log's rendered content is left as it is. Defaults to an empty hostname.
    pub fn with_default_hostname(mut self, default_hostname: impl Into<MetaString>) -> Self {
        self.default_hostname = default_hostname.into();
        self
    }

    /// Sets the source of the current time, so tests can pin the envelope `timestamp`.
    #[cfg(test)]
    pub(crate) fn with_clock(mut self, clock: fn() -> DateTime<Utc>) -> Self {
        self.clock = clock;
        self
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

        let mut request_builder = RequestBuilder::new(
            LogsEndpointEncoder::new(self.default_hostname.clone(), self.clock),
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
        // TODO: How do we properly represent the requests we can generate that may be sitting around in-flight?

        builder
            .minimum()
            .with_single_value::<DatadogLogs>("component struct")
            .with_fixed_amount("tags buffer", TAGS_BUFFER_RETENTION_LIMIT_BYTES);
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

    async fn flush(&mut self, dispatcher: &PayloadsDispatcher) -> Result<usize, FlushError> {
        let mut dispatched_events = 0;
        let pending_events = self.request_builder.pending_inputs();

        let maybe_requests = self.request_builder.flush().await;
        for maybe_request in maybe_requests {
            match maybe_request {
                Ok((events, _data_points, request)) => {
                    let payload_meta = PayloadMetadata::from_event_count(events);
                    let http_payload = HttpPayload::new(payload_meta, request);
                    let payload = Payload::Http(http_payload);
                    dispatcher
                        .dispatch(payload)
                        .await
                        .map_err(|source| FlushError::new(dispatched_events, source))?;
                    dispatched_events += events;
                }
                Err(e) => error!(error = %e, "Failed to build Datadog logs payload. Continuing..."),
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
struct LogsEndpointEncoder {
    tags_deduplicator: ReusableDeduplicator<Tag>,
    tags_buffer: String,
    default_hostname: MetaString,
    clock: fn() -> DateTime<Utc>,
}

impl LogsEndpointEncoder {
    fn new(default_hostname: MetaString, clock: fn() -> DateTime<Utc>) -> Self {
        Self {
            tags_deduplicator: ReusableDeduplicator::new(),
            tags_buffer: String::new(),
            default_hostname,
            clock,
        }
    }

    /// Writes the envelope of a log.
    ///
    /// The envelope has the same fields, in the same order, as the logs intake's JSON envelope: the message, the
    /// status, the time the log was encoded in milliseconds, the hostname, the service, the source, and the tags. Every
    /// field is present, empty when the log has no value for it, except that a log without a hostname gets the
    /// default hostname. The message is written as it is: a log's producer renders whatever content the message should
    /// carry, such as a JSON object holding the log's properties, so the log's additional properties aren't written.
    fn write_envelope(&mut self, log: &Log, buffer: &mut Vec<u8>) -> Result<(), serde_json::Error> {
        // A log without a status is sent as `info`.
        let status = if log.status().is_empty() {
            DEFAULT_STATUS
        } else {
            log.status()
        };
        let source = log.source().as_deref().unwrap_or_default();

        buffer.extend_from_slice(b"{\"message\":");
        serde_json::to_writer(&mut *buffer, log.message())?;
        buffer.extend_from_slice(b",\"status\":");
        serde_json::to_writer(&mut *buffer, status)?;
        buffer.extend_from_slice(b",\"timestamp\":");
        buffer.extend_from_slice(itoa::Buffer::new().format((self.clock)().timestamp_millis()).as_bytes());
        buffer.extend_from_slice(b",\"hostname\":");
        // A log without a hostname gets the default hostname, in the envelope only.
        let hostname = if log.hostname().is_empty() {
            &*self.default_hostname
        } else {
            log.hostname()
        };
        serde_json::to_writer(&mut *buffer, hostname)?;
        buffer.extend_from_slice(b",\"service\":");
        serde_json::to_writer(&mut *buffer, log.service())?;
        buffer.extend_from_slice(b",\"ddsource\":");
        serde_json::to_writer(&mut *buffer, source)?;

        // ddtags: comma-separated, deduplicated
        buffer.extend_from_slice(b",\"ddtags\":");
        self.tags_buffer.clear();
        for (index, tag) in self.tags_deduplicator.deduplicated(log.tags().into_iter()).enumerate() {
            if index > 0 {
                self.tags_buffer.push(',');
            }
            self.tags_buffer.push_str(tag.as_str());
        }
        let written = serde_json::to_writer(&mut *buffer, &self.tags_buffer);
        if self.tags_buffer.capacity() > TAGS_BUFFER_RETENTION_LIMIT_BYTES {
            self.tags_buffer.clear();
            self.tags_buffer.shrink_to(TAGS_BUFFER_RETENTION_LIMIT_BYTES);
        }
        written?;
        buffer.push(b'}');
        Ok(())
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
        self.write_envelope(input, buffer)
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
mod characterization;

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use chrono::{DateTime, Utc};
    use saluki_core::data_model::{
        event::log::Log,
        tags::{Tag, TagSet},
    };
    use serde_json::json;
    use stringtheory::MetaString;

    use super::{LogsEndpointEncoder, TAGS_BUFFER_RETENTION_LIMIT_BYTES};

    /// The encoder's clock in these tests, and the envelope `timestamp` it gives.
    const NOW: &str = "2026-01-02T03:04:05.678Z";
    const NOW_MS: i64 = 1_767_323_045_678;

    fn fixed_clock() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(NOW)
            .expect("NOW should be RFC3339")
            .with_timezone(&Utc)
    }

    fn envelope(log: &Log) -> String {
        let mut encoder = LogsEndpointEncoder::new(MetaString::empty(), fixed_clock);
        let mut buffer = Vec::new();
        encoder
            .write_envelope(log, &mut buffer)
            .expect("envelope should serialize");
        String::from_utf8(buffer).expect("envelope should be UTF-8")
    }

    fn tag_set<const N: usize>(tags: [&'static str; N]) -> TagSet {
        tags.into_iter().map(Tag::from_static).collect()
    }

    #[test]
    fn releases_the_excess_of_an_oversized_tags_buffer() {
        let mut encoder = LogsEndpointEncoder::new(MetaString::empty(), fixed_clock);
        let mut buffer = Vec::new();

        let ordinary = Log::new("m").with_tags(tag_set(["env:prod"]));
        encoder
            .write_envelope(&ordinary, &mut buffer)
            .expect("envelope should serialize");
        let ordinary_capacity = encoder.tags_buffer.capacity();
        assert!(ordinary_capacity >= "env:prod".len());
        assert!(ordinary_capacity <= TAGS_BUFFER_RETENTION_LIMIT_BYTES);

        // An ordinary log keeps the buffer it grew.
        encoder
            .write_envelope(&ordinary, &mut buffer)
            .expect("envelope should serialize");
        assert_eq!(encoder.tags_buffer.capacity(), ordinary_capacity);

        // An oversized log is written in full, then has the excess released.
        let tag = format!("big:{}", "x".repeat(2 * TAGS_BUFFER_RETENTION_LIMIT_BYTES));
        let oversized = Log::new("m").with_tags([Tag::from(tag.clone())].into_iter().collect::<TagSet>());
        encoder
            .write_envelope(&oversized, &mut buffer)
            .expect("envelope should serialize");
        assert!(String::from_utf8_lossy(&buffer).contains(&tag));
        assert!(encoder.tags_buffer.capacity() <= TAGS_BUFFER_RETENTION_LIMIT_BYTES);
    }

    #[test]
    fn writes_every_envelope_field_in_order() {
        assert_eq!(fixed_clock().timestamp_millis(), NOW_MS);
        let log = Log::new("hello world")
            .with_status("Err")
            .with_source(MetaString::from_static("nginx"))
            .with_hostname(MetaString::from_static("host-a"))
            .with_service(MetaString::from_static("web"))
            .with_tags(tag_set(["env:prod", "team:core", "env:prod"]));

        assert_eq!(
            envelope(&log),
            format!(
                r#"{{"message":"hello world","status":"Err","timestamp":{},"hostname":"host-a","service":"web","ddsource":"nginx","ddtags":"env:prod,team:core"}}"#,
                NOW_MS
            )
        );
    }

    #[test]
    fn writes_empty_fields_and_the_default_status() {
        assert_eq!(
            envelope(&Log::new("bare")),
            format!(
                r#"{{"message":"bare","status":"info","timestamp":{},"hostname":"","service":"","ddsource":"","ddtags":""}}"#,
                NOW_MS
            )
        );
    }

    #[test]
    fn uses_the_default_hostname_only_when_the_log_has_none() {
        let mut encoder = LogsEndpointEncoder::new(MetaString::from_static("default-host"), fixed_clock);
        let content = r#"{"message":"hi","status":""}"#;
        let mut write = |log: &Log| {
            let mut buffer = Vec::new();
            encoder
                .write_envelope(log, &mut buffer)
                .expect("envelope should serialize");
            serde_json::from_slice::<serde_json::Value>(&buffer).expect("envelope should be JSON")
        };

        // The default hostname goes only into the envelope: the rendered content is sent as it is.
        let without_host = write(&Log::new(content));
        assert_eq!(without_host["hostname"], json!("default-host"));
        assert_eq!(without_host["message"], json!(content));

        let with_host = write(&Log::new("hello").with_hostname(MetaString::from_static("log-host")));
        assert_eq!(with_host["hostname"], json!("log-host"));
    }

    #[test]
    fn writes_the_message_as_it_is_without_properties() {
        // The producer renders the content; the encoder neither wraps it nor adds the properties or an `@timestamp`.
        let mut properties = HashMap::new();
        properties.insert(MetaString::from_static("custom"), json!(42));
        let log = Log::new("ignored")
            .with_additional_properties(properties)
            .with_message(r#"{"custom":42,"message":"hi"}"#)
            .with_status("debug");

        assert_eq!(
            envelope(&log),
            format!(
                r#"{{"message":"{{\"custom\":42,\"message\":\"hi\"}}","status":"debug","timestamp":{},"hostname":"","service":"","ddsource":"","ddtags":""}}"#,
                NOW_MS
            )
        );
    }

    #[test]
    fn escapes_strings_as_serde_json_does() {
        // `serde_json` doesn't escape `<`, `>`, `&`, or U+2028, which Go escapes (`gap/json-encoding`).
        let log = Log::new("<a href=\"x\">&amp;</a> \u{2028} \t")
            .with_service(MetaString::from_static("s<v>c"))
            .with_tags(tag_set(["a&b"]));

        assert_eq!(
            envelope(&log),
            format!(
                "{{\"message\":\"<a href=\\\"x\\\">&amp;</a> \u{2028} \\t\",\"status\":\"info\",\"timestamp\":{},\"hostname\":\"\",\"service\":\"s<v>c\",\"ddsource\":\"\",\"ddtags\":\"a&b\"}}",
                NOW_MS
            )
        );
    }
}
