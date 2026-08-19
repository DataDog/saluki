//! Errors produced while decoding the v1.0 APM trace wire format.

use snafu::Snafu;

/// An error encountered while decoding a v1.0 tracer payload.
#[derive(Debug, Snafu)]
#[snafu(context(suffix(false)))]
pub enum DecodeError {
    /// A low-level MessagePack read failed (truncated input, wrong marker type, invalid UTF-8, and so on).
    ///
    /// `context` names the field or structure being read when the failure occurred, and `detail`
    /// carries the underlying MessagePack decoder message.
    #[snafu(display("failed to read {context}: {detail}"))]
    Msgpack {
        /// The field or structure being read when the error occurred.
        context: &'static str,
        /// The underlying MessagePack decoder error message.
        detail: String,
    },

    /// An array or map header declared more elements than the decoder permits.
    #[snafu(display("{context} header too large: {len} exceeds maximum of {max}"))]
    OversizeHeader {
        /// The field or structure whose header was oversized.
        context: &'static str,
        /// The declared element count.
        len: u32,
        /// The maximum permitted element count.
        max: u32,
    },

    /// A streaming string referenced an index that has not yet been added to the string table.
    #[snafu(display("streaming string referenced unseen string index {index} (string table length: {len})"))]
    UnseenStringIndex {
        /// The out-of-range index that was referenced.
        index: u32,
        /// The current length of the string table.
        len: usize,
    },

    /// An attribute map's flat array length was not a multiple of three (`key`, `type`, `value`).
    #[snafu(display("invalid attribute array length {len} - must be a multiple of 3"))]
    InvalidAttributeArrayLen {
        /// The invalid element count.
        len: u32,
    },

    /// An `AnyValue` array's flat length was not a multiple of two (`type`, `value`).
    #[snafu(display("invalid array value length {len} - must be a multiple of 2"))]
    InvalidArrayValueLen {
        /// The invalid element count.
        len: u32,
    },

    /// An `AnyValue` carried a type discriminant the decoder does not recognize.
    #[snafu(display("unknown AnyValue type {value_type}"))]
    UnknownAnyValueType {
        /// The unrecognized type discriminant.
        value_type: u32,
    },

    /// `AnyValue` nesting exceeded the maximum permitted depth.
    #[snafu(display("AnyValue nesting depth exceeds maximum of {max}"))]
    DepthExceeded {
        /// The maximum permitted nesting depth.
        max: usize,
    },

    /// The tracer payload string table appeared after other fields had already populated it.
    ///
    /// The string table must be the first field so that later streaming-string references resolve.
    #[snafu(display("unexpected strings field: the string table must be sent first"))]
    StringsNotFirst,
}
