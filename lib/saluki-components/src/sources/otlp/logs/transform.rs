//! Transforms OTLP log records into logs.
//!
//! [`transform_log_record`] follows `transform` in the Agent's `pkg/opentelemetry-mapping-go/otlp/logs/transform.go`,
//! phase by phase. Each phase writes [`Properties`], so a later phase overwrites the keys of an earlier one.

use std::borrow::Cow;
use std::collections::HashMap;

use otlp_common::any_value::Value as OtlpValue;
use otlp_common::any_value::Value::StringValue as OtlpStringValue;
use otlp_common::any_value::Value::{ArrayValue, BoolValue, DoubleValue, IntValue, KvlistValue};
use otlp_protos::opentelemetry::proto::common::v1 as otlp_common;
use otlp_protos::opentelemetry::proto::logs::v1::LogRecord;
use otlp_protos::opentelemetry::proto::resource::v1::Resource;
use saluki_common::time::{Timestamp, TimestampFormat};
use saluki_core::data_model::{event::log::Log, tags::TagSet};
use saluki_error::{generic_error, GenericError};
use serde_json::Value as JsonValue;
use stringtheory::MetaString;

use crate::common::otlp::traces::transform::bytes_to_hex_lowercase;
use crate::common::otlp::value::otlp_value_as_string;
use crate::sources::otlp::logs::ddtags::{record_ddtags, RecordDdtags};

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
const DD_TIMESTAMP: &str = "@timestamp";

static DEFAULT_SOURCE: MetaString = MetaString::from_static("otlp_log_ingestion");
const LOG_SOURCE_ATTR: &str = "datadog.log.source";

/// Transforms an OTLP log record, with its resource and scope, into a log.
///
/// The message, status, `ddtags`, and trace and span ID attributes, the `otel.library.*` aliases, the resource and scope
/// attributes, and the body become strings. Other record attributes keep their types, as [`Flattener`] describes.
///
/// The source is the final `datadog.log.source` property when it's a non-empty string, and `otlp_log_ingestion`
/// otherwise, as the Agent's logs agent exporter chooses it. The property stays in the content.
///
/// `infra_ddtags` holds the resource's diverted infra tags. They become tags as if they were merged into the record's
/// `ddtags` first, as [`record_ddtags`] describes.
///
/// # Errors
///
/// Returns an error when a record attribute holds a NaN or an infinite double that no later property overwrites. The
/// caller drops the log, as the Agent drops a log that it fails to marshal.
pub(super) fn transform_log_record(
    lr: LogRecord, resource: &Resource, scope: Option<&otlp_common::InstrumentationScope>,
    record_host: Option<MetaString>, record_service: Option<MetaString>, mut tags: TagSet, infra_ddtags: Option<&str>,
) -> Result<Log, GenericError> {
    let mut properties = Properties::default();
    let mut flattener = Flattener::default();

    // Resource attributes overwrite record attributes, and scope attributes overwrite both.
    let reserved = apply_record_attributes(&lr.attributes, &mut flattener, &mut properties);
    let (record_ddtags, infra_ddtags) = record_ddtags(reserved.ddtags, infra_ddtags);
    for ddtags in record_ddtags.as_deref().into_iter().chain(infra_ddtags) {
        add_ddtags(&mut tags, ddtags);
    }
    apply_resource_attributes(&resource.attributes, &mut properties);
    if let Some(scope) = scope {
        apply_scope(scope, &mut properties);
    }
    apply_trace_context(&lr, &mut properties);

    let status = derive_status(reserved.status.as_deref(), &lr.severity_text, lr.severity_number);
    // The Agent sends the derived status in place of any resource or scope attribute named `status`. The encoder
    // writes additional properties over the status, so the attribute must not be among them.
    properties.remove("status");

    apply_severity(&lr, &mut properties);
    apply_timestamp(&lr, &mut properties);
    // The event name overwrites an `otel.event_name` attribute from any source.
    if !lr.event_name.is_empty() {
        properties.insert_static(OTEL_EVENT_NAME, lr.event_name);
    }

    let message = message(reserved.message, lr.body.as_ref());

    // A non-finite double is still a placeholder only if no later property overwrote it.
    if flattener.wrote_non_finite {
        if let Some(key) = properties.non_finite_placeholder() {
            return Err(generic_error!(
                "Log attribute '{}' holds a NaN or infinite double, which JSON can't represent.",
                key
            ));
        }
    }

    let source = match properties.get(LOG_SOURCE_ATTR) {
        Some(JsonValue::String(source)) if !source.is_empty() => MetaString::from(source.as_str()),
        _ => DEFAULT_SOURCE.clone(),
    };

    Ok(Log::new(message)
        .with_status(status)
        .with_source(source)
        .with_hostname(record_host)
        .with_service(record_service)
        .with_tags(tags)
        .with_additional_properties(properties.into_map()))
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

    fn get(&self, key: &str) -> Option<&JsonValue> {
        self.0.get(key)
    }

    fn contains(&self, key: &str) -> bool {
        self.0.contains_key(key)
    }

    fn remove(&mut self, key: &str) {
        self.0.remove(key);
    }

    fn non_finite_placeholder(&self) -> Option<&MetaString> {
        self.0
            .iter()
            .find(|(_, value)| holds_non_finite_placeholder(value))
            .map(|(key, _)| key)
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
    message: Option<Cow<'a, str>>,
    status: Option<Cow<'a, str>>,
    ddtags: RecordDdtags<'a>,
}

/// Sets the properties of the record attributes, and returns the reserved ones.
///
/// Reserved keys match in any case, and every other key keeps its original spelling. The first trace and span IDs
/// that decode win.
fn apply_record_attributes<'a>(
    attributes: &'a [otlp_common::KeyValue], flattener: &mut Flattener, properties: &mut Properties,
) -> ReservedAttributes<'a> {
    let mut reserved = ReservedAttributes::default();
    for (index, kv) in attributes.iter().enumerate() {
        match kv.key.as_str() {
            k if is_reserved(k, MESSAGE_KEYS) => reserved.message = Some(as_string(kv)),
            k if is_reserved(k, STATUS_KEYS) => reserved.status = Some(as_string(kv)),
            k if is_reserved(k, TRACE_ID_ATTR_KEYS) => {
                if !properties.contains(DD_TRACE_ID) {
                    let trace_hex = as_string(kv);
                    if let Some(bytes) = decode_hex_exact_to_bytes(&trace_hex, 16) {
                        insert_trace_id(properties, trace_hex.into_owned(), &bytes);
                    }
                }
            }
            k if is_reserved(k, SPAN_ID_ATTR_KEYS) => {
                if !properties.contains(DD_SPAN_ID) {
                    let span_hex = as_string(kv);
                    if let Some(bytes) = decode_hex_exact_to_bytes(&span_hex, 8) {
                        insert_span_id(properties, span_hex.into_owned(), &bytes);
                    }
                }
            }
            k if k.eq_ignore_ascii_case(DDTAGS_ATTR) => {
                if k == DDTAGS_ATTR && reserved.ddtags.first_exact.is_none() {
                    reserved.ddtags.first_exact = Some(index);
                }
                reserved.ddtags.last = Some((index, as_string(kv)));
            }
            k if k.eq_ignore_ascii_case(OTEL_LIBRARY_NAME) => {
                insert_scope_alias(properties, attributes, OTEL_SCOPE_NAME, kv)
            }
            k if k.eq_ignore_ascii_case(OTEL_LIBRARY_VERSION) => {
                insert_scope_alias(properties, attributes, OTEL_SCOPE_VERSION, kv)
            }
            _ => flattener.flatten(Cow::Borrowed(kv.key.as_str()), attribute_value(kv), 1, properties),
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
                let value = JsonValue::String(as_string(kv).into_owned());
                match key {
                    "hostname" => properties.insert(MetaString::from_static(OTEL_HOSTNAME), value),
                    "service" => properties.insert(MetaString::from_static(OTEL_SERVICE), value),
                    _ => properties.insert(key, value),
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
        properties.insert(kv.key.as_str(), JsonValue::String(as_string(kv).into_owned()));
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
fn message(attribute: Option<Cow<'_, str>>, body: Option<&otlp_common::AnyValue>) -> MetaString {
    match attribute {
        Some(message) if !message.is_empty() => MetaString::from(message),
        _ => MetaString::from(otlp_value_as_string(body.and_then(|body| body.value.as_ref()))),
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

/// Maps a deprecated `otel.library.*` attribute to its canonical `otel.scope.*` key, unless the attribute's own
/// collection also has the canonical key, spelled exactly.
fn insert_scope_alias(
    properties: &mut Properties, collection: &[otlp_common::KeyValue], canonical: &'static str,
    kv: &otlp_common::KeyValue,
) {
    if collection.iter().any(|other| other.key == canonical) {
        return;
    }
    properties.insert_static(canonical, as_string(kv).into_owned());
}

fn attribute_value(kv: &otlp_common::KeyValue) -> Option<&OtlpValue> {
    kv.value.as_ref().and_then(|av| av.value.as_ref())
}

fn as_string(kv: &otlp_common::KeyValue) -> Cow<'_, str> {
    otlp_value_as_string(attribute_value(kv))
}

/// The depth at which [`Flattener`] renders a map as a string instead of flattening it.
const FLATTEN_STRING_DEPTH: usize = 10;

/// Where [`Flattener`] writes the properties of a record attribute.
trait PropertySink {
    fn set(&mut self, key: String, value: JsonValue);
}

impl PropertySink for Properties {
    fn set(&mut self, key: String, value: JsonValue) {
        self.insert(key, value);
    }
}

impl PropertySink for serde_json::Map<String, JsonValue> {
    fn set(&mut self, key: String, value: JsonValue) {
        self.insert(key, value);
    }
}

/// Flattens record attributes as `flattenAttribute` in the Agent does.
///
/// - A map spreads its entries into dotted keys. Under an empty key, the entries keep their own keys.
/// - An array stays an array at any depth, as [`Flattener::array_element`] describes.
/// - Strings, integers, Booleans, and doubles keep their types. Bytes, empty values, and maps at depth 10 become
///   strings.
///
/// A map becomes a string only at exactly depth 10. An array at depth 10 moves its elements to depth 11, so the maps
/// below it flatten without a limit. This is a quirk of the Agent since it added array support
/// (DataDog/datadog-agent#45708): the array branch comes before the depth check, so depth can skip past 10. We copy it.
///
/// JSON can't hold a NaN or an infinite double, and the Agent fails to marshal a log that holds one. A non-finite
/// double becomes a `null` placeholder, and the flattener remembers that it wrote one, so the caller can drop the log
/// if the placeholder is still there once every property is set. No other value becomes `null`.
#[derive(Default)]
struct Flattener {
    wrote_non_finite: bool,
}

impl Flattener {
    /// Flattens a value under `key` into `out`. A record attribute starts at depth 1.
    fn flatten<S: PropertySink>(&mut self, key: Cow<'_, str>, value: Option<&OtlpValue>, depth: usize, out: &mut S) {
        match value {
            Some(KvlistValue(map)) if depth != FLATTEN_STRING_DEPTH => {
                for kv in &map.values {
                    let nested_key = if key.is_empty() {
                        Cow::Borrowed(kv.key.as_str())
                    } else {
                        Cow::Owned(format!("{}.{}", key, kv.key))
                    };
                    self.flatten(nested_key, attribute_value(kv), depth + 1, out);
                }
            }
            _ => {
                let value = self.value(value, depth);
                out.set(key.into_owned(), value);
            }
        }
    }

    /// Converts a value that doesn't spread into dotted keys.
    fn value(&mut self, value: Option<&OtlpValue>, depth: usize) -> JsonValue {
        match value {
            Some(ArrayValue(array)) => JsonValue::Array(
                array
                    .values
                    .iter()
                    .map(|element| self.array_element(element.value.as_ref(), depth + 1))
                    .collect(),
            ),
            Some(OtlpStringValue(s)) => JsonValue::String(s.clone()),
            Some(IntValue(i)) => JsonValue::from(*i),
            Some(BoolValue(b)) => JsonValue::Bool(*b),
            Some(DoubleValue(d)) => match serde_json::Number::from_f64(*d) {
                Some(number) => JsonValue::Number(number),
                None => {
                    self.wrote_non_finite = true;
                    JsonValue::Null
                }
            },
            other => JsonValue::String(otlp_value_as_string(other).into_owned()),
        }
    }

    /// Converts an element of an array. The element is one level deeper than its array.
    ///
    /// The Agent flattens each element under an empty key. An element whose result has an empty key becomes that
    /// value, and any other element becomes an object of its results. So a map that spreads becomes an object of its
    /// dotted keys, unless it has an empty key itself, and every other element becomes a single value.
    fn array_element(&mut self, element: Option<&OtlpValue>, depth: usize) -> JsonValue {
        match element {
            Some(map @ KvlistValue(_)) if depth != FLATTEN_STRING_DEPTH => {
                let mut flattened = serde_json::Map::new();
                self.flatten(Cow::Borrowed(""), Some(map), depth, &mut flattened);
                match flattened.remove("") {
                    Some(value) => value,
                    None => JsonValue::Object(flattened),
                }
            }
            other => self.value(other, depth),
        }
    }
}

fn holds_non_finite_placeholder(value: &JsonValue) -> bool {
    match value {
        JsonValue::Null => true,
        JsonValue::Array(values) => values.iter().any(holds_non_finite_placeholder),
        JsonValue::Object(map) => map.values().any(holds_non_finite_placeholder),
        _ => false,
    }
}

#[cfg(test)]
mod tests;
