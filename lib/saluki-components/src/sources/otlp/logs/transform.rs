//! Transforms OTLP log records into logs.
//!
//! [`transform_log_record`] follows `transform` in the Agent's `pkg/opentelemetry-mapping-go/otlp/logs/transform.go`,
//! phase by phase. Each phase writes [`Properties`], so a later phase overwrites the keys of an earlier one.

use std::collections::HashMap;

use otlp_common::any_value::Value::StringValue as OtlpStringValue;
use otlp_common::any_value::Value::{ArrayValue, BoolValue, BytesValue, DoubleValue, IntValue, KvlistValue};
use otlp_protos::opentelemetry::proto::common::v1 as otlp_common;
use otlp_protos::opentelemetry::proto::logs::v1::LogRecord;
use otlp_protos::opentelemetry::proto::resource::v1::Resource;
use saluki_common::time::{Timestamp, TimestampFormat};
use saluki_core::data_model::{event::log::Log, tags::TagSet};
use serde_json::Value as JsonValue;
use stringtheory::MetaString;

use crate::common::otlp::traces::transform::bytes_to_hex_lowercase;

pub(super) const DDTAGS_ATTR: &str = "ddtags";
const STATUS_KEYS: &[&str] = &["status", "severity", "level", "syslog.severity"];
const MESSAGE_KEYS: &[&str] = &["msg", "message", "log"];
const TRACE_ID_ATTR_KEYS: &[&str] = &["traceid", "trace_id", "contextmap.traceid", "oteltraceid"];
const SPAN_ID_ATTR_KEYS: &[&str] = &["spanid", "span_id", "contextmap.spanid", "otelspanid"];

// Property keys that keep the OpenTelemetry fields of a record, as in the Agent's `transform.go`.
const OTEL_TRACE_ID: &str = "otel.trace_id";
const OTEL_SPAN_ID: &str = "otel.span_id";
const OTEL_SEVERITY_NUMBER: &str = "otel.severity_number";
const OTEL_SEVERITY_TEXT: &str = "otel.severity_text";
const OTEL_TIMESTAMP: &str = "otel.timestamp";
const OTEL_SCOPE_NAME: &str = "otel.scope.name";
const OTEL_SCOPE_VERSION: &str = "otel.scope.version";
const OTEL_LIBRARY_NAME: &str = "otel.library.name";
const OTEL_LIBRARY_VERSION: &str = "otel.library.version";
const OTEL_EVENT_NAME: &str = "otel.event_name";
const OTEL_HOSTNAME: &str = "otel.hostname";
const OTEL_SERVICE: &str = "otel.service";

// Property keys of the Datadog counterparts of those fields.
const DD_TRACE_ID: &str = "dd.trace_id";
const DD_SPAN_ID: &str = "dd.span_id";
const DD_STATUS: &str = "status";
const DD_TIMESTAMP: &str = "@timestamp";

static DEFAULT_SOURCE: MetaString = MetaString::from_static("otlp_log_ingestion");

/// Transforms an OTLP log record, with its resource and scope, into a log.
pub(super) fn transform_log_record(
    lr: LogRecord, resource: &Resource, scope: Option<&otlp_common::InstrumentationScope>,
    record_host: Option<MetaString>, record_service: Option<MetaString>, mut tags: TagSet,
) -> Log {
    let mut properties = Properties::default();

    // Resource attributes overwrite record attributes, and scope attributes overwrite both.
    let reserved = apply_record_attributes(&lr.attributes, &mut properties);
    if let Some(ddtags) = reserved.ddtags {
        add_ddtags(&mut tags, ddtags);
    }
    apply_resource_attributes(&resource.attributes, &mut properties);
    if let Some(scope) = scope {
        apply_scope(scope, &mut properties);
    }
    apply_trace_context(&lr, &mut properties);

    let status = derive_status(reserved.status, &lr.severity_text, lr.severity_number);
    // The Agent sends the derived status in place of any resource or scope attribute named `status`. The encoder
    // writes additional properties over the status, so the attribute must not be among them.
    properties.remove(DD_STATUS);

    apply_severity(&lr, &mut properties);
    apply_timestamp(&lr, &mut properties);
    // The event name overwrites an `otel.event_name` attribute from any source.
    if !lr.event_name.is_empty() {
        properties.insert_static(OTEL_EVENT_NAME, lr.event_name);
    }

    Log::new(message(reserved.message, lr.body.as_ref()))
        .with_status(status)
        .with_source(DEFAULT_SOURCE.clone())
        .with_hostname(record_host)
        .with_service(record_service)
        .with_tags(tags)
        .with_additional_properties(properties.into_map())
}

/// The additional properties of a log, which the content renderer writes over the log's fields.
#[derive(Default)]
struct Properties(HashMap<MetaString, JsonValue>);

impl Properties {
    /// Sets a property, overwriting an earlier one with the same key.
    fn insert(&mut self, key: impl Into<MetaString>, value: JsonValue) {
        self.0.insert(key.into(), value);
    }

    /// Sets a string property under a fixed key, overwriting an earlier one.
    fn insert_static(&mut self, key: &'static str, value: String) {
        self.0.insert(MetaString::from_static(key), JsonValue::String(value));
    }

    /// Sets a string property under a fixed key, unless the key is already set.
    fn insert_static_if_missing(&mut self, key: &'static str, value: &str) {
        self.0
            .entry(MetaString::from_static(key))
            .or_insert_with(|| JsonValue::String(value.to_string()));
    }

    fn contains(&self, key: &str) -> bool {
        self.0.contains_key(key)
    }

    fn remove(&mut self, key: &str) {
        self.0.remove(key);
    }

    fn into_map(self) -> HashMap<MetaString, JsonValue> {
        self.0
    }
}

/// The reserved record attributes that become fields of the log instead of properties.
///
/// When a reserved key repeats, the last attribute wins.
#[derive(Default)]
struct ReservedAttributes<'a> {
    message: Option<&'a str>,
    status: Option<&'a str>,
    ddtags: Option<&'a str>,
}

/// Sets the properties of the record attributes, and returns the reserved ones.
///
/// Reserved keys match in any case, and every other key keeps its original spelling. The first trace and span IDs
/// that decode win.
fn apply_record_attributes<'a>(
    attributes: &'a [otlp_common::KeyValue], properties: &mut Properties,
) -> ReservedAttributes<'a> {
    let mut reserved = ReservedAttributes::default();
    for kv in attributes {
        match kv.key.as_str() {
            k if is_reserved(k, MESSAGE_KEYS) => reserved.message = Some(string_value_or_empty(kv)),
            k if is_reserved(k, STATUS_KEYS) => reserved.status = Some(string_value_or_empty(kv)),
            k if is_reserved(k, TRACE_ID_ATTR_KEYS) => {
                if let Some(OtlpStringValue(trace_hex)) = kv.value.as_ref().and_then(|av| av.value.as_ref()) {
                    if !properties.contains(DD_TRACE_ID) {
                        if let Some(bytes) = decode_hex_exact_to_bytes(trace_hex, 16) {
                            insert_trace_id(properties, trace_hex.clone(), &bytes);
                        }
                    }
                }
            }
            k if is_reserved(k, SPAN_ID_ATTR_KEYS) => {
                if let Some(OtlpStringValue(span_hex)) = kv.value.as_ref().and_then(|av| av.value.as_ref()) {
                    if !properties.contains(DD_SPAN_ID) {
                        if let Some(bytes) = decode_hex_exact_to_bytes(span_hex, 8) {
                            insert_span_id(properties, span_hex.clone(), &bytes);
                        }
                    }
                }
            }
            k if k.eq_ignore_ascii_case(DDTAGS_ATTR) => reserved.ddtags = Some(string_value_or_empty(kv)),
            k if k.eq_ignore_ascii_case(OTEL_LIBRARY_NAME) => {
                insert_scope_alias(properties, attributes, OTEL_SCOPE_NAME, kv)
            }
            k if k.eq_ignore_ascii_case(OTEL_LIBRARY_VERSION) => {
                insert_scope_alias(properties, attributes, OTEL_SCOPE_VERSION, kv)
            }
            _ => {
                if let Some(av) = kv.value.as_ref() {
                    for (key, val) in flatten_attribute(&kv.key, av, 1) {
                        if !val.is_null() {
                            properties.insert(key, val);
                        }
                    }
                }
            }
        }
    }
    reserved
}

/// Adds the tags of a `ddtags` attribute. The resource tags are already in `tags`.
fn add_ddtags(tags: &mut TagSet, ddtags: &str) {
    for tag in ddtags.split(',').map(str::trim) {
        if !tag.is_empty() {
            tags.insert_tag(tag);
        }
    }
}

/// Sets the properties of the resource attributes.
///
/// Unlike record attributes, their aliases match case-sensitively. `hostname` and `service` are renamed to
/// `otel.hostname` and `otel.service`, so they don't overwrite the log's own fields.
fn apply_resource_attributes(attributes: &[otlp_common::KeyValue], properties: &mut Properties) {
    for kv in attributes {
        match kv.key.as_str() {
            OTEL_LIBRARY_NAME => insert_scope_alias(properties, attributes, OTEL_SCOPE_NAME, kv),
            OTEL_LIBRARY_VERSION => insert_scope_alias(properties, attributes, OTEL_SCOPE_VERSION, kv),
            key => {
                if let Some(av) = kv.value.as_ref() {
                    let val = match av.value.as_ref() {
                        Some(OtlpStringValue(s)) => JsonValue::String(s.clone()),
                        _ => any_value_to_json(av),
                    };
                    if !val.is_null() {
                        match key {
                            "hostname" => properties.insert(MetaString::from_static(OTEL_HOSTNAME), val),
                            "service" => properties.insert(MetaString::from_static(OTEL_SERVICE), val),
                            _ => properties.insert(key, val),
                        }
                    }
                }
            }
        }
    }
}

/// Sets the properties of the scope's attributes, then its name and version.
///
/// The attributes keep their own keys: their aliases aren't mapped, and `hostname` and `service` aren't renamed. The
/// name and version fill in only the keys that are still missing.
fn apply_scope(scope: &otlp_common::InstrumentationScope, properties: &mut Properties) {
    for kv in &scope.attributes {
        if let Some(av) = kv.value.as_ref() {
            let val = match av.value.as_ref() {
                Some(OtlpStringValue(s)) => JsonValue::String(s.clone()),
                _ => any_value_to_json(av),
            };
            if !val.is_null() {
                properties.insert(kv.key.as_str(), val);
            }
        }
    }
    for (key, value) in [(OTEL_SCOPE_NAME, &scope.name), (OTEL_SCOPE_VERSION, &scope.version)] {
        if !value.is_empty() {
            properties.insert_static_if_missing(key, value);
        }
    }
}

/// Sets the trace and span IDs of the record, overwriting the IDs from its attributes.
fn apply_trace_context(lr: &LogRecord, properties: &mut Properties) {
    if lr.trace_id.iter().any(|&b| b != 0) {
        insert_trace_id(properties, bytes_to_hex_lowercase(&lr.trace_id), &lr.trace_id);
    }
    if lr.span_id.iter().any(|&b| b != 0) {
        insert_span_id(properties, bytes_to_hex_lowercase(&lr.span_id), &lr.span_id);
    }
}

/// Sets `otel.trace_id` to the hex form of a trace ID, and `dd.trace_id` to the decimal form of its last 8 bytes.
fn insert_trace_id(properties: &mut Properties, hex: String, bytes: &[u8]) {
    properties.insert_static(OTEL_TRACE_ID, hex);
    properties.insert_static(DD_TRACE_ID, u64_from_last_8(bytes).to_string());
}

/// Sets `otel.span_id` to the hex form of a span ID, and `dd.span_id` to the decimal form of its first 8 bytes.
fn insert_span_id(properties: &mut Properties, hex: String, bytes: &[u8]) {
    properties.insert_static(OTEL_SPAN_ID, hex);
    properties.insert_static(DD_SPAN_ID, u64_from_first_8(bytes).to_string());
}

/// Sets the severity text and number of the record, when they're set.
fn apply_severity(lr: &LogRecord, properties: &mut Properties) {
    if !lr.severity_text.is_empty() {
        properties.insert_static(OTEL_SEVERITY_TEXT, lr.severity_text.clone());
    }
    if lr.severity_number != 0 {
        properties.insert_static(OTEL_SEVERITY_NUMBER, lr.severity_number.to_string());
    }
}

/// Sets the timestamp of the record, in nanoseconds and in the Go layout `2006-01-02T15:04:05.000Z07:00`, in UTC.
///
/// The observed timestamp is the fallback when the timestamp isn't set. Nanoseconds are read as signed, so a
/// timestamp past `i64::MAX` wraps to before 1970.
fn apply_timestamp(lr: &LogRecord, properties: &mut Properties) {
    let timestamp = if lr.time_unix_nano != 0 {
        lr.time_unix_nano
    } else {
        lr.observed_time_unix_nano
    };
    if timestamp == 0 {
        return;
    }
    let nanos = timestamp as i64;
    properties.insert_static(OTEL_TIMESTAMP, nanos.to_string());
    let instant = Timestamp::from_unix_nanos(i128::from(nanos))
        .expect("any signed 64-bit count of nanoseconds should be a valid timestamp");
    properties.insert_static(
        DD_TIMESTAMP,
        instant.format(TimestampFormat::Rfc3339UtcMillis).to_string(),
    );
}

/// Returns the message: the last message attribute, or the body when that attribute is missing or empty.
fn message(attribute: Option<&str>, body: Option<&otlp_common::AnyValue>) -> MetaString {
    match attribute {
        Some(message) if !message.is_empty() => MetaString::from(message),
        _ => body
            .map(any_value_to_message_string)
            .map(MetaString::from)
            .unwrap_or_default(),
    }
}

/// Derives a log's status from its status attributes, severity text, and severity number.
///
/// The status is the last status attribute (`status`, `severity`, `level`, or `syslog.severity`), then the severity
/// text, then the name of the severity number's range, taking the first that isn't empty. Attribute and severity
/// text are used as they are, without normalization. A severity number of 0 has no name, so a log with nothing else
/// gets an empty status.
pub(crate) fn derive_status(status: Option<&str>, severity_text: &str, severity_number: i32) -> MetaString {
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
fn status_from_severity_number(severity_number: i32) -> &'static str {
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

/// Returns the last 8 bytes of a trace ID as a big-endian integer. A shorter ID is padded with leading zeros.
fn u64_from_last_8(bytes: &[u8]) -> u64 {
    let n = bytes.len();
    let slice = if n >= 8 { &bytes[n - 8..n] } else { bytes };
    let mut buf = [0u8; 8];
    let start = 8 - slice.len();
    buf[start..].copy_from_slice(slice);
    u64::from_be_bytes(buf)
}

/// Returns the first 8 bytes of a span ID as a big-endian integer. A shorter ID is padded with leading zeros.
fn u64_from_first_8(bytes: &[u8]) -> u64 {
    let slice = if bytes.len() >= 8 { &bytes[..8] } else { bytes };
    let mut buf = [0u8; 8];
    let start = 8 - slice.len();
    buf[start..].copy_from_slice(slice);
    u64::from_be_bytes(buf)
}

/// Decodes a hex string of exactly `expected_len_bytes` bytes.
fn decode_hex_exact_to_bytes(s: &str, expected_len_bytes: usize) -> Option<Vec<u8>> {
    if s.len() != expected_len_bytes * 2 {
        return None;
    }
    let mut out = vec![0u8; expected_len_bytes];
    faster_hex::hex_decode(s.as_bytes(), &mut out).ok()?;
    Some(out)
}

/// Returns whether `key` is one of the `reserved` keys, ignoring ASCII case.
///
/// Only ASCII letters are folded. Go's `strings.ToLower` also turns the dotted `İ` (U+0130) into `i`, so a key such
/// as `TRACEİD` is reserved under Go's matching. We choose to ignore that edge case.
fn is_reserved(key: &str, reserved: &[&str]) -> bool {
    reserved.iter().any(|reserved| reserved.eq_ignore_ascii_case(key))
}

/// Returns an attribute's string value, or an empty string for any other value.
///
/// Converting other values to strings, as `pcommon.Value.AsString` does, is still missing. Until then, a non-string
/// message, status, or `ddtags` attribute counts as empty, and so does a non-string duplicate that comes after a string
/// one.
fn string_value_or_empty(kv: &otlp_common::KeyValue) -> &str {
    match kv.value.as_ref().and_then(|av| av.value.as_ref()) {
        Some(OtlpStringValue(s)) => s,
        _ => "",
    }
}

/// Maps a deprecated `otel.library.*` attribute to its canonical `otel.scope.*` key, unless the attribute's own
/// collection also has the canonical key, spelled exactly.
fn insert_scope_alias(
    properties: &mut Properties, collection: &[otlp_common::KeyValue], canonical: &'static str,
    kv: &otlp_common::KeyValue,
) {
    if collection.iter().any(|other| other.key == canonical) {
        return;
    }
    let value = kv.value.as_ref().map(any_value_to_message_string).unwrap_or_default();
    properties.insert_static(canonical, value);
}

/// Renders a value as the message: a string as it is, and any other value as JSON.
fn any_value_to_message_string(av: &otlp_common::AnyValue) -> String {
    match av.value.as_ref() {
        Some(OtlpStringValue(s)) => s.clone(),
        _ => serde_json::to_string(&any_value_to_json(av)).unwrap_or_else(|_| String::new()),
    }
}

/// Converts a value into JSON.
fn any_value_to_json(av: &otlp_common::AnyValue) -> JsonValue {
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

/// Flattens a map into dotted keys, up to a maximum depth.
///
/// Leaf strings, integers, Booleans, and doubles keep their types, and other leaf values become strings.
fn flatten_attribute(base_key: &str, av: &otlp_common::AnyValue, depth: usize) -> Vec<(String, JsonValue)> {
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

#[cfg(test)]
mod tests;
