use chrono::DateTime;
use otlp_common::any_value::Value::StringValue as OtlpStringValue;
use otlp_protos::opentelemetry::proto::common::v1 as otlp_common;
use saluki_context::tags::{SharedTagSet, TagSet};
use saluki_core::data_model::event::log::{Log, LogStatus};
use serde_json::Value as JsonValue;
use stringtheory::MetaString;
use tracing::error;

pub const DDTAGS_ATTR: &str = "ddtags";
pub const STATUS_KEYS: &[&str] = &["status", "severity", "level", "syslog.severity"];
pub const MESSAGE_KEYS: &[&str] = &["msg", "message", "log"];
pub const TRACE_ID_ATTR_KEYS: &[&str] = &["traceid", "trace_id", "contextmap.traceid", "oteltraceid"];
pub const SPAN_ID_ATTR_KEYS: &[&str] = &["spanid", "span_id", "contextmap.spanid", "otelspanid"];

pub fn get_string_attribute<'a>(attributes: &'a [otlp_common::KeyValue], key: &str) -> Option<&'a str> {
    attributes.iter().find_map(|kv| {
        if kv.key == key {
            if let Some(OtlpStringValue(s_val)) = kv.value.as_ref().and_then(|v| v.value.as_ref()) {
                Some(s_val.as_str())
            } else {
                None
            }
        } else {
            None
        }
    })
}

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
        Some(otlp_common::any_value::Value::BoolValue(b)) => JsonValue::Bool(*b),
        Some(otlp_common::any_value::Value::IntValue(i)) => JsonValue::from(*i),
        Some(otlp_common::any_value::Value::DoubleValue(d)) => JsonValue::from(*d),
        Some(OtlpStringValue(s)) => JsonValue::String(s.clone()),
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
            Some(otlp_common::any_value::Value::IntValue(i)) => JsonValue::from(*i),
            Some(otlp_common::any_value::Value::BoolValue(b)) => JsonValue::Bool(*b),
            Some(otlp_common::any_value::Value::DoubleValue(d)) => JsonValue::from(*d),
            Some(otlp_common::any_value::Value::BytesValue(bytes)) => match String::from_utf8(bytes.clone()) {
                Ok(s) => JsonValue::String(s),
                Err(_) => JsonValue::String(format!("<{} bytes>", bytes.len())),
            },
            _ => JsonValue::String(serde_json::to_string(&any_value_to_json(av)).unwrap_or_else(|_| String::new())),
        }
    }

    if depth < MAX_DEPTH {
        if let Some(otlp_common::any_value::Value::KvlistValue(kvl)) = av.value.as_ref() {
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

// Transformer that converts a single OTLP LogRecord into a dd native log.
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
    ) -> Log {
        let mut tags = base_tags_for_resource.clone();

        // Build additional properties map with resource, scope and record attributes
        let mut additional_properties = std::collections::HashMap::<String, JsonValue>::new();

        // Helper to insert safely avoid overwriting fields that are already set
        fn safe_insert(map: &mut std::collections::HashMap<String, JsonValue>, key: &str, value: JsonValue) {
            if key == "hostname" || key == "service" {
                map.insert(format!("otel.{}", key), value);
            } else {
                map.insert(key.to_string(), value);
            }
        }

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
                    if let Some(av) = kv.value.as_ref() {
                        if let Some(OtlpStringValue(trace_hex)) = av.value.as_ref() {
                            if !additional_properties.contains_key("dd.trace_id") {
                                if let Some(bytes) = decode_hex_exact_to_bytes(trace_hex, 16) {
                                    let dd = u64_from_last_8(&bytes);
                                    additional_properties.insert("dd.trace_id".to_string(), JsonValue::from(dd));
                                    additional_properties
                                        .insert("otel.trace_id".to_string(), JsonValue::String(trace_hex.clone()));
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
                                    additional_properties.insert("dd.span_id".to_string(), JsonValue::from(dd));
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
                        println!("HEHEXD flatten_attribute: {}, {:?}", kv.key, av);
                        let flattened: Vec<(String, JsonValue)> = flatten_attribute(&kv.key, av, 1);
                        println!("HEHEXD2 flattened: {:?}", flattened);
                        for (key, val) in flattened {
                            println!("HEHEXD3 key: {}, val: {:?}", key, val);
                            if !val.is_null() {
                                safe_insert(&mut additional_properties, &key, val);
                            }
                            println!("HEHEXD4 additional_properties: {:?}", additional_properties);
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

        if lr.trace_id.len() == 16 && !lr.trace_id.iter().all(|&b| b == 0) {
            let hex = bytes_to_hex_lowercase(&lr.trace_id);
            additional_properties.insert("otel.trace_id".to_string(), JsonValue::String(hex));

            let dd = u64_from_last_8(&lr.trace_id);
            additional_properties.insert("dd.trace_id".to_string(), JsonValue::from(dd));
        }

        if lr.span_id.len() == 8 && !lr.span_id.iter().all(|&b| b == 0) {
            let hex = bytes_to_hex_lowercase(&lr.span_id);
            additional_properties.insert("otel.span_id".to_string(), JsonValue::String(hex));
            let dd = u64_from_first_8(&lr.span_id);
            additional_properties.insert("dd.span_id".to_string(), JsonValue::from(dd));
        }

        // Derive status once using attribute-provided status, severity text and number
        let status = derive_status(
            status_text_from_attrs.as_deref(),
            lr.severity_text.as_str(),
            lr.severity_number,
        );

        // Add explicit OpenTelemetry severity properties to additional_properties
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

        // For Datadog to use the same timestamp add both otel.timestamp (ns) and @timestamp (RFC3339-like)
        if lr.time_unix_nano != 0 {
            additional_properties.insert(
                "otel.timestamp".to_string(),
                JsonValue::String(lr.time_unix_nano.to_string()),
            );

            let secs = (lr.time_unix_nano / 1_000_000_000) as i64;
            let subnsec = (lr.time_unix_nano % 1_000_000_000) as u32;
            if let Some(dt) = DateTime::from_timestamp(secs, subnsec) {
                // Match Go's pattern 2006-01-02T15:04:05.000Z07:00
                let formatted = dt.format("%Y-%m-%dT%H:%M:%S%.3fZ%:z").to_string();
                additional_properties.insert("@timestamp".to_string(), JsonValue::String(formatted));
            }
        }

        // Message: prefer attributes (msg|message|log), else body
        let message = match msg_from_attrs {
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
        let log = Log::new(message)
            .with_status(status)
            .with_timestamp_unix_s(ts_s)
            .with_hostname(host_for_record.as_deref().map(MetaString::from))
            .with_service(service_for_record.as_deref().map(MetaString::from))
            .with_ddtags(tags)
            .with_additional_properties(Some(additional_properties));

        log
    }
}

#[cfg(test)]
mod tests {
    use opentelemetry_semantic_conventions::resource::SERVICE_NAME;

    use super::*;

    fn kv_str(key: &str, value: &str) -> otlp_common::KeyValue {
        otlp_common::KeyValue {
            key: key.to_string(),
            value: Some(otlp_common::AnyValue {
                value: Some(otlp_common::any_value::Value::StringValue(value.to_string())),
            }),
        }
    }

    fn av_str(val: &str) -> otlp_common::AnyValue {
        otlp_common::AnyValue {
            value: Some(otlp_common::any_value::Value::StringValue(val.to_string())),
        }
    }

    fn av_map(entries: Vec<(&str, otlp_common::AnyValue)>) -> otlp_common::AnyValue {
        let kvs: Vec<otlp_common::KeyValue> = entries
            .into_iter()
            .map(|(k, v)| otlp_common::KeyValue {
                key: k.to_string(),
                value: Some(v),
            })
            .collect();
        otlp_common::AnyValue {
            value: Some(otlp_common::any_value::Value::KvlistValue(otlp_common::KeyValueList {
                values: kvs,
            })),
        }
    }

    fn empty_resource() -> otlp_protos::opentelemetry::proto::resource::v1::Resource {
        otlp_protos::opentelemetry::proto::resource::v1::Resource {
            attributes: vec![],
            dropped_attributes_count: 0,
            entity_refs: vec![],
        }
    }

    fn base_tags(tags: &[&str]) -> SharedTagSet {
        let mut ts = TagSet::default();
        for t in tags {
            ts.insert_tag((*t).to_string());
        }
        ts.into_shared()
    }

    #[test]
    fn test_flatten_attribute() {
        let av = av_str("test");
        let flattened = flatten_attribute("test", &av, 1);
        assert_eq!(
            flattened,
            vec![("test".to_string(), JsonValue::String("test".to_string()))]
        );
    }

    #[test]
    fn test_transform_basic() {
        let transformer = LogRecordTransformer::new();
        let lr = otlp_protos::opentelemetry::proto::logs::v1::LogRecord {
            time_unix_nano: 0,
            observed_time_unix_nano: 0,
            severity_number: 5,
            severity_text: String::new(),
            body: None,
            attributes: vec![kv_str("app", "test")],
            dropped_attributes_count: 0,
            flags: 0,
            trace_id: Vec::new(),
            span_id: Vec::new(),
            event_name: String::new(),
        };

        // No special host/service. Provide base tag to mimic agent test's otel_source:test
        let tags = base_tags(&["otel_source:test"]);
        let log = transformer.transform(lr.clone(), &empty_resource(), None, None, None, &tags);

        assert_eq!(log.message(), "");
        assert_eq!(log.status(), Some(LogStatus::Debug));
        assert!(log.tags().has_tag("otel_source:test"));
        let props = log.additional_properties();
        assert_eq!(props.get("app"), Some(&JsonValue::String("test".to_string())));
        assert_eq!(
            props.get("otel.severity_number"),
            Some(&JsonValue::String("5".to_string()))
        );
    }

    #[test]
    fn test_transform_resource_service_and_tags() {
        let transformer = LogRecordTransformer::new();
        let lr = otlp_protos::opentelemetry::proto::logs::v1::LogRecord {
            time_unix_nano: 0,
            observed_time_unix_nano: 0,
            severity_number: 5,
            severity_text: String::new(),
            body: None,
            attributes: vec![kv_str("app", "test")],
            dropped_attributes_count: 0,
            flags: 0,
            trace_id: Vec::new(),
            span_id: Vec::new(),
            event_name: String::new(),
        };

        let resource = otlp_protos::opentelemetry::proto::resource::v1::Resource {
            attributes: vec![kv_str(SERVICE_NAME, "otlp_col")],
            dropped_attributes_count: 0,
            entity_refs: vec![],
        };

        // tags_from_attributes(resource) would include service:otlp_col. Provide both service tag and otel_source
        let tags = base_tags(&["service:otlp_col", "otel_source:test"]);
        let log = transformer.transform(lr, &resource, None, None, Some("otlp_col".to_string()), &tags);
        assert_eq!(log.service(), "otlp_col");
        assert!(log.tags().has_tag("service:otlp_col"));
        assert!(log.tags().has_tag("otel_source:test"));
        let props = log.additional_properties();
        assert_eq!(
            props.get("service.name"),
            Some(&JsonValue::String("otlp_col".to_string()))
        );
    }

    #[test]
    fn test_transform_append_ddtags() {
        let transformer = LogRecordTransformer::new();
        let lr = otlp_protos::opentelemetry::proto::logs::v1::LogRecord {
            time_unix_nano: 0,
            observed_time_unix_nano: 0,
            severity_number: 5,
            severity_text: String::new(),
            body: None,
            attributes: vec![kv_str("app", "test"), kv_str(DDTAGS_ATTR, "foo:bar")],
            dropped_attributes_count: 0,
            flags: 0,
            trace_id: Vec::new(),
            span_id: Vec::new(),
            event_name: String::new(),
        };
        let resource = otlp_protos::opentelemetry::proto::resource::v1::Resource {
            attributes: vec![kv_str(SERVICE_NAME, "otlp_col")],
            dropped_attributes_count: 0,
            entity_refs: vec![],
        };
        let tags = base_tags(&["service:otlp_col", "otel_source:test"]);
        let log = transformer.transform(lr, &resource, None, None, Some("otlp_col".to_string()), &tags);
        assert!(log.tags().has_tag("foo:bar"));
        assert!(log.tags().has_tag("service:otlp_col"));
    }

    #[test]
    fn test_transform_service_from_log_attribute() {
        let transformer = LogRecordTransformer::new();
        let lr = otlp_protos::opentelemetry::proto::logs::v1::LogRecord {
            time_unix_nano: 0,
            observed_time_unix_nano: 0,
            severity_number: 5,
            severity_text: String::new(),
            body: None,
            attributes: vec![kv_str("app", "test"), kv_str(SERVICE_NAME, "otlp_col")],
            dropped_attributes_count: 0,
            flags: 0,
            trace_id: Vec::new(),
            span_id: Vec::new(),
            event_name: String::new(),
        };
        let tags = base_tags(&["otel_source:test"]);
        let log = transformer.transform(lr, &empty_resource(), None, None, Some("otlp_col".to_string()), &tags);
        assert_eq!(log.service(), "otlp_col");
        let props = log.additional_properties();
        assert_eq!(
            props.get("service.name"),
            Some(&JsonValue::String("otlp_col".to_string()))
        );
    }

    #[test]
    fn test_transform_trace_from_bytes() {
        let transformer = LogRecordTransformer::new();
        // Build trace id with some bytes, and span id as last 8 bytes of trace
        let mut trace_id = vec![
            0x08, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x00, 0x00, 0x00, 0x00, 0x0a,
        ];
        trace_id.resize(16, 0x00);
        let span_id = trace_id[8..16].to_vec();
        let dd_tr = u64_from_last_8(&trace_id);
        let dd_sp = u64_from_first_8(&span_id);

        let lr = otlp_protos::opentelemetry::proto::logs::v1::LogRecord {
            time_unix_nano: 0,
            observed_time_unix_nano: 0,
            severity_number: 5,
            severity_text: String::new(),
            body: None,
            attributes: vec![kv_str("app", "test"), kv_str(SERVICE_NAME, "otlp_col")],
            dropped_attributes_count: 0,
            flags: 0,
            trace_id,
            span_id,
            event_name: String::new(),
        };

        let tags = base_tags(&["otel_source:test"]);
        let log = transformer.transform(lr, &empty_resource(), None, None, Some("otlp_col".to_string()), &tags);
        let props = log.additional_properties();
        assert_eq!(
            props.get("otel.severity_number"),
            Some(&JsonValue::String("5".to_string()))
        );
        assert_eq!(props.get("dd.trace_id"), Some(&JsonValue::from(dd_tr)));
        assert_eq!(props.get("dd.span_id"), Some(&JsonValue::from(dd_sp)));
        assert!(props.get("otel.trace_id").is_some());
        assert!(props.get("otel.span_id").is_some());
    }

    #[test]
    fn test_transform_trace_from_attributes() {
        let transformer = LogRecordTransformer::new();
        let lr = otlp_protos::opentelemetry::proto::logs::v1::LogRecord {
            time_unix_nano: 0,
            observed_time_unix_nano: 0,
            severity_number: 5,
            severity_text: String::new(),
            body: None,
            attributes: vec![
                kv_str("app", "test"),
                kv_str("spanid", "2e26da881214cd7c"),
                kv_str("traceid", "437ab4d83468c540bb0f3398a39faa59"),
                kv_str(SERVICE_NAME, "otlp_col"),
            ],
            dropped_attributes_count: 0,
            flags: 0,
            trace_id: Vec::new(),
            span_id: Vec::new(),
            event_name: String::new(),
        };
        let tags = base_tags(&["otel_source:test"]);
        let log = transformer.transform(lr, &empty_resource(), None, None, Some("otlp_col".to_string()), &tags);
        let props = log.additional_properties();
        assert_eq!(
            props.get("otel.trace_id"),
            Some(&JsonValue::String("437ab4d83468c540bb0f3398a39faa59".to_string()))
        );
        assert_eq!(
            props.get("otel.span_id"),
            Some(&JsonValue::String("2e26da881214cd7c".to_string()))
        );
        assert_eq!(props.get("dd.span_id"), Some(&JsonValue::from(3325585652813450620u64)));
        assert_eq!(
            props.get("dd.trace_id"),
            Some(&JsonValue::from(13479048940416379481u64))
        );
    }

    #[test]
    fn test_transform_trace_from_attributes_decode_error() {
        let transformer = LogRecordTransformer::new();
        let lr = otlp_protos::opentelemetry::proto::logs::v1::LogRecord {
            time_unix_nano: 0,
            observed_time_unix_nano: 0,
            severity_number: 5,
            severity_text: String::new(),
            body: None,
            attributes: vec![
                kv_str("app", "test"),
                kv_str("spanid", "2e26da881214cd7c"),
                kv_str("traceid", "invalidtraceid"),
                kv_str(SERVICE_NAME, "otlp_col"),
            ],
            dropped_attributes_count: 0,
            flags: 0,
            trace_id: Vec::new(),
            span_id: Vec::new(),
            event_name: String::new(),
        };
        let tags = base_tags(&["otel_source:test"]);
        let log = transformer.transform(lr, &empty_resource(), None, None, Some("otlp_col".to_string()), &tags);
        let props = log.additional_properties();
        assert!(props.get("otel.trace_id").is_none());
        assert!(props.get("dd.trace_id").is_none());
        assert_eq!(
            props.get("otel.span_id"),
            Some(&JsonValue::String("2e26da881214cd7c".to_string()))
        );
        assert_eq!(props.get("dd.span_id"), Some(&JsonValue::from(3325585652813450620u64)));
    }

    #[test]
    fn test_transform_trace_from_attributes_size_error() {
        let transformer = LogRecordTransformer::new();
        let lr = otlp_protos::opentelemetry::proto::logs::v1::LogRecord {
            time_unix_nano: 0,
            observed_time_unix_nano: 0,
            severity_number: 5,
            severity_text: String::new(),
            body: None,
            attributes: vec![
                kv_str("app", "test"),
                kv_str("spanid", "2023675201651514964"),
                kv_str(
                    "traceid",
                    "eb068afe5e53704f3b0dc3d3e1e397cb760549a7b58547db4f1dee845d9101f8db1ccf8fdd0976a9112f",
                ),
                kv_str(SERVICE_NAME, "otlp_col"),
            ],
            dropped_attributes_count: 0,
            flags: 0,
            trace_id: Vec::new(),
            span_id: Vec::new(),
            event_name: String::new(),
        };
        let tags = base_tags(&["otel_source:test"]);
        let log = transformer.transform(lr, &empty_resource(), None, None, Some("otlp_col".to_string()), &tags);
        let props = log.additional_properties();
        assert!(props.get("otel.trace_id").is_none());
        assert!(props.get("dd.trace_id").is_none());
        assert!(props.get("otel.span_id").is_none());
        assert!(props.get("dd.span_id").is_none());
    }

    #[test]
    fn test_derive_status_precedence() {
        let transformer = LogRecordTransformer::new();
        let lr = otlp_protos::opentelemetry::proto::logs::v1::LogRecord {
            time_unix_nano: 0,
            observed_time_unix_nano: 0,
            severity_number: 5,
            severity_text: "alert".to_string(),
            body: None,
            attributes: vec![kv_str("app", "test")],
            dropped_attributes_count: 0,
            flags: 0,
            trace_id: vec![0; 16],
            span_id: vec![0; 8],
            event_name: String::new(),
        };
        let log = transformer.transform(
            lr,
            &empty_resource(),
            None,
            None,
            Some("otlp_col".to_string()),
            &base_tags(&[]),
        );
        assert_eq!(log.status(), Some(LogStatus::Alert));
        let props = log.additional_properties();
        assert_eq!(
            props.get("otel.severity_text"),
            Some(&JsonValue::String("alert".to_string()))
        );
        assert_eq!(
            props.get("otel.severity_number"),
            Some(&JsonValue::String("5".to_string()))
        );
    }

    #[test]
    fn test_transform_body_message_comes_from_body() {
        let transformer = LogRecordTransformer::new();
        let body = av_str("This is log");
        let lr = otlp_protos::opentelemetry::proto::logs::v1::LogRecord {
            time_unix_nano: 0,
            observed_time_unix_nano: 0,
            severity_number: 13,
            severity_text: String::new(),
            body: Some(body),
            attributes: vec![kv_str("app", "test"), kv_str(SERVICE_NAME, "otlp_col")],
            dropped_attributes_count: 0,
            flags: 0,
            trace_id: vec![0; 16],
            span_id: vec![0; 8],
            event_name: String::new(),
        };
        let log = transformer.transform(
            lr,
            &empty_resource(),
            None,
            None,
            Some("otlp_col".to_string()),
            &base_tags(&["otel_source:test"]),
        );
        assert_eq!(log.message(), "This is log");
        assert_eq!(log.status(), Some(LogStatus::Warning));
        let props = log.additional_properties();
        assert_eq!(
            props.get("otel.severity_number"),
            Some(&JsonValue::String("13".to_string()))
        );
    }

    #[test]
    fn test_log_level_attribute_sets_status() {
        let transformer = LogRecordTransformer::new();
        let body = av_str("This is log");
        let lr = otlp_protos::opentelemetry::proto::logs::v1::LogRecord {
            time_unix_nano: 0,
            observed_time_unix_nano: 0,
            severity_number: 0,
            severity_text: String::new(),
            body: Some(body),
            attributes: vec![kv_str("app", "test"), kv_str("level", "error")],
            dropped_attributes_count: 0,
            flags: 0,
            trace_id: vec![0; 16],
            span_id: vec![0; 8],
            event_name: String::new(),
        };
        let log = transformer.transform(
            lr,
            &empty_resource(),
            None,
            None,
            Some("otlp_col".to_string()),
            &base_tags(&["otel_source:test"]),
        );
        assert_eq!(log.message(), "This is log");
        assert_eq!(log.status(), Some(LogStatus::Error));
    }

    #[test]
    fn test_resource_attributes_in_additional_properties() {
        let transformer = LogRecordTransformer::new();
        let lr = otlp_protos::opentelemetry::proto::logs::v1::LogRecord {
            time_unix_nano: 0,
            observed_time_unix_nano: 0,
            severity_number: 5,
            severity_text: String::new(),
            body: None,
            attributes: vec![kv_str("app", "test")],
            dropped_attributes_count: 0,
            flags: 0,
            trace_id: Vec::new(),
            span_id: Vec::new(),
            event_name: String::new(),
        };
        let resource = otlp_protos::opentelemetry::proto::resource::v1::Resource {
            attributes: vec![kv_str(SERVICE_NAME, "otlp_col"), kv_str("key", "val")],
            dropped_attributes_count: 0,
            entity_refs: vec![],
        };
        let log = transformer.transform(
            lr,
            &resource,
            None,
            None,
            Some("otlp_col".to_string()),
            &base_tags(&["service:otlp_col", "otel_source:test"]),
        );
        let props = log.additional_properties();
        assert_eq!(props.get("key"), Some(&JsonValue::String("val".to_string())));
        assert_eq!(
            props.get("service.name"),
            Some(&JsonValue::String("otlp_col".to_string()))
        );
    }

    #[test]
    fn test_dd_hostname_and_service_preserved() {
        let transformer = LogRecordTransformer::new();
        let lr = otlp_protos::opentelemetry::proto::logs::v1::LogRecord {
            time_unix_nano: 0,
            observed_time_unix_nano: 0,
            severity_number: 5,
            severity_text: String::new(),
            body: None,
            attributes: vec![kv_str("app", "test")],
            dropped_attributes_count: 0,
            flags: 0,
            trace_id: Vec::new(),
            span_id: Vec::new(),
            event_name: String::new(),
        };
        let resource = otlp_protos::opentelemetry::proto::resource::v1::Resource {
            attributes: vec![kv_str("hostname", "example_host"), kv_str("service", "otlp_col")],
            dropped_attributes_count: 0,
            entity_refs: vec![],
        };
        let log = transformer.transform(lr, &resource, None, None, None, &base_tags(&["otel_source:test"]));
        let props = log.additional_properties();
        assert_eq!(
            props.get("otel.service"),
            Some(&JsonValue::String("otlp_col".to_string()))
        );
        assert_eq!(
            props.get("otel.hostname"),
            Some(&JsonValue::String("example_host".to_string()))
        );
    }

    #[test]
    fn test_nestings() {
        let transformer = LogRecordTransformer::new();
        // Build nested structure described in the agent test
        let nested = av_map(vec![
            ("nest1", av_map(vec![("nest2", av_str("val"))])),
            (
                "nest12",
                av_map(vec![("nest22", av_map(vec![("nest3", av_str("val2"))]))]),
            ),
            ("nest13", av_str("val3")),
        ]);

        let lr = otlp_protos::opentelemetry::proto::logs::v1::LogRecord {
            time_unix_nano: 0,
            observed_time_unix_nano: 0,
            severity_number: 0,
            severity_text: String::new(),
            body: None,
            attributes: vec![otlp_common::KeyValue {
                key: "root".to_string(),
                value: Some(nested),
            }],
            dropped_attributes_count: 0,
            flags: 0,
            trace_id: Vec::new(),
            span_id: Vec::new(),
            event_name: String::new(),
        };
        let log = transformer.transform(
            lr,
            &empty_resource(),
            None,
            None,
            None,
            &base_tags(&["otel_source:test"]),
        );
        let props = log.additional_properties();
        assert_eq!(
            props.get("root.nest1.nest2"),
            Some(&JsonValue::String("val".to_string()))
        );
        assert_eq!(
            props.get("root.nest12.nest22.nest3"),
            Some(&JsonValue::String("val2".to_string()))
        );
        assert_eq!(props.get("root.nest13"), Some(&JsonValue::String("val3".to_string())));
        assert_eq!(log.status(), None);
    }

    #[test]
    fn test_too_many_nestings() {
        let transformer = LogRecordTransformer::new();
        // Construct deeper than MAX_DEPTH entries
        let deep = av_map(vec![(
            "nest2",
            av_map(vec![(
                "nest3",
                av_map(vec![(
                    "nest4",
                    av_map(vec![(
                        "nest5",
                        av_map(vec![
                            (
                                "nest6",
                                av_map(vec![(
                                    "nest7",
                                    av_map(vec![(
                                        "nest8",
                                        av_map(vec![(
                                            "nest9",
                                            av_map(vec![(
                                                "nest10",
                                                av_map(vec![("nest11", av_map(vec![("nest12", av_str("ok"))]))]),
                                            )]),
                                        )]),
                                    )]),
                                )]),
                            ),
                            ("nest14", av_map(vec![("nest15", av_str("ok2"))])),
                        ]),
                    )]),
                )]),
            )]),
        )]);
        let lr = otlp_protos::opentelemetry::proto::logs::v1::LogRecord {
            time_unix_nano: 0,
            observed_time_unix_nano: 0,
            severity_number: 0,
            severity_text: String::new(),
            body: None,
            attributes: vec![otlp_common::KeyValue {
                key: "nest1".to_string(),
                value: Some(deep),
            }],
            dropped_attributes_count: 0,
            flags: 0,
            trace_id: Vec::new(),
            span_id: Vec::new(),
            event_name: String::new(),
        };
        let log = transformer.transform(
            lr,
            &empty_resource(),
            None,
            None,
            None,
            &base_tags(&["otel_source:test"]),
        );
        let props = log.additional_properties();
        assert_eq!(
            props.get("nest1.nest2.nest3.nest4.nest5.nest6.nest7.nest8.nest9.nest10"),
            Some(&JsonValue::String("{\"nest11\":{\"nest12\":\"ok\"}}".to_string()))
        );
        assert_eq!(
            props.get("nest1.nest2.nest3.nest4.nest5.nest14.nest15"),
            Some(&JsonValue::String("ok2".to_string()))
        );
    }

    #[test]
    fn test_timestamps_formatted_properly() {
        let transformer = LogRecordTransformer::new();
        let lr = otlp_protos::opentelemetry::proto::logs::v1::LogRecord {
            time_unix_nano: 1_700_499_303_397_000_000u64,
            observed_time_unix_nano: 0,
            severity_number: 5,
            severity_text: String::new(),
            body: None,
            attributes: vec![],
            dropped_attributes_count: 0,
            flags: 0,
            trace_id: Vec::new(),
            span_id: Vec::new(),
            event_name: String::new(),
        };
        let log = transformer.transform(
            lr,
            &empty_resource(),
            None,
            None,
            None,
            &base_tags(&["otel_source:test"]),
        );
        let props = log.additional_properties();
        assert_eq!(
            props.get("otel.severity_number"),
            Some(&JsonValue::String("5".to_string()))
        );
        assert_eq!(
            props.get("otel.timestamp"),
            Some(&JsonValue::String("1700499303397000000".to_string()))
        );
        assert_eq!(
            props.get("@timestamp"),
            Some(&JsonValue::String("2023-11-20T16:55:03.397Z+00:00".to_string()))
        );
    }

    #[test]
    fn test_scope_attributes_included() {
        let transformer = LogRecordTransformer::new();
        let lr = otlp_protos::opentelemetry::proto::logs::v1::LogRecord {
            time_unix_nano: 0,
            observed_time_unix_nano: 0,
            severity_number: 5,
            severity_text: String::new(),
            body: Some(av_str("hello world")),
            attributes: vec![],
            dropped_attributes_count: 0,
            flags: 0,
            trace_id: Vec::new(),
            span_id: Vec::new(),
            event_name: String::new(),
        };
        let mut scope = otlp_common::InstrumentationScope::default();
        scope.attributes.push(kv_str("otelcol.component.id", "otlp"));
        scope.attributes.push(kv_str("otelcol.component.kind", "Receiver"));
        let log = transformer.transform(
            lr,
            &empty_resource(),
            Some(&scope),
            None,
            None,
            &base_tags(&["otel_source:test"]),
        );
        assert_eq!(log.message(), "hello world");
        let props = log.additional_properties();
        assert_eq!(
            props.get("otelcol.component.id"),
            Some(&JsonValue::String("otlp".to_string()))
        );
        assert_eq!(
            props.get("otelcol.component.kind"),
            Some(&JsonValue::String("Receiver".to_string()))
        );
    }
}
