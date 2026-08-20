//! Decoder for the Datadog v1.0 APM trace wire format (also called the `idx`/`etp` format).
//!
//! The v1.0 trace API (`Content-Type: application/msgpack`, endpoint `v1.0`) sends a tracer payload
//! in a custom MessagePack layout rather than protobuf-over-msgpack. Its defining traits:
//!
//! - A **streaming string table**: a string appears once as a literal, then by `uint32` index
//!   thereafter.
//! - **Structs are maps keyed by field number** (`uint32`), not by name.
//! - **Attribute maps are flat arrays** with three slots per entry (`key`, `type`, `value`).
//! - **`AnyValue`** is a `uint32` type discriminant followed by the value.
//!
//! [`decode_v1_payload`](datadog::decode_v1_payload) decodes one tracer payload into the unified
//! `Trace` model, producing one `Trace` per trace chunk. String references are resolved to owned
//! strings during decode.

use std::sync::Arc;

use saluki_common::collections::FastHashMap;
use saluki_core::data_model::event::trace::{AttributeValue, PayloadFields, Span, SpanEvent, SpanLink, Trace};
use stringtheory::MetaString;

mod error;
mod read;
mod string_table;
mod value;

#[cfg(test)]
mod tests;

pub use self::error::DecodeError;
use self::string_table::StringTable;

/// Decodes a single v1.0 tracer payload into unified trace events.
///
/// Each trace chunk in the payload becomes one [`Trace`]. Payload-level metadata (container ID,
/// tracer language, environment, and so on) is cloned onto every trace, and payload-level
/// attributes are merged into each trace's attributes with per-chunk attributes taking precedence.
///
/// # Errors
///
/// Returns a [`DecodeError`] if the input is not a well-formed v1.0 tracer payload (truncated,
/// malformed MessagePack, oversized headers, out-of-range string references, and so on).
pub fn decode_v1_payload(bytes: &[u8]) -> Result<Vec<Trace>, DecodeError> {
    let mut reader = bytes;
    let payload = decode_tracer_payload(&mut reader)?;

    let payload_fields = PayloadFields {
        container_id: payload.container_id,
        language_name: payload.language_name,
        language_version: payload.language_version,
        tracer_version: payload.tracer_version,
        runtime_id: payload.runtime_id,
        env: payload.env,
        hostname: payload.hostname,
        app_version: payload.app_version,
        client_dropped_p0s_weight: 0.0,
    };

    let mut traces = Vec::with_capacity(payload.chunks.len());
    for chunk in payload.chunks {
        // Payload-level attributes form the base; per-chunk attributes override on key conflict.
        let mut attributes = payload.attributes.clone();
        attributes.extend(chunk.attributes);

        let mut trace = Trace::new(chunk.spans);
        trace.trace_id_high = chunk.trace_id_high;
        trace.trace_id_low = chunk.trace_id_low;
        trace.origin = chunk.origin;
        trace.payload = payload_fields.clone();
        trace.attributes = Arc::new(attributes);
        trace.priority = chunk.priority;
        trace.dropped_trace = chunk.dropped_trace;
        trace.sampling_mechanism = chunk.sampling_mechanism;
        traces.push(trace);
    }

    Ok(traces)
}

/// Intermediate representation of a decoded tracer payload before chunks are lifted into `Trace`s.
struct DecodedPayload {
    container_id: MetaString,
    language_name: MetaString,
    language_version: MetaString,
    tracer_version: MetaString,
    runtime_id: MetaString,
    env: MetaString,
    hostname: MetaString,
    app_version: MetaString,
    attributes: FastHashMap<MetaString, AttributeValue>,
    chunks: Vec<DecodedChunk>,
}

/// Intermediate representation of a decoded trace chunk.
struct DecodedChunk {
    priority: Option<i32>,
    origin: MetaString,
    attributes: FastHashMap<MetaString, AttributeValue>,
    spans: Vec<Span>,
    dropped_trace: bool,
    trace_id_high: u64,
    trace_id_low: u64,
    sampling_mechanism: u32,
}

/// Splits a big-endian trace ID byte string into its high and low 64-bit halves.
///
/// IDs of 16 or more bytes use the first 16 bytes; shorter IDs are right-aligned (high-order bytes
/// treated as zero), matching the reference decoder's legacy-ID handling.
fn split_trace_id(id: &[u8]) -> (u64, u64) {
    let mut buf = [0u8; 16];
    if id.len() >= 16 {
        buf.copy_from_slice(&id[..16]);
    } else {
        buf[16 - id.len()..].copy_from_slice(id);
    }
    let high = u64::from_be_bytes(buf[0..8].try_into().unwrap());
    let low = u64::from_be_bytes(buf[8..16].try_into().unwrap());
    (high, low)
}

/// Decodes the top-level `TracerPayload` map.
fn decode_tracer_payload(r: &mut &[u8]) -> Result<DecodedPayload, DecodeError> {
    let mut strings = StringTable::new();
    let mut payload = DecodedPayload {
        container_id: MetaString::empty(),
        language_name: MetaString::empty(),
        language_version: MetaString::empty(),
        tracer_version: MetaString::empty(),
        runtime_id: MetaString::empty(),
        env: MetaString::empty(),
        hostname: MetaString::empty(),
        app_version: MetaString::empty(),
        attributes: FastHashMap::default(),
        chunks: Vec::new(),
    };

    let num_fields = read::read_map_len(r, "tracer payload")?;
    for _ in 0..num_fields {
        let field = read::read_u32(r, "tracer payload field")?;
        match field {
            1 => {
                // The string table must arrive first so later references resolve. Anything beyond
                // the seeded empty string means fields were decoded before it.
                if strings.len() > 1 {
                    return Err(DecodeError::StringsNotFirst);
                }
                decode_string_table(r, &mut strings)?;
            }
            2 => payload.container_id = value::read_streaming_string(r, &mut strings, "container ID")?,
            3 => payload.language_name = value::read_streaming_string(r, &mut strings, "language name")?,
            4 => payload.language_version = value::read_streaming_string(r, &mut strings, "language version")?,
            5 => payload.tracer_version = value::read_streaming_string(r, &mut strings, "tracer version")?,
            6 => payload.runtime_id = value::read_streaming_string(r, &mut strings, "runtime ID")?,
            7 => payload.env = value::read_streaming_string(r, &mut strings, "env")?,
            8 => payload.hostname = value::read_streaming_string(r, &mut strings, "hostname")?,
            9 => payload.app_version = value::read_streaming_string(r, &mut strings, "app version")?,
            10 => payload.attributes = value::read_attributes_map(r, &mut strings, "tracer payload attributes")?,
            11 => payload.chunks = decode_chunk_list(r, &mut strings)?,
            _ => value::harvest_unknown_field(r, &mut strings, "tracer payload field")?,
        }
    }

    Ok(payload)
}

/// Decodes the string table array (field 1 of the tracer payload).
///
/// Empty strings are skipped: index 0 is always the pre-seeded empty string, matching the reference
/// encoder which never emits duplicate or additional empty strings.
fn decode_string_table(r: &mut &[u8], strings: &mut StringTable) -> Result<(), DecodeError> {
    let num_strings = read::read_array_len(r, "string table")?;
    for _ in 0..num_strings {
        let s = read::read_str(r, "string table entry")?;
        if s.is_empty() {
            continue;
        }
        strings.add(s);
    }
    Ok(())
}

/// Decodes the list of trace chunks (field 11 of the tracer payload).
fn decode_chunk_list(r: &mut &[u8], strings: &mut StringTable) -> Result<Vec<DecodedChunk>, DecodeError> {
    let num_chunks = read::read_array_len(r, "trace chunk list")?;
    let mut chunks = Vec::with_capacity(num_chunks as usize);
    for _ in 0..num_chunks {
        chunks.push(decode_chunk(r, strings)?);
    }
    Ok(chunks)
}

/// Decodes a single `TraceChunk` map.
fn decode_chunk(r: &mut &[u8], strings: &mut StringTable) -> Result<DecodedChunk, DecodeError> {
    let mut priority = None;
    let mut origin = MetaString::empty();
    let mut attributes = FastHashMap::default();
    let mut spans = Vec::new();
    let mut dropped_trace = false;
    let mut trace_id_high = 0;
    let mut trace_id_low = 0;
    let mut sampling_mechanism = 0;

    let num_fields = read::read_map_len(r, "trace chunk")?;
    for _ in 0..num_fields {
        let field = read::read_u32(r, "trace chunk field")?;
        match field {
            1 => priority = Some(read::read_i32(r, "trace chunk priority")?),
            2 => origin = value::read_streaming_string(r, strings, "trace chunk origin")?,
            3 => attributes = value::read_attributes_map(r, strings, "trace chunk attributes")?,
            4 => spans = decode_span_list(r, strings)?,
            5 => dropped_trace = read::read_bool(r, "trace chunk droppedTrace")?,
            6 => {
                let id = read::read_bytes(r, "trace chunk traceID")?;
                (trace_id_high, trace_id_low) = split_trace_id(&id);
            }
            7 => sampling_mechanism = read::read_u32(r, "trace chunk samplingMechanism")?,
            _ => value::harvest_unknown_field(r, strings, "trace chunk field")?,
        }
    }

    Ok(DecodedChunk {
        priority,
        origin,
        attributes,
        spans,
        dropped_trace,
        trace_id_high,
        trace_id_low,
        sampling_mechanism,
    })
}

/// Decodes the list of spans within a trace chunk (field 4).
fn decode_span_list(r: &mut &[u8], strings: &mut StringTable) -> Result<Vec<Span>, DecodeError> {
    let num_spans = read::read_array_len(r, "span list")?;
    let mut spans = Vec::with_capacity(num_spans as usize);
    for _ in 0..num_spans {
        spans.push(decode_span(r, strings)?);
    }
    Ok(spans)
}

/// Decodes a single `Span` map.
fn decode_span(r: &mut &[u8], strings: &mut StringTable) -> Result<Span, DecodeError> {
    let mut service = MetaString::empty();
    let mut name = MetaString::empty();
    let mut resource = MetaString::empty();
    let mut span_id = 0;
    let mut parent_id = 0;
    let mut start = 0;
    let mut duration = 0;
    let mut error = 0;
    let mut attributes = FastHashMap::default();
    let mut span_type = MetaString::empty();
    let mut links = Vec::new();
    let mut events = Vec::new();
    let mut env = MetaString::empty();
    let mut version = MetaString::empty();
    let mut component = MetaString::empty();
    let mut kind = 0;

    let num_fields = read::read_map_len(r, "span")?;
    for _ in 0..num_fields {
        let field = read::read_u32(r, "span field")?;
        match field {
            1 => service = value::read_streaming_string(r, strings, "span service")?,
            2 => name = value::read_streaming_string(r, strings, "span name")?,
            3 => resource = value::read_streaming_string(r, strings, "span resource")?,
            4 => span_id = read::read_u64(r, "span spanID")?,
            5 => parent_id = read::read_u64(r, "span parentID")?,
            6 => start = read::read_u64(r, "span start")?,
            7 => duration = read::read_u64(r, "span duration")?,
            8 => error = i32::from(read::read_bool(r, "span error")?),
            9 => attributes = value::read_attributes_map(r, strings, "span attributes")?,
            10 => span_type = value::read_streaming_string(r, strings, "span type")?,
            11 => links = decode_span_link_list(r, strings)?,
            12 => events = decode_span_event_list(r, strings)?,
            13 => env = value::read_streaming_string(r, strings, "span env")?,
            14 => version = value::read_streaming_string(r, strings, "span version")?,
            15 => component = value::read_streaming_string(r, strings, "span component")?,
            16 => kind = read::read_u32(r, "span kind")?,
            _ => value::harvest_unknown_field(r, strings, "span field")?,
        }
    }

    Ok(Span::new(
        service, name, resource, span_type, span_id, parent_id, start, duration, error,
    )
    .with_attributes(attributes)
    .with_span_links(links)
    .with_span_events(events)
    .with_env(env)
    .with_version(version)
    .with_component(component)
    .with_kind(kind))
}

/// Decodes the list of span links within a span (field 11).
fn decode_span_link_list(r: &mut &[u8], strings: &mut StringTable) -> Result<Vec<SpanLink>, DecodeError> {
    let num_links = read::read_array_len(r, "span link list")?;
    let mut links = Vec::with_capacity(num_links as usize);
    for _ in 0..num_links {
        links.push(decode_span_link(r, strings)?);
    }
    Ok(links)
}

/// Decodes a single `SpanLink` map.
fn decode_span_link(r: &mut &[u8], strings: &mut StringTable) -> Result<SpanLink, DecodeError> {
    let mut trace_id_high = 0;
    let mut trace_id_low = 0;
    let mut span_id = 0;
    let mut attributes = FastHashMap::default();
    let mut tracestate = MetaString::empty();
    let mut flags = 0;

    let num_fields = read::read_map_len(r, "span link")?;
    for _ in 0..num_fields {
        let field = read::read_u32(r, "span link field")?;
        match field {
            1 => {
                let id = read::read_bytes(r, "span link traceID")?;
                (trace_id_high, trace_id_low) = split_trace_id(&id);
            }
            2 => span_id = read::read_u64(r, "span link spanID")?,
            3 => attributes = value::read_attributes_map(r, strings, "span link attributes")?,
            4 => tracestate = value::read_streaming_string(r, strings, "span link tracestate")?,
            5 => flags = read::read_u32(r, "span link flags")?,
            _ => value::harvest_unknown_field(r, strings, "span link field")?,
        }
    }

    Ok(SpanLink::new(trace_id_low, span_id)
        .with_trace_id_high(trace_id_high)
        .with_attributes(attributes)
        .with_tracestate(tracestate)
        .with_flags(flags))
}

/// Decodes the list of span events within a span (field 12).
fn decode_span_event_list(r: &mut &[u8], strings: &mut StringTable) -> Result<Vec<SpanEvent>, DecodeError> {
    let num_events = read::read_array_len(r, "span event list")?;
    let mut events = Vec::with_capacity(num_events as usize);
    for _ in 0..num_events {
        events.push(decode_span_event(r, strings)?);
    }
    Ok(events)
}

/// Decodes a single `SpanEvent` map.
fn decode_span_event(r: &mut &[u8], strings: &mut StringTable) -> Result<SpanEvent, DecodeError> {
    let mut time = 0;
    let mut name = MetaString::empty();
    let mut attributes = FastHashMap::default();

    let num_fields = read::read_map_len(r, "span event")?;
    for _ in 0..num_fields {
        let field = read::read_u32(r, "span event field")?;
        match field {
            1 => time = read::read_u64(r, "span event time")?,
            2 => name = value::read_streaming_string(r, strings, "span event name")?,
            3 => attributes = value::read_attributes_map(r, strings, "span event attributes")?,
            _ => value::harvest_unknown_field(r, strings, "span event field")?,
        }
    }

    Ok(SpanEvent::new(time, name).with_attributes(attributes))
}
