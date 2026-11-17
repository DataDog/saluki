//! Conversion of the classic tracer payload model (`datadog_protos::traces::TracerPayload`) into unified traces.
//!
//! [`convert_tracer_payload`] converts the classic model as the trace-agent's receiver does
//! for the bodies of `/v0.2`-`/v0.4` in JSON, `/v0.1/spans`, `/spans` and `/v0.7/traces`. It
//! produces the same output as [`decode_v1_payload`](super::decode_v1_payload).
//!
//! # Promoted fields
//!
//! - `meta["env"]`, `meta["version"]`, `meta["component"]` and `meta["span.kind"]` set the span's `env`, `version`,
//!   `component` and kind, and are left out of its attributes, as are `metrics` and `meta_struct` entries under those
//!   keys (`converter.go:62-64`). A span without a kind, or with an unknown one, is internal.
//! - The chunk's trace ID is the first span's `trace_id`, with the high 64 bits from its `meta["otel.trace_id"]` or
//!   `meta["_dd.p.tid"]` (hexadecimal; `Get128BitTraceID`). A value that doesn't parse leaves the high bits at 0.
//! - The *first* span of a chunk that carries a non-empty `env`, `version`, `_dd.hostname`, `_dd.git.commit.sha`,
//!   `_dd.apm_mode`, `_dd.origin` or `_dd.p.dm` sets it for the chunk. `_dd.p.dm` parses as `strconv.ParseUint`, with
//!   its quirks: a value that isn't a number reads as 0, so a later span can still set it, and one that overflows
//!   reads as `u32::MAX`. The chunk's own origin and `_dd.p.dm` tag are only fallbacks.
//! - `metrics["_sampling_priority_v1"]` sets the chunk's priority, and once a root span (parent ID 0) has one, it wins
//!   over the spans after it. The chunk's own priority is a fallback.
//! - The *last* chunk with a non-empty `env`, app version, hostname, git commit SHA or APM mode sets it for the payload,
//!   and the payload's own env, hostname and app version win over the promoted ones.
//!
//! # Attributes
//!
//! `meta`, then `metrics`, then `meta_struct` go into one attribute map, so on a key clash `meta_struct` wins over
//! `metrics`, which wins over `meta` (`converter.go:84-91`). Every span then gets a `_dd.convertedv1` attribute naming
//! the format it was converted from.
//!
//! Chunks without spans are dropped. Span event attributes of an unknown type are dropped, as are unknown array
//! elements, and an array attribute without an array is an empty array.

use std::sync::Arc;

use datadog_protos::traces::{
    AttributeAnyValue, AttributeArrayValue, Span as ClassicSpan, SpanEvent as ClassicSpanEvent,
    SpanLink as ClassicSpanLink, TraceChunk, TracerPayload,
};
use saluki_common::collections::FastHashMap;
use saluki_core::data_model::event::trace::{
    AttributeValue, PayloadFields, Span, SpanEvent, SpanLink, Trace, TraceIngestInfo,
};
use stringtheory::MetaString;

use super::v04::{
    span_kind, ChunkPromoted, PayloadPromoted, RootPriority, PRIORITY_NONE, TAG_APM_MODE, TAG_CONVERTED_V1,
    TAG_GIT_COMMIT_SHA,
};
use super::DecodedV1;

#[cfg(test)]
mod tests;

/// The `meta` keys promoted to span fields, and so never kept as attributes.
fn is_promoted_tag(key: &str) -> bool {
    matches!(key, "env" | "version" | "component" | "span.kind")
}

/// Converts a classic tracer payload into unified trace events, as the trace-agent does.
///
/// `converted_from` names the payload's original format in each span's `_dd.convertedv1` attribute (`v04-json`,
/// `v01-json`, `v07-pb`), as the trace-agent names it. If it is empty, no such attribute is set.
pub fn convert_tracer_payload(payload: TracerPayload, converted_from: &'static str) -> DecodedV1 {
    let mut payload_attributes = string_attributes(payload.tags);
    let mut promoted = PayloadPromoted::default();
    let mut traces = Vec::with_capacity(payload.chunks.len());
    for chunk in payload.chunks {
        if let Some(trace) = convert_chunk(chunk, &mut promoted, converted_from) {
            traces.push(trace);
        }
    }

    // As in the trace-agent, the payload's own fields win, the promoted attributes don't.
    // Every trace carries these, so each is shared rather than copied per trace.
    let or_promoted = |own: String, promoted: MetaString| shared(if own.is_empty() { &promoted } else { own.as_str() });
    let fields = PayloadFields {
        container_id: shared(&payload.containerID),
        language_name: shared(&payload.languageName),
        language_version: shared(&payload.languageVersion),
        tracer_version: shared(&payload.tracerVersion),
        runtime_id: shared(&payload.runtimeID),
        env: or_promoted(payload.env, promoted.env),
        hostname: or_promoted(payload.hostname, promoted.hostname),
        app_version: or_promoted(payload.appVersion, promoted.app_version),
        ..PayloadFields::default()
    };
    if !promoted.git_commit_sha.is_empty() {
        payload_attributes.insert(TAG_GIT_COMMIT_SHA, AttributeValue::String(promoted.git_commit_sha));
    }
    if !promoted.apm_mode.is_empty() {
        payload_attributes.insert(TAG_APM_MODE, AttributeValue::String(promoted.apm_mode));
    }
    for trace in &mut traces {
        trace.payload = fields.clone();
    }

    DecodedV1 {
        traces,
        payload_attributes,
    }
}

/// Converts one chunk, or returns `None` for a chunk without spans.
fn convert_chunk(chunk: TraceChunk, payload: &mut PayloadPromoted, converted_from: &'static str) -> Option<Trace> {
    let first = chunk.spans.first()?;
    let mut promoted = ChunkPromoted::default();
    (promoted.trace_id_high, promoted.trace_id_low) = trace_id_128(first);

    let mut root = RootPriority::default();
    let spans = chunk
        .spans
        .into_iter()
        .map(|span| convert_span(span, &mut promoted, &mut root, converted_from))
        .collect();

    let attributes = string_attributes(chunk.tags);
    let mut trace = Trace::new(spans, TraceIngestInfo::Datadog);
    trace.trace_id_high = promoted.trace_id_high;
    trace.trace_id_low = promoted.trace_id_low;
    trace.dropped_trace = chunk.droppedTrace;
    trace.origin = if promoted.origin.is_empty() {
        meta(chunk.origin)
    } else {
        std::mem::take(&mut promoted.origin)
    };
    let mut priority = promoted.sampling_priority;
    if chunk.priority != PRIORITY_NONE && priority == PRIORITY_NONE {
        priority = chunk.priority;
    }
    trace.priority = (priority != PRIORITY_NONE).then_some(priority);
    trace.sampling_mechanism = promoted.sampling_mechanism;
    if trace.sampling_mechanism == 0 {
        if let Some(AttributeValue::String(dm)) = attributes.get("_dd.p.dm") {
            trace.sampling_mechanism = parse_decision_maker(dm);
        }
    }
    trace.attributes = attributes.into();
    payload.apply(&mut promoted);
    Some(trace)
}

/// Converts one span, collecting the fields it promotes to its chunk.
fn convert_span(
    span: ClassicSpan, promoted: &mut ChunkPromoted, root: &mut RootPriority, converted_from: &'static str,
) -> Span {
    if let Some(priority) = span.metrics.get("_sampling_priority_v1") {
        // Out-of-range values saturate here, where the trace-agent's conversion is platform-dependent.
        promoted.sampling_priority = *priority as i32;
    }
    root.reconcile(promoted, span.parentID);

    let get = |key: &str| span.meta.get(key).map(String::as_str).unwrap_or_default();
    let env = get("env");
    let version = get("version");
    take_first(&mut promoted.env, env);
    take_first(&mut promoted.hostname, get("_dd.hostname"));
    take_first(&mut promoted.app_version, version);
    take_first(&mut promoted.git_commit_sha, get("_dd.git.commit.sha"));
    let dm = get("_dd.p.dm");
    if !dm.is_empty() && promoted.sampling_mechanism == 0 {
        promoted.sampling_mechanism = parse_decision_maker(dm);
    }
    take_first(&mut promoted.apm_mode, get("_dd.apm_mode"));
    take_first(&mut promoted.origin, get("_dd.origin"));
    let (env, version, component) = (
        MetaString::from(env),
        MetaString::from(version),
        MetaString::from(get("component")),
    );
    let kind = span_kind(get("span.kind").as_bytes());

    let mut attributes = FastHashMap::default();
    attributes.reserve(span.meta.len() + span.metrics.len() + span.meta_struct.len() + 1);
    for (key, value) in span.meta {
        if !is_promoted_tag(&key) {
            attributes.insert(meta(key), AttributeValue::String(meta(value)));
        }
    }
    for (key, value) in span.metrics {
        if !is_promoted_tag(&key) {
            attributes.insert(meta(key), AttributeValue::Float(value));
        }
    }
    for (key, value) in span.meta_struct {
        if !is_promoted_tag(&key) {
            attributes.insert(meta(key), AttributeValue::Bytes(value));
        }
    }
    if !converted_from.is_empty() {
        attributes.insert(
            TAG_CONVERTED_V1,
            AttributeValue::String(MetaString::from_static(converted_from)),
        );
    }

    Span::new(
        meta(span.service),
        meta(span.name),
        meta(span.resource),
        meta(span.type_),
        span.spanID,
        span.parentID,
        span.start as u64,
        span.duration as u64,
        i32::from(span.error > 0),
    )
    .with_attributes(attributes)
    .with_span_links(span.spanLinks.into_iter().map(convert_link).collect::<Vec<_>>())
    .with_span_events(span.spanEvents.into_iter().map(convert_event).collect::<Vec<_>>())
    .with_env(env)
    .with_version(version)
    .with_component(component)
    .with_kind(kind)
}

/// Sets a promoted field from a span's value, unless an earlier span already set it.
fn take_first(field: &mut MetaString, value: &str) {
    if field.is_empty() && !value.is_empty() {
        *field = MetaString::from(value);
    }
}

fn convert_link(link: ClassicSpanLink) -> SpanLink {
    SpanLink::new(link.traceID, link.spanID)
        .with_trace_id_high(link.traceID_high)
        .with_attributes(string_attributes(link.attributes))
        .with_tracestate(meta(link.tracestate))
        .with_flags(link.flags)
}

fn convert_event(event: ClassicSpanEvent) -> SpanEvent {
    let attributes = event
        .attributes
        .into_iter()
        .filter_map(|(key, value)| Some((meta(key), convert_any_value(value)?)))
        .collect::<FastHashMap<_, _>>();
    SpanEvent::new(event.time_unix_nano, meta(event.name)).with_attributes(attributes)
}

/// Converts a span event attribute, or returns `None` for an unknown type, as the trace-agent drops it.
fn convert_any_value(value: AttributeAnyValue) -> Option<AttributeValue> {
    Some(match value.type_.value() {
        0 => AttributeValue::String(meta(value.string_value)),
        1 => AttributeValue::Bool(value.bool_value),
        2 => AttributeValue::Int(value.int_value),
        3 => AttributeValue::Float(value.double_value),
        4 => AttributeValue::Array(
            value
                .array_value
                .into_option()
                .map(|array| array.values.into_iter().filter_map(convert_array_value).collect())
                .unwrap_or_default(),
        ),
        _ => return None,
    })
}

fn convert_array_value(value: AttributeArrayValue) -> Option<AttributeValue> {
    Some(match value.type_.value() {
        0 => AttributeValue::String(meta(value.string_value)),
        1 => AttributeValue::Bool(value.bool_value),
        2 => AttributeValue::Int(value.int_value),
        3 => AttributeValue::Float(value.double_value),
        _ => return None,
    })
}

fn string_attributes(map: std::collections::HashMap<String, String>) -> FastHashMap<MetaString, AttributeValue> {
    map.into_iter()
        .map(|(key, value)| (meta(key), AttributeValue::String(meta(value))))
        .collect()
}

/// Returns a span's 128-bit trace ID as its high and low halves, as `Span.Get128BitTraceID` does.
fn trace_id_128(span: &ClassicSpan) -> (u64, u64) {
    let high = if let Some(otel) = span.meta.get("otel.trace_id") {
        // An OTel trace ID holds all 128 bits as 32 hex characters; only the first 16 are the high bits. Go slices the
        // bytes, which can split a character: the bytes then don't parse.
        let otel = otel.as_bytes();
        let otel = if otel.len() == 32 { &otel[..16] } else { otel };
        parse_uint(otel, 16, 64).ok()
    } else if let Some(tid) = span.meta.get("_dd.p.tid") {
        parse_uint(tid.as_bytes(), 16, 64).ok()
    } else {
        None
    };
    (high.unwrap_or(0), span.traceID)
}

/// Parses a `_dd.p.dm` value, with or without a leading `-`, as the trace-agent does: the value
/// `strconv.ParseUint(s, 10, 32)` returns, error or not.
fn parse_decision_maker(value: &str) -> u32 {
    let value = value.strip_prefix('-').unwrap_or(value);
    match parse_uint(value.as_bytes(), 10, 32) {
        Ok(value) | Err(value) => value as u32,
    }
}

/// Parses an unsigned integer exactly as Go's `strconv.ParseUint(s, base, bit_size)` does for a base from 2 to 36.
///
/// On error, returns the value Go returns alongside it: 0 for a syntax error, and the largest value for a range error.
/// Go checks each digit in order, so whichever error comes first wins.
fn parse_uint(s: &[u8], base: u64, bit_size: u32) -> Result<u64, u64> {
    if s.is_empty() {
        return Err(0);
    }
    let cutoff = u64::MAX / base + 1;
    let max = if bit_size == 64 { u64::MAX } else { (1 << bit_size) - 1 };
    let mut n: u64 = 0;
    for &c in s {
        let digit = match c {
            b'0'..=b'9' => c - b'0',
            _ if c.to_ascii_lowercase().is_ascii_lowercase() => c.to_ascii_lowercase() - b'a' + 10,
            _ => return Err(0),
        };
        if u64::from(digit) >= base {
            return Err(0);
        }
        if n >= cutoff {
            return Err(max);
        }
        n *= base;
        match n.checked_add(u64::from(digit)) {
            Some(next) if next <= max => n = next,
            _ => return Err(max),
        }
    }
    Ok(n)
}

/// Converts a string, inlining it when it is short enough and sharing one allocation otherwise: cloning an owned
/// [`MetaString`] copies it.
fn shared(value: &str) -> MetaString {
    MetaString::try_inline(value).unwrap_or_else(|| MetaString::from(Arc::<str>::from(value)))
}

/// Converts an owned string, inlining it when it is short enough rather than keeping its allocation.
fn meta(value: String) -> MetaString {
    MetaString::try_inline(&value).unwrap_or_else(|| MetaString::from(value))
}
