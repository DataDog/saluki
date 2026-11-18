//! Decoder for the Datadog v0.5 APM trace wire format, the MessagePack dictionary body that tracers send to
//! `/v0.5/traces`.
//!
//! A v0.5 payload is a two-element array: a dictionary, the list of every string in the payload, then a `[][]Span`
//! list in which every string is a `uint32` index into the dictionary. Each span is a fixed 12-element array:
//!
//! | # | Field | Type |
//! |---|---|---|
//! | 0 | service | string index |
//! | 1 | name | string index |
//! | 2 | resource | string index |
//! | 3 | trace ID | `uint64` |
//! | 4 | span ID | `uint64` |
//! | 5 | parent ID | `uint64` |
//! | 6 | start | `int64` |
//! | 7 | duration | `int64` |
//! | 8 | error | `int32` |
//! | 9 | meta | map of string index to string index |
//! | 10 | metrics | map of string index to `float64` |
//! | 11 | type | string index |
//!
//! [`decode_v05_payload`] mirrors the reference receiver's `InternalTracerPayload.UnmarshalMsgDictionary`
//! (`pkg/proto/pbgo/trace/idx/span_v05.go`), which decodes these bodies straight into the v1.0 model. It produces the
//! same output as [`decode_v1_payload`](super::decode_v1_payload): one [`Trace`] per chunk, plus the payload
//! attributes.
//!
//! # The dictionary
//!
//! The reference rebuilds the dictionary into a v1.0 string table, which must hold the empty string at index 0
//! (`buildStringTable`):
//!
//! - If the dictionary starts with the empty string, it is kept as it is.
//! - Otherwise, if it holds the empty string, its first one swaps places with the dictionary's first string.
//! - Otherwise, the first string is moved to the end of the table and the empty string takes its place.
//!
//! A reference to index 0 then reads the moved string (`readV05StringRef`). A reference to the index the empty string
//! was swapped from reads the moved string too, not the empty string. This decoder keeps that quirk, as tracers put
//! the empty string first, if at all. An index at or past the dictionary's length is an error
//! ([`DecodeError::DictionaryIndexOutOfRange`]).
//!
//! The dictionary holds each string once, and every reference to it shares that one copy: a payload that references
//! one large string from many spans holds it once.
//!
//! # Resolved string bytes
//!
//! Every reference is charged its string's length against a bound of 512 times the payload's length (at least
//! 1 MiB), and a payload past it fails to decode ([`DecodeError::ResolvedStringsTooLarge`]), as a v1.0 payload does.
//! Each resolved string is hashed into an attribute map here and again downstream, so without the bound a small
//! payload that references one large string from many spans would cost work out of all proportion to its size. The
//! reference keeps its references as indexes and has no such bound: this is a deliberate deviation, which only
//! payloads built to amplify meet. The payload's env, hostname, app version and attributes are charged once per
//! trace, as every trace carries them.
//!
//! # Promoted fields
//!
//! The same fields are promoted to the span, chunk and payload as for v0.4 (see [`decode_v04_payload`]), by the same
//! `handlePromotedMetaFields` and `ApplyPromotedFields` code, and so with the same *last* span and *last* chunk wins
//! rules. The differences:
//!
//! - Only the payload's first span gets the `_dd.convertedv1: v05` attribute (`span_v05.go:96-103`).
//! - A span without a `span.kind` has kind 0, unspecified, rather than internal: the reference's v0.5 spans start
//!   empty (`span_v05.go:93`).
//! - The chunk's low trace ID comes from the last span, whatever it holds: a `nil` trace ID reads as 0.
//! - A promoted field is set by a non-zero dictionary reference, rather than by a non-empty string: an empty string at
//!   a non-zero index overrides an earlier chunk's value, and sets an empty `_dd.git.commit.sha` or `_dd.apm_mode`
//!   payload attribute.
//! - `metrics` is always read after `meta`, so on a key clash the metric wins.
//! - Two dictionary entries with the same string are distinct keys in the reference's attribute map. Here they are
//!   one, the later entry winning.
//!
//! # Other reference behaviors kept
//!
//! - Numbers are as tolerant as the reference's `parseXBytes` helpers, and string indexes as strict as
//!   `msgp.ReadUint32Bytes`: see the `v0_*` helpers in [`read`](super::read).
//! - Dictionary strings may be `str` or `bin`, and `nil` reads as the empty string. Invalid UTF-8 is repaired one byte
//!   at a time.
//! - `nil` spans, `meta` and `metrics` are type errors, as are spans of any other length than 12.
//! - Chunks without spans are kept: the receiver counts them, and the normalizer drops them.
//! - The outer array's length isn't checked, and bytes after the trace list are ignored.
//! - Short input maps to [`DecodeError::ShortBytes`] wherever the reference returns `msgp.ErrShortBytes`, including
//!   a collection header that declares more entries than bytes remain.
//!
//! [`decode_v04_payload`]: super::decode_v04_payload

use std::sync::Arc;

use saluki_common::collections::FastHashMap;
use saluki_core::data_model::event::trace::{AttributeValue, PayloadFields, Span, Trace, TraceIngestInfo};
use stringtheory::MetaString;

use super::error::DecodeError;
use super::read::{self, ResolvedBytes};
use super::v04::{
    parse_unsigned, span_kind, ChunkPromoted, PayloadPromoted, RootPriority, PRIORITY_NONE, TAG_APM_MODE,
    TAG_CONVERTED_V1, TAG_GIT_COMMIT_SHA,
};
use super::{charge_per_trace, DecodedV1};

#[cfg(test)]
pub(super) mod tests;

/// The number of fields of a span, the reference's `spanPropertyCount`.
const SPAN_PROPERTY_COUNT: u32 = 12;

const CONVERTED_V1_VALUE: MetaString = MetaString::from_static("v05");

/// Decodes a v0.5 MessagePack trace payload into unified trace events.
///
/// Each trace chunk becomes one [`Trace`], including chunks without spans. The payload's env, hostname and app
/// version are promoted from the spans; the other payload fields (container ID, tracer language and so on) come from
/// the request headers and are left for the caller to set.
///
/// # Errors
///
/// Returns a [`DecodeError`] if the input is not a well-formed v0.5 payload. Input that ends early is
/// [`DecodeError::ShortBytes`].
pub fn decode_v05_payload(bytes: &[u8]) -> Result<DecodedV1, DecodeError> {
    let mut r = bytes;
    read::v0_read_array_len(&mut r, "payload")?;
    let dictionary = Dictionary::read(&mut r, bytes.len())?;

    let num_chunks = read::v0_read_array_len(&mut r, "trace list")?;
    let mut traces = Vec::with_capacity(read::preallocation(num_chunks));
    let mut promoted = PayloadPromoted::<u32>::default();
    // Only the payload's first span is tagged.
    let mut tag_converted = true;
    for _ in 0..num_chunks {
        let trace = decode_chunk(&mut r, &dictionary, &mut promoted, tag_converted)?;
        tag_converted &= trace.spans().is_empty();
        traces.push(trace);
    }

    let payload = PayloadFields {
        env: dictionary.get(promoted.env)?,
        hostname: dictionary.get(promoted.hostname)?,
        app_version: dictionary.get(promoted.app_version)?,
        ..PayloadFields::default()
    };

    // As `InternalTracerPayload.ApplyPromotedFields`, a set reference makes an attribute, even to an empty string.
    let mut payload_attributes = FastHashMap::default();
    if promoted.git_commit_sha != 0 {
        payload_attributes.insert(
            TAG_GIT_COMMIT_SHA,
            AttributeValue::String(dictionary.get(promoted.git_commit_sha)?),
        );
    }
    if promoted.apm_mode != 0 {
        payload_attributes.insert(TAG_APM_MODE, AttributeValue::String(dictionary.get(promoted.apm_mode)?));
    }
    let fields = [&payload.env, &payload.hostname, &payload.app_version];
    charge_per_trace(&dictionary.resolved, &fields, &payload_attributes, traces.len())?;

    for trace in &mut traces {
        trace.payload = payload.clone();
    }

    Ok(DecodedV1 {
        traces,
        payload_attributes,
    })
}

/// A payload's dictionary, rebuilt as the reference's string table (`buildStringTable`).
struct Dictionary {
    /// The string table: the dictionary with the empty string moved to index 0.
    strings: Vec<MetaString>,
    /// The number of strings on the wire, which bounds the references (`dictSize`).
    wire_len: u32,
    /// Where the dictionary's first string moved to, or 0 if it didn't move (`newZeroRef`).
    zero_ref: u32,
    /// The bytes the payload's references may still resolve to.
    resolved: ResolvedBytes,
}

impl Dictionary {
    /// Reads the dictionary of a payload of `payload_len` bytes, as the first half of `UnmarshalMsgDictionary`.
    fn read(r: &mut &[u8], payload_len: usize) -> Result<Self, DecodeError> {
        let wire_len = read::v0_read_dictionary_len(r, "dictionary")?;
        // The count is backed by as many bytes, but a string takes more memory than its one-byte encoding: the table
        // grows as the strings decode.
        let mut strings = Vec::with_capacity(read::preallocation(wire_len));
        for _ in 0..wire_len {
            strings.push(meta_string(read::v0_read_str_bytes(r, "dictionary string")?));
        }

        let mut zero_ref = 0;
        if strings.first().is_some_and(|first| !first.is_empty()) {
            match strings.iter().position(MetaString::is_empty) {
                Some(empty) => {
                    strings.swap(0, empty);
                    zero_ref = empty;
                }
                None => {
                    let first = std::mem::take(&mut strings[0]);
                    strings.push(first);
                    zero_ref = strings.len() - 1;
                }
            }
        }
        // Both fit: `zero_ref` is at most the wire length, which is at most `read::MAX_SIZE`.
        Ok(Self {
            strings,
            wire_len,
            zero_ref: zero_ref as u32,
            resolved: ResolvedBytes::for_payload(payload_len),
        })
    }

    /// Reads a string reference, mirroring `readV05StringRef`.
    fn read_ref(&self, r: &mut &[u8], context: &'static str) -> Result<u32, DecodeError> {
        let index = read::v0_read_u32(r, context)?;
        if index >= self.wire_len {
            return Err(DecodeError::DictionaryIndexOutOfRange { index });
        }
        Ok(if index == 0 { self.zero_ref } else { index })
    }

    /// Returns the string a reference read by [`read_ref`](Self::read_ref) points to, sharing it.
    ///
    /// # Errors
    ///
    /// Returns [`DecodeError::ResolvedStringsTooLarge`] if the payload's references have resolved to too many bytes.
    fn get(&self, reference: u32) -> Result<MetaString, DecodeError> {
        // Every reference `read_ref` returns is in the table, and 0 always is.
        let value = self.strings.get(reference as usize).cloned().unwrap_or_default();
        self.resolved.charge(value.len())?;
        Ok(value)
    }

    fn read_string(&self, r: &mut &[u8], context: &'static str) -> Result<MetaString, DecodeError> {
        self.get(self.read_ref(r, context)?)
    }
}

/// Converts a dictionary string, as the reference's `parseStringBytes`: short strings are inlined, and longer ones get
/// one shared allocation.
fn meta_string(bytes: &[u8]) -> MetaString {
    match simdutf8::basic::from_utf8(bytes) {
        Ok(s) => MetaString::try_inline(s).unwrap_or_else(|| MetaString::from(Arc::<str>::from(s))),
        Err(_) => MetaString::from(Arc::<str>::from(read::repair_utf8(bytes))),
    }
}

/// Decodes one trace chunk, a list of spans, as the loop body of `UnmarshalMsgDictionary`. With `tag_converted`, the
/// chunk's first span gets the `_dd.convertedv1` attribute.
fn decode_chunk(
    r: &mut &[u8], dictionary: &Dictionary, payload: &mut PayloadPromoted<u32>, mut tag_converted: bool,
) -> Result<Trace, DecodeError> {
    let num_spans = read::v0_read_array_len(r, "span list")?;
    let mut spans = Vec::with_capacity(read::preallocation(num_spans));
    let mut promoted = ChunkPromoted::<u32>::default();
    let mut root = RootPriority::default();
    for _ in 0..num_spans {
        let mut span = decode_span(r, dictionary, &mut promoted)?;
        if tag_converted {
            span.attributes
                .insert(TAG_CONVERTED_V1, AttributeValue::String(CONVERTED_V1_VALUE));
            tag_converted = false;
        }
        root.reconcile(&mut promoted, span.parent_id());
        spans.push(span);
    }

    let mut trace = Trace::new(spans, TraceIngestInfo::Datadog);
    trace.trace_id_low = promoted.trace_id_low;
    trace.trace_id_high = promoted.trace_id_high;
    trace.sampling_mechanism = promoted.sampling_mechanism;
    trace.priority = (promoted.sampling_priority != PRIORITY_NONE).then_some(promoted.sampling_priority);
    trace.origin = dictionary.get(promoted.origin)?;
    payload.apply(&mut promoted);
    Ok(trace)
}

/// Decodes one span, a 12-element array, mirroring `InternalSpan.UnmarshalMsgDictionaryConverted`.
fn decode_span(r: &mut &[u8], dictionary: &Dictionary, promoted: &mut ChunkPromoted<u32>) -> Result<Span, DecodeError> {
    let len = read::v0_read_array_len(r, "span")?;
    if len != SPAN_PROPERTY_COUNT {
        return Err(DecodeError::Msgpack {
            context: "span",
            detail: "encoded span needs exactly 12 elements in array".to_string(),
        });
    }
    let service = dictionary.read_string(r, "span service")?;
    let name = dictionary.read_string(r, "span name")?;
    let resource = dictionary.read_string(r, "span resource")?;
    promoted.trace_id_low = read::v0_parse_u64(r, "span trace_id")?;
    let span_id = read::v0_parse_u64(r, "span span_id")?;
    let parent_id = read::v0_parse_u64(r, "span parent_id")?;
    let start = read::v0_parse_i64(r, "span start")? as u64;
    let duration = read::v0_parse_i64(r, "span duration")? as u64;
    let error = read::v0_parse_i32(r, "span error")? != 0;

    let mut span = Span::new(
        service,
        name,
        resource,
        MetaString::empty(),
        span_id,
        parent_id,
        start,
        duration,
        i32::from(error),
    );
    decode_meta(r, dictionary, &mut span, promoted)?;
    decode_metrics(r, dictionary, &mut span.attributes, promoted)?;
    let span_type = dictionary.read_string(r, "span type")?;
    Ok(span.with_span_type(span_type))
}

/// Decodes a span's `meta` map into its attributes, applying the promoted fields as `handlePromotedMetaFields` does.
fn decode_meta(
    r: &mut &[u8], dictionary: &Dictionary, span: &mut Span, promoted: &mut ChunkPromoted<u32>,
) -> Result<(), DecodeError> {
    let len = read::v0_read_map_len(r, "span meta")?;
    if len == 0 {
        return Ok(());
    }
    read::v0_check_slab_count(len, r, "span meta")?;
    span.attributes.reserve(read::preallocation(len));
    for _ in 0..len {
        let key = dictionary.read_string(r, "span meta key")?;
        let value_ref = dictionary.read_ref(r, "span meta value")?;
        let value = dictionary.get(value_ref)?;
        match key.as_ref() {
            "_dd.p.tid" => {
                if let Some(high) = parse_unsigned(value.as_bytes(), 16) {
                    promoted.trace_id_high = high;
                }
            }
            "env" => {
                span.env = value.clone();
                promoted.env = value_ref;
            }
            "version" => {
                span.version = value.clone();
                promoted.app_version = value_ref;
            }
            "component" => span.component = value.clone(),
            "span.kind" => span.kind = span_kind(value.as_bytes()),
            "_dd.git.commit.sha" => promoted.git_commit_sha = value_ref,
            "_dd.p.dm" => {
                let mechanism = value.strip_prefix('-').unwrap_or(&value);
                if let Some(mechanism) = parse_unsigned(mechanism.as_bytes(), 10).and_then(|m| u32::try_from(m).ok()) {
                    promoted.sampling_mechanism = mechanism;
                }
            }
            "_dd.hostname" => promoted.hostname = value_ref,
            "_dd.origin" => promoted.origin = value_ref,
            "_dd.apm_mode" => promoted.apm_mode = value_ref,
            _ => {}
        }
        span.attributes.insert(key, AttributeValue::String(value));
    }
    read::release_unused_capacity(&mut span.attributes);
    Ok(())
}

/// Decodes a span's `metrics` map into its attributes, taking `_sampling_priority_v1` as the chunk priority.
fn decode_metrics(
    r: &mut &[u8], dictionary: &Dictionary, attributes: &mut FastHashMap<MetaString, AttributeValue>,
    promoted: &mut ChunkPromoted<u32>,
) -> Result<(), DecodeError> {
    let len = read::v0_read_map_len(r, "span metrics")?;
    if len == 0 {
        return Ok(());
    }
    read::v0_check_slab_count(len, r, "span metrics")?;
    attributes.reserve(read::preallocation(len));
    for _ in 0..len {
        let key = dictionary.read_string(r, "span metrics key")?;
        let value = read::v0_parse_f64(r, "span metrics value")?;
        if key.as_ref() == "_sampling_priority_v1" {
            // Go's `int32(float64)`; out-of-range values saturate here, where Go's result is platform-dependent.
            promoted.sampling_priority = value as i32;
        }
        attributes.insert(key, AttributeValue::Float(value));
    }
    read::release_unused_capacity(attributes);
    Ok(())
}
