use std::collections::HashMap;

use chrono::DateTime;
use otlp_common::any_value::Value::StringValue as OtlpStringValue;
use otlp_common::any_value::Value::{ArrayValue, BoolValue, BytesValue, DoubleValue, IntValue, KvlistValue};
use otlp_protos::opentelemetry::proto::common::v1 as otlp_common;
use otlp_protos::opentelemetry::proto::logs::v1::LogRecord;
use otlp_protos::opentelemetry::proto::resource::v1::Resource;
use saluki_core::data_model::{event::log::Log, tags::TagSet};
use serde_json::Value as JsonValue;
use stringtheory::MetaString;
use tracing::error;

pub const DDTAGS_ATTR: &str = "ddtags";
pub const STATUS_KEYS: &[&str] = &["status", "severity", "level", "syslog.severity"];
pub const MESSAGE_KEYS: &[&str] = &["msg", "message", "log"];
pub const TRACE_ID_ATTR_KEYS: &[&str] = &["traceid", "trace_id", "contextmap.traceid", "oteltraceid"];
pub const SPAN_ID_ATTR_KEYS: &[&str] = &["spanid", "span_id", "contextmap.spanid", "otelspanid"];

static DEFAULT_SOURCE: MetaString = MetaString::from_static("otlp_log_ingestion");

/// Derives a log's status from its status attributes, severity text, and severity number.
///
/// The status is the last status attribute (`status`, `severity`, `level`, or `syslog.severity`), then the severity
/// text, then the name of the severity number's range, taking the first that isn't empty. Attribute and severity
/// text are used as they are, without normalization. A severity number of 0 has no name, so a log with nothing else
/// gets an empty status.
pub fn derive_status(status: Option<String>, severity_text: &str, severity_number: i32) -> MetaString {
    match status {
        Some(status) if !status.is_empty() => MetaString::from(status),
        _ if !severity_text.is_empty() => MetaString::from(severity_text),
        _ if severity_number != 0 => MetaString::from_static(status_from_severity_number(severity_number)),
        _ => MetaString::empty(),
    }
}

/// Returns the name of the range that a severity number falls in.
///
/// The names follow the ranges of the OpenTelemetry log data model, and numbers above the last range are errors:
/// <https://github.com/open-telemetry/opentelemetry-specification/blob/main/specification/logs/data-model.md#field-severitynumber>
pub fn status_from_severity_number(severity_number: i32) -> &'static str {
    match severity_number {
        ..=4 => "trace",
        5..=8 => "debug",
        9..=12 => "info",
        13..=16 => "warn",
        17..=20 => "error",
        21..=24 => "fatal",
        _ => "error",
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
fn safe_insert(map: &mut HashMap<MetaString, JsonValue>, key: &str, value: JsonValue) {
    if key == "hostname" || key == "service" {
        map.insert(format!("otel.{}", key).into(), value);
    } else {
        map.insert(key.into(), value);
    }
}

pub fn transform_log_record(
    mut lr: LogRecord, resource: &Resource, scope: Option<&otlp_common::InstrumentationScope>,
    record_host: Option<MetaString>, record_service: Option<MetaString>, mut tags: TagSet,
) -> Log {
    // Build additional properties map with resource, scope and record attributes
    let mut additional_properties = HashMap::new();

    // Single-pass over record attributes: handle special keys and flatten others
    let mut status_text_from_attrs: Option<String> = None;
    let mut msg_from_attrs: Option<String> = None;
    for kv in lr.attributes.iter_mut() {
        kv.key.make_ascii_lowercase();
        match kv.key.as_str() {
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
                if let Some(av) = kv.value.as_ref() {
                    if let Some(OtlpStringValue(trace_hex)) = av.value.as_ref() {
                        if !additional_properties.contains_key("dd.trace_id") {
                            if let Some(bytes) = decode_hex_exact_to_bytes(trace_hex, 16) {
                                let dd = u64_from_last_8(&bytes);
                                additional_properties.insert(
                                    MetaString::from_static("dd.trace_id"),
                                    JsonValue::String(dd.to_string()),
                                );
                                additional_properties.insert(
                                    MetaString::from_static("otel.trace_id"),
                                    JsonValue::String(trace_hex.clone()),
                                );
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
                                    .insert(MetaString::from_static("dd.span_id"), JsonValue::String(dd.to_string()));
                                additional_properties.insert(
                                    MetaString::from_static("otel.span_id"),
                                    JsonValue::String(span_hex.clone()),
                                );
                            }
                        }
                    }
                }
            }
            // ddtags aggregation
            k if k == DDTAGS_ATTR => {
                if let Some(av) = kv.value.as_ref() {
                    if let Some(OtlpStringValue(s)) = av.value.as_ref() {
                        for raw in s.split(',') {
                            let t = raw.trim();
                            if !t.is_empty() {
                                tags.insert_tag(t);
                            }
                        }
                    }
                }
            }
            // Default: flatten map and insert safely
            _ => {
                if let Some(av) = kv.value.as_ref() {
                    let flattened: Vec<(String, JsonValue)> = flatten_attribute(&kv.key, av, 1);
                    for (key, val) in flattened {
                        if !val.is_null() {
                            additional_properties.insert(key.into(), val);
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
        additional_properties.insert(MetaString::from_static("otel.trace_id"), JsonValue::String(hex));
        let dd = u64_from_last_8(&lr.trace_id);
        additional_properties.insert(
            MetaString::from_static("dd.trace_id"),
            JsonValue::String(dd.to_string()),
        );
    }

    if !lr.span_id.iter().all(|&b| b == 0) {
        let hex = bytes_to_hex_lowercase(&lr.span_id);
        additional_properties.insert(MetaString::from_static("otel.span_id"), JsonValue::String(hex));
        let dd = u64_from_first_8(&lr.span_id);
        additional_properties.insert(MetaString::from_static("dd.span_id"), JsonValue::String(dd.to_string()));
    }

    // Derive status once using attribute-provided status, severity text and number
    let status = derive_status(status_text_from_attrs, lr.severity_text.as_str(), lr.severity_number);
    // The Agent sends the derived status in place of any resource or scope attribute named `status`. The encoder
    // writes additional properties over the status, so the attribute must not be among them.
    additional_properties.remove("status");

    if !lr.severity_text.is_empty() {
        additional_properties.insert(
            MetaString::from_static("otel.severity_text"),
            JsonValue::String(lr.severity_text.clone()),
        );
    }
    if lr.severity_number != 0 {
        additional_properties.insert(
            MetaString::from_static("otel.severity_number"),
            JsonValue::String(lr.severity_number.to_string()),
        );
    }

    if lr.time_unix_nano != 0 {
        additional_properties.insert(
            MetaString::from_static("otel.timestamp"),
            JsonValue::String(lr.time_unix_nano.to_string()),
        );

        let secs = (lr.time_unix_nano / 1_000_000_000) as i64;
        let subnsec = (lr.time_unix_nano % 1_000_000_000) as u32;
        if let Some(dt) = DateTime::from_timestamp(secs, subnsec) {
            // Match pattern 2006-01-02T15:04:05.000Z
            let formatted = dt.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string();
            additional_properties.insert(MetaString::from_static("@timestamp"), JsonValue::String(formatted));
        }
    }

    // Message: prefer attributes, else body
    let message = match msg_from_attrs {
        Some(m) => m,
        None => lr.body.as_ref().map(any_value_to_message_string).unwrap_or_default(),
    };

    // Build Log
    Log::new(message)
        .with_status(status)
        .with_source(DEFAULT_SOURCE.clone())
        .with_hostname(record_host)
        .with_service(record_service)
        .with_tags(tags)
        .with_additional_properties(additional_properties)
}

#[cfg(test)]
mod tests {
    use super::{derive_status, status_from_severity_number};

    #[test]
    fn derive_status_follows_the_reference_precedence() {
        // (status attribute, severity text, severity number, status)
        let cases = [
            // The status attribute comes first, then the severity text, then the severity number.
            (Some("error"), "alert", 5, "error"),
            (None, "alert", 5, "alert"),
            (None, "", 5, "debug"),
            // Attribute and severity text are kept as they are, even when they aren't a known status.
            (Some("Not A Status"), "", 0, "Not A Status"),
            (None, "Information", 9, "Information"),
            (None, "WARN", 0, "WARN"),
            // An empty attribute or severity text falls through to the next source.
            (Some(""), "notice", 13, "notice"),
            (Some(""), "", 13, "warn"),
            // Severity number 0 has no name, so a log with nothing else has no status.
            (None, "", 0, ""),
            (Some(""), "", 0, ""),
        ];

        for (status, severity_text, severity_number, expected) in cases {
            assert_eq!(
                derive_status(status.map(String::from), severity_text, severity_number),
                expected,
                "derive_status({:?}, {:?}, {})",
                status,
                severity_text,
                severity_number
            );
        }
    }

    #[test]
    fn status_from_severity_number_matches_the_reference() {
        // Ported from `TestDeriveStatus` in the reference implementation:
        // https://github.com/DataDog/datadog-agent/blob/e0c8d8d93befb6864f13e2d88acbe13fb2d380b7/pkg/opentelemetry-mapping-go/otlp/logs/transform_test.go
        // with the boundaries of every range, and negative numbers, which the reference implementation treats as trace.
        let cases = [
            (-1, "trace"),
            (1, "trace"),
            (3, "trace"),
            (4, "trace"),
            (5, "debug"),
            (7, "debug"),
            (8, "debug"),
            (9, "info"),
            (12, "info"),
            (13, "warn"),
            (16, "warn"),
            (17, "error"),
            (20, "error"),
            (21, "fatal"),
            (24, "fatal"),
            (25, "error"),
            (50, "error"),
        ];

        for (severity_number, expected) in cases {
            assert_eq!(
                status_from_severity_number(severity_number),
                expected,
                "status_from_severity_number({})",
                severity_number
            );
        }
    }
}
