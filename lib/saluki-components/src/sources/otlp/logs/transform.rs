use otlp_protos::opentelemetry::proto::common::v1 as otlp_common;
use otlp_protos::opentelemetry::proto::logs::v1::SeverityNumber as OtlpSeverityNumber;
use saluki_context::tags::{SharedTagSet, TagSet};
use saluki_core::data_model::event::log::{Log as DdLog, LogStatus};
use serde_json::Value as JsonValue;
use stringtheory::MetaString;

pub const DDTAGS_ATTR: &str = "ddtags";
pub const STATUS_KEYS: &[&str] = &["status", "severity", "level", "syslog.severity"];
pub const MESSAGE_KEYS: &[&str] = &["msg", "message", "log"];
pub const TRACE_ID_ATTR_KEYS: &[&str] = &["traceid", "trace_id", "contextmap.traceid", "oteltraceid"];
pub const SPAN_ID_ATTR_KEYS: &[&str] = &["spanid", "span_id", "contextmap.spanid", "otelspanid"];

pub fn get_string_attribute<'a>(attributes: &'a [otlp_common::KeyValue], key: &str) -> Option<&'a str> {
    attributes.iter().find_map(|kv| {
        if kv.key == key {
            if let Some(otlp_common::any_value::Value::StringValue(s_val)) =
                kv.value.as_ref().and_then(|v| v.value.as_ref())
            {
                Some(s_val.as_str())
            } else {
                None
            }
        } else {
            None
        }
    })
}

pub fn get_string_attribute_case_insensitive<'a>(
    attributes: &'a [otlp_common::KeyValue], key: &str,
) -> Option<&'a str> {
    attributes.iter().find_map(|kv| {
        if kv.key.eq_ignore_ascii_case(key) {
            if let Some(otlp_common::any_value::Value::StringValue(s_val)) =
                kv.value.as_ref().and_then(|v| v.value.as_ref())
            {
                Some(s_val.as_str())
            } else {
                None
            }
        } else {
            None
        }
    })
}

pub fn get_first_string_attr_case_insensitive<'a>(
    attributes: &'a [otlp_common::KeyValue], keys: &[&str],
) -> Option<&'a str> {
    for key in keys {
        if let Some(v) = get_string_attribute_case_insensitive(attributes, key) {
            return Some(v);
        }
    }
    None
}

pub fn derive_status(status_text_opt: Option<&str>, severity_text: &str, severity_number: i32) -> Option<LogStatus> {
    if let Some(text) = status_text_opt {
        if let Some(s) = map_status_text(text) {
            return Some(s);
        }
    }
    if let Some(s) = map_status_text(severity_text) {
        return Some(s);
    }
    map_severity_number(severity_number)
}

pub fn map_status_text(text: &str) -> Option<LogStatus> {
    if text.is_empty() {
        return None;
    }
    match text.to_ascii_lowercase().as_str() {
        "emerg" | "emergency" => Some(LogStatus::Emergency),
        "alert" => Some(LogStatus::Alert),
        "crit" | "critical" | "fatal" => Some(LogStatus::Critical),
        "err" | "error" => Some(LogStatus::Error),
        "warn" | "warning" => Some(LogStatus::Warning),
        "notice" => Some(LogStatus::Notice),
        "info" | "information" => Some(LogStatus::Info),
        "debug" | "trace" => Some(LogStatus::Debug),
        _ => None,
    }
}

pub fn map_severity_number(severity_number: i32) -> Option<LogStatus> {
    match OtlpSeverityNumber::try_from(severity_number) {
        Ok(
            OtlpSeverityNumber::Trace
            | OtlpSeverityNumber::Trace2
            | OtlpSeverityNumber::Trace3
            | OtlpSeverityNumber::Trace4
            | OtlpSeverityNumber::Debug
            | OtlpSeverityNumber::Debug2
            | OtlpSeverityNumber::Debug3
            | OtlpSeverityNumber::Debug4,
        ) => Some(LogStatus::Debug),
        Ok(
            OtlpSeverityNumber::Info
            | OtlpSeverityNumber::Info2
            | OtlpSeverityNumber::Info3
            | OtlpSeverityNumber::Info4,
        ) => Some(LogStatus::Info),
        Ok(
            OtlpSeverityNumber::Warn
            | OtlpSeverityNumber::Warn2
            | OtlpSeverityNumber::Warn3
            | OtlpSeverityNumber::Warn4,
        ) => Some(LogStatus::Warning),
        Ok(
            OtlpSeverityNumber::Error
            | OtlpSeverityNumber::Error2
            | OtlpSeverityNumber::Error3
            | OtlpSeverityNumber::Error4,
        ) => Some(LogStatus::Error),
        Ok(
            OtlpSeverityNumber::Fatal
            | OtlpSeverityNumber::Fatal2
            | OtlpSeverityNumber::Fatal3
            | OtlpSeverityNumber::Fatal4,
        ) => Some(LogStatus::Critical),
        Ok(OtlpSeverityNumber::Unspecified) => None,
        Err(_) => None,
    }
}

pub fn any_value_to_message_string(av: &otlp_common::AnyValue) -> String {
    match av.value.as_ref() {
        Some(otlp_common::any_value::Value::StringValue(s)) => s.clone(),
        _ => serde_json::to_string(&any_value_to_json(av)).unwrap_or_else(|_| String::new()),
    }
}

pub fn any_value_to_json(av: &otlp_common::AnyValue) -> JsonValue {
    match av.value.as_ref() {
        Some(otlp_common::any_value::Value::BoolValue(b)) => JsonValue::Bool(*b),
        Some(otlp_common::any_value::Value::IntValue(i)) => JsonValue::from(*i),
        Some(otlp_common::any_value::Value::DoubleValue(d)) => JsonValue::from(*d),
        Some(otlp_common::any_value::Value::StringValue(s)) => JsonValue::String(s.clone()),
        Some(otlp_common::any_value::Value::BytesValue(bytes)) => match String::from_utf8(bytes.clone()) {
            Ok(s) => JsonValue::String(s),
            Err(_) => JsonValue::String(format!("<{} bytes>", bytes.len())),
        },
        Some(otlp_common::any_value::Value::ArrayValue(arr)) => {
            JsonValue::Array(arr.values.iter().map(any_value_to_json).collect::<Vec<_>>())
        }
        Some(otlp_common::any_value::Value::KvlistValue(kvl)) => {
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

pub fn be_u64_from_last_8(bytes: &[u8]) -> u64 {
    let n = bytes.len();
    let slice = if n >= 8 { &bytes[n - 8..n] } else { bytes };
    let mut buf = [0u8; 8];
    let start = 8 - slice.len();
    buf[start..].copy_from_slice(slice);
    u64::from_be_bytes(buf)
}

pub fn be_u64_from_first_8(bytes: &[u8]) -> u64 {
    let slice = if bytes.len() >= 8 { &bytes[..8] } else { bytes };
    let mut buf = [0u8; 8];
    let start = 8 - slice.len();
    buf[start..].copy_from_slice(slice);
    u64::from_be_bytes(buf)
}

pub fn to_hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0x0F) as usize] as char);
    }
    s
}

pub fn decode_hex_exact(s: &str, expected_len_bytes: usize) -> Option<Vec<u8>> {
    if s.len() != expected_len_bytes * 2 {
        return None;
    }
    let mut out = vec![0u8; expected_len_bytes];
    let bytes = s.as_bytes();
    for i in 0..expected_len_bytes {
        let hi = from_hex_nibble(bytes[2 * i])?;
        let lo = from_hex_nibble(bytes[2 * i + 1])?;
        out[i] = (hi << 4) | lo;
    }
    Some(out)
}

pub fn from_hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(10 + (b - b'a')),
        b'A'..=b'F' => Some(10 + (b - b'A')),
        _ => None,
    }
}

// Transformer that converts a single OTLP LogRecord into a Datadog-native log (DdLog),
// given resource/scope context and resolved host/service plus base tags.
pub struct LogRecordTransformer;

impl LogRecordTransformer {
    pub fn new() -> Self {
        Self
    }

    pub fn transform(
        &self, lr: otlp_protos::opentelemetry::proto::logs::v1::LogRecord,
        resource: &otlp_protos::opentelemetry::proto::resource::v1::Resource,
        scope: Option<&otlp_common::InstrumentationScope>, host_for_record: Option<String>,
        service_for_record: Option<String>, base_tags_for_resource: &SharedTagSet,
    ) -> DdLog {
        // Start with curated resource tags only
        let mut tags = base_tags_for_resource.clone();

        // Merge ddtags (append, not overwrite)
        if let Some(ddtags) = get_string_attribute_case_insensitive(&lr.attributes, DDTAGS_ATTR) {
            let mut extra = TagSet::default();
            for raw in ddtags.split(',') {
                let s = raw.trim();
                if !s.is_empty() {
                    extra.insert_tag(s.to_string());
                }
            }
            tags.extend_from_shared(&extra.into_shared());
        }

        // Status derivation
        let status_text_from_attrs =
            get_first_string_attr_case_insensitive(&lr.attributes, STATUS_KEYS).map(|s| s.to_string());
        let status = derive_status(
            status_text_from_attrs.as_deref(),
            lr.severity_text.as_str(),
            lr.severity_number,
        );

        // Build additional properties map with resource, scope and record attributes
        let mut additional_properties = std::collections::HashMap::<String, JsonValue>::new();

        // Helper to insert safely avoiding collisions with reserved top-level fields
        fn safe_insert(map: &mut std::collections::HashMap<String, JsonValue>, key: &str, value: JsonValue) {
            if key == "hostname" || key == "service" {
                map.insert(format!("otel.{}", key), value);
            } else {
                map.insert(key.to_string(), value);
            }
        }

        // Resource attributes
        for kv in &resource.attributes {
            if let Some(av) = kv.value.as_ref() {
                let val = match av.value.as_ref() {
                    Some(otlp_common::any_value::Value::StringValue(s)) => JsonValue::String(s.clone()),
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
                        Some(otlp_common::any_value::Value::StringValue(s)) => JsonValue::String(s.clone()),
                        _ => any_value_to_json(av),
                    };
                    if !val.is_null() {
                        safe_insert(&mut additional_properties, &kv.key, val);
                    }
                }
            }
        }

        // Record attributes (skip keys that collide with message/status/tags/correlation)
        fn is_skip_key(key: &str) -> bool {
            let k = key.to_ascii_lowercase();
            k == DDTAGS_ATTR
                || MESSAGE_KEYS.iter().any(|m| k == *m)
                || STATUS_KEYS.iter().any(|s| k == *s)
                || TRACE_ID_ATTR_KEYS.iter().any(|t| k == *t)
                || SPAN_ID_ATTR_KEYS.iter().any(|s| k == *s)
        }

        for kv in &lr.attributes {
            if is_skip_key(&kv.key) {
                continue;
            }
            if let Some(av) = kv.value.as_ref() {
                let val = match av.value.as_ref() {
                    Some(otlp_common::any_value::Value::StringValue(s)) => JsonValue::String(s.clone()),
                    _ => any_value_to_json(av),
                };
                if !val.is_null() {
                    safe_insert(&mut additional_properties, &kv.key, val);
                }
            }
        }

        // Correlation fields: from record bytes first
        if !lr.trace_id.is_empty() {
            let hex = to_hex_lower(&lr.trace_id);
            additional_properties.insert("otel.trace_id".to_string(), JsonValue::String(hex));
            if lr.trace_id.len() >= 8 {
                let dd = be_u64_from_last_8(&lr.trace_id);
                additional_properties.insert("dd.trace_id".to_string(), JsonValue::from(dd));
            }
        } else if let Some(trace_hex) = get_first_string_attr_case_insensitive(&lr.attributes, TRACE_ID_ATTR_KEYS) {
            if let Some(bytes) = decode_hex_exact(trace_hex, 16) {
                additional_properties.insert("otel.trace_id".to_string(), JsonValue::String(trace_hex.to_string()));
                let dd = be_u64_from_last_8(&bytes);
                additional_properties.insert("dd.trace_id".to_string(), JsonValue::from(dd));
            }
        }

        if !lr.span_id.is_empty() {
            let hex = to_hex_lower(&lr.span_id);
            additional_properties.insert("otel.span_id".to_string(), JsonValue::String(hex));
            if lr.span_id.len() == 8 {
                let dd = be_u64_from_first_8(&lr.span_id);
                additional_properties.insert("dd.span_id".to_string(), JsonValue::from(dd));
            }
        } else if let Some(span_hex) = get_first_string_attr_case_insensitive(&lr.attributes, SPAN_ID_ATTR_KEYS) {
            if let Some(bytes) = decode_hex_exact(span_hex, 8) {
                additional_properties.insert("otel.span_id".to_string(), JsonValue::String(span_hex.to_string()));
                let dd = be_u64_from_first_8(&bytes);
                additional_properties.insert("dd.span_id".to_string(), JsonValue::from(dd));
            }
        }

        // Message: prefer attributes (msg|message|log), else body
        let message_from_attrs =
            get_first_string_attr_case_insensitive(&lr.attributes, MESSAGE_KEYS).map(|s| s.to_string());
        let message = match message_from_attrs {
            Some(m) => m,
            None => lr.body.as_ref().map(any_value_to_message_string).unwrap_or_default(),
        };

        // Timestamp: prefer event time, else observed time; seconds
        let ts_ns = if lr.time_unix_nano != 0 {
            lr.time_unix_nano
        } else {
            lr.observed_time_unix_nano
        };
        let ts_s = if ts_ns != 0 { Some(ts_ns / 1_000_000_000) } else { None };

        // Build Log
        let log = DdLog::new(message)
            .with_status(status)
            .with_timestamp_unix_s(ts_s)
            .with_hostname(host_for_record.as_deref().map(MetaString::from))
            .with_service(service_for_record.as_deref().map(MetaString::from))
            .with_ddtags(tags)
            .with_additional_properties(Some(additional_properties));

        log
    }
}

// Re-export service/host names for callers that need them alongside helpers.
