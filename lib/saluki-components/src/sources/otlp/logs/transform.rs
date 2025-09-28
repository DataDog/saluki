use std::collections::HashMap;

use chrono::DateTime;
use otlp_common::any_value::Value::StringValue as OtlpStringValue;
use otlp_common::any_value::Value::{ArrayValue, BoolValue, BytesValue, DoubleValue, IntValue, KvlistValue};
use otlp_protos::opentelemetry::proto::common::v1 as otlp_common;
use otlp_protos::opentelemetry::proto::logs::v1::LogRecord;
use otlp_protos::opentelemetry::proto::resource::v1::Resource;
use saluki_context::tags::{SharedTagSet, TagSet};
use saluki_core::data_model::event::log::{Log, LogStatus};
use serde_json::Value as JsonValue;
use stringtheory::MetaString;
use tracing::{info, error};

pub const DDTAGS_ATTR: &str = "ddtags";
pub const STATUS_KEYS: &[&str] = &["status", "severity", "level", "syslog.severity"];
pub const MESSAGE_KEYS: &[&str] = &["msg", "message", "log"];
pub const TRACE_ID_ATTR_KEYS: &[&str] = &["traceid", "trace_id", "contextmap.traceid", "oteltraceid"];
pub const SPAN_ID_ATTR_KEYS: &[&str] = &["spanid", "span_id", "contextmap.spanid", "otelspanid"];

pub fn derive_status(status: Option<&str>, severity: &str, severity_number: i32) -> Option<LogStatus> {
    if let Some(text) = status {
        if let Some(s) = map_status_text(text) {
            return Some(s);
        }
    }
    if let Some(s) = map_status_text(severity) {
        return Some(s);
    }
    status_from_severity_number(severity_number)
}

pub fn map_status_text(text: &str) -> Option<LogStatus> {
    if text.is_empty() {
        return None;
    }
    match text.to_ascii_lowercase().as_str() {
        "emerg" | "emergency" => Some(LogStatus::Emergency),
        "alert" => Some(LogStatus::Alert),
        "crit" | "critical" | "fatal" => Some(LogStatus::Fatal),
        "err" | "error" => Some(LogStatus::Error),
        "warn" | "warning" => Some(LogStatus::Warning),
        "notice" => Some(LogStatus::Notice),
        "info" | "information" => Some(LogStatus::Info),
        "debug" => Some(LogStatus::Debug),
        "trace" => Some(LogStatus::Trace),
        _ => None,
    }
}

/// status_from_severity_number converts the severity number to log level
///
/// https://github.com/open-telemetry/opentelemetry-specification/blob/main/specification/logs/data-model.md#field-severitynumber
pub fn status_from_severity_number(severity_number: i32) -> Option<LogStatus> {
    if severity_number <= 4 {
        Some(LogStatus::Trace)
    } else if severity_number <= 8 {
        Some(LogStatus::Debug)
    } else if severity_number <= 12 {
        Some(LogStatus::Info)
    } else if severity_number <= 16 {
        Some(LogStatus::Warning)
    } else if severity_number <= 20 {
        Some(LogStatus::Error)
    } else if severity_number <= 24 {
        Some(LogStatus::Fatal)
    } else {
        Some(LogStatus::Error)
    }
}

pub fn any_value_to_message_string(av: &otlp_common::AnyValue) -> String {
    match av.value.as_ref() {
        Some(OtlpStringValue(s)) => s.clone(),
        _ => serde_json::to_string(&any_value_to_json(av)).unwrap_or_else(|_| String::new()),
    }
}

pub fn any_value_to_json(av: &otlp_common::AnyValue) -> JsonValue {
    match av.value.as_ref() {
        Some(BoolValue(b)) => JsonValue::Bool(*b),
        Some(IntValue(i)) => JsonValue::from(*i),
        Some(DoubleValue(d)) => JsonValue::from(*d),
        Some(OtlpStringValue(s)) => JsonValue::String(s.clone()),
        Some(BytesValue(bytes)) => match String::from_utf8(bytes.clone()) {
            Ok(s) => JsonValue::String(s),
            Err(_) => JsonValue::String(format!("<{} bytes>", bytes.len())),
        },
        Some(ArrayValue(arr)) => JsonValue::Array(arr.values.iter().map(any_value_to_json).collect::<Vec<_>>()),
        Some(KvlistValue(kvl)) => {
            let mut obj = serde_json::Map::new();
            for kv in &kvl.values {
                if let Some(val) = kv.value.as_ref() {
                    obj.insert(kv.key.clone(), any_value_to_json(val));
                }
            }
            JsonValue::Object(obj)
        }
        None => JsonValue::Null,
    }
}

// Convert the last 8 bytes of a byte slice to a u64 to get the trace id
pub fn u64_from_last_8(bytes: &[u8]) -> u64 {
    let n = bytes.len();
    let slice = if n >= 8 { &bytes[n - 8..n] } else { bytes };
    let mut buf = [0u8; 8];
    let start = 8 - slice.len();
    buf[start..].copy_from_slice(slice);
    u64::from_be_bytes(buf)
}

// Convert the first 8 bytes of a byte slice to a u64 to get the span id
pub fn u64_from_first_8(bytes: &[u8]) -> u64 {
    let slice = if bytes.len() >= 8 { &bytes[..8] } else { bytes };
    let mut buf = [0u8; 8];
    let start = 8 - slice.len();
    buf[start..].copy_from_slice(slice);
    u64::from_be_bytes(buf)
}

// Convert a byte slice to a lower case hex string.
pub fn bytes_to_hex_lowercase(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return String::new();
    }
    let mut out = vec![0u8; bytes.len() * 2];
    if faster_hex::hex_encode(bytes, &mut out).is_err() {
        error!("Failed to convert byte slice to hex string");
        return String::new();
    }
    // Guaranteed ASCII hex
    unsafe { String::from_utf8_unchecked(out) }
}

/// Decode a hex string into a vector of bytes.
pub fn decode_hex_exact_to_bytes(s: &str, expected_len_bytes: usize) -> Option<Vec<u8>> {
    if s.len() != expected_len_bytes * 2 {
        return None;
    }
    let mut out = vec![0u8; expected_len_bytes];
    faster_hex::hex_decode(s.as_bytes(), &mut out).ok()?;
    Some(out)
}

// Flatten an AnyValue map into dotted keys, up to a maximum depth.
// For leaf values, coerce primitives to native JSON types, otherwise stringify.
pub fn flatten_attribute(base_key: &str, av: &otlp_common::AnyValue, depth: usize) -> Vec<(String, JsonValue)> {
    const MAX_DEPTH: usize = 10;

    fn to_leaf_json(av: &otlp_common::AnyValue) -> JsonValue {
        match av.value.as_ref() {
            Some(OtlpStringValue(s)) => JsonValue::String(s.clone()),
            Some(IntValue(i)) => JsonValue::from(*i),
            Some(BoolValue(b)) => JsonValue::Bool(*b),
            Some(DoubleValue(d)) => JsonValue::from(*d),
            Some(BytesValue(bytes)) => match String::from_utf8(bytes.clone()) {
                Ok(s) => JsonValue::String(s),
                Err(_) => JsonValue::String(format!("<{} bytes>", bytes.len())),
            },
            _ => JsonValue::String(serde_json::to_string(&any_value_to_json(av)).unwrap_or_else(|_| String::new())),
        }
    }

    if depth < MAX_DEPTH {
        if let Some(KvlistValue(kvl)) = av.value.as_ref() {
            let mut out = Vec::new();
            for kv in &kvl.values {
                if let Some(v) = kv.value.as_ref() {
                    let new_key = format!("{}.{}", base_key, kv.key);
                    let nested = flatten_attribute(&new_key, v, depth + 1);
                    if nested.is_empty() {
                        out.push((new_key, to_leaf_json(v)));
                    } else {
                        out.extend(nested);
                    }
                }
            }
            return out;
        }
    }

    vec![(base_key.to_string(), to_leaf_json(av))]
}

// Helper to insert safely avoid overwriting fields that are already set
fn safe_insert(map: &mut HashMap<String, JsonValue>, key: &str, value: JsonValue) {
    if key == "hostname" || key == "service" {
        map.insert(format!("otel.{}", key), value);
    } else {
        map.insert(key.to_string(), value);
    }
}

// Transformer that converts a OTLP LogRecord into a dd native log.
pub struct LogRecordTransformer;

impl LogRecordTransformer {
    pub fn new() -> Self {
        Self
    }

    pub fn transform(
        &self, lr: LogRecord, resource: &Resource, scope: Option<&otlp_common::InstrumentationScope>,
        host_for_record: Option<String>, service_for_record: Option<String>, base_tags_for_resource: &SharedTagSet,
    ) -> Log {
        let mut tags = base_tags_for_resource.clone();

        // Build additional properties map with resource, scope and record attributes
        let mut additional_properties = HashMap::<String, JsonValue>::new();

        // Single-pass over record attributes: handle special keys and flatten others
        let mut status_text_from_attrs: Option<String> = None;
        let mut msg_from_attrs: Option<String> = None;
        for kv in &lr.attributes {
            let k_lower = kv.key.to_ascii_lowercase();
            match k_lower.as_str() {
                // Message keys
                k if MESSAGE_KEYS.contains(&k) => {
                    if let Some(av) = kv.value.as_ref() {
                        if let Some(OtlpStringValue(s)) = av.value.as_ref() {
                            msg_from_attrs = Some(s.clone());
                        }
                    }
                }
                // Status/severity text from attributes
                k if STATUS_KEYS.contains(&k) => {
                    if let Some(av) = kv.value.as_ref() {
                        if let Some(OtlpStringValue(s)) = av.value.as_ref() {
                            status_text_from_attrs = Some(s.clone());
                        }
                    }
                }
                // Trace correlation from attributes
                k if TRACE_ID_ATTR_KEYS.contains(&k) => {
                    info!("\n HUH11 {:?}\n", kv.value);

                    if let Some(av) = kv.value.as_ref() {
                        info!("huh22?");
                        if let Some(OtlpStringValue(trace_hex)) = av.value.as_ref() {
                            info!("huh33?");
                            if !additional_properties.contains_key("dd.trace_id") {
                                info!("huh44?");
                                if let Some(bytes) = decode_hex_exact_to_bytes(trace_hex, 16) {
                                    info!("huh55?");
                                    let dd = u64_from_last_8(&bytes);
                                    additional_properties
                                        .insert("dd.trace_id".to_string(), JsonValue::String(dd.to_string()));
                                    additional_properties
                                        .insert("otel.trace_id".to_string(), JsonValue::String(trace_hex.clone()));
                                    info!("hehexd 12345? {:?}", additional_properties.get("otel.trace_id"));
                                }
                            }
                        }
                    }
                }
                // Span correlation from attributes
                k if SPAN_ID_ATTR_KEYS.contains(&k) => {
                    if let Some(av) = kv.value.as_ref() {
                        if let Some(OtlpStringValue(span_hex)) = av.value.as_ref() {
                            if !additional_properties.contains_key("dd.span_id") {
                                if let Some(bytes) = decode_hex_exact_to_bytes(span_hex, 8) {
                                    let dd = u64_from_first_8(&bytes);
                                    additional_properties
                                        .insert("dd.span_id".to_string(), JsonValue::String(dd.to_string()));
                                    additional_properties
                                        .insert("otel.span_id".to_string(), JsonValue::String(span_hex.clone()));
                                }
                            }
                        }
                    }
                }
                // ddtags aggregation
                k if k == DDTAGS_ATTR => {
                    if let Some(av) = kv.value.as_ref() {
                        if let Some(OtlpStringValue(s)) = av.value.as_ref() {
                            let mut extra = TagSet::default();
                            for raw in s.split(',') {
                                let t = raw.trim();
                                if !t.is_empty() {
                                    extra.insert_tag(t.to_string());
                                }
                            }
                            tags.extend_from_shared(&extra.into_shared());
                        }
                    }
                }
                // Default: flatten map and insert safely
                _ => {
                    if let Some(av) = kv.value.as_ref() {
                        let flattened: Vec<(String, JsonValue)> = flatten_attribute(&kv.key, av, 1);
                        for (key, val) in flattened {
                            if !val.is_null() {
                                additional_properties.insert(key, val);
                            }
                        }
                    }
                }
            }
        }

        // Resource attributes
        for kv in &resource.attributes {
            if let Some(av) = kv.value.as_ref() {
                let val: JsonValue = match av.value.as_ref() {
                    Some(OtlpStringValue(s)) => JsonValue::String(s.clone()),
                    _ => any_value_to_json(av),
                };
                if !val.is_null() {
                    safe_insert(&mut additional_properties, &kv.key, val);
                }
            }
        }

        // Scope attributes
        if let Some(scope) = scope {
            for kv in &scope.attributes {
                if let Some(av) = kv.value.as_ref() {
                    let val = match av.value.as_ref() {
                        Some(OtlpStringValue(s)) => JsonValue::String(s.clone()),
                        _ => any_value_to_json(av),
                    };
                    if !val.is_null() {
                        safe_insert(&mut additional_properties, &kv.key, val);
                    }
                }
            }
        }

        if !lr.trace_id.iter().all(|&b| b == 0) {
            let hex = bytes_to_hex_lowercase(&lr.trace_id);
            additional_properties.insert("otel.trace_id".to_string(), JsonValue::String(hex));
            let dd = u64_from_last_8(&lr.trace_id);
            additional_properties.insert("dd.trace_id".to_string(), JsonValue::String(dd.to_string()));
        }

        if !lr.span_id.iter().all(|&b| b == 0) {
            let hex = bytes_to_hex_lowercase(&lr.span_id);
            additional_properties.insert("otel.span_id".to_string(), JsonValue::String(hex));
            let dd = u64_from_first_8(&lr.span_id);
            additional_properties.insert("dd.span_id".to_string(), JsonValue::String(dd.to_string()));
        }

        // Derive status once using attribute-provided status, severity text and number
        let status = derive_status(
            status_text_from_attrs.as_deref(),
            lr.severity_text.as_str(),
            lr.severity_number,
        );

        if !lr.severity_text.is_empty() {
            additional_properties.insert(
                "otel.severity_text".to_string(),
                JsonValue::String(lr.severity_text.clone()),
            );
        }
        if lr.severity_number != 0 {
            additional_properties.insert(
                "otel.severity_number".to_string(),
                JsonValue::String(lr.severity_number.to_string()),
            );
        }

        if lr.time_unix_nano != 0 {
            additional_properties.insert(
                "otel.timestamp".to_string(),
                JsonValue::String(lr.time_unix_nano.to_string()),
            );

            let secs = (lr.time_unix_nano / 1_000_000_000) as i64;
            let subnsec = (lr.time_unix_nano % 1_000_000_000) as u32;
            if let Some(dt) = DateTime::from_timestamp(secs, subnsec) {
                // Match pattern 2006-01-02T15:04:05.000Z
                let formatted = dt.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string();
                additional_properties.insert("@timestamp".to_string(), JsonValue::String(formatted));
            }
        }

        // Message: prefer attributes, else body
        let message = match msg_from_attrs {
            Some(m) => m,
            None => lr.body.as_ref().map(any_value_to_message_string).unwrap_or_default(),
        };

        // Build Log
        let log = Log::new(message)
            .with_status(status)
            .with_hostname(host_for_record.as_deref().map(MetaString::from))
            .with_service(service_for_record.as_deref().map(MetaString::from))
            .with_tags(tags)
            .with_additional_properties(Some(additional_properties));

        log
    }
}
