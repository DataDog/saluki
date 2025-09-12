use otlp_protos::opentelemetry::proto::common::v1 as otlp_common;
use otlp_protos::opentelemetry::proto::logs::v1::{ SeverityNumber as OtlpSeverityNumber};

use saluki_core::data_model::event::log::LogStatus;
use serde_json::Value as JsonValue;

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

pub fn get_string_attribute_case_insensitive<'a>(attributes: &'a [otlp_common::KeyValue], key: &str) -> Option<&'a str> {
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

pub fn get_first_string_attr_case_insensitive<'a>(attributes: &'a [otlp_common::KeyValue], keys: &[&str]) -> Option<&'a str> {
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
            OtlpSeverityNumber::Info | OtlpSeverityNumber::Info2 | OtlpSeverityNumber::Info3 | OtlpSeverityNumber::Info4,
        ) => Some(LogStatus::Info),
        Ok(
            OtlpSeverityNumber::Warn | OtlpSeverityNumber::Warn2 | OtlpSeverityNumber::Warn3 | OtlpSeverityNumber::Warn4,
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

// Re-export service/host names for callers that need them alongside helpers.

