//! Low-level MessagePack read helpers for the Datadog APM trace wire formats.
//!
//! Each helper operates on a `&mut &[u8]` cursor: reads advance the slice in place, mirroring the
//! byte-slice threading (`o []byte`) used by the reference Go decoder. Failures are mapped to
//! [`DecodeError`] with a static context string naming the field being read. Input that ends early
//! maps to [`DecodeError::ShortBytes`], so that callers can count it apart, as the trace-agent counts
//! short input apart.
//!
//! The `v0_*` helpers reproduce the trace-agent's v0.x reads, including which numeric types they
//! tolerate and which failures count as short input.

use std::cell::Cell;
use std::collections::HashMap;
use std::fmt::Display;
use std::hash::{BuildHasher, Hash};

use rmp::decode::{bytes::BytesReadError, DecodeStringError, NumValueReadError, ValueReadError};

use super::error::DecodeError;

/// Maximum element count permitted in any array or map header.
///
/// Protects the decoder from payloads that declare an enormous collection size to force large
/// allocations. Matches the reference decoder's `maxSize` (25,000,000). This alone doesn't stop a
/// tiny payload from claiming a count near this ceiling; [`read_array_len`] and [`read_map_len`]
/// additionally reject counts that couldn't possibly be backed by the remaining input, via
/// [`check_slab_count`].
pub const MAX_SIZE: u32 = 25_000_000;

/// Maximum number of elements pre-allocated from a wire count.
///
/// A count only proves that as many bytes remain, and a decoded element is far larger than one wire byte, so sizing a
/// collection from the count alone would let a small payload reserve many times its size. Collections start at most
/// this large and grow as elements actually decode.
pub const MAX_PREALLOCATED_ELEMENTS: usize = 1024;

/// Returns the capacity to pre-allocate for a collection whose header declares `count` elements.
pub fn preallocation(count: u32) -> usize {
    (count as usize).min(MAX_PREALLOCATED_ELEMENTS)
}

/// How many bytes of strings a payload's string references may resolve to, per byte of the payload.
///
/// The v0.5 and v1.0 formats send each string once and then refer to it by index, so a few bytes can stand for a
/// string of any length. The decoders share one copy of a string between its references, but the sharing doesn't
/// survive the pipeline: each resolved string is hashed into an attribute map, the encoder interns every span's
/// strings, and transforms such as SQL obfuscation turn one shared input into a separate output per span. So both the
/// work and the downstream memory a payload costs follow the bytes its references resolve to, not its own size. The
/// bound caps them at a multiple of the payload's size. It doesn't bound the cost of strings the payload spells out,
/// which already grows with its size, nor the per-span cost of what isn't a string. The reference receiver keeps the
/// references as indexes end to end and has no such limit: this one is a deliberate deviation, which only payloads
/// built to amplify meet.
///
/// A legitimate payload stays below it. A typical span costs about 130 bytes on the wire and resolves to about 500 (a
/// ratio near 4). Long strings shared across spans push the ratio up: a batch of identical database spans, each
/// referencing a 5 KB query as its resource, costs about 50 bytes a span in v0.5 and resolves to about 5 KB (a ratio
/// near 100), and about 200 if each span also repeats the query as a tag. The limit is five times the first and more
/// than twice the second, and bounds a hostile payload at the maximum request size (25 MiB) to about 13 GB of
/// resolved strings.
pub const RESOLVED_BYTES_PER_PAYLOAD_BYTE: usize = 512;

/// The fewest bytes of strings a payload's string references may resolve to, however small the payload.
pub const MIN_RESOLVED_BYTES: usize = 1 << 20;

/// Bounds the total length of the strings a payload's string references resolve to.
///
/// The bound is [`RESOLVED_BYTES_PER_PAYLOAD_BYTE`] times the payload's length, and at least [`MIN_RESOLVED_BYTES`].
pub struct ResolvedBytes {
    remaining: Cell<usize>,
    limit: usize,
}

impl ResolvedBytes {
    /// Creates the budget of a payload of `payload_len` bytes.
    pub fn for_payload(payload_len: usize) -> Self {
        let limit = payload_len
            .saturating_mul(RESOLVED_BYTES_PER_PAYLOAD_BYTE)
            .max(MIN_RESOLVED_BYTES);
        Self {
            remaining: Cell::new(limit),
            limit,
        }
    }

    /// Records a resolved string of `len` bytes.
    ///
    /// # Errors
    ///
    /// Returns [`DecodeError::ResolvedStringsTooLarge`] once the payload's references resolve to more than its bound.
    pub fn charge(&self, len: usize) -> Result<(), DecodeError> {
        match self.remaining.get().checked_sub(len) {
            Some(remaining) => {
                self.remaining.set(remaining);
                Ok(())
            }
            None => Err(DecodeError::ResolvedStringsTooLarge { limit: self.limit }),
        }
    }
}

/// Releases the capacity a map reserved with [`preallocation`] when far fewer entries decoded.
///
/// Repeated keys, `nil` ones included, collapse into one entry, so a map can end up with one entry and the capacity of
/// [`MAX_PREALLOCATED_ELEMENTS`], kept for as long as its span lives. A map that holds what its header declared isn't
/// touched.
pub fn release_unused_capacity<K: Eq + Hash, V, S: BuildHasher>(map: &mut HashMap<K, V, S>) {
    if map.len() * 4 < map.capacity() {
        map.shrink_to_fit();
    }
}

/// Conservative lower bound on the wire size of one array element: every element is at least a
/// one-byte `nil` or fixint.
const MIN_BYTES_PER_ARRAY_ELEMENT: u32 = 1;

/// Conservative lower bound on the wire size of one map entry: a key and a value, each at least
/// one byte.
const MIN_BYTES_PER_MAP_ENTRY: u32 = 2;

/// Maximum nesting depth when skipping an unknown value.
///
/// Bounds recursion so a deeply nested unknown field cannot overflow the stack. Matches default OTel limit.
const MAX_SKIP_DEPTH: usize = 64;

/// An `rmp` read error that can tell whether the input ran out. Reading from a byte slice can only fail to read a
/// marker or data when the slice is exhausted.
trait ReadError: Display {
    fn is_short(&self) -> bool;
}

impl ReadError for ValueReadError {
    fn is_short(&self) -> bool {
        matches!(self, Self::InvalidMarkerRead(_) | Self::InvalidDataRead(_))
    }
}

impl ReadError for NumValueReadError {
    fn is_short(&self) -> bool {
        matches!(self, Self::InvalidMarkerRead(_) | Self::InvalidDataRead(_))
    }
}

impl ReadError for DecodeStringError<'_, BytesReadError> {
    fn is_short(&self) -> bool {
        matches!(
            self,
            Self::InvalidMarkerRead(_) | Self::InvalidDataRead(_) | Self::BufferSizeTooSmall(_)
        )
    }
}

/// Builds a closure that maps an `rmp` decoder error into a [`DecodeError`] carrying the given static
/// context: [`DecodeError::ShortBytes`] when the input ran out, [`DecodeError::Msgpack`] otherwise.
fn mp<E: ReadError>(context: &'static str) -> impl FnOnce(E) -> DecodeError {
    move |e| {
        if e.is_short() {
            DecodeError::ShortBytes { context }
        } else {
            DecodeError::Msgpack {
                context,
                detail: e.to_string(),
            }
        }
    }
}

/// Builds a [`DecodeError::Msgpack`] for a value of the wrong type or out of range.
fn invalid(context: &'static str, detail: impl Into<String>) -> DecodeError {
    DecodeError::Msgpack {
        context,
        detail: detail.into(),
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
/// Returns [`DecodeError::ShortBytes`] if `len` exceeds the number of bytes remaining on the cursor,
/// as the trace-agent does, or [`DecodeError::ImplausibleHeaderCount`] if
/// only `len * min_bytes_per_entry` does.
fn check_slab_count(
    len: u32, min_bytes_per_entry: u32, remaining: &[u8], context: &'static str,
) -> Result<(), DecodeError> {
    if u64::from(len) > remaining.len() as u64 {
        return Err(DecodeError::ShortBytes { context });
    }
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
    read_array_len_with_minimum(r, MIN_BYTES_PER_ARRAY_ELEMENT, context)
}

/// Reads an array header and checks that the remaining input can plausibly contain its elements.
///
/// `min_bytes_per_element` is a format-specific plausibility estimate for the average encoded size
/// of each element. Use [`read_array_len`] when the element type does not have a tighter bound than
/// one byte.
///
/// # Errors
///
/// Returns an error if the next value is not an array header, if the declared length exceeds
/// [`MAX_SIZE`], or if the declared elements would require more bytes than remain on the cursor.
pub fn read_array_len_with_minimum(
    r: &mut &[u8], min_bytes_per_element: u32, context: &'static str,
) -> Result<u32, DecodeError> {
    let len = rmp::decode::read_array_len(r).map_err(mp(context))?;
    if len > MAX_SIZE {
        return Err(DecodeError::OversizeHeader {
            context,
            len,
            max: MAX_SIZE,
        });
    }
    check_slab_count(len, min_bytes_per_element, r, context)?;
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
    Ok(take(r, len, context)?.to_vec())
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

/// Takes the next `n` bytes off the cursor, returning [`DecodeError::ShortBytes`] if fewer remain.
fn take<'a>(r: &mut &'a [u8], n: usize, context: &'static str) -> Result<&'a [u8], DecodeError> {
    if r.len() < n {
        return Err(DecodeError::ShortBytes { context });
    }
    let (head, tail) = r.split_at(n);
    *r = tail;
    Ok(head)
}

/// Advances the cursor by `n` bytes, returning an error if fewer remain.
fn advance(r: &mut &[u8], n: usize, context: &'static str) -> Result<(), DecodeError> {
    take(r, n, context).map(|_| ())
}

/// Reads `n` big-endian length bytes into a `usize`.
fn read_len(r: &mut &[u8], n: usize, context: &'static str) -> Result<usize, DecodeError> {
    Ok(take(r, n, context)?
        .iter()
        .fold(0usize, |len, &b| (len << 8) | b as usize))
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

    let marker = rmp::decode::read_marker(r).map_err(|_| DecodeError::ShortBytes { context })?;

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

/// Element-count limit of the trace-agent's v0.x decoders.
pub const V0_MAX_COLLECTION_SIZE: u32 = 500_000;

/// Reads an array header as the trace-agent does: `nil` is a type error, a count over
/// [`V0_MAX_COLLECTION_SIZE`] is too long, and a count over the remaining bytes is short input.
pub fn v0_read_array_len(r: &mut &[u8], context: &'static str) -> Result<u32, DecodeError> {
    let len = rmp::decode::read_array_len(r).map_err(mp(context))?;
    v0_check_header_len(len, V0_MAX_COLLECTION_SIZE, r, context)
}

/// Reads the v0.5 dictionary's array header as the trace-agent does: as [`v0_read_array_len`], but with the string
/// table's larger limit, [`MAX_SIZE`].
pub fn v0_read_dictionary_len(r: &mut &[u8], context: &'static str) -> Result<u32, DecodeError> {
    let len = rmp::decode::read_array_len(r).map_err(mp(context))?;
    v0_check_header_len(len, MAX_SIZE, r, context)
}

/// Reads a map header as the trace-agent does, with the same checks as [`v0_read_array_len`].
pub fn v0_read_map_len(r: &mut &[u8], context: &'static str) -> Result<u32, DecodeError> {
    let len = rmp::decode::read_map_len(r).map_err(mp(context))?;
    v0_check_header_len(len, V0_MAX_COLLECTION_SIZE, r, context)
}

fn v0_check_header_len(len: u32, max: u32, remaining: &[u8], context: &'static str) -> Result<u32, DecodeError> {
    if len > max {
        return Err(DecodeError::OversizeHeader { context, len, max });
    }
    if u64::from(len) > remaining.len() as u64 {
        return Err(DecodeError::ShortBytes { context });
    }
    Ok(len)
}

/// Checks a map's entry count against the remaining bytes, as the trace-agent does before it
/// pre-allocates the entries: each entry takes at least two bytes.
pub fn v0_check_slab_count(len: u32, remaining: &[u8], context: &'static str) -> Result<(), DecodeError> {
    if u64::from(len) * u64::from(MIN_BYTES_PER_MAP_ENTRY) > remaining.len() as u64 {
        return Err(DecodeError::ImplausibleHeaderCount {
            context,
            len,
            remaining: remaining.len(),
        });
    }
    Ok(())
}

/// Returns `true` if the next value on the cursor is `nil`. Does not advance the cursor.
pub fn peek_is_nil(r: &&[u8]) -> bool {
    r.first() == Some(&0xc0)
}

/// Consumes a `nil` the caller has already peeked.
pub fn skip_nil(r: &mut &[u8]) {
    debug_assert!(peek_is_nil(r));
    *r = &r[1..];
}

/// An integer as the trace-agent classifies it: signed (which includes positive fixints) or
/// unsigned (the `uint8` to `uint64` markers).
enum WireInt {
    Signed(i64),
    Unsigned(u64),
}

fn is_int_marker(b: u8) -> bool {
    matches!(b, 0x00..=0x7f | 0xe0..=0xff | 0xcc..=0xd3)
}

fn take_array<const N: usize>(r: &mut &[u8], context: &'static str) -> Result<[u8; N], DecodeError> {
    Ok(take(r, N, context)?.try_into().expect("take returns exactly N bytes"))
}

/// Reads an integer of any width. The caller has checked that the next marker is an integer.
fn read_wire_int(r: &mut &[u8], context: &'static str) -> Result<WireInt, DecodeError> {
    let marker = take(r, 1, context)?[0];
    Ok(match marker {
        0x00..=0x7f => WireInt::Signed(i64::from(marker)),
        0xe0..=0xff => WireInt::Signed(i64::from(marker as i8)),
        0xcc => WireInt::Unsigned(u64::from(take_array::<1>(r, context)?[0])),
        0xcd => WireInt::Unsigned(u64::from(u16::from_be_bytes(take_array(r, context)?))),
        0xce => WireInt::Unsigned(u64::from(u32::from_be_bytes(take_array(r, context)?))),
        0xcf => WireInt::Unsigned(u64::from_be_bytes(take_array(r, context)?)),
        0xd0 => WireInt::Signed(i64::from(take_array::<1>(r, context)?[0] as i8)),
        0xd1 => WireInt::Signed(i64::from(i16::from_be_bytes(take_array(r, context)?))),
        0xd2 => WireInt::Signed(i64::from(i32::from_be_bytes(take_array(r, context)?))),
        0xd3 => WireInt::Signed(i64::from_be_bytes(take_array(r, context)?)),
        other => unreachable!("read_wire_int called on marker {other:#04x}"),
    })
}

/// The leading step of the trace-agent's v0.x number reads: `nil` reads as zero, and a value that
/// isn't an integer is a type error. An empty cursor is a type error too, not short input, as in the
/// trace-agent.
fn v0_parse_int(r: &mut &[u8], context: &'static str) -> Result<Option<WireInt>, DecodeError> {
    match r.first() {
        Some(0xc0) => {
            skip_nil(r);
            Ok(None)
        }
        Some(&b) if is_int_marker(b) => read_wire_int(r, context).map(Some),
        Some(&b) => Err(invalid(context, format!("expected an integer, found marker {b:#04x}"))),
        None => Err(invalid(context, "expected an integer, found the end of input")),
    }
}

/// Reads a `uint64` as the trace-agent does: `nil` is zero, and a signed integer is
/// reinterpreted as unsigned, since some tracers have no unsigned types.
pub fn v0_parse_u64(r: &mut &[u8], context: &'static str) -> Result<u64, DecodeError> {
    Ok(match v0_parse_int(r, context)? {
        None => 0,
        Some(WireInt::Signed(i)) => i as u64,
        Some(WireInt::Unsigned(u)) => u,
    })
}

/// Reads an `int64` as the trace-agent does: `nil` is zero, and an unsigned integer
/// above `i64::MAX` is an error.
pub fn v0_parse_i64(r: &mut &[u8], context: &'static str) -> Result<i64, DecodeError> {
    match v0_parse_int(r, context)? {
        None => Ok(0),
        Some(WireInt::Signed(i)) => Ok(i),
        Some(WireInt::Unsigned(u)) => i64::try_from(u).map_err(|_| invalid(context, "found uint64, overflows int64")),
    }
}

/// Reads an `int32` as the trace-agent does: `nil` is zero, and a value outside the
/// `int32` range is an error.
pub fn v0_parse_i32(r: &mut &[u8], context: &'static str) -> Result<i32, DecodeError> {
    match v0_parse_int(r, context)? {
        None => Ok(0),
        Some(WireInt::Signed(i)) => i32::try_from(i).map_err(|_| invalid(context, "integer overflows int32")),
        Some(WireInt::Unsigned(u)) => i32::try_from(u).map_err(|_| invalid(context, "found uint32, overflows int32")),
    }
}

/// Reads a `float64` as the trace-agent does: `nil` is zero and integers of any width convert, but a
/// `float32` is a type error.
pub fn v0_parse_f64(r: &mut &[u8], context: &'static str) -> Result<f64, DecodeError> {
    if r.first() == Some(&0xcb) {
        *r = &r[1..];
        return Ok(f64::from_be_bytes(take_array(r, context)?));
    }
    Ok(match v0_parse_int(r, context)? {
        None => 0.0,
        Some(WireInt::Signed(i)) => i as f64,
        Some(WireInt::Unsigned(u)) => u as f64,
    })
}

/// Reads a `uint64` as `msgp.ReadUint64Bytes`: a non-negative integer of any width. `nil` and
/// negative integers are errors.
pub fn v0_read_u64(r: &mut &[u8], context: &'static str) -> Result<u64, DecodeError> {
    match r.first() {
        None => Err(DecodeError::ShortBytes { context }),
        Some(&b) if is_int_marker(b) => match read_wire_int(r, context)? {
            WireInt::Signed(i) => u64::try_from(i).map_err(|_| invalid(context, "negative integer for a uint")),
            WireInt::Unsigned(u) => Ok(u),
        },
        Some(&b) => Err(invalid(context, format!("expected a uint, found marker {b:#04x}"))),
    }
}

/// Reads a `uint32` as `msgp.ReadUint32Bytes`.
pub fn v0_read_u32(r: &mut &[u8], context: &'static str) -> Result<u32, DecodeError> {
    u32::try_from(v0_read_u64(r, context)?).map_err(|_| invalid(context, "integer overflows uint32"))
}

/// Reads an `int64` as `msgp.ReadInt64Bytes`: an integer of any width up to `i64::MAX`.
pub fn v0_read_i64(r: &mut &[u8], context: &'static str) -> Result<i64, DecodeError> {
    match r.first() {
        None => Err(DecodeError::ShortBytes { context }),
        Some(&b) if is_int_marker(b) => match read_wire_int(r, context)? {
            WireInt::Signed(i) => Ok(i),
            WireInt::Unsigned(u) => i64::try_from(u).map_err(|_| invalid(context, "uint64 overflows int64")),
        },
        Some(&b) => Err(invalid(context, format!("expected an int, found marker {b:#04x}"))),
    }
}

/// Reads a `float64` strictly, as the trace-agent does for some fields: a `float64` or a `float32`, but not an integer.
///
/// Like the trace-agent, it checks the length before the marker: with fewer than nine bytes left, anything but a
/// complete `float32` is short input, whatever its marker.
pub fn v0_read_f64(r: &mut &[u8], context: &'static str) -> Result<f64, DecodeError> {
    if r.len() < 9 && !(r.len() >= 5 && r[0] == 0xca) {
        return Err(DecodeError::ShortBytes { context });
    }
    match r[0] {
        0xcb => {
            *r = &r[1..];
            Ok(f64::from_be_bytes(take_array(r, context)?))
        }
        0xca => {
            *r = &r[1..];
            Ok(f64::from(f32::from_be_bytes(take_array(r, context)?)))
        }
        b => Err(invalid(context, format!("expected a float, found marker {b:#04x}"))),
    }
}

/// Reads the length of a `str` or `bin` value, or returns `None` if the next value is neither.
fn read_str_or_bin_len(r: &mut &[u8], context: &'static str) -> Result<Option<usize>, DecodeError> {
    let Some(&marker) = r.first() else {
        return Ok(None);
    };
    let width = match marker {
        0xa0..=0xbf => {
            *r = &r[1..];
            return Ok(Some(usize::from(marker & 0x1f)));
        }
        0xd9 | 0xc4 => 1,
        0xda | 0xc5 => 2,
        0xdb | 0xc6 => 4,
        _ => return Ok(None),
    };
    *r = &r[1..];
    read_len(r, width, context).map(Some)
}

/// Reads a string's raw bytes as the trace-agent does: `nil` is the empty string, and both `str` and
/// `bin` values are accepted. The bytes may not be valid UTF-8. An empty cursor is a type error.
pub fn v0_read_str_bytes<'a>(r: &mut &'a [u8], context: &'static str) -> Result<&'a [u8], DecodeError> {
    if peek_is_nil(r) {
        skip_nil(r);
        return Ok(&[]);
    }
    match read_str_or_bin_len(r, context)? {
        Some(len) => take(r, len, context),
        None => Err(invalid(context, "expected a string")),
    }
}

/// Reads a map key's raw bytes as the trace-agent does: a `str` or `bin` value. `nil` is an error.
pub fn v0_read_map_key<'a>(r: &mut &'a [u8], context: &'static str) -> Result<&'a [u8], DecodeError> {
    if r.is_empty() {
        return Err(DecodeError::ShortBytes { context });
    }
    match read_str_or_bin_len(r, context)? {
        Some(len) => take(r, len, context),
        None => Err(invalid(context, "expected a string map key")),
    }
}

/// Converts bytes to a string as the trace-agent does: each byte that doesn't start a
/// valid UTF-8 sequence becomes one U+FFFD replacement character. This differs from
/// `String::from_utf8_lossy`, which replaces a whole invalid sequence with a single character.
pub fn repair_utf8(mut bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() + 2);
    loop {
        match simdutf8::compat::from_utf8(bytes) {
            Ok(valid) => {
                out.push_str(valid);
                return out;
            }
            Err(e) => {
                let (valid, rest) = bytes.split_at(e.valid_up_to());
                out.push_str(simdutf8::basic::from_utf8(valid).expect("prefix is valid UTF-8"));
                out.push(char::REPLACEMENT_CHARACTER);
                bytes = &rest[1..];
            }
        }
    }
}
