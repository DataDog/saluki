//! Serde deserialization for `format: duration` schema fields.
//!
//! The Datadog Agent hands a duration to ADP in one of two shapes: an integer count of nanoseconds
//! (Go's `time.Duration` marshals to its `int64` nanosecond value), or a Go duration string such as
//! `"10s"` that a user wrote in config. A single field must accept both. Deserializing here keeps
//! that duality in the deserialization layer, so `DatadogConfiguration` stores a real
//! `std::time::Duration` and every downstream consumer (the witness, the translator) receives an
//! already-parsed value instead of a bare number it has to guess the unit of.

use std::fmt;
use std::time::Duration;

use go_duration::{parse_duration, ParseDurationError};
use serde::de::{self, Deserializer, Visitor};

/// Largest nanosecond count we accept, matching the Agent's cap: Go's `time.Duration` is an `int64`,
/// so `i64::MAX` nanoseconds is the largest representable value. This mirrors the bound enforced by
/// `saluki_config::DurationString`, the type this deserializer replaced on the typed config path.
const MAX_NANOS_U64: u64 = i64::MAX as u64;

/// Deserialize a duration expressed as integer nanoseconds or a Go duration string.
///
/// A numeric value is nanoseconds (matching the wire encoding and the classifier's
/// `duration_value_as_nanos`); a string is parsed with the shared `go-duration` parser.
///
/// Acceptance and rejection match `saluki_config::DurationString`, the type this replaced on the
/// typed config path: negative numeric and negative string durations are rejected (not clamped to
/// zero), and numeric values above the `time.Duration` bound (`i64::MAX` nanoseconds) are rejected
/// as overflow.
pub(crate) fn deserialize_go_duration<'de, D>(deserializer: D) -> Result<Duration, D::Error>
where
    D: Deserializer<'de>,
{
    struct DurationVisitor;

    impl Visitor<'_> for DurationVisitor {
        type Value = Duration;

        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("a duration as integer nanoseconds or a Go duration string (for example \"10s\")")
        }

        fn visit_u64<E: de::Error>(self, nanos: u64) -> Result<Duration, E> {
            if nanos > MAX_NANOS_U64 {
                return Err(E::custom(ParseDurationError::Overflow));
            }
            Ok(Duration::from_nanos(nanos))
        }

        fn visit_u128<E: de::Error>(self, nanos: u128) -> Result<Duration, E> {
            if nanos > MAX_NANOS_U64 as u128 {
                return Err(E::custom(ParseDurationError::Overflow));
            }
            Ok(Duration::from_nanos(nanos as u64))
        }

        fn visit_i64<E: de::Error>(self, nanos: i64) -> Result<Duration, E> {
            if nanos < 0 {
                return Err(E::custom(ParseDurationError::Negative));
            }
            Ok(Duration::from_nanos(nanos as u64))
        }

        fn visit_i128<E: de::Error>(self, nanos: i128) -> Result<Duration, E> {
            if nanos < 0 {
                return Err(E::custom(ParseDurationError::Negative));
            }
            if nanos > MAX_NANOS_U64 as i128 {
                return Err(E::custom(ParseDurationError::Overflow));
            }
            Ok(Duration::from_nanos(nanos as u64))
        }

        fn visit_f64<E: de::Error>(self, nanos: f64) -> Result<Duration, E> {
            if !nanos.is_finite() {
                return Err(E::custom("duration nanoseconds must be finite"));
            }
            if nanos < 0.0 {
                return Err(E::custom(ParseDurationError::Negative));
            }
            if nanos > MAX_NANOS_U64 as f64 {
                return Err(E::custom(ParseDurationError::Overflow));
            }
            Ok(Duration::from_nanos(nanos as u64))
        }

        fn visit_str<E: de::Error>(self, text: &str) -> Result<Duration, E> {
            parse_duration(text).map_err(de::Error::custom)
        }

        fn visit_string<E: de::Error>(self, text: String) -> Result<Duration, E> {
            self.visit_str(&text)
        }
    }

    deserializer.deserialize_any(DurationVisitor)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(serde::Deserialize)]
    struct Holder {
        #[serde(deserialize_with = "deserialize_go_duration")]
        d: Duration,
    }

    fn parse(json: &str) -> Duration {
        serde_json::from_str::<Holder>(json).unwrap().d
    }

    #[test]
    fn number_is_nanoseconds() {
        assert_eq!(parse(r#"{"d": 10000000000}"#), Duration::from_secs(10));
        assert_eq!(parse(r#"{"d": 0}"#), Duration::ZERO);
    }

    #[test]
    fn string_is_go_duration() {
        assert_eq!(parse(r#"{"d": "10s"}"#), Duration::from_secs(10));
        assert_eq!(parse(r#"{"d": "1m30s"}"#), Duration::from_secs(90));
        assert_eq!(parse(r#"{"d": "0s"}"#), Duration::ZERO);
    }

    fn err(json: &str) -> String {
        match serde_json::from_str::<Holder>(json) {
            Ok(_) => panic!("expected an error deserializing {json}"),
            Err(e) => e.to_string(),
        }
    }

    #[test]
    fn negative_number_is_rejected() {
        // Legacy `DurationString` rejected negative numeric durations; the typed path must not
        // silently clamp them to zero (which would disable duration-gated features like host-tag
        // enrichment).
        assert!(err(r#"{"d": -5}"#).contains("negative"));
        assert!(err(r#"{"d": -1}"#).contains("negative"));
        assert!(err(r#"{"d": -0.5}"#).contains("negative"));
    }

    #[test]
    fn negative_string_is_rejected() {
        assert!(err(r#"{"d": "-1s"}"#).contains("negative"));
        assert!(err(r#"{"d": "-0.5h"}"#).contains("negative"));
    }

    #[test]
    fn overflow_number_is_rejected() {
        // `i64::MAX` nanoseconds is the largest value Go's `time.Duration` can represent, and the
        // largest `DurationString` accepted. One past it must be rejected, not truncated.
        let max = i64::MAX as u64;
        assert_eq!(parse(&format!(r#"{{"d": {max}}}"#)), Duration::from_nanos(max));
        assert!(err(&format!(r#"{{"d": {}}}"#, max + 1)).contains("exceeds"));
        assert!(err(r#"{"d": 18446744073709551615}"#).contains("exceeds"));
    }

    #[test]
    fn overflow_string_is_rejected() {
        assert!(err(r#"{"d": "9223372036854775808ns"}"#).contains("exceeds"));
    }

    #[test]
    fn invalid_string_is_rejected() {
        assert!(serde_json::from_str::<Holder>(r#"{"d": "not-a-duration"}"#).is_err());
    }
}
