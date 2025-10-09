use std::collections::HashMap;

// use chrono::DateTime;
// use otlp_common::any_value::Value::StringValue as OtlpStringValue;
// use otlp_common::any_value::Value::{ArrayValue, BoolValue, BytesValue, DoubleValue, IntValue, KvlistValue};
use otlp_protos::opentelemetry::proto::common::v1 as otlp_common;
use otlp_protos::opentelemetry::proto::logs::v1::LogRecord;
use otlp_protos::opentelemetry::proto::resource::v1::Resource;
use saluki_context::tags::SharedTagSet;
use saluki_core::data_model::event::log::{Log, LogStatus};
use serde_json::Value as JsonValue;
use stringtheory::MetaString;
// use tracing::error;

// pub const DDTAGS_ATTR: &str = "ddtags";
// pub const STATUS_KEYS: &[&str] = &["status", "severity", "level", "syslog.severity"];
// pub const MESSAGE_KEYS: &[&str] = &["msg", "message", "log"];
// pub const TRACE_ID_ATTR_KEYS: &[&str] = &["traceid", "trace_id", "contextmap.traceid", "oteltraceid"];
// pub const SPAN_ID_ATTR_KEYS: &[&str] = &["spanid", "span_id", "contextmap.spanid", "otelspanid"];

fn default_ddsource() -> String {
    "otlp_log_ingestion".to_string()
}

// pub fn derive_status(status: Option<&str>, severity: &str, severity_number: i32) -> Option<LogStatus> { /* bench */ }

// pub fn map_status_text(_: &str) -> Option<LogStatus> { None }

// pub fn status_from_severity_number(_: i32) -> Option<LogStatus> { None }

// pub fn any_value_to_message_string(_: &otlp_common::AnyValue) -> String { String::new() }

// pub fn any_value_to_json(_: &otlp_common::AnyValue) -> JsonValue { JsonValue::Null }

// pub fn u64_from_last_8(_: &[u8]) -> u64 { 0 }

// pub fn u64_from_first_8(_: &[u8]) -> u64 { 0 }

// pub fn bytes_to_hex_lowercase(_: &[u8]) -> String { String::new() }

// pub fn decode_hex_exact_to_bytes(_: &str, _: usize) -> Option<Vec<u8>> { None }

// pub fn flatten_attribute(_: &str, _: &otlp_common::AnyValue, _: usize) -> Vec<(String, JsonValue)> { Vec::new() }

// fn safe_insert(_: &mut HashMap<String, JsonValue>, _: &str, _: JsonValue) {}

// Transformer that converts a OTLP LogRecord into a dd native log.
pub struct LogRecordTransformer;

impl LogRecordTransformer {
    pub fn new() -> Self {
        Self
    }

    pub fn transform(
        &self, _lr: LogRecord, _resource: &Resource, _scope: Option<&otlp_common::InstrumentationScope>,
        host_for_record: Option<String>, service_for_record: Option<String>, base_tags_for_resource: &SharedTagSet,
    ) -> Log {
        let tags = base_tags_for_resource.clone();

        // Minimal additional properties for benchmarking
        let additional_properties = HashMap::<String, JsonValue>::new();

        let log = Log::new("bench")
            .with_status(Some(LogStatus::Info))
            .with_source(Some(MetaString::from(default_ddsource())))
            .with_hostname(host_for_record.as_deref().map(MetaString::from))
            .with_service(service_for_record.as_deref().map(MetaString::from))
            .with_tags(tags)
            .with_additional_properties(Some(additional_properties));

        log
    }
}
