//! Serde deserialization for `format: duration` schema fields.
//!
//! The Datadog Agent hands a duration to ADP as an integer count of nanoseconds (Go's
//! `time.Duration` marshals to its `int64` nanosecond value) or as a string. A string is usually a
//! Go duration such as `"10s"`, but env vars and quoted config values also arrive as unit-less bare
//! numbers like `"30"`, which the Agent reads as nanoseconds. A single field must accept all three.
//! Deserializing here keeps that flexibility in the deserialization layer, so `DatadogConfiguration`
//! stores a real `std::time::Duration` and every downstream consumer (the witness, the translator)
//! receives an already-parsed value instead of a bare number it has to guess the unit of.

use std::fmt;
use std::time::Duration;

use go_duration::parse_duration_or_nanos;
use serde::de::{self, Deserializer, Visitor};

/// Deserialize a duration expressed as integer nanoseconds or a Go duration string.
///
/// A numeric value is nanoseconds (matching the wire encoding and the classifier's
/// `duration_value_as_nanos`); a string is parsed with `go_duration::parse_duration_or_nanos`,
/// which accepts a Go duration or a bare integer count of nanoseconds. Negative numeric values
/// clamp to zero since `Duration` cannot represent them, but a negative string is rejected.
pub(crate) fn deserialize_go_duration<'de, D>(deserializer: D) -> Result<Duration, D::Error>
where
    D: Deserializer<'de>,
{
    struct DurationVisitor;

    impl Visitor<'_> for DurationVisitor {
        type Value = Duration;

        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str(
                "a duration as integer nanoseconds, a Go duration string (for example \"10s\"), or a bare number of nanoseconds as a string",
            )
        }

        fn visit_u64<E: de::Error>(self, nanos: u64) -> Result<Duration, E> {
            Ok(Duration::from_nanos(nanos))
        }

        fn visit_i64<E: de::Error>(self, nanos: i64) -> Result<Duration, E> {
            Ok(Duration::from_nanos(nanos.max(0) as u64))
        }

        fn visit_f64<E: de::Error>(self, nanos: f64) -> Result<Duration, E> {
            Ok(Duration::from_nanos(if nanos < 0.0 { 0 } else { nanos as u64 }))
        }

        fn visit_str<E: de::Error>(self, text: &str) -> Result<Duration, E> {
            parse_duration_or_nanos(text).map_err(de::Error::custom)
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

    #[test]
    fn bare_number_string_is_nanoseconds() {
        // Env vars and quoted config values arrive as unit-less strings. The Agent reads `"30"` as
        // 30 nanoseconds, so this value must be accepted.
        assert_eq!(parse(r#"{"d": "30"}"#), Duration::from_nanos(30));
        assert_eq!(parse(r#"{"d": "0"}"#), Duration::ZERO);
    }

    #[test]
    fn negative_number_clamps_to_zero() {
        assert_eq!(parse(r#"{"d": -5}"#), Duration::ZERO);
    }

    #[test]
    fn invalid_string_is_rejected() {
        assert!(serde_json::from_str::<Holder>(r#"{"d": "not-a-duration"}"#).is_err());
        // Negative strings are rejected by the shared parser, while negative wire numbers are
        // clamped to zero.
        assert!(serde_json::from_str::<Holder>(r#"{"d": "-5"}"#).is_err());
    }
}
