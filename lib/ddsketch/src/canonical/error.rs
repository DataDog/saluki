//! Error types for protobuf conversion.

use std::fmt;

/// Errors that can occur during protobuf conversion.
#[derive(Debug, Clone, PartialEq)]
pub enum ProtoConversionError {
    /// The protobuf message is missing the required mapping field.
    MissingMapping,

    /// The gamma value in the protobuf does not match the expected gamma.
    GammaMismatch {
        /// The expected gamma value.
        expected: f64,
        /// The actual gamma value from the protobuf.
        actual: f64,
    },

    /// The index offset is non-zero, which is not supported by LogarithmicMapping.
    NonZeroIndexOffset {
        /// The actual index offset from the protobuf.
        actual: f64,
    },

    /// The interpolation mode is not supported.
    UnsupportedInterpolation {
        /// The actual interpolation mode value from the protobuf.
        actual: i32,
    },

    /// A bin count value is negative, which is invalid.
    NegativeBinCount {
        /// The bin index.
        index: i32,
        /// The negative count value.
        count: f64,
    },

    /// A bin count value is not a valid integer.
    NonIntegerBinCount {
        /// The bin index.
        index: i32,
        /// The non-integer count value.
        count: f64,
    },

    /// The zero count is negative.
    NegativeZeroCount {
        /// The negative zero count value.
        count: f64,
    },

    /// The zero count is not a valid integer.
    NonIntegerZeroCount {
        /// The non-integer zero count value.
        count: f64,
    },
}

impl fmt::Display for ProtoConversionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingMapping => write!(f, "protobuf message is missing required mapping field"),
            Self::GammaMismatch { expected, actual } => {
                write!(f, "gamma mismatch: expected {}, got {}", expected, actual)
            }
            Self::NonZeroIndexOffset { actual } => {
                write!(f, "non-zero index offset not supported: {}", actual)
            }
            Self::UnsupportedInterpolation { actual } => {
                write!(f, "unsupported interpolation mode: {}", actual)
            }
            Self::NegativeBinCount { index, count } => {
                write!(f, "negative bin count at index {}: {}", index, count)
            }
            Self::NonIntegerBinCount { index, count } => {
                write!(f, "non-integer bin count at index {}: {}", index, count)
            }
            Self::NegativeZeroCount { count } => {
                write!(f, "negative zero count: {}", count)
            }
            Self::NonIntegerZeroCount { count } => {
                write!(f, "non-integer zero count: {}", count)
            }
        }
    }
}

impl std::error::Error for ProtoConversionError {}
