//! Decoder for the Datadog v0.7 APM trace wire format, the MessagePack-encoded `TracerPayload` that tracers send to
//! `/v0.7/traces`.
//!
//! [`decode_v07_payload`] mirrors the reference receiver's `decodeConvertedTracerPayload` `V07` case
//! (`pkg/trace/api/api.go`): it decodes the body into the classic tracer payload model as the generated
//! `TracerPayload.UnmarshalMsg` does (`pkg/proto/pbgo/trace/tracer_payload_gen.go`, `span_gen.go`), then converts it
//! with [`convert_tracer_payload`] as `ConvertToIdx(payload, "v07-pb")` does. The payload's own container ID, tracer
//! language and versions are kept: unlike the other v0.x formats, v0.7 carries them in its body.
//!
//! # The generated decoder
//!
//! Every message is a map keyed by field name (`container_id`, `chunks`, `spans`, `span_links`, and so on). The
//! decoder is as strict or as tolerant as the reference, field by field:
//!
//! - A span's strings, IDs, times, error and `meta` and `metrics` entries go through the reference's `parseXBytes`
//!   helpers: `nil` reads as zero or empty, `str` and `bin` both read as strings, invalid UTF-8 is repaired one byte at
//!   a time, a signed integer is reinterpreted as unsigned, and a `float32` metric is a type error. A `nil` `meta` or
//!   `metrics` is an empty map. A span event's name, and a span link's trace and span IDs, are as tolerant.
//! - Every other field is read with the strict `msgp` readers: strings must be `str` (not `nil` or `bin`), integers
//!   must be in range, and `nil` is a type error. The bytes of a strict string aren't checked: invalid UTF-8 is
//!   repaired as above.
//! - Map keys may be `str` or `bin`. Unknown fields are skipped.
//! - Arrays and maps hold at most 500,000 elements, and `meta_struct` 10 MiB ones (`msgp.ErrLimitExceeded`). A count
//!   isn't checked against the bytes left: the input runs out first.
//! - `container_debug` is decoded and discarded.
//! - Bytes after the payload are ignored.
//! - Short input maps to [`DecodeError::ShortBytes`] wherever the reference returns `msgp.ErrShortBytes`.
//!
//! A field that appears twice in a message decodes as it does there: a scalar takes the last value, and a map the last
//! entries. A list of messages decodes into the elements an earlier occurrence left, merging into them, when it has
//! no more elements than were ever allocated for the field (`cap(z.Spans) >= n`), and into new ones otherwise.
//!
//! # Conversion
//!
//! `nil` chunks, spans, span links, span events, span event attributes and array elements are dropped, as
//! `ConvertToIdx` drops them. They are held until the payload is decoded, since a repeated field can still fill their
//! slot, at one pointer each. Chunks left without spans are dropped. A chunk without a `priority` has priority 0, not
//! none, since the field defaults to 0. See [`convert_tracer_payload`] for the rest.
//!
//! Unknown fields are skipped with a nesting limit of 64, where the reference's `msgp.Skip` allows 100,000.
//!
//! # Resolved string bytes
//!
//! The format has no string references: every string is spelled out where it is used, so its spans cost what their
//! bytes cost. The payload's own fields and tags are copied onto every trace, which the intake bounds once per trace.

use std::collections::HashMap;

use datadog_protos::traces::{
    AttributeAnyValue, AttributeArrayValue, Span as ClassicSpan, SpanEvent as ClassicSpanEvent,
    SpanLink as ClassicSpanLink, TraceChunk, TracerPayload,
};
use protobuf::{EnumOrUnknown, MessageField};

use super::error::DecodeError;
use super::read::{self, MSGP_MAX_COLLECTION_SIZE};
use super::{convert_tracer_payload, DecodedV1};

#[cfg(test)]
mod tests;

/// The `msgp:limit` of a span's `meta_struct` map and of each of its values.
const META_STRUCT_MAX_SIZE: u32 = 10 << 20;

/// Decodes a v0.7 MessagePack tracer payload into unified trace events.
///
/// Each chunk with spans becomes one [`Trace`](saluki_core::data_model::event::trace::Trace), with the payload's own
/// fields, as [`convert_tracer_payload`] converts it.
///
/// # Errors
///
/// Returns a [`DecodeError`] if the input is not a well-formed v0.7 payload. Input that ends early is
/// [`DecodeError::ShortBytes`].
pub fn decode_v07_payload(bytes: &[u8]) -> Result<DecodedV1, DecodeError> {
    let mut r = bytes;
    let mut payload = Payload::default();
    payload.decode(&mut r)?;
    Ok(convert_tracer_payload(payload.into_proto(), "v07-pb"))
}

/// A list of messages as a generated decoder holds it: `nil` or a message per slot, of which the first `len` are the
/// list. The slots past `len` are kept, as a Go slice keeps its capacity.
struct Slots<T> {
    slots: Vec<Option<Box<T>>>,
    len: usize,
}

impl<T> Default for Slots<T> {
    fn default() -> Self {
        Self {
            slots: Vec::new(),
            len: 0,
        }
    }
}

impl<T: Default> Slots<T> {
    /// Decodes a list of messages, as a generated decoder decodes a `[]*T` field.
    fn decode<F>(&mut self, r: &mut &[u8], context: &'static str, mut decode_into: F) -> Result<(), DecodeError>
    where
        F: FnMut(&mut T, &mut &[u8]) -> Result<(), DecodeError>,
    {
        let len = read::msgp_read_array_len(r, MSGP_MAX_COLLECTION_SIZE, context)? as usize;
        // `make([]*T, n)` when the slots allocated so far are too few, and `z[:n]` otherwise.
        let fresh = len > self.slots.len();
        if fresh {
            self.slots.clear();
            // Each element takes at least a byte, but a message takes far more memory than that: grow as they decode.
            self.slots.reserve(read::preallocation(len as u32).min(r.len()));
        }
        for index in 0..len {
            if fresh {
                self.slots.push(None);
            }
            let slot = &mut self.slots[index];
            if read::peek_is_nil(r) {
                read::skip_nil(r);
                *slot = None;
            } else {
                decode_into(slot.get_or_insert_with(Box::default), r)?;
            }
        }
        self.len = len;
        Ok(())
    }

    /// Returns the list's messages, without its `nil` entries.
    fn into_messages(self) -> impl Iterator<Item = T> {
        self.slots.into_iter().take(self.len).flatten().map(|message| *message)
    }
}

/// Reads a message's map header and decodes each field with `decode_field`, which is given the field's key and skips
/// fields it doesn't know with [`skip`].
fn decode_message<F>(r: &mut &[u8], context: &'static str, mut decode_field: F) -> Result<(), DecodeError>
where
    F: FnMut(&[u8], &mut &[u8]) -> Result<(), DecodeError>,
{
    let fields = read::msgp_read_map_len(r, MSGP_MAX_COLLECTION_SIZE, context)?;
    for _ in 0..fields {
        let key = read::v0_read_map_key(r, context)?;
        decode_field(key, r)?;
    }
    Ok(())
}

/// Skips an unknown field's value, as `msgp.Skip`.
fn skip(r: &mut &[u8]) -> Result<(), DecodeError> {
    read::skip_value(r, "unknown field")
}

/// Converts string bytes, repairing invalid UTF-8 as the reference's `parseStringBytes` does.
fn string(bytes: &[u8]) -> String {
    match simdutf8::basic::from_utf8(bytes) {
        Ok(s) => s.to_owned(),
        Err(_) => read::repair_utf8(bytes),
    }
}

/// Reads a string as `msgp.ReadStringBytes`.
fn strict_string(r: &mut &[u8], context: &'static str) -> Result<String, DecodeError> {
    read::msgp_read_str_bytes(r, context).map(string)
}

/// Reads a string as the reference's `parseStringBytes`.
fn tolerant_string(r: &mut &[u8], context: &'static str) -> Result<String, DecodeError> {
    read::v0_read_str_bytes(r, context).map(string)
}

/// The capacity to reserve for a map whose header declares `len` entries, each of which takes at least two bytes.
fn map_capacity(len: u32, r: &[u8]) -> usize {
    read::preallocation(len).min(r.len() / 2)
}

/// Reads a map of strings into `map`, replacing what it held, as a generated decoder decodes a `map[string]string`.
fn decode_string_map<K, V>(
    map: &mut HashMap<String, String>, r: &mut &[u8], context: &'static str, mut key: K, mut value: V,
) -> Result<(), DecodeError>
where
    K: FnMut(&mut &[u8]) -> Result<String, DecodeError>,
    V: FnMut(&mut &[u8]) -> Result<String, DecodeError>,
{
    let len = read::msgp_read_map_len(r, MSGP_MAX_COLLECTION_SIZE, context)?;
    map.clear();
    map.reserve(map_capacity(len, r));
    for _ in 0..len {
        let k = key(r)?;
        map.insert(k, value(r)?);
    }
    read::release_unused_capacity(map);
    Ok(())
}

/// Reads a `map[string]string` whose keys and values are strict strings.
fn decode_strict_string_map(
    map: &mut HashMap<String, String>, r: &mut &[u8], context: &'static str,
) -> Result<(), DecodeError> {
    decode_string_map(
        map,
        r,
        context,
        |r| strict_string(r, context),
        |r| strict_string(r, context),
    )
}

/// A `TracerPayload` as it decodes, with its chunks held as [`Slots`].
#[derive(Default)]
struct Payload {
    proto: TracerPayload,
    chunks: Slots<Chunk>,
}

impl Payload {
    fn decode(&mut self, r: &mut &[u8]) -> Result<(), DecodeError> {
        let p = &mut self.proto;
        let chunks = &mut self.chunks;
        decode_message(r, "tracer payload", |key, r| {
            match key {
                b"container_id" => p.containerID = strict_string(r, "tracer payload container_id")?,
                b"language_name" => p.languageName = strict_string(r, "tracer payload language_name")?,
                b"language_version" => p.languageVersion = strict_string(r, "tracer payload language_version")?,
                b"tracer_version" => p.tracerVersion = strict_string(r, "tracer payload tracer_version")?,
                b"runtime_id" => p.runtimeID = strict_string(r, "tracer payload runtime_id")?,
                b"chunks" => chunks.decode(r, "tracer payload chunks", Chunk::decode)?,
                b"tags" => decode_strict_string_map(&mut p.tags, r, "tracer payload tags")?,
                b"env" => p.env = strict_string(r, "tracer payload env")?,
                b"hostname" => p.hostname = strict_string(r, "tracer payload hostname")?,
                b"app_version" => p.appVersion = strict_string(r, "tracer payload app_version")?,
                b"container_debug" => {
                    if read::peek_is_nil(r) {
                        read::skip_nil(r);
                    } else {
                        decode_container_debug(r)?;
                    }
                }
                _ => skip(r)?,
            }
            Ok(())
        })
    }

    fn into_proto(self) -> TracerPayload {
        let mut proto = self.proto;
        proto.chunks = self.chunks.into_messages().map(Chunk::into_proto).collect();
        proto
    }
}

/// Decodes a `ContainerDebug`, which the conversion doesn't use, checking its fields as the reference does.
fn decode_container_debug(r: &mut &[u8]) -> Result<(), DecodeError> {
    decode_message(r, "container_debug", |key, r| {
        match key {
            b"error" | b"buffer_eviction_reason" => {
                read::msgp_read_str_bytes(r, "container_debug string")?;
            }
            b"latency_ms" | b"buffer_ms" => {
                read::v0_read_i64(r, "container_debug int")?;
            }
            b"was_buffered" => {
                read::read_bool(r, "container_debug was_buffered")?;
            }
            _ => skip(r)?,
        }
        Ok(())
    })
}

/// A `TraceChunk` as it decodes, with its spans held as [`Slots`].
#[derive(Default)]
struct Chunk {
    proto: TraceChunk,
    spans: Slots<SpanMessage>,
}

impl Chunk {
    fn decode(&mut self, r: &mut &[u8]) -> Result<(), DecodeError> {
        let c = &mut self.proto;
        let spans = &mut self.spans;
        decode_message(r, "trace chunk", |key, r| {
            match key {
                b"priority" => c.priority = read::v0_read_i32(r, "trace chunk priority")?,
                b"origin" => c.origin = strict_string(r, "trace chunk origin")?,
                b"spans" => spans.decode(r, "trace chunk spans", SpanMessage::decode)?,
                b"tags" => decode_strict_string_map(&mut c.tags, r, "trace chunk tags")?,
                b"dropped_trace" => c.droppedTrace = read::read_bool(r, "trace chunk dropped_trace")?,
                _ => skip(r)?,
            }
            Ok(())
        })
    }

    fn into_proto(self) -> TraceChunk {
        let mut proto = self.proto;
        proto.spans = self.spans.into_messages().map(SpanMessage::into_proto).collect();
        proto
    }
}

/// A `Span` as it decodes, with its links and events held as [`Slots`].
#[derive(Default)]
struct SpanMessage {
    proto: ClassicSpan,
    links: Slots<ClassicSpanLink>,
    events: Slots<ClassicSpanEvent>,
}

impl SpanMessage {
    fn decode(&mut self, r: &mut &[u8]) -> Result<(), DecodeError> {
        let s = &mut self.proto;
        let links = &mut self.links;
        let events = &mut self.events;
        decode_message(r, "span", |key, r| {
            match key {
                b"service" => s.service = tolerant_string(r, "span service")?,
                b"name" => s.name = tolerant_string(r, "span name")?,
                b"resource" => s.resource = tolerant_string(r, "span resource")?,
                b"trace_id" => s.traceID = read::v0_parse_u64(r, "span trace_id")?,
                b"span_id" => s.spanID = read::v0_parse_u64(r, "span span_id")?,
                b"parent_id" => s.parentID = read::v0_parse_u64(r, "span parent_id")?,
                b"start" => s.start = read::v0_parse_i64(r, "span start")?,
                b"duration" => s.duration = read::v0_parse_i64(r, "span duration")?,
                b"error" => s.error = read::v0_parse_i32(r, "span error")?,
                b"meta" => {
                    if read::peek_is_nil(r) {
                        read::skip_nil(r);
                        s.meta = HashMap::new();
                    } else {
                        let context = "span meta";
                        decode_string_map(
                            &mut s.meta,
                            r,
                            context,
                            |r| tolerant_string(r, context),
                            |r| tolerant_string(r, context),
                        )?;
                    }
                }
                b"metrics" => {
                    if read::peek_is_nil(r) {
                        read::skip_nil(r);
                        s.metrics = HashMap::new();
                    } else {
                        decode_metrics(&mut s.metrics, r)?;
                    }
                }
                b"type" => s.type_ = tolerant_string(r, "span type")?,
                b"meta_struct" => decode_meta_struct(&mut s.meta_struct, r)?,
                b"span_links" => links.decode(r, "span links", decode_span_link)?,
                b"span_events" => events.decode(r, "span events", decode_span_event)?,
                _ => skip(r)?,
            }
            Ok(())
        })
    }

    fn into_proto(self) -> ClassicSpan {
        let mut proto = self.proto;
        proto.spanLinks = self.links.into_messages().collect();
        proto.spanEvents = self.events.into_messages().collect();
        proto
    }
}

fn decode_metrics(metrics: &mut HashMap<String, f64>, r: &mut &[u8]) -> Result<(), DecodeError> {
    let len = read::msgp_read_map_len(r, MSGP_MAX_COLLECTION_SIZE, "span metrics")?;
    metrics.clear();
    metrics.reserve(map_capacity(len, r));
    for _ in 0..len {
        let key = tolerant_string(r, "span metrics")?;
        metrics.insert(key, read::v0_parse_f64(r, "span metrics")?);
    }
    read::release_unused_capacity(metrics);
    Ok(())
}

fn decode_meta_struct(meta_struct: &mut HashMap<String, Vec<u8>>, r: &mut &[u8]) -> Result<(), DecodeError> {
    let context = "span meta_struct";
    let len = read::msgp_read_map_len(r, META_STRUCT_MAX_SIZE, context)?;
    meta_struct.clear();
    meta_struct.reserve(map_capacity(len, r));
    for _ in 0..len {
        let key = tolerant_string(r, context)?;
        let value = read::msgp_read_bin_bytes(r, META_STRUCT_MAX_SIZE, context)?;
        meta_struct.insert(key, value.to_vec());
    }
    read::release_unused_capacity(meta_struct);
    Ok(())
}

fn decode_span_link(link: &mut ClassicSpanLink, r: &mut &[u8]) -> Result<(), DecodeError> {
    decode_message(r, "span link", |key, r| {
        match key {
            b"trace_id" => link.traceID = read::v0_parse_u64(r, "span link trace_id")?,
            b"trace_id_high" => link.traceID_high = read::v0_read_u64(r, "span link trace_id_high")?,
            b"span_id" => link.spanID = read::v0_parse_u64(r, "span link span_id")?,
            b"attributes" => decode_strict_string_map(&mut link.attributes, r, "span link attributes")?,
            b"tracestate" => link.tracestate = strict_string(r, "span link tracestate")?,
            b"flags" => link.flags = read::v0_read_u32(r, "span link flags")?,
            _ => skip(r)?,
        }
        Ok(())
    })
}

fn decode_span_event(event: &mut ClassicSpanEvent, r: &mut &[u8]) -> Result<(), DecodeError> {
    decode_message(r, "span event", |key, r| {
        match key {
            b"time_unix_nano" => event.time_unix_nano = read::v0_read_u64(r, "span event time_unix_nano")?,
            b"name" => event.name = tolerant_string(r, "span event name")?,
            b"attributes" => {
                let context = "span event attributes";
                let len = read::msgp_read_map_len(r, MSGP_MAX_COLLECTION_SIZE, context)?;
                let attributes = &mut event.attributes;
                attributes.clear();
                attributes.reserve(map_capacity(len, r));
                for _ in 0..len {
                    let key = strict_string(r, context)?;
                    // A `nil` value replaces the key's value with `nil`, which the conversion drops.
                    if read::peek_is_nil(r) {
                        read::skip_nil(r);
                        attributes.remove(&key);
                    } else {
                        attributes.insert(key, decode_any_value(r)?);
                    }
                }
                read::release_unused_capacity(attributes);
            }
            _ => skip(r)?,
        }
        Ok(())
    })
}

/// Decodes a span event attribute value, an `AttributeAnyValue`.
fn decode_any_value(r: &mut &[u8]) -> Result<AttributeAnyValue, DecodeError> {
    let mut value = AttributeAnyValue::default();
    let mut array: Option<Slots<AttributeArrayValue>> = None;
    decode_message(r, "attribute value", |key, r| {
        match key {
            b"type" => value.type_ = EnumOrUnknown::from_i32(read::v0_read_i32(r, "attribute value type")?),
            b"string_value" => value.string_value = strict_string(r, "attribute value string_value")?,
            b"bool_value" => value.bool_value = read::read_bool(r, "attribute value bool_value")?,
            b"int_value" => value.int_value = read::v0_read_i64(r, "attribute value int_value")?,
            b"double_value" => value.double_value = read::v0_read_f64(r, "attribute value double_value")?,
            b"array_value" => {
                if read::peek_is_nil(r) {
                    read::skip_nil(r);
                    array = None;
                } else {
                    let values = array.get_or_insert_with(Slots::default);
                    decode_message(r, "attribute array", |key, r| {
                        match key {
                            b"values" => values.decode(r, "attribute array values", decode_array_value)?,
                            _ => skip(r)?,
                        }
                        Ok(())
                    })?;
                }
            }
            _ => skip(r)?,
        }
        Ok(())
    })?;
    if let Some(array) = array {
        value.array_value = MessageField::some(datadog_protos::traces::AttributeArray {
            values: array.into_messages().collect(),
            ..Default::default()
        });
    }
    Ok(value)
}

fn decode_array_value(value: &mut AttributeArrayValue, r: &mut &[u8]) -> Result<(), DecodeError> {
    decode_message(r, "attribute array value", |key, r| {
        match key {
            b"type" => value.type_ = EnumOrUnknown::from_i32(read::v0_read_i32(r, "attribute array value type")?),
            b"string_value" => value.string_value = strict_string(r, "attribute array value string_value")?,
            b"bool_value" => value.bool_value = read::read_bool(r, "attribute array value bool_value")?,
            b"int_value" => value.int_value = read::v0_read_i64(r, "attribute array value int_value")?,
            b"double_value" => value.double_value = read::v0_read_f64(r, "attribute array value double_value")?,
            _ => skip(r)?,
        }
        Ok(())
    })
}
