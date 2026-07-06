//! A duration configuration value compatible with the Agent.
//!
//! The Agent loads configuration via [spf13/viper][viper], which uses [spf13/cast][cast] to coerce YAML/JSON/env
//! values into Go's [`time.Duration`][go-duration]. [`DurationString`] reproduces that coercion so ADP accepts the
//! same inputs and interprets them the same way.
//!
//! [viper]: https://github.com/spf13/viper
//! [cast]: https://github.com/spf13/cast
//! [go-duration]: https://pkg.go.dev/time#ParseDuration

use std::fmt;
use std::fmt::{Debug, Display, Formatter};
use std::str::FromStr;
use std::time::Duration;

use go_duration::{
    checked_duration_from_nanos_f64, checked_duration_from_nanos_i128, checked_duration_from_nanos_u128,
    parse_viper_duration,
};
pub use go_duration::{parse_duration, ParseDurationError};
use serde::de::{self, Deserializer, Visitor};
use serde::{Deserialize, Serialize, Serializer};

/// A duration value that deserializes from the formats accepted by the Agent's configuration loader.
///
/// # Deserialization
///
/// Accepted inputs:
///
/// - Strings with Go time-unit suffixes: `"30s"`, `"1h30m"`, `"250ms"`, `"2h45m30s"`, `"1.5h"`. Valid suffixes: `ns`,
///   `us`, `µs`, `μs`, `ms`, `s`, `m`, `h`.
///
/// - Strings containing only a bare integer: `"5"` is 5 **nanoseconds**. This matches vipers `cast.ToDurationE`'s
///   fallback for unit-less string values.
///
/// - Integer numbers: `5` is 5 **nanoseconds**.
///
/// - Floating-point numbers: `5.0` is 5 **nanoseconds** (truncated toward zero).
///
/// Negative durations (for example `"-1h"`) are rejected because [`std::time::Duration`] can't represent them.
///
/// # Bare numbers are nanoseconds, not seconds (!!)
///
/// A configuration value like `expected_tags_duration: 30` means 30 **nanoseconds**, not 30 seconds. Use `"30s"` for
/// 30 seconds. This matches the Agent's `time.Duration` coercion.
///
/// # Serialization
///
/// Serializes as `"{seconds}s{nanoseconds}ns"`. For example, 30 seconds becomes `"30s0ns"` and 30.5 seconds becomes
/// `"30s500000000ns"`. Whole seconds are maximized and the nanosecond component is always less than `1_000_000_000`,
/// so the form is unambiguous and round-trips through this parser and would be accepted by the Agent as well.
#[derive(Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DurationString(Duration);

impl Debug for DurationString {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        Debug::fmt(&self.0, f)
    }
}

impl DurationString {
    /// Creates a new `DurationString` wrapping the given [`Duration`].
    pub const fn new(d: Duration) -> Self {
        Self(d)
    }

    /// Returns the underlying [`Duration`].
    pub const fn as_duration(&self) -> Duration {
        self.0
    }
}

impl From<Duration> for DurationString {
    fn from(d: Duration) -> Self {
        Self(d)
    }
}

impl From<DurationString> for Duration {
    fn from(d: DurationString) -> Self {
        d.0
    }
}

impl std::ops::Deref for DurationString {
    type Target = Duration;

    fn deref(&self) -> &Duration {
        &self.0
    }
}

impl FromStr for DurationString {
    type Err = ParseDurationError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        parse_viper_duration(s).map(Self)
    }
}

impl Display for DurationString {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "{}s{}ns", self.0.as_secs(), self.0.subsec_nanos())
    }
}

impl Serialize for DurationString {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for DurationString {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(DurationStringVisitor)
    }
}

struct DurationStringVisitor;

impl<'de> Visitor<'de> for DurationStringVisitor {
    type Value = DurationString;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a duration string (e.g. \"30s\", \"1h30m\") or a non-negative number of nanoseconds")
    }

    fn visit_i64<E: de::Error>(self, n: i64) -> Result<DurationString, E> {
        checked_duration_from_nanos_i128(n as i128)
            .map(DurationString)
            .map_err(E::custom)
    }

    fn visit_i128<E: de::Error>(self, n: i128) -> Result<DurationString, E> {
        checked_duration_from_nanos_i128(n)
            .map(DurationString)
            .map_err(E::custom)
    }

    fn visit_u64<E: de::Error>(self, n: u64) -> Result<DurationString, E> {
        checked_duration_from_nanos_u128(n as u128)
            .map(DurationString)
            .map_err(E::custom)
    }

    fn visit_u128<E: de::Error>(self, n: u128) -> Result<DurationString, E> {
        checked_duration_from_nanos_u128(n)
            .map(DurationString)
            .map_err(E::custom)
    }

    fn visit_f64<E: de::Error>(self, f: f64) -> Result<DurationString, E> {
        checked_duration_from_nanos_f64(f)
            .map(DurationString)
            .map_err(E::custom)
    }

    fn visit_str<E: de::Error>(self, s: &str) -> Result<DurationString, E> {
        parse_viper_duration(s).map(DurationString).map_err(E::custom)
    }

    fn visit_string<E: de::Error>(self, s: String) -> Result<DurationString, E> {
        self.visit_str(&s)
    }
}

#[cfg(test)]
mod tests {
    use anyhow::Context as _;
    use serde_json::json;

    use super::*;

    const NS: Duration = Duration::from_nanos(1);
    const _US: Duration = Duration::from_micros(1);
    const MS: Duration = Duration::from_millis(1);
    const S: Duration = Duration::from_secs(1);
    const M: Duration = Duration::from_secs(60);
    const H: Duration = Duration::from_secs(3600);

    #[test]
    fn deserialize_integer_succeeds() {
        let json = r#"{ "value": 15 }"#;
        let deserialized: SerdeTest = serde_json::from_str(json).unwrap();
        assert_eq!(deserialized.value.as_duration(), 15 * NS);
    }

    /// Interesting test case because the 1.5 is interpreted as nanoseconds then truncated to 1 ns in the Duration
    #[test]
    fn deserialize_float_succeeds() {
        let json = r#"{ "value": 1.5 }"#;
        let deserialized: SerdeTest = serde_json::from_str(json).unwrap();
        assert_eq!(deserialized.value.as_duration(), 1 * NS);
    }

    #[derive(Default, Debug, Clone, Copy, Eq, PartialEq, Deserialize, Serialize)]
    struct SerdeTest {
        value: DurationString,
    }

    impl From<Duration> for SerdeTest {
        fn from(value: Duration) -> Self {
            Self { value: value.into() }
        }
    }

    fn test_json(input_value: &str) -> String {
        json!({"value": input_value}).to_string()
    }

    fn test_yaml(input_value: &str) -> String {
        format!("value: {input_value}")
    }

    fn run_success_case(input: &str, expected: Duration, serialized: &str) -> anyhow::Result<()> {
        let expected_struct: SerdeTest = expected.into();
        let json = test_json(input);
        let yaml = test_yaml(input);
        let msg = format!("failure for duration test case '{input}'");
        let parsed_duration = DurationString::from_str(input).context(msg.clone())?;
        anyhow::ensure!(
            expected == parsed_duration.as_duration(),
            "{msg}, expected: {expected:?}, got {:?}",
            parsed_duration.as_duration()
        );
        let deserialized_from_json: SerdeTest = serde_json::from_str(&json).context(msg.clone())?;
        anyhow::ensure!(
            expected_struct == deserialized_from_json,
            "{msg}, expected: {expected_struct:?}, got {deserialized_from_json:?}"
        );
        let roundtrip_json = serde_json::from_str(&serde_json::to_string(&expected_struct)?)?;
        anyhow::ensure!(
            expected_struct == roundtrip_json,
            "{msg}, expected json roundrip to produce {expected_struct:?}, but got {roundtrip_json:?}"
        );
        let deserialized_from_yaml: SerdeTest = serde_yaml::from_str(&yaml).context(msg.clone())?;
        anyhow::ensure!(
            expected_struct == deserialized_from_yaml,
            "{msg}, expected: {expected_struct:?}, got {deserialized_from_yaml:?}"
        );
        let roundtrip_yaml = serde_yaml::from_str(&serde_yaml::to_string(&expected_struct)?)?;
        anyhow::ensure!(
            expected_struct == roundtrip_yaml,
            "{msg}, expected json roundrip to produce {expected_struct:?}, but got {roundtrip_yaml:?}"
        );
        let actual_serialized = parsed_duration.to_string();
        anyhow::ensure!(
            serialized == actual_serialized,
            "Expected the input '{input}' to be serialized as '{serialized}' but got '{actual_serialized}'"
        );
        Ok(())
    }

    #[test]
    fn parse_duration_supports_go_style_units() {
        assert_eq!(parse_duration("10s").unwrap(), Duration::from_secs(10));
        assert_eq!(parse_duration("1m0s").unwrap(), Duration::from_secs(60));
        assert_eq!(parse_duration("500ms").unwrap(), Duration::from_millis(500));
        assert_eq!(
            parse_duration("1h2m3.5s").unwrap(),
            Duration::from_secs(3723) + Duration::from_millis(500)
        );
        assert_eq!(parse_duration("250us").unwrap(), Duration::from_micros(250));
        assert_eq!(parse_duration("250µs").unwrap(), Duration::from_micros(250));
        assert_eq!(parse_duration("250μs").unwrap(), Duration::from_micros(250));
    }

    #[test]
    fn parse_duration_rejects_config_only_values() {
        assert!(parse_duration("").is_err());
        assert!(parse_duration("abc").is_err());
        assert!(parse_duration("10").is_err());
        assert!(parse_duration(" 10s").is_err());
        assert!(parse_duration("1xs").is_err());
    }

    #[test]
    fn duration_string_success_cases() {
        let cases: &[(&str, Duration, &str)] = &[
            ("0", Duration::ZERO, "0s0ns"),
            ("-0", Duration::ZERO, "0s0ns"),
            ("+0", Duration::ZERO, "0s0ns"),
            ("+5h", Duration::from_hours(5), "18000s0ns"),
            (".5s", Duration::from_millis(500), "0s500000000ns"),
            ("5.s", Duration::from_secs(5), "5s0ns"),
            ("0.000000001s", Duration::from_nanos(1), "0s1ns"),
            ("1.5h", Duration::from_mins(90), "5400s0ns"),
            (
                "2h45m30.5s",
                (2 * H) + (45 * M) + (30 * S) + (500 * MS),
                "9930s500000000ns",
            ),
            ("12µs", Duration::from_micros(12), "0s12000ns"),
            ("12μs", Duration::from_micros(12), "0s12000ns"),
            ("0s", Duration::ZERO, "0s0ns"),
            ("1h1m1s1ms1us1ns", H + M + S + MS + (1000 * NS) + NS, "3661s1001001ns"),
            ("24h", Duration::from_hours(24), "86400s0ns"),
            (
                "9223372036854775807ns",
                Duration::from_nanos(9223372036854775807),
                "9223372036s854775807ns",
            ),
            (
                "9223372036854775.807us",
                Duration::from_secs(9223372036) + (854775807 * NS),
                "9223372036s854775807ns",
            ),
            (
                "2562047h47m16.854775807s",
                Duration::from_secs(9223372036) + (854775807 * NS),
                "9223372036s854775807ns",
            ),
            ("0.1ns", Duration::ZERO, "0s0ns"),
            ("05s", Duration::from_secs(5), "5s0ns"),
            ("1ns1s", S + NS, "1s1ns"),
            ("100h100m100s", (100 * H) + (100 * M) + (100 * S), "366100s0ns"),
            ("5m32s", (5 * M) + (32 * S), "332s0ns"),
            ("1m0s", M, "60s0ns"),
            ("5m0s", 5 * M, "300s0ns"),
            ("6m0s", 6 * M, "360s0ns"),
            ("10m0s", 10 * M, "600s0ns"),
            ("15m0s", 15 * M, "900s0ns"),
            ("30m0s", 30 * M, "1800s0ns"),
            ("40m0s", 40 * M, "2400s0ns"),
            ("50m0s", 50 * M, "3000s0ns"),
            ("87600h0m0s", 87600 * H, "315360000s0ns"),
            ("5", 5 * NS, "0s5ns"),
            (" 5s", 5 * S, "5s0ns"),
            ("5s", 5 * S, "5s0ns"),
        ];

        for (input, expected, serialized) in cases {
            run_success_case(input, *expected, serialized).unwrap();
        }
    }

    fn run_failure_case(input: &str, expected_msg: &str) -> anyhow::Result<()> {
        let result = DurationString::from_str(input);
        match result {
            Ok(value) => {
                anyhow::bail!("Expected an error when parsing '{input}', but instead received the value '{value:?}'")
            }
            Err(e) => {
                anyhow::ensure!(
                    e.to_string().contains(expected_msg),
                    "Expected the error message when parsing '{input}' to contain {expected_msg:?}, but the message is {e}"
                );
            }
        }

        Ok(())
    }

    #[test]
    fn duration_string_failure_cases() {
        let cases: &[(&str, &str)] = &[
            ("5m32sFOO", "unknown unit 'sFOO'"),
            ("", "empty duration"),
            (" ", "empty duration"),
            ("+", "empty duration"),
            ("-", "empty duration"),
            (".", "expected digits"),
            ("s", "expected digits"),
            (".s", "expected digits"),
            ("--5s", "expected digits"),
            ("5.5.5s", "missing unit"),
            ("1e3s", "unknown unit 'e'"),
            ("5ns5", "missing unit"),
            ("9223372036854775808ns", "exceeds"),
            ("-1s", "negative"),
            ("-0.5h", "negative"),
            ("1d", "unknown unit 'd'"),
            ("1w", "unknown unit 'w'"),
            ("1S", "unknown unit 'S'"),
            ("12 µs", "missing unit"),
            ("5 s", "missing unit"),
            ("5. s", "missing unit"),
        ];

        for (input, expected_msg) in cases {
            run_failure_case(input, expected_msg).unwrap();
        }
    }
}
