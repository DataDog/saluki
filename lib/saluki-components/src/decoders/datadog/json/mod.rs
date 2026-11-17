//! Decoders for the Datadog v0.4 APM trace format in JSON, the `[][]Span` body that tracers send to `/v0.4/traces`,
//! `/v0.3/traces` and `/v0.2/traces` with a JSON media type, and for the v0.1 format, the `[]Span` body of
//! `/v0.1/spans` and `/spans`.
//!
//! [`decode_v04_json`] decodes these bodies as the trace-agent's receiver does: `json.NewDecoder(body).Decode` into
//! `pb.Traces`, then a conversion to the v1.0 model. It decodes in three steps:
//!
//! 1. A [syntax pass](scan) over the first JSON value, which stops where `json.Decoder` stops: the body ending early is
//!    [`DecodeError::UnexpectedEof`], as Go's `io.EOF` and `io.ErrUnexpectedEOF`, a syntax error wins over a type
//!    error, and bytes after the first value are ignored.
//! 2. [Deserialization](de) into the classic protos, as `encoding/json` decodes into `pb.Traces`.
//! 3. [Conversion](super::convert) into unified traces.
//!
//! [`decode_v04_any`] is the trace-agent's "do our best" branch for any other media type: JSON first, then MessagePack.
//!
//! [`decode_v01_json`] decodes v0.1 bodies the same way, grouping the spans into chunks by trace ID. The trace-agent
//! decodes a v0.1 body as JSON whatever its media type.
//!
//! # Limits
//!
//! The decoder reads the body as a whole, where Go's reads only as far as the first value: a body whose first value is
//! complete but whose trailing bytes reach `max_request_bytes` is rejected here, and accepted there.

use std::borrow::Cow;

use datadog_protos::traces::{TraceChunk, TracerPayload};

use super::convert::convert_tracer_payload;
use super::error::DecodeError;
use super::{decode_v04_payload, read, DecodedV1};

mod de;
mod scan;

#[cfg(test)]
pub(super) mod tests;

/// Decodes a v0.4 JSON trace payload into unified trace events.
///
/// Each trace with at least one span becomes one [`Trace`](saluki_core::data_model::event::trace::Trace). As for
/// [`decode_v04_payload`], the payload fields that come from the request headers are left for the caller to set.
///
/// # Errors
///
/// Returns [`DecodeError::UnexpectedEof`] if the body ends before its first JSON value is complete, and
/// [`DecodeError::Json`] if that value isn't valid JSON or isn't a list of lists of spans.
pub fn decode_v04_json(body: &[u8]) -> Result<DecodedV1, DecodeError> {
    decode_json(body).map_err(|failure| failure.error)
}

/// Decodes a v0.4 trace payload of an unknown media type, as JSON if it is JSON, and as MessagePack otherwise.
///
/// This matches the trace-agent's "do our best" branch with its quirk: the MessagePack decoder only gets what the JSON
/// decoder left of the body. Go's `json.Decoder` reads the body 512, then 1,024, then 2,048 bytes at a time, and so on,
/// so a MessagePack body is decoded from the end of the read in which the JSON decoder gave up. One of up to 512 bytes
/// is always [`DecodeError::ShortBytes`].
///
/// # Errors
///
/// Returns the MessagePack decoder's error if the body is neither.
pub fn decode_v04_any(body: &[u8]) -> Result<DecodedV1, DecodeError> {
    decode_json(body).or_else(|failure| decode_v04_payload(&body[failure.read..]))
}

/// Decodes a v0.1 JSON span list into unified trace events, one per trace ID.
///
/// The spans of each trace keep their order in the list, and the traces come in the order their IDs first appear. As
/// for [`decode_v04_json`], the payload fields that come from the request headers are left for the caller to set.
///
/// # Errors
///
/// Returns [`DecodeError::UnexpectedEof`] if the body ends before its first JSON value is complete, and
/// [`DecodeError::Json`] if that value isn't valid JSON or isn't a list of spans.
pub fn decode_v01_json(body: &[u8]) -> Result<DecodedV1, DecodeError> {
    let chunks = decode_json_value(body, de::decode_spans).map_err(|failure| failure.error)?;
    Ok(convert_chunks(chunks, "v01-json"))
}

/// A JSON decoding failure, with how much of the body Go's decoder would have read by then.
struct JsonFailure {
    error: DecodeError,
    read: usize,
}

fn decode_json(body: &[u8]) -> Result<DecodedV1, JsonFailure> {
    let chunks = decode_json_value(body, de::decode_traces)?;
    Ok(convert_chunks(chunks, "v04-json"))
}

/// Decodes the first JSON value of the body into chunks with `decode`, as `json.Decoder.Decode` reads it.
fn decode_json_value(
    body: &[u8], decode: fn(&[u8]) -> Result<Vec<TraceChunk>, serde_json::Error>,
) -> Result<Vec<TraceChunk>, JsonFailure> {
    let scanned = scan::scan(body);
    let read = scan::decoder_read(scanned.examined, body.len());
    let fail = |error| JsonFailure { error, read };
    let len = match scanned.outcome {
        scan::Outcome::Value { len } => len,
        scan::Outcome::Eof => return Err(fail(DecodeError::UnexpectedEof)),
        scan::Outcome::Syntax { at } => {
            return Err(fail(DecodeError::Json {
                detail: format!("invalid character at offset {at}"),
            }))
        }
    };

    // Go's decoder replaces each invalid byte in a string with U+FFFD, and the scan rejected any outside of one.
    let value = &body[..len];
    let value = match simdutf8::basic::from_utf8(value) {
        Ok(_) => Cow::Borrowed(value),
        Err(_) => Cow::Owned(read::repair_utf8(value).into_bytes()),
    };
    decode(&value).map_err(|error| {
        fail(DecodeError::Json {
            detail: error.to_string(),
        })
    })
}

fn convert_chunks(chunks: Vec<TraceChunk>, converted_from: &'static str) -> DecodedV1 {
    let payload = TracerPayload {
        chunks,
        ..TracerPayload::default()
    };
    convert_tracer_payload(payload, converted_from)
}
