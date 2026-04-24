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

use serde::de::{self, Deserializer, Visitor};
use serde::{Deserialize, Serialize, Serializer};
use snafu::Snafu;

/// A duration value that deserializes from the formats accepted by the Agent's configuration loader.
///
/// # Deserialization
///
/// Accepted inputs:
///
/// - Strings with Go time-unit suffixes: `"30s"`, `"1h30m"`, `"250ms"`, `"2h45m30s"`, `"1.5h"`. Valid suffixes: `ns`,
///   `us`, `µs`, `ms`, `s`, `m`, `h`.
///
/// - Strings containing only a bare integer: `"5"` is 5 **nanoseconds**. This matches vipers `cast.ToDurationE`'s
///   fallback for unit-less string values.
///
/// - Integer numbers: `5` is 5 **nanoseconds**.
///
/// - Floating-point numbers: `5.0` is 5 **nanoseconds** (truncated toward zero).
///
/// Negative durations (for example `"-1h"`) are rejected because [`std::time::Duration`] cannot represent them.
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
        parse_string(s).map(Self)
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
        if n < 0 {
            return Err(E::custom(ParseDurationError::Negative));
        }
        Ok(DurationString(Duration::from_nanos(n as u64)))
    }

    fn visit_i128<E: de::Error>(self, n: i128) -> Result<DurationString, E> {
        if n < 0 {
            return Err(E::custom(ParseDurationError::Negative));
        }
        if n > MAX_NANOS_U64 as i128 {
            return Err(E::custom(ParseDurationError::Overflow));
        }
        Ok(DurationString(Duration::from_nanos(n as u64)))
    }

    fn visit_u64<E: de::Error>(self, n: u64) -> Result<DurationString, E> {
        if n > MAX_NANOS_U64 {
            return Err(E::custom(ParseDurationError::Overflow));
        }
        Ok(DurationString(Duration::from_nanos(n)))
    }

    fn visit_u128<E: de::Error>(self, n: u128) -> Result<DurationString, E> {
        if n > MAX_NANOS_U64 as u128 {
            return Err(E::custom(ParseDurationError::Overflow));
        }
        Ok(DurationString(Duration::from_nanos(n as u64)))
    }

    fn visit_f64<E: de::Error>(self, f: f64) -> Result<DurationString, E> {
        if !f.is_finite() {
            return Err(E::custom("duration nanoseconds must be finite"));
        }
        if f < 0.0 {
            return Err(E::custom(ParseDurationError::Negative));
        }
        if f > MAX_NANOS_U64 as f64 {
            return Err(E::custom(ParseDurationError::Overflow));
        }
        Ok(DurationString(Duration::from_nanos(f as u64)))
    }

    fn visit_str<E: de::Error>(self, s: &str) -> Result<DurationString, E> {
        parse_string(s).map(DurationString).map_err(E::custom)
    }

    fn visit_string<E: de::Error>(self, s: String) -> Result<DurationString, E> {
        self.visit_str(&s)
    }
}

/// Maximum number of nanoseconds we will accept, matching the Agent's cap (Go's `time.Duration` is `int64`, so
/// `i64::MAX` nanoseconds is the largest representable value).
const MAX_NANOS_U64: u64 = i64::MAX as u64;

/// Error returned when a duration value cannot be parsed.
#[derive(Debug, Snafu)]
pub enum ParseDurationError {
    /// The value was syntactically invalid.
    #[snafu(display("invalid duration '{}': {}", input, reason))]
    Invalid {
        /// The original input string.
        input: String,
        /// Reason the input was rejected.
        reason: String,
    },
    /// The value parsed to a negative duration.
    #[snafu(display("negative durations are not supported"))]
    Negative,
    /// The value exceeds the range of [`std::time::Duration`] as nanoseconds.
    #[snafu(display("duration value exceeds supported range"))]
    Overflow,
}

fn invalid(input: &str, reason: impl Into<String>) -> ParseDurationError {
    ParseDurationError::Invalid {
        input: input.to_string(),
        reason: reason.into(),
    }
}

/// Parses a string using viper/cast precedence: try matching Go's `time.ParseDuration` first (with our
/// `parse_duration`, then fall back to a bare integer (treated as nanoseconds).
fn parse_string(s: &str) -> Result<Duration, ParseDurationError> {
    let trimmed = s.trim();
    match parse_duration(trimmed) {
        Ok(d) => Ok(d),
        Err(err) => match trimmed.parse::<i128>() {
            Ok(n) if n < 0 => Err(ParseDurationError::Negative),
            Ok(n) => {
                if n > MAX_NANOS_U64 as i128 {
                    return Err(ParseDurationError::Overflow);
                }
                Ok(Duration::from_nanos(n as u64))
            }
            Err(_) => Err(err),
        },
    }
}

/// Parses a string in the exact format accepted by Go's `time.ParseDuration`, restricted to non-negative values
/// (since [`std::time::Duration`] cannot represent negatives).
fn parse_duration(s: &str) -> Result<Duration, ParseDurationError> {
    let orig = s;
    let mut rest = s;
    let mut total_ns: u128 = 0;
    let mut negative = false;

    if let Some(c) = rest.chars().next() {
        if c == '+' || c == '-' {
            negative = c == '-';
            rest = &rest[1..];
        }
    }

    // Special case: "0" alone (possibly after a sign) is zero.
    if rest == "0" {
        return Ok(Duration::ZERO);
    }
    if rest.is_empty() {
        return Err(invalid(orig, "empty duration"));
    }

    while !rest.is_empty() {
        let (int_part, after_int) = consume_digits(rest);
        let had_int = !int_part.is_empty();

        let (frac_part, after_frac) = if let Some(stripped) = after_int.strip_prefix('.') {
            consume_digits(stripped)
        } else {
            ("", after_int)
        };
        let consumed_dot = after_int.starts_with('.');
        let had_frac = consumed_dot && !frac_part.is_empty();

        if !had_int && !had_frac {
            return Err(invalid(orig, "expected digits"));
        }

        rest = after_frac;

        let unit_str = consume_unit(rest);
        if unit_str.is_empty() {
            return Err(invalid(orig, "missing unit"));
        }
        rest = &rest[unit_str.len()..];

        let unit_ns: u128 = match unit_str {
            "ns" => 1,
            "us" | "µs" => 1_000,
            "ms" => 1_000_000,
            "s" => 1_000_000_000,
            "m" => 60 * 1_000_000_000,
            "h" => 3_600 * 1_000_000_000,
            other => return Err(invalid(orig, format!("unknown unit '{}'", other))),
        };

        let int_val: u128 = if int_part.is_empty() {
            0
        } else {
            int_part
                .parse::<u128>()
                .map_err(|_| invalid(orig, "integer overflow"))?
        };

        let mut ns = int_val.checked_mul(unit_ns).ok_or_else(|| invalid(orig, "overflow"))?;

        if !frac_part.is_empty() {
            // Truncate the fraction to at most 18 digits to keep the intermediate u128 math well within range. 18
            // decimal digits of precision is well beyond nanoseconds for every supported unit.
            let keep = frac_part.len().min(18);
            let frac_digits = &frac_part[..keep];
            let mut scale: u128 = 1;
            for _ in 0..keep {
                scale *= 10;
            }
            let f: u128 = frac_digits
                .parse::<u128>()
                .map_err(|_| invalid(orig, "invalid fractional"))?;
            let frac_ns = f.checked_mul(unit_ns).ok_or_else(|| invalid(orig, "overflow"))? / scale;
            ns = ns.checked_add(frac_ns).ok_or_else(|| invalid(orig, "overflow"))?;
        }

        total_ns = total_ns.checked_add(ns).ok_or_else(|| invalid(orig, "overflow"))?;
    }

    if negative && total_ns != 0 {
        return Err(ParseDurationError::Negative);
    }

    if total_ns > MAX_NANOS_U64 as u128 {
        return Err(ParseDurationError::Overflow);
    }
    Ok(Duration::from_nanos(total_ns as u64))
}

fn consume_digits(s: &str) -> (&str, &str) {
    let end = s.bytes().take_while(|b| b.is_ascii_digit()).count();
    s.split_at(end)
}

fn consume_unit(s: &str) -> &str {
    let mut end = 0;
    for (i, c) in s.char_indices() {
        if c.is_ascii_alphabetic() || c == 'µ' {
            end = i + c.len_utf8();
        } else {
            break;
        }
    }
    &s[..end]
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

    /// Interesting test case because the 1.5 is interpreted as nanoseconds then truncated to 1ns in the Duration
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
    fn duration_string_success_01() {
        run_success_case("0", Duration::default(), "0s0ns").unwrap();
    }

    #[test]
    fn duration_string_success_02() {
        run_success_case("-0", Duration::default(), "0s0ns").unwrap();
    }

    #[test]
    fn duration_string_success_03() {
        run_success_case("+0", Duration::default(), "0s0ns").unwrap();
    }

    #[test]
    fn duration_string_success_04() {
        run_success_case("+5h", Duration::from_hours(5), "18000s0ns").unwrap();
    }

    #[test]
    fn duration_string_success_05() {
        run_success_case(".5s", Duration::from_millis(500), "0s500000000ns").unwrap();
    }

    #[test]
    fn duration_string_success_06() {
        run_success_case("5.s", Duration::from_secs(5), "5s0ns").unwrap();
    }

    #[test]
    fn duration_string_success_07() {
        run_success_case("0.000000001s", Duration::from_nanos(1), "0s1ns").unwrap();
    }

    #[test]
    fn duration_string_success_08() {
        run_success_case("1.5h", Duration::from_mins(90), "5400s0ns").unwrap();
    }

    #[test]
    fn duration_string_success_09() {
        run_success_case(
            "2h45m30.5s",
            (2 * H) + (45 * M) + (30 * S) + (500 * MS),
            "9930s500000000ns",
        )
        .unwrap();
    }

    #[test]
    fn duration_string_success_10() {
        run_success_case("12µs", Duration::from_micros(12), "0s12000ns").unwrap();
    }

    #[test]
    fn duration_string_success_11() {
        run_success_case("0s", Duration::default(), "0s0ns").unwrap();
    }

    #[test]
    fn duration_string_success_12() {
        run_success_case("1h1m1s1ms1us1ns", H + M + S + MS + (1000 * NS) + NS, "3661s1001001ns").unwrap();
    }

    #[test]
    fn duration_string_success_13() {
        run_success_case("24h", Duration::from_hours(24), "86400s0ns").unwrap();
    }

    #[test]
    fn duration_string_success_14() {
        run_success_case(
            "9223372036854775807ns",
            Duration::from_nanos(9223372036854775807),
            "9223372036s854775807ns",
        )
        .unwrap();
    }

    #[test]
    fn duration_string_success_15() {
        run_success_case(
            "9223372036854775.807us",
            Duration::from_secs(9223372036) + (854775807 * NS),
            "9223372036s854775807ns",
        )
        .unwrap();
    }

    #[test]
    fn duration_string_success_16() {
        run_success_case(
            "2562047h47m16.854775807s",
            Duration::from_secs(9223372036) + (854775807 * NS),
            "9223372036s854775807ns",
        )
        .unwrap();
    }

    #[test]
    fn duration_string_success_17() {
        run_success_case("0.1ns", Duration::default(), "0s0ns").unwrap();
    }

    #[test]
    fn duration_string_success_18() {
        run_success_case("05s", Duration::from_secs(5), "5s0ns").unwrap();
    }

    #[test]
    fn duration_string_success_19() {
        run_success_case("1ns1s", S + NS, "1s1ns").unwrap();
    }

    #[test]
    fn duration_string_success_20() {
        run_success_case("100h100m100s", (100 * H) + (100 * M) + (100 * S), "366100s0ns").unwrap();
    }

    #[test]
    fn duration_string_success_21() {
        run_success_case("5m32s", (5 * M) + (32 * S), "332s0ns").unwrap();
    }

    #[test]
    fn duration_string_success_22() {
        run_success_case("0s", Duration::default(), "0s0ns").unwrap();
    }

    #[test]
    fn duration_string_success_23() {
        run_success_case("1m0s", M, "60s0ns").unwrap();
    }

    #[test]
    fn duration_string_success_24() {
        run_success_case("5m0s", 5 * M, "300s0ns").unwrap();
    }

    #[test]
    fn duration_string_success_25() {
        run_success_case("6m0s", 6 * M, "360s0ns").unwrap();
    }

    #[test]
    fn duration_string_success_26() {
        run_success_case("10m0s", 10 * M, "600s0ns").unwrap();
    }

    #[test]
    fn duration_string_success_27() {
        run_success_case("15m0s", 15 * M, "900s0ns").unwrap();
    }

    #[test]
    fn duration_string_success_28() {
        run_success_case("30m0s", 30 * M, "1800s0ns").unwrap();
    }

    #[test]
    fn duration_string_success_29() {
        run_success_case("40m0s", 40 * M, "2400s0ns").unwrap();
    }

    #[test]
    fn duration_string_success_30() {
        run_success_case("50m0s", 50 * M, "3000s0ns").unwrap();
    }

    #[test]
    fn duration_string_success_31() {
        run_success_case("87600h0m0s", 87600 * H, "315360000s0ns").unwrap();
    }

    #[test]
    fn duration_string_success_32() {
        run_success_case("5", 5 * NS, "0s5ns").unwrap();
    }

    #[test]
    fn duration_string_success_33() {
        run_success_case(" 5s", 5 * S, "5s0ns").unwrap();
    }

    #[test]
    fn duration_string_success_34() {
        run_success_case("5s", 5 * S, "5s0ns").unwrap();
    }

    #[test]
    fn duration_string_success_35() {
        run_success_case("5.s", 5 * S, "5s0ns").unwrap();
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
    fn duration_string_failure_01() {
        run_failure_case("5m32sFOO", "unknown unit 'sFOO'").unwrap();
    }

    #[test]
    fn duration_string_failure_02() {
        run_failure_case("", "empty duration").unwrap();
    }

    #[test]
    fn duration_string_failure_03() {
        run_failure_case(" ", "empty duration").unwrap();
    }

    #[test]
    fn duration_string_failure_04() {
        run_failure_case("+", "empty duration").unwrap();
    }

    #[test]
    fn duration_string_failure_05() {
        run_failure_case("-", "empty duration").unwrap();
    }

    #[test]
    fn duration_string_failure_06() {
        run_failure_case(".", "expected digits").unwrap();
    }

    #[test]
    fn duration_string_failure_07() {
        run_failure_case("s", "expected digits").unwrap();
    }

    #[test]
    fn duration_string_failure_08() {
        run_failure_case(".s", "expected digits").unwrap();
    }

    #[test]
    fn duration_string_failure_09() {
        run_failure_case("--5s", "expected digits").unwrap();
    }

    #[test]
    fn duration_string_failure_10() {
        run_failure_case("5.5.5s", "missing unit").unwrap();
    }

    #[test]
    fn duration_string_failure_11() {
        run_failure_case("1e3s", "unknown unit 'e'").unwrap();
    }

    #[test]
    fn duration_string_failure_12() {
        run_failure_case("5ns5", "missing unit").unwrap();
    }

    #[test]
    fn duration_string_failure_13() {
        run_failure_case("9223372036854775808ns", "exceeds").unwrap();
    }

    #[test]
    fn duration_string_failure_14() {
        run_failure_case("-1s", "negative").unwrap();
    }

    #[test]
    fn duration_string_failure_15() {
        run_failure_case("-0.5h", "negative").unwrap();
    }

    #[test]
    fn duration_string_failure_16() {
        run_failure_case("1d", "unknown unit 'd'").unwrap();
    }

    #[test]
    fn duration_string_failure_17() {
        run_failure_case("1w", "unknown unit 'w'").unwrap();
    }

    #[test]
    fn duration_string_failure_18() {
        run_failure_case("1S", "unknown unit 'S'").unwrap();
    }

    #[test]
    fn duration_string_failure_19() {
        run_failure_case("12 µs", "missing unit").unwrap();
    }

    #[test]
    fn duration_string_failure_20() {
        run_failure_case("5 s", "missing unit").unwrap();
    }

    #[test]
    fn duration_string_failure_21() {
        run_failure_case("5. s", "missing unit").unwrap();
    }
}
