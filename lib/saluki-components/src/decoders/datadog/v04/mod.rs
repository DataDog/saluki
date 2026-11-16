//! Decoder for the Datadog v0.4 APM trace wire format, the MessagePack `[][]Span` body that tracers send to
//! `/v0.4/traces` (and to `/v0.3/traces` and `/v0.2/traces` with a MessagePack content type).
//!
//! [`decode_v04_payload`] mirrors the reference receiver's `InternalTracerPayload.UnmarshalMsgConverted`
//! (`pkg/proto/pbgo/trace/idx/span.go`), which decodes these bodies straight into the v1.0 model. It produces the
//! same output as [`decode_v1_payload`](super::decode_v1_payload): one [`Trace`] per chunk, plus the payload
//! attributes.
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
//! attribute (`span.go:1469`).
//!
//! # Differences from `ConvertToIdx`
//!
//! The reference converts the other v0.x bodies (JSON, v0.1, v0.7) with `ConvertToIdx`
//! (`pkg/trace/api/converter.go`), which differs from this decoder in ways that are visible downstream. This decoder
//! follows `UnmarshalMsgConverted`:
//!
//! 1. Promoted tags (`env`, `version`, `component`, `span.kind`) stay in the span's attributes (`span.go:1300-1303`).
//!    `ConvertToIdx` drops them (`converter.go:62-64`).
//! 2. Promoted fields are taken from the *last* span that carries them in a chunk (`span.go:1846-1896`), and from the
//!    last chunk that carries them in a payload (`span.go:1080-1091`). An empty `env` or `version` value on a later
//!    span clears an earlier one within the chunk. `ConvertToIdx` takes the first span in a chunk.
//! 3. Chunks without spans are kept (`span.go:1052-1059`): the receiver counts them, and the normalizer drops them.
//!    `ConvertToIdx` drops them (`converter.go:38-40`). `nil` spans are dropped by both (`span.go:1115-1121`).
//! 4. The chunk's trace ID comes from the last span with a `trace_id` (`span.go:1189-1202`), not the first.
//! 5. `meta` and `metrics` share one attribute map, so on a key clash the map that comes later on the wire wins
//!    (`span.go:1303`, `span.go:1345`). `ConvertToIdx` always lets `metrics` win (`converter.go:84-91`).
//! 6. An invalid `_dd.p.dm` is ignored, keeping an earlier span's mechanism (`span.go:1880-1889`), where
//!    `ConvertToIdx` resets it to 0. A `float32` metric is a type error (`parseFloat64Bytes`, `span.go:2125-2161`).
//!    There are no chunk tags to fall back on for the priority or the mechanism.
//!
//! # Other reference behaviors kept
//!
//! - Numbers are as tolerant as the reference's `parseXBytes` helpers: see the `v0_*` helpers in
//!   [`read`](super::read).
//! - Strings may be `str` or `bin`, and `nil` reads as the empty string. Invalid UTF-8 is repaired one byte at a time.
//! - A `nil` `trace_id` after a non-zero one in the same chunk is an error only when it is the last field of its span,
//!   because the reference overwrites the error with the next field's (`span.go:1190-1196`).
//! - Bytes after the trace list are ignored (`span.go:1061`).
//! - Short input maps to [`DecodeError::ShortBytes`] wherever the reference returns `msgp.ErrShortBytes`, including
//!   a collection header that declares more entries than bytes remain.
//!
//! Unknown fields are skipped with a nesting limit of 64, where the reference's `msgp.Skip` allows 100,000.
//! `meta_struct`, `span_links` and `span_events` are skipped too, until they are decoded.
//!
//! # Performance
//!
//! Strings are interned per payload: short strings are inlined in the [`MetaString`] itself, and longer ones are
//! shared through one allocation per distinct value, as the reference interns into its string table.

use std::sync::Arc;

use saluki_common::collections::FastHashMap;
use saluki_core::data_model::event::trace::{AttributeValue, PayloadFields, Span, Trace, TraceIngestInfo};
use stringtheory::MetaString;

use super::error::DecodeError;
use super::read;
use super::DecodedV1;

#[cfg(test)]
mod tests;

/// The reference's `sampler.PriorityNone`: no sampling priority.
const PRIORITY_NONE: i32 = i8::MIN as i32;

/// The OTel span kinds, as `Span::kind` stores them.
const SPAN_KIND_INTERNAL: u32 = 1;
const SPAN_KIND_SERVER: u32 = 2;
const SPAN_KIND_CLIENT: u32 = 3;
const SPAN_KIND_PRODUCER: u32 = 4;
const SPAN_KIND_CONSUMER: u32 = 5;

/// A plausible minimum wire size for a span, used only to bound pre-allocations.
const MIN_BYTES_PER_SPAN: usize = 21;

/// A plausible minimum wire size for a trace chunk, used only to bound pre-allocations.
const MIN_BYTES_PER_CHUNK: usize = 1;

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
    let mut traces = Vec::with_capacity((num_chunks as usize).min(r.len() / MIN_BYTES_PER_CHUNK));
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

/// Fields promoted from a chunk's spans to the chunk, the reference's `SpanConvertedFields`.
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

/// Fields promoted from the chunks to the payload, the reference's `ChunkConvertedFields`.
#[derive(Default)]
struct PayloadPromoted {
    env: MetaString,
    app_version: MetaString,
    git_commit_sha: MetaString,
    hostname: MetaString,
    apm_mode: MetaString,
}

impl PayloadPromoted {
    /// Takes every non-empty field of a decoded chunk, mirroring `InternalTraceChunk.ApplyPromotedFields`.
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

/// The reference's `RootSamplingMergeState`: once a root span has a priority, it overrides the later spans'.
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

/// Decodes one trace chunk, a list of spans, mirroring `InternalTraceChunk.UnmarshalMsgConverted`.
fn decode_chunk<'a>(
    r: &mut &'a [u8], strings: &mut Strings<'a>, payload: &mut PayloadPromoted,
) -> Result<Trace, DecodeError> {
    let num_spans = read::v0_read_array_len(r, "span list")?;
    let mut spans = Vec::with_capacity((num_spans as usize).min(r.len() / MIN_BYTES_PER_SPAN));
    let mut promoted = ChunkPromoted::default();
    let mut root = RootPriority::default();
    for _ in 0..num_spans {
        if read::peek_is_nil(r) {
            read::skip_nil(r);
            continue;
        }
        let span = decode_span(r, strings, &mut promoted)?;
        root.reconcile(&mut promoted, span.parent_id());
        spans.push(span);
    }

    let mut trace = Trace::new(spans, TraceIngestInfo::Datadog);
    trace.trace_id_low = promoted.trace_id_low;
    trace.trace_id_high = promoted.trace_id_high;
    trace.sampling_mechanism = promoted.sampling_mechanism;
    trace.priority = (promoted.sampling_priority != PRIORITY_NONE).then_some(promoted.sampling_priority);
    trace.origin = std::mem::take(&mut promoted.origin);
    payload.apply(&mut promoted);
    Ok(trace)
}

/// Decodes one span, a map keyed by field name, mirroring `InternalSpan.UnmarshalMsgConverted`.
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

/// Decodes a span's `meta` map into its attributes, applying the promoted fields as `handlePromotedMetaFields` does.
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
    attributes.reserve(len as usize);
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
    attributes.reserve(len as usize);
    for _ in 0..len {
        let key_bytes = read::v0_read_str_bytes(r, "span metrics key")?;
        let value = read::v0_parse_f64(r, "span metrics value")?;
        if key_bytes == KEY_SAMPLING_PRIORITY {
            // Go's `int32(float64)`; out-of-range values saturate here, where Go's result is platform-dependent.
            promoted.sampling_priority = value as i32;
        }
        attributes.insert(strings.get(key_bytes), AttributeValue::Float(value));
    }
    Ok(())
}

/// Maps a `span.kind` value to the span kind, defaulting to internal as the reference does.
fn span_kind(value: &[u8]) -> u32 {
    match value {
        b"server" => SPAN_KIND_SERVER,
        b"client" => SPAN_KIND_CLIENT,
        b"producer" => SPAN_KIND_PRODUCER,
        b"consumer" => SPAN_KIND_CONSUMER,
        _ => SPAN_KIND_INTERNAL,
    }
}

/// Parses an unsigned 64-bit integer as Go's `strconv.ParseUint(s, radix, 64)` does, which rejects a sign.
fn parse_unsigned(value: &[u8], radix: u32) -> Option<u64> {
    let value = simdutf8::basic::from_utf8(value).ok()?;
    if value.starts_with('+') {
        return None;
    }
    u64::from_str_radix(value, radix).ok()
}
