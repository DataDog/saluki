//! Low-level MessagePack read helpers for the v1.0 APM trace wire format.
//!
//! Each helper operates on a `&mut &[u8]` cursor: reads advance the slice in place, mirroring the
//! byte-slice threading (`o []byte`) used by the reference Go decoder. Failures are mapped to
//! [`DecodeError`] with a static context string naming the field being read.

use std::fmt::Display;

use super::error::DecodeError;

/// Maximum element count permitted in any array or map header.
///
/// Protects the decoder from payloads that declare an enormous collection size to force large
/// allocations. Matches the reference decoder's `maxSize` (25,000,000). This alone doesn't stop a
/// tiny payload from claiming a count near this ceiling; [`read_array_len`] and [`read_map_len`]
/// additionally reject counts that couldn't possibly be backed by the remaining input, via
/// [`check_slab_count`].
pub const MAX_SIZE: u32 = 25_000_000;

/// Conservative lower bound on the wire size of one array element: every element is at least a
/// one-byte `nil` or fixint.
const MIN_BYTES_PER_ARRAY_ELEMENT: u32 = 1;

/// Conservative lower bound on the wire size of one map entry: a key and a value, each at least
/// one byte.
const MIN_BYTES_PER_MAP_ENTRY: u32 = 2;

/// Builds a closure that maps a MessagePack decoder error into a [`DecodeError::Msgpack`] carrying
/// the given static context.
fn mp<E: Display>(context: &'static str) -> impl FnOnce(E) -> DecodeError {
    move |e| DecodeError::Msgpack {
        context,
        detail: e.to_string(),
    }
}

/// Guards a collection pre-allocation (`Vec::with_capacity(len)`) against a claimed element count
/// that couldn't possibly be backed by the remaining bytes.
///
/// Without this, a small malicious payload could set an array or map header to millions of
/// entries and force a large allocation before decoding ever reaches the missing bytes and fails
/// naturally. This is a much tighter bound than [`MAX_SIZE`] for small inputs, since it scales
/// with the actual data available rather than a fixed ceiling.
///
/// # Errors
///
/// Returns an error if `len * min_bytes_per_entry` exceeds the number of bytes remaining on the
/// cursor.
fn check_slab_count(
    len: u32, min_bytes_per_entry: u32, remaining: &[u8], context: &'static str,
) -> Result<(), DecodeError> {
    if u64::from(len) * u64::from(min_bytes_per_entry) > remaining.len() as u64 {
        return Err(DecodeError::ImplausibleHeaderCount {
            context,
            len,
            remaining: remaining.len(),
        });
    }
    Ok(())
}

/// Reads an array header, returning its element count.
///
/// # Errors
///
/// Returns an error if the next value is not an array header, if the declared length exceeds
/// [`MAX_SIZE`], or if the declared length couldn't possibly be backed by the remaining input (see
/// [`check_slab_count`]).
pub fn read_array_len(r: &mut &[u8], context: &'static str) -> Result<u32, DecodeError> {
    let len = rmp::decode::read_array_len(r).map_err(mp(context))?;
    if len > MAX_SIZE {
        return Err(DecodeError::OversizeHeader {
            context,
            len,
            max: MAX_SIZE,
        });
    }
    check_slab_count(len, MIN_BYTES_PER_ARRAY_ELEMENT, r, context)?;
    Ok(len)
}

/// Reads a map header, returning its entry count.
///
/// # Errors
///
/// Returns an error if the next value is not a map header, if the declared length exceeds
/// [`MAX_SIZE`], or if the declared length couldn't possibly be backed by the remaining input (see
/// [`check_slab_count`]).
pub fn read_map_len(r: &mut &[u8], context: &'static str) -> Result<u32, DecodeError> {
    let len = rmp::decode::read_map_len(r).map_err(mp(context))?;
    if len > MAX_SIZE {
        return Err(DecodeError::OversizeHeader {
            context,
            len,
            max: MAX_SIZE,
        });
    }
    check_slab_count(len, MIN_BYTES_PER_MAP_ENTRY, r, context)?;
    Ok(len)
}

/// Reads any integer that fits in a `u32` (fixint or any width).
pub fn read_u32(r: &mut &[u8], context: &'static str) -> Result<u32, DecodeError> {
    rmp::decode::read_int(r).map_err(mp(context))
}

/// Reads any integer that fits in a `u64` (fixint or any width).
pub fn read_u64(r: &mut &[u8], context: &'static str) -> Result<u64, DecodeError> {
    rmp::decode::read_int(r).map_err(mp(context))
}

/// Reads any integer that fits in an `i32` (fixint or any width).
pub fn read_i32(r: &mut &[u8], context: &'static str) -> Result<i32, DecodeError> {
    rmp::decode::read_int(r).map_err(mp(context))
}

/// Reads any integer that fits in an `i64` (fixint or any width).
pub fn read_i64(r: &mut &[u8], context: &'static str) -> Result<i64, DecodeError> {
    rmp::decode::read_int(r).map_err(mp(context))
}

/// Reads a boolean.
pub fn read_bool(r: &mut &[u8], context: &'static str) -> Result<bool, DecodeError> {
    rmp::decode::read_bool(r).map_err(mp(context))
}

/// Reads a double-precision float.
pub fn read_f64(r: &mut &[u8], context: &'static str) -> Result<f64, DecodeError> {
    rmp::decode::read_f64(r).map_err(mp(context))
}

/// Reads a UTF-8 string, borrowing from the input for the lifetime of the underlying buffer.
pub fn read_str<'a>(r: &mut &'a [u8], context: &'static str) -> Result<&'a str, DecodeError> {
    let (s, tail) = rmp::decode::read_str_from_slice(*r).map_err(mp(context))?;
    *r = tail;
    Ok(s)
}

/// Reads a binary blob, copying it into an owned `Vec<u8>`.
pub fn read_bytes(r: &mut &[u8], context: &'static str) -> Result<Vec<u8>, DecodeError> {
    let len = rmp::decode::read_bin_len(r).map_err(mp(context))? as usize;
    if r.len() < len {
        return Err(DecodeError::Msgpack {
            context,
            detail: format!("expected {len} bytes but only {} remain", r.len()),
        });
    }
    let (head, tail) = (*r).split_at(len);
    *r = tail;
    Ok(head.to_vec())
}

/// Returns `true` if the next value on the cursor is encoded as a MessagePack string.
///
/// Used to distinguish the two encodings of a streaming string: a literal string (to add to the
/// table) versus a `uint32` index (a reference into the table). Does not advance the cursor.
pub fn peek_is_str(r: &&[u8]) -> bool {
    match r.first() {
        // fixstr (0xa0..=0xbf), str8 (0xd9), str16 (0xda), str32 (0xdb).
        Some(&b) => is_fixstr(b) || matches!(b, 0xd9..=0xdb),
        None => false,
    }
}

/// Returns `true` if the next value on the cursor is encoded as a MessagePack array.
///
/// Used when walking an unknown field's value, where the shape is not known ahead of time. Does not
/// advance the cursor.
pub fn peek_is_array(r: &&[u8]) -> bool {
    match r.first() {
        // fixarray (0x90..=0x9f), array16 (0xdc), array32 (0xdd).
        Some(&b) => b & 0xf0 == 0x90 || matches!(b, 0xdc | 0xdd),
        None => false,
    }
}

/// Returns `true` if the next value on the cursor is encoded as a MessagePack map.
///
/// Used when walking an unknown field's value, where the shape is not known ahead of time. Does not
/// advance the cursor.
pub fn peek_is_map(r: &&[u8]) -> bool {
    match r.first() {
        // fixmap (0x80..=0x8f), map16 (0xde), map32 (0xdf).
        Some(&b) => b & 0xf0 == 0x80 || matches!(b, 0xde | 0xdf),
        None => false,
    }
}

/// Returns `true` if the marker byte is a MessagePack fixstr.
fn is_fixstr(b: u8) -> bool {
    b & 0xe0 == 0xa0
}

/// Maximum nesting depth when skipping an unknown value.
///
/// Bounds recursion so a deeply nested unknown field cannot overflow the stack.
const MAX_SKIP_DEPTH: usize = 200;

/// Advances the cursor by `n` bytes, returning an error if fewer remain.
fn advance(r: &mut &[u8], n: usize, context: &'static str) -> Result<(), DecodeError> {
    if r.len() < n {
        return Err(DecodeError::Msgpack {
            context,
            detail: format!("expected {n} bytes but only {} remain", r.len()),
        });
    }
    *r = &r[n..];
    Ok(())
}

/// Reads `n` big-endian length bytes into a `usize`.
fn read_len(r: &mut &[u8], n: usize, context: &'static str) -> Result<usize, DecodeError> {
    if r.len() < n {
        return Err(DecodeError::Msgpack {
            context,
            detail: format!("expected {n} length bytes but only {} remain", r.len()),
        });
    }
    let mut len = 0usize;
    for &b in &r[..n] {
        len = (len << 8) | b as usize;
    }
    *r = &r[n..];
    Ok(len)
}

/// Skips over a single MessagePack value of any type without interpreting it.
///
/// This keeps the cursor aligned but does nothing else. Unknown *fields* must not be skipped with
/// this directly: any inline string they carry still occupies a slot in the payload's streaming
/// string table, so it has to be harvested. Use
/// [`value::harvest_unknown_field`](super::value::harvest_unknown_field) for that; it falls back to
/// this function for scalars, which cannot contain strings.
///
/// # Errors
///
/// Returns an error on truncated input, a reserved marker, or nesting deeper than
/// [`MAX_SKIP_DEPTH`].
pub fn skip_value(r: &mut &[u8], context: &'static str) -> Result<(), DecodeError> {
    skip_value_depth(r, context, 0)
}

fn skip_value_depth(r: &mut &[u8], context: &'static str, depth: usize) -> Result<(), DecodeError> {
    use rmp::Marker::*;

    if depth > MAX_SKIP_DEPTH {
        return Err(DecodeError::DepthExceeded { max: MAX_SKIP_DEPTH });
    }

    let marker = rmp::decode::read_marker(r).map_err(|e| DecodeError::Msgpack {
        context,
        detail: e.0.to_string(),
    })?;

    match marker {
        Null | True | False | FixPos(_) | FixNeg(_) => Ok(()),
        U8 | I8 => advance(r, 1, context),
        U16 | I16 => advance(r, 2, context),
        U32 | I32 | F32 => advance(r, 4, context),
        U64 | I64 | F64 => advance(r, 8, context),
        FixStr(n) => advance(r, n as usize, context),
        Str8 | Bin8 => {
            let n = read_len(r, 1, context)?;
            advance(r, n, context)
        }
        Str16 | Bin16 => {
            let n = read_len(r, 2, context)?;
            advance(r, n, context)
        }
        Str32 | Bin32 => {
            let n = read_len(r, 4, context)?;
            advance(r, n, context)
        }
        FixArray(n) => skip_seq(r, n as usize, context, depth),
        Array16 => {
            let n = read_len(r, 2, context)?;
            skip_seq(r, n, context, depth)
        }
        Array32 => {
            let n = read_len(r, 4, context)?;
            skip_seq(r, n, context, depth)
        }
        FixMap(n) => skip_seq(r, n as usize * 2, context, depth),
        Map16 => {
            let n = read_len(r, 2, context)?;
            skip_seq(r, n * 2, context, depth)
        }
        Map32 => {
            let n = read_len(r, 4, context)?;
            skip_seq(r, n * 2, context, depth)
        }
        // Extension types: a type byte plus a fixed or length-prefixed data payload.
        FixExt1 => advance(r, 1 + 1, context),
        FixExt2 => advance(r, 1 + 2, context),
        FixExt4 => advance(r, 1 + 4, context),
        FixExt8 => advance(r, 1 + 8, context),
        FixExt16 => advance(r, 1 + 16, context),
        Ext8 => {
            let n = read_len(r, 1, context)?;
            advance(r, 1 + n, context)
        }
        Ext16 => {
            let n = read_len(r, 2, context)?;
            advance(r, 1 + n, context)
        }
        Ext32 => {
            let n = read_len(r, 4, context)?;
            advance(r, 1 + n, context)
        }
        Reserved => Err(DecodeError::Msgpack {
            context,
            detail: "encountered reserved marker 0xc1".to_string(),
        }),
    }
}

/// Skips `count` consecutive values (array elements or flattened map key/value slots).
fn skip_seq(r: &mut &[u8], count: usize, context: &'static str, depth: usize) -> Result<(), DecodeError> {
    for _ in 0..count {
        skip_value_depth(r, context, depth + 1)?;
    }
    Ok(())
}
