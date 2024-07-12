//! A module to compute the binary size of data once encoded
//!
//! This module is used primilarly when implementing the `MessageWrite::get_size`

/// Wire type.
pub enum WireType {
    /// Variable-width integer.
    ///
    /// Encodes integers using a variable number of bytes, depending on the magnitude of the value,
    /// consuming between one and ten bytes on the wire.
    ///
    /// See https://protobuf.dev/programming-guides/encoding/#varints for more information.
    Varint,

    /// Fixed 64-bit integer or double-precision floating-point number.
    ///
    /// Consumes eight bytes (64-bit) on the wire.
    Fixed64,

    /// Length-delimiter field.
    ///
    /// Used for fields with variable length, such as strings, bytes, embedded messages, and packed
    /// repeated fields.
    LengthDelimited,

    /// Fixed 32-bit integer or single-precision floating-point number.
    ///
    /// Consumes four bytes (32-bit) on the wire.
    Fixed32,
}

impl WireType {
    /// Gets the integer representation of the wire type.
    pub const fn as_u32(&self) -> u32 {
        match self {
            WireType::Varint => 0,
            WireType::Fixed64 => 1,
            WireType::LengthDelimited => 2,
            WireType::Fixed32 => 5,
        }
    }
}

/// Computes the tag for the given field number and wire type.
pub const fn tag(field_number: u32, wire_type: WireType) -> u32 {
    (field_number << 3) | wire_type.as_u32()
}

/// Computes the binary size of the varint encoded u64
///
/// https://developers.google.com/protocol-buffers/docs/encoding
pub fn sizeof_varint(v: u64) -> usize {
    match v {
        0x0..=0x7F => 1,
        0x80..=0x3FFF => 2,
        0x4000..=0x1FFFFF => 3,
        0x200000..=0xFFFFFFF => 4,
        0x10000000..=0x7FFFFFFFF => 5,
        0x0800000000..=0x3FFFFFFFFFF => 6,
        0x040000000000..=0x1FFFFFFFFFFFF => 7,
        0x02000000000000..=0xFFFFFFFFFFFFFF => 8,
        0x0100000000000000..=0x7FFFFFFFFFFFFFFF => 9,
        _ => 10,
    }
}

/// Computes the binary size of a string
///
/// The total size is the varint encoded length size plus the length itself
/// https://developers.google.com/protocol-buffers/docs/encoding
pub fn sizeof_str(s: &str) -> usize {
    sizeof_len(s.len())
}

/// Computes the binary size of a byte slice
///
/// The total size is the varint encoded length size plus the length itself
/// https://developers.google.com/protocol-buffers/docs/encoding
pub fn sizeof_bytes(b: &[u8]) -> usize {
    sizeof_len(b.len())
}

/// Computes the binary size of a variable length chunk of data (wire type 2)
///
/// The total size is the varint encoded length size plus the length itself
/// https://developers.google.com/protocol-buffers/docs/encoding
pub fn sizeof_len(len: usize) -> usize {
    sizeof_varint(len as u64) + len
}

/// Computes the binary size of the varint encoded i32
pub fn sizeof_int32(v: i32) -> usize {
    sizeof_varint(v as u64)
}

/// Computes the binary size of the varint encoded i64
pub fn sizeof_int64(v: i64) -> usize {
    sizeof_varint(v as u64)
}

/// Computes the binary size of the varint encoded uint32
pub fn sizeof_uint32(v: u32) -> usize {
    sizeof_varint(v as u64)
}

/// Computes the binary size of the varint encoded uint64
pub fn sizeof_uint64(v: u64) -> usize {
    sizeof_varint(v)
}

/// Computes the binary size of the varint encoded sint32
pub fn sizeof_sint32(v: i32) -> usize {
    sizeof_varint(((v << 1) ^ (v >> 31)) as u64)
}

/// Computes the binary size of the varint encoded sint64
pub fn sizeof_sint64(v: i64) -> usize {
    sizeof_varint(((v << 1) ^ (v >> 63)) as u64)
}

/// Computes the binary size of the varint encoded bool (always = 1)
pub fn sizeof_bool(_: bool) -> usize {
    1
}

/// Computes the binary size of the fixed size f32 (always = 4)
pub fn sizeof_f32(_: f32) -> usize {
    4
}

/// Computes the binary size of the fixed size f64 (always = 8)
pub fn sizeof_f64(_: f64) -> usize {
    8
}

/// Computes the binary size of the varint encoded enum
pub fn sizeof_enum(v: i32) -> usize {
    sizeof_int32(v)
}

/// A simple container for holding an owned or borrowed value.
///
/// While practically identical to `Cow`, this type has no requirements for `T` being `Clone`, and
/// so can be used purely for generalizing over owned and borrowed variants of a value.
pub enum Repeatable<'a, T> {
    /// A single owned value.
    Owned(T),

    /// A single borrowed value.
    Borrowed(&'a T),
}

impl<T> From<T> for Repeatable<'_, T> {
    fn from(v: T) -> Self {
        Repeatable::Owned(v)
    }
}

impl<'a, T> From<&'a T> for Repeatable<'a, T> {
    fn from(v: &'a T) -> Self {
        Repeatable::Borrowed(v)
    }
}
