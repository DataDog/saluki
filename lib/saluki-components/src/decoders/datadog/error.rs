//! Errors produced while decoding the Datadog APM trace wire formats.

use snafu::Snafu;

/// An error encountered while decoding a tracer payload.
#[derive(Debug, Snafu)]
#[snafu(context(suffix(false)))]
pub enum DecodeError {
    /// The input ended before the value being read was complete.
    ///
    /// The trace-agent's receiver counts short input apart from other decoding errors (`reason:msgp_short_bytes`).
    #[snafu(display("failed to read {context}: unexpected end of input"))]
    ShortBytes {
        /// The field or structure being read when the input ran out.
        context: &'static str,
    },

    /// A JSON body ended before its first value was complete.
    ///
    /// The counterpart of the `io.EOF` and `io.ErrUnexpectedEOF` that Go's JSON decoder returns, which the
    /// trace-agent's receiver counts apart from other decoding errors (`error:unexpected-eof`).
    #[snafu(display("unexpected end of JSON input"))]
    UnexpectedEof,

    /// A JSON body isn't valid JSON, or isn't a list of lists of spans.
    #[snafu(display("failed to decode JSON: {detail}"))]
    Json {
        /// What was wrong with the body.
        detail: String,
    },

    /// A low-level MessagePack read failed (wrong marker type, invalid UTF-8, out-of-range number, and so on).
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

    /// An array or map header declared more elements than the remaining input could possibly back.
    #[snafu(display("{context} declares {len} entries but only {remaining} byte(s) remain"))]
    ImplausibleHeaderCount {
        /// The field or structure whose header declared the implausible count.
        context: &'static str,
        /// The declared element count.
        len: u32,
        /// The number of bytes remaining on the cursor after the header.
        remaining: usize,
    },

    /// A streaming string referenced an index that has not yet been added to the string table.
    #[snafu(display("streaming string referenced unseen string index {index} (string table length: {len})"))]
    UnseenStringIndex {
        /// The out-of-range index that was referenced.
        index: u32,
        /// The current length of the string table.
        len: usize,
    },

    /// A v0.5 string reference was past the end of the payload's dictionary.
    #[snafu(display("dictionary index {index} out of range"))]
    DictionaryIndexOutOfRange {
        /// The out-of-range index that was referenced.
        index: u32,
    },

    /// The payload's string references resolve to more string bytes than its size allows: 512 times its length, and at
    /// least 1 MiB. The reference receiver has no such limit, which only payloads built to amplify meet.
    #[snafu(display("string references resolve to more than {limit} bytes"))]
    ResolvedStringsTooLarge {
        /// The payload's bound, in bytes.
        limit: usize,
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

    /// The input contained bytes after the top-level tracer payload was fully decoded.
    #[snafu(display("{len} trailing byte(s) after the tracer payload"))]
    TrailingBytes {
        /// The number of unconsumed bytes remaining after decoding.
        len: usize,
    },
}
