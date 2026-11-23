//! Decoder for the Datadog v0.4 APM trace wire format, the MessagePack `[][]Span` body that tracers send to
//! `/v0.4/traces` (and to `/v0.3/traces` and `/v0.2/traces` with a MessagePack content type).
//!
//! [`decode_v04_payload`] decodes these bodies straight into the v1.0 model, as the trace-agent's receiver does. It
//! produces the same output as [`decode_v1_payload`](super::decode_v1_payload): one [`Trace`] per chunk, plus the
//! payload attributes.
//!
//! # Promoted fields
//!
//! Some span fields are promoted to the span, chunk, or payload as the spans are decoded:
//!
//! - `meta["env"]`, `meta["version"]` and `meta["component"]` set the span's `env`, `version` and `component`.
//! - `meta["span.kind"]` sets the span's kind. A span without one, or with an unknown one, is internal.
//! - `trace_id` and `meta["_dd.p.tid"]` (hexadecimal) set the low and high 64 bits of the chunk's trace ID.
//! - `meta["_dd.p.dm"]` (with or without a leading `-`) sets the chunk's sampling mechanism.
//! - `meta["_dd.origin"]` sets the chunk's origin.
//! - `metrics["_sampling_priority_v1"]` sets the chunk's priority. Once a root span (parent ID 0) has a priority, it
//!   wins over the spans after it.
//! - `meta["env"]`, `meta["version"]` and `meta["_dd.hostname"]` set the payload's env, app version and hostname,
//!   and `meta["_dd.git.commit.sha"]` and `meta["_dd.apm_mode"]` set payload attributes.
//!
//! A value that doesn't parse (`_dd.p.tid`, `_dd.p.dm`) is ignored. Every span also gets a `_dd.convertedv1: v04`
//! attribute.
//!
//! # Differences from the trace-agent's other v0.x conversion
//!
//! The trace-agent converts the other v0.x bodies (JSON, v0.1, v0.7) to the v1.0 model in a separate step, which
//! differs from this decoder in ways that are visible downstream. This decoder follows the trace-agent's v0.4
//! decoding:
//!
//! 1. Promoted tags (`env`, `version`, `component`, `span.kind`) stay in the span's attributes. The other conversion
//!    drops them.
//! 2. Promoted fields are taken from the *last* span that carries them in a chunk, and from the last chunk that
//!    carries them in a payload. An empty `env` or `version` value on a later span clears an earlier one within the
//!    chunk. The other conversion takes the first span in a chunk.
//! 3. Chunks without spans are kept: the receiver counts them, and the normalizer drops them. The other conversion
//!    drops them. `nil` spans are dropped by both.
//! 4. The chunk's trace ID comes from the last span with a `trace_id`, not the first.
//! 5. `meta` and `metrics` share one attribute map, so on a key clash the map that comes later on the wire wins. The
//!    other conversion always lets `metrics` win.
//! 6. An invalid `_dd.p.dm` is ignored, keeping an earlier span's mechanism, where the other conversion resets it to
//!    0. A `float32` metric is a type error. There are no chunk tags to fall back on for the priority or the
//!    mechanism.
//!
//! # Other trace-agent behaviors kept
//!
//! - Numbers are as tolerant as the trace-agent's: see the `v0_*` helpers in [`read`](super::read).
//! - Strings may be `str` or `bin`, and `nil` reads as the empty string. Invalid UTF-8 is repaired one byte at a time.
//! - A `nil` `trace_id` after a non-zero one in the same chunk is an error only when it is the last field of its span,
//!   because the trace-agent overwrites the error with the next field's.
//! - Bytes after the trace list are ignored.
//! - Short input maps to [`DecodeError::ShortBytes`] wherever the trace-agent reports short input, including a
//!   collection header that declares more entries than bytes remain.
//!
//! Unknown fields are skipped with a nesting limit of 64, where the trace-agent allows 100,000.
//!
//! # Performance
//!
//! Strings are interned per payload: short strings are inlined in the [`MetaString`] itself, and longer ones are
//! shared through one allocation per distinct value, as the trace-agent interns into its string table.

use std::sync::Arc;

use saluki_common::collections::FastHashMap;
use saluki_core::data_model::event::trace::{
    AttributeValue, PayloadFields, Span, SpanEvent, SpanLink, Trace, TraceIngestInfo,
};
use stringtheory::MetaString;

use super::error::DecodeError;
use super::read;
use super::DecodedV1;

#[cfg(test)]
mod tests;

/// No sampling priority, as the trace-agent marks it.
const PRIORITY_NONE: i32 = i8::MIN as i32;

/// The OTel span kinds, as `Span::kind` stores them.
const SPAN_KIND_INTERNAL: u32 = 1;
const SPAN_KIND_SERVER: u32 = 2;
const SPAN_KIND_CLIENT: u32 = 3;
const SPAN_KIND_PRODUCER: u32 = 4;
const SPAN_KIND_CONSUMER: u32 = 5;

/// Maximum span event attribute nesting depth, as in the trace-agent.
const MAX_ANY_VALUE_DEPTH: usize = 64;

const TAG_CONVERTED_V1: MetaString = MetaString::from_static("_dd.convertedv1");
const CONVERTED_V1_VALUE: MetaString = MetaString::from_static("v04");
const TAG_GIT_COMMIT_SHA: MetaString = MetaString::from_static("_dd.git.commit.sha");
const TAG_APM_MODE: MetaString = MetaString::from_static("_dd.apm_mode");
const KEY_SAMPLING_PRIORITY: &[u8] = b"_sampling_priority_v1";

/// Decodes a v0.4 MessagePack trace payload into unified trace events.
///
/// Each trace chunk becomes one [`Trace`], including chunks without spans. The payload's env, hostname and app
/// version are promoted from the spans; the other payload fields (container ID, tracer language and so on) come from
/// the request headers and are left for the caller to set.
///
/// # Errors
///
/// Returns a [`DecodeError`] if the input is not a well-formed v0.4 payload. Input that ends early is
/// [`DecodeError::ShortBytes`].
pub fn decode_v04_payload(bytes: &[u8]) -> Result<DecodedV1, DecodeError> {
    let mut r = bytes;
    let mut strings = Strings::default();

    let num_chunks = read::v0_read_array_len(&mut r, "trace list")?;
    let mut traces = Vec::with_capacity(read::preallocation(num_chunks));
    let mut promoted = PayloadPromoted::default();
    for _ in 0..num_chunks {
        traces.push(decode_chunk(&mut r, &mut strings, &mut promoted)?);
    }

    let payload = PayloadFields {
        env: promoted.env,
        hostname: promoted.hostname,
        app_version: promoted.app_version,
        ..PayloadFields::default()
    };
    for trace in &mut traces {
        trace.payload = payload.clone();
    }

    let mut payload_attributes = FastHashMap::default();
    if !promoted.git_commit_sha.is_empty() {
        payload_attributes.insert(TAG_GIT_COMMIT_SHA, AttributeValue::String(promoted.git_commit_sha));
    }
    if !promoted.apm_mode.is_empty() {
        payload_attributes.insert(TAG_APM_MODE, AttributeValue::String(promoted.apm_mode));
    }

    Ok(DecodedV1 {
        traces,
        payload_attributes,
    })
}

/// Per-payload string interner keyed by the raw bytes on the wire.
#[derive(Default)]
struct Strings<'a> {
    interned: FastHashMap<&'a [u8], MetaString>,
}

impl<'a> Strings<'a> {
    fn get(&mut self, bytes: &'a [u8]) -> MetaString {
        let utf8 = simdutf8::basic::from_utf8(bytes);
        if let Some(inlined) = utf8.ok().and_then(MetaString::try_inline) {
            return inlined;
        }
        if let Some(interned) = self.interned.get(bytes) {
            return interned.clone();
        }
        let owned: Arc<str> = match utf8 {
            Ok(s) => Arc::from(s),
            Err(_) => Arc::from(read::repair_utf8(bytes)),
        };
        let value = MetaString::from(owned);
        self.interned.insert(bytes, value.clone());
        value
    }

    fn read(&mut self, r: &mut &'a [u8], context: &'static str) -> Result<MetaString, DecodeError> {
        let bytes = read::v0_read_str_bytes(r, context)?;
        Ok(self.get(bytes))
    }
}

/// Fields promoted from a chunk's spans to the chunk.
struct ChunkPromoted {
    trace_id_low: u64,
    trace_id_high: u64,
    env: MetaString,
    app_version: MetaString,
    git_commit_sha: MetaString,
    hostname: MetaString,
    origin: MetaString,
    apm_mode: MetaString,
    sampling_mechanism: u32,
    sampling_priority: i32,
}

impl Default for ChunkPromoted {
    fn default() -> Self {
        Self {
            trace_id_low: 0,
            trace_id_high: 0,
            env: MetaString::empty(),
            app_version: MetaString::empty(),
            git_commit_sha: MetaString::empty(),
            hostname: MetaString::empty(),
            origin: MetaString::empty(),
            apm_mode: MetaString::empty(),
            sampling_mechanism: 0,
            sampling_priority: PRIORITY_NONE,
        }
    }
}

/// Fields promoted from the chunks to the payload.
#[derive(Default)]
struct PayloadPromoted {
    env: MetaString,
    app_version: MetaString,
    git_commit_sha: MetaString,
    hostname: MetaString,
    apm_mode: MetaString,
}

impl PayloadPromoted {
    /// Takes every non-empty field of a decoded chunk.
    fn apply(&mut self, chunk: &mut ChunkPromoted) {
        for (to, from) in [
            (&mut self.env, &mut chunk.env),
            (&mut self.app_version, &mut chunk.app_version),
            (&mut self.git_commit_sha, &mut chunk.git_commit_sha),
            (&mut self.hostname, &mut chunk.hostname),
            (&mut self.apm_mode, &mut chunk.apm_mode),
        ] {
            if !from.is_empty() {
                *to = std::mem::take(from);
            }
        }
    }
}

/// Root sampling state: once a root span has a priority, it overrides the later spans'.
#[derive(Default)]
struct RootPriority {
    priority: Option<i32>,
}

impl RootPriority {
    fn reconcile(&mut self, promoted: &mut ChunkPromoted, parent_id: u64) {
        if parent_id == 0 && promoted.sampling_priority != PRIORITY_NONE {
            self.priority = Some(promoted.sampling_priority);
        } else if let Some(priority) = self.priority {
            promoted.sampling_priority = priority;
        }
    }
}

/// Decodes one trace chunk, a list of spans.
fn decode_chunk<'a>(
    r: &mut &'a [u8], strings: &mut Strings<'a>, payload: &mut PayloadPromoted,
) -> Result<Trace, DecodeError> {
    let mut promoted = ChunkPromoted::default();
    let spans = decode_span_list(r, strings, &mut promoted)?;

    let mut trace = Trace::new(spans, TraceIngestInfo::Datadog);
    trace.trace_id_low = promoted.trace_id_low;
    trace.trace_id_high = promoted.trace_id_high;
    trace.sampling_mechanism = promoted.sampling_mechanism;
    trace.priority = (promoted.sampling_priority != PRIORITY_NONE).then_some(promoted.sampling_priority);
    trace.origin = std::mem::take(&mut promoted.origin);
    payload.apply(&mut promoted);
    Ok(trace)
}

/// Decodes a chunk's list of spans, dropping `nil` entries and reconciling the chunk priority after each span.
fn decode_span_list<'a>(
    r: &mut &'a [u8], strings: &mut Strings<'a>, promoted: &mut ChunkPromoted,
) -> Result<Vec<Span>, DecodeError> {
    let num_spans = read::v0_read_array_len(r, "span list")?;
    let mut spans = Vec::with_capacity(read::preallocation(num_spans));
    let mut root = RootPriority::default();
    let mut skipped_nil = false;
    for _ in 0..num_spans {
        if read::peek_is_nil(r) {
            read::skip_nil(r);
            skipped_nil = true;
            continue;
        }
        let span = decode_span(r, strings, promoted)?;
        root.reconcile(promoted, span.parent_id());
        spans.push(span);
    }
    // `nil` spans take one wire byte each, so the capacity reserved for them is released rather than kept.
    if skipped_nil {
        spans.shrink_to_fit();
    }
    Ok(spans)
}

/// Decodes one span, a map keyed by field name.
fn decode_span<'a>(
    r: &mut &'a [u8], strings: &mut Strings<'a>, promoted: &mut ChunkPromoted,
) -> Result<Span, DecodeError> {
    let mut service = MetaString::empty();
    let mut name = MetaString::empty();
    let mut resource = MetaString::empty();
    let mut span_type = MetaString::empty();
    let mut span_id = 0;
    let mut parent_id = 0;
    let mut start = 0;
    let mut duration = 0;
    let mut error = false;
    let mut attributes = FastHashMap::default();
    let mut links = Vec::new();
    let mut events = Vec::new();
    let mut fields = PromotedSpanFields::default();

    let num_fields = read::v0_read_map_len(r, "span")?;
    for field_index in 0..num_fields {
        match read::v0_read_map_key(r, "span field name")? {
            b"service" => service = strings.read(r, "span service")?,
            b"name" => name = strings.read(r, "span name")?,
            b"resource" => resource = strings.read(r, "span resource")?,
            b"type" => span_type = strings.read(r, "span type")?,
            b"trace_id" => {
                if read::peek_is_nil(r) {
                    read::skip_nil(r);
                    if promoted.trace_id_low != 0 && field_index + 1 == num_fields {
                        return Err(DecodeError::Msgpack {
                            context: "span trace_id",
                            detail: "already found lower 64 bits of trace ID but found a 0 traceID".to_string(),
                        });
                    }
                } else {
                    promoted.trace_id_low = read::v0_parse_u64(r, "span trace_id")?;
                }
            }
            b"span_id" => span_id = read::v0_parse_u64(r, "span span_id")?,
            b"parent_id" => parent_id = read::v0_parse_u64(r, "span parent_id")?,
            b"start" => start = read::v0_parse_i64(r, "span start")? as u64,
            b"duration" => duration = read::v0_parse_i64(r, "span duration")? as u64,
            b"error" => error = read::v0_parse_i32(r, "span error")? != 0,
            b"meta" => decode_meta(r, strings, &mut attributes, &mut fields, promoted)?,
            b"metrics" => decode_metrics(r, strings, &mut attributes, promoted)?,
            b"meta_struct" => decode_meta_struct(r, strings, &mut attributes)?,
            b"span_links" => decode_span_links(r, strings, &mut links)?,
            b"span_events" => decode_span_events(r, strings, &mut events)?,
            _ => read::skip_value(r, "span field")?,
        }
    }
    attributes.insert(TAG_CONVERTED_V1, AttributeValue::String(CONVERTED_V1_VALUE));

    Ok(Span::new(
        service,
        name,
        resource,
        span_type,
        span_id,
        parent_id,
        start,
        duration,
        i32::from(error),
    )
    .with_attributes(attributes)
    .with_span_links(links)
    .with_span_events(events)
    .with_env(fields.env)
    .with_version(fields.version)
    .with_component(fields.component)
    .with_kind(fields.kind))
}

/// Span fields promoted from `meta`.
struct PromotedSpanFields {
    env: MetaString,
    version: MetaString,
    component: MetaString,
    kind: u32,
}

impl Default for PromotedSpanFields {
    fn default() -> Self {
        Self {
            env: MetaString::empty(),
            version: MetaString::empty(),
            component: MetaString::empty(),
            kind: SPAN_KIND_INTERNAL,
        }
    }
}

/// Decodes a span's `meta` map into its attributes, applying the promoted fields.
fn decode_meta<'a>(
    r: &mut &'a [u8], strings: &mut Strings<'a>, attributes: &mut FastHashMap<MetaString, AttributeValue>,
    fields: &mut PromotedSpanFields, promoted: &mut ChunkPromoted,
) -> Result<(), DecodeError> {
    if read::peek_is_nil(r) {
        read::skip_nil(r);
        return Ok(());
    }
    let len = read::v0_read_map_len(r, "span meta")?;
    if len == 0 {
        return Ok(());
    }
    read::v0_check_slab_count(len, r, "span meta")?;
    attributes.reserve(read::preallocation(len));
    for _ in 0..len {
        let key_bytes = read::v0_read_str_bytes(r, "span meta key")?;
        let value_bytes = read::v0_read_str_bytes(r, "span meta value")?;
        let value = strings.get(value_bytes);
        match key_bytes {
            b"_dd.p.tid" => {
                if let Some(high) = parse_unsigned(value_bytes, 16) {
                    promoted.trace_id_high = high;
                }
            }
            b"env" => {
                fields.env = value.clone();
                promoted.env = value.clone();
            }
            b"version" => {
                fields.version = value.clone();
                promoted.app_version = value.clone();
            }
            b"component" => fields.component = value.clone(),
            b"span.kind" => fields.kind = span_kind(value_bytes),
            b"_dd.git.commit.sha" => promoted.git_commit_sha = value.clone(),
            b"_dd.p.dm" => {
                let mechanism = value_bytes.strip_prefix(b"-").unwrap_or(value_bytes);
                if let Some(mechanism) = parse_unsigned(mechanism, 10).and_then(|m| u32::try_from(m).ok()) {
                    promoted.sampling_mechanism = mechanism;
                }
            }
            b"_dd.hostname" => promoted.hostname = value.clone(),
            b"_dd.origin" => promoted.origin = value.clone(),
            b"_dd.apm_mode" => promoted.apm_mode = value.clone(),
            _ => {}
        }
        attributes.insert(strings.get(key_bytes), AttributeValue::String(value));
    }
    Ok(())
}

/// Decodes a span's `metrics` map into its attributes, taking `_sampling_priority_v1` as the chunk priority.
fn decode_metrics<'a>(
    r: &mut &'a [u8], strings: &mut Strings<'a>, attributes: &mut FastHashMap<MetaString, AttributeValue>,
    promoted: &mut ChunkPromoted,
) -> Result<(), DecodeError> {
    if read::peek_is_nil(r) {
        read::skip_nil(r);
        return Ok(());
    }
    let len = read::v0_read_map_len(r, "span metrics")?;
    if len == 0 {
        return Ok(());
    }
    read::v0_check_slab_count(len, r, "span metrics")?;
    attributes.reserve(read::preallocation(len));
    for _ in 0..len {
        let key_bytes = read::v0_read_str_bytes(r, "span metrics key")?;
        let value = read::v0_parse_f64(r, "span metrics value")?;
        if key_bytes == KEY_SAMPLING_PRIORITY {
            // Out-of-range values saturate here, where the trace-agent's conversion is platform-dependent.
            promoted.sampling_priority = value as i32;
        }
        attributes.insert(strings.get(key_bytes), AttributeValue::Float(value));
    }
    Ok(())
}

/// Decodes a span's `meta_struct` map, whose values are opaque `bin` blobs, into its attributes. Unlike `meta` and
/// `metrics`, a `nil` map is an error, as it is in the trace-agent.
fn decode_meta_struct<'a>(
    r: &mut &'a [u8], strings: &mut Strings<'a>, attributes: &mut FastHashMap<MetaString, AttributeValue>,
) -> Result<(), DecodeError> {
    let len = read::v0_read_map_len(r, "span meta_struct")?;
    if len == 0 {
        return Ok(());
    }
    read::v0_check_slab_count(len, r, "span meta_struct")?;
    attributes.reserve(read::preallocation(len));
    for _ in 0..len {
        let key = strings.read(r, "span meta_struct key")?;
        let value = read::read_bytes(r, "span meta_struct value")?;
        attributes.insert(key, AttributeValue::Bytes(value));
    }
    Ok(())
}

/// Decodes a span's `span_links` list, dropping `nil` entries. A later `span_links` field replaces an earlier one.
fn decode_span_links<'a>(
    r: &mut &'a [u8], strings: &mut Strings<'a>, links: &mut Vec<SpanLink>,
) -> Result<(), DecodeError> {
    let len = read::v0_read_array_len(r, "span links")?;
    *links = Vec::with_capacity(read::preallocation(len));
    let mut skipped_nil = false;
    for _ in 0..len {
        if read::peek_is_nil(r) {
            read::skip_nil(r);
            skipped_nil = true;
            continue;
        }
        links.push(decode_span_link(r, strings)?);
    }
    if skipped_nil {
        links.shrink_to_fit();
    }
    Ok(())
}

/// Decodes one span link.
fn decode_span_link<'a>(r: &mut &'a [u8], strings: &mut Strings<'a>) -> Result<SpanLink, DecodeError> {
    let mut trace_id_low = 0;
    let mut trace_id_high = 0;
    let mut span_id = 0;
    let mut attributes = FastHashMap::default();
    let mut tracestate = MetaString::empty();
    let mut flags = 0;

    let num_fields = read::v0_read_map_len(r, "span link")?;
    for _ in 0..num_fields {
        match read::v0_read_map_key(r, "span link field name")? {
            b"trace_id" => trace_id_low = read::v0_parse_u64(r, "span link trace_id")?,
            b"trace_id_high" => trace_id_high = read::v0_read_u64(r, "span link trace_id_high")?,
            b"span_id" => span_id = read::v0_parse_u64(r, "span link span_id")?,
            b"attributes" => {
                let len = read::v0_read_map_len(r, "span link attributes")?;
                read::v0_check_slab_count(len, r, "span link attributes")?;
                attributes.reserve(read::preallocation(len));
                for _ in 0..len {
                    let key = strings.read(r, "span link attribute key")?;
                    let value = strings.read(r, "span link attribute value")?;
                    attributes.insert(key, AttributeValue::String(value));
                }
            }
            b"tracestate" => tracestate = strings.read(r, "span link tracestate")?,
            b"flags" => flags = read::v0_read_u32(r, "span link flags")?,
            _ => read::skip_value(r, "span link field")?,
        }
    }

    Ok(SpanLink::new(trace_id_low, span_id)
        .with_trace_id_high(trace_id_high)
        .with_attributes(attributes)
        .with_tracestate(tracestate)
        .with_flags(flags))
}

/// Decodes a span's `span_events` list, dropping `nil` entries. A later `span_events` field replaces an earlier one.
fn decode_span_events<'a>(
    r: &mut &'a [u8], strings: &mut Strings<'a>, events: &mut Vec<SpanEvent>,
) -> Result<(), DecodeError> {
    let len = read::v0_read_array_len(r, "span events")?;
    *events = Vec::with_capacity(read::preallocation(len));
    let mut skipped_nil = false;
    for _ in 0..len {
        if read::peek_is_nil(r) {
            read::skip_nil(r);
            skipped_nil = true;
            continue;
        }
        events.push(decode_span_event(r, strings)?);
    }
    if skipped_nil {
        events.shrink_to_fit();
    }
    Ok(())
}

/// Decodes one span event.
fn decode_span_event<'a>(r: &mut &'a [u8], strings: &mut Strings<'a>) -> Result<SpanEvent, DecodeError> {
    let mut time = 0;
    let mut name = MetaString::empty();
    let mut attributes = FastHashMap::default();

    let num_fields = read::v0_read_map_len(r, "span event")?;
    for _ in 0..num_fields {
        match read::v0_read_map_key(r, "span event field name")? {
            b"time_unix_nano" => time = read::v0_read_u64(r, "span event time_unix_nano")?,
            b"name" => name = strings.read(r, "span event name")?,
            b"attributes" => {
                let len = read::v0_read_map_len(r, "span event attributes")?;
                read::v0_check_slab_count(len, r, "span event attributes")?;
                attributes.reserve(read::preallocation(len));
                for _ in 0..len {
                    let key = strings.read(r, "span event attribute key")?;
                    if read::peek_is_nil(r) {
                        read::skip_nil(r);
                        continue;
                    }
                    if let Some(value) = decode_any_value(r, strings, 0)? {
                        attributes.insert(key, value);
                    }
                }
            }
            _ => read::skip_value(r, "span event field")?,
        }
    }

    Ok(SpanEvent::new(time, name).with_attributes(attributes))
}

/// Decodes a span event attribute value, a map holding a `type` and the matching `*_value` field.
///
/// Returns `None` for an unknown type. The trace-agent keeps such a value as an empty value, which has no equivalent
/// here.
fn decode_any_value<'a>(
    r: &mut &'a [u8], strings: &mut Strings<'a>, depth: usize,
) -> Result<Option<AttributeValue>, DecodeError> {
    if depth > MAX_ANY_VALUE_DEPTH {
        return Err(DecodeError::DepthExceeded {
            max: MAX_ANY_VALUE_DEPTH,
        });
    }

    let mut value_type = 0;
    let mut string_value = MetaString::empty();
    let mut bool_value = false;
    let mut int_value = 0;
    let mut double_value = 0.0;
    let mut array_value = Vec::new();

    let num_fields = read::v0_read_map_len(r, "span event attribute")?;
    for _ in 0..num_fields {
        match read::v0_read_map_key(r, "span event attribute field name")? {
            b"type" => value_type = read::v0_parse_i32(r, "span event attribute type")?,
            b"string_value" => string_value = strings.read(r, "span event attribute string_value")?,
            b"bool_value" => bool_value = read::read_bool(r, "span event attribute bool_value")?,
            b"int_value" => int_value = read::v0_read_i64(r, "span event attribute int_value")?,
            b"double_value" => double_value = read::v0_read_f64(r, "span event attribute double_value")?,
            b"array_value" => {
                array_value.clear();
                if read::peek_is_nil(r) {
                    read::skip_nil(r);
                    continue;
                }
                let num_array_fields = read::v0_read_map_len(r, "span event attribute array_value")?;
                for _ in 0..num_array_fields {
                    if read::v0_read_map_key(r, "span event attribute array_value field name")? != b"values" {
                        read::skip_value(r, "span event attribute array_value field")?;
                        continue;
                    }
                    let len = read::v0_read_array_len(r, "span event attribute array values")?;
                    array_value.clear();
                    for _ in 0..len {
                        if read::peek_is_nil(r) {
                            read::skip_nil(r);
                            continue;
                        }
                        if let Some(value) = decode_any_value(r, strings, depth + 1)? {
                            array_value.push(value);
                        }
                    }
                }
            }
            _ => read::skip_value(r, "span event attribute field")?,
        }
    }

    Ok(match value_type {
        0 => Some(AttributeValue::String(string_value)),
        1 => Some(AttributeValue::Bool(bool_value)),
        2 => Some(AttributeValue::Int(int_value)),
        3 => Some(AttributeValue::Float(double_value)),
        4 => Some(AttributeValue::Array(array_value)),
        _ => None,
    })
}

/// Maps a `span.kind` value to the span kind, defaulting to internal as the trace-agent does.
fn span_kind(value: &[u8]) -> u32 {
    match value {
        b"server" => SPAN_KIND_SERVER,
        b"client" => SPAN_KIND_CLIENT,
        b"producer" => SPAN_KIND_PRODUCER,
        b"consumer" => SPAN_KIND_CONSUMER,
        _ => SPAN_KIND_INTERNAL,
    }
}

/// Parses an unsigned 64-bit integer in the given radix, rejecting a sign as the trace-agent does.
fn parse_unsigned(value: &[u8], radix: u32) -> Option<u64> {
    let value = simdutf8::basic::from_utf8(value).ok()?;
    if value.starts_with('+') {
        return None;
    }
    u64::from_str_radix(value, radix).ok()
}
