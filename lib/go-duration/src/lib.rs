//! Go `time.Duration` string parsing.
//!
//! A small, dependency-free primitive that parses duration strings in the exact format accepted by Go's
//! [`time.ParseDuration`][go-duration]. The Datadog Agent (via [spf13/viper][viper] and [spf13/cast][cast]) coerces
//! configuration values into Go durations, and several places in Saluki need to interpret those same strings,
//! including the runtime configuration layer and build-time config-schema codegen. This crate is the single owner of
//! that algorithm so it isn't duplicated in each of those places.
//!
//! Two layers are provided. [`parse_duration`] is the strict Go grammar primitive. [`parse_viper_duration`] wraps it
//! with the viper/cast coercion the Agent actually applies to configuration values (trim whitespace, then fall back
//! to interpreting a bare integer as nanoseconds). Consumers that must accept exactly what the Agent accepts should
//! use [`parse_viper_duration`] so that coercion lives in one place instead of being re-derived per call site.
//!
//! [go-duration]: https://pkg.go.dev/time#ParseDuration
//! [viper]: https://github.com/spf13/viper
//! [cast]: https://github.com/spf13/cast
#![deny(warnings)]
#![deny(missing_docs)]

use std::error::Error;
use std::fmt::{self, Display, Formatter};
use std::time::Duration;

/// Maximum number of nanoseconds we accept, matching the Agent's cap: Go's `time.Duration` is an `int64`, so
/// `i64::MAX` nanoseconds is the largest representable value.
pub const MAX_NANOS_U64: u64 = i64::MAX as u64;

/// Error returned when a duration string can't be parsed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ParseDurationError {
    /// The value was syntactically invalid.
    Invalid {
        /// The original input string.
        input: String,
        /// Reason the input was rejected.
        reason: String,
    },
    /// The value parsed to a negative duration, which [`std::time::Duration`] can't represent.
    Negative,
    /// The value exceeds the range of [`std::time::Duration`] as nanoseconds.
    Overflow,
}

impl Display for ParseDurationError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            ParseDurationError::Invalid { input, reason } => {
                write!(f, "invalid duration '{}': {}", input, reason)
            }
            ParseDurationError::Negative => write!(f, "negative durations are not supported"),
            ParseDurationError::Overflow => write!(f, "duration value exceeds supported range"),
        }
    }
}

impl Error for ParseDurationError {}

fn invalid(input: &str, reason: impl Into<String>) -> ParseDurationError {
    ParseDurationError::Invalid {
        input: input.to_string(),
        reason: reason.into(),
    }
}

/// Parses a string in the exact format accepted by Go's `time.ParseDuration`, restricted to non-negative values
/// (since [`std::time::Duration`] can't represent negatives).
///
/// Accepts a decimal number with a required unit suffix, optionally repeated (for example, `"300ms"`, `"1h30m"`,
/// `"2h45m30.5s"`), with an optional leading sign. Valid units are `ns`, `us` (or `µs`/`μs`), `ms`, `s`, `m`, and `h`.
/// A bare `0` (optionally signed) is accepted as zero.
///
/// # Errors
///
/// Returns [`ParseDurationError`] if the string is empty, has a missing or unknown unit, contains no digits, parses to
/// a negative duration, or overflows the supported nanosecond range.
pub fn parse_duration(s: &str) -> Result<Duration, ParseDurationError> {
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
            "us" | "µs" | "μs" => 1_000,
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

/// Parses a duration the way the Datadog Agent's configuration loader does.
///
/// The Agent loads configuration through [spf13/viper][viper], which coerces values via [spf13/cast][cast]'s
/// `cast.ToDurationE`. For a string that means: trim surrounding whitespace, try Go's `time.ParseDuration` grammar
/// ([`parse_duration`]), and if that fails, fall back to interpreting the entire string as a bare integer count of
/// nanoseconds. This is the coercion ADP must reproduce to accept the same configuration the Agent does, including a
/// unit-less string such as `"5"` (5 nanoseconds), env-var input like `DD_EXPECTED_TAGS_DURATION=5`, and
/// whitespace-padded input like `" 5s"`.
///
/// Negative values and values beyond the `time.Duration` bound ([`MAX_NANOS_U64`] nanoseconds) are rejected in both
/// the Go-grammar and the bare-integer path.
///
/// [viper]: https://github.com/spf13/viper
/// [cast]: https://github.com/spf13/cast
pub fn parse_viper_duration(s: &str) -> Result<Duration, ParseDurationError> {
    let trimmed = s.trim();
    match parse_duration(trimmed) {
        Ok(d) => Ok(d),
        // Go's grammar rejected it; fall back to viper's bare-integer-nanoseconds interpretation. Only substitute
        // the fallback when the string is actually an integer; otherwise surface the original grammar error.
        Err(grammar_err) => match trimmed.parse::<i128>() {
            Ok(nanos) => checked_duration_from_nanos_i128(nanos),
            Err(_) => Err(grammar_err),
        },
    }
}

/// Converts a signed nanosecond count into a [`Duration`], rejecting negative values and values beyond the
/// `time.Duration` bound ([`MAX_NANOS_U64`] nanoseconds).
pub fn checked_duration_from_nanos_i128(nanos: i128) -> Result<Duration, ParseDurationError> {
    if nanos < 0 {
        return Err(ParseDurationError::Negative);
    }
    if nanos > MAX_NANOS_U64 as i128 {
        return Err(ParseDurationError::Overflow);
    }
    Ok(Duration::from_nanos(nanos as u64))
}

/// Converts an unsigned nanosecond count into a [`Duration`], rejecting values beyond the `time.Duration` bound
/// ([`MAX_NANOS_U64`] nanoseconds).
pub fn checked_duration_from_nanos_u128(nanos: u128) -> Result<Duration, ParseDurationError> {
    if nanos > MAX_NANOS_U64 as u128 {
        return Err(ParseDurationError::Overflow);
    }
    Ok(Duration::from_nanos(nanos as u64))
}

/// Converts a floating-point nanosecond count into a [`Duration`], rejecting non-finite, negative, and out-of-range
/// values. The fractional part is truncated toward zero, matching [`Duration::from_nanos`] and viper's `int64`
/// coercion.
pub fn checked_duration_from_nanos_f64(nanos: f64) -> Result<Duration, ParseDurationError> {
    if !nanos.is_finite() {
        return Err(invalid("<non-finite>", "duration nanoseconds must be finite"));
    }
    if nanos < 0.0 {
        return Err(ParseDurationError::Negative);
    }
    if nanos > MAX_NANOS_U64 as f64 {
        return Err(ParseDurationError::Overflow);
    }
    Ok(Duration::from_nanos(nanos as u64))
}

fn consume_digits(s: &str) -> (&str, &str) {
    let end = s.bytes().take_while(|b| b.is_ascii_digit()).count();
    s.split_at(end)
}

fn consume_unit(s: &str) -> &str {
    let mut end = 0;
    for (i, c) in s.char_indices() {
        if c.is_ascii_alphabetic() || c == 'µ' || c == 'μ' {
            end = i + c.len_utf8();
        } else {
            break;
        }
    }
    &s[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    const NS: Duration = Duration::from_nanos(1);
    const MS: Duration = Duration::from_millis(1);
    const S: Duration = Duration::from_secs(1);
    const M: Duration = Duration::from_secs(60);
    const H: Duration = Duration::from_secs(3600);

    #[test]
    fn supports_go_style_units() {
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
    fn supports_signs_zero_and_fractions() {
        assert_eq!(parse_duration("0").unwrap(), Duration::ZERO);
        assert_eq!(parse_duration("+0").unwrap(), Duration::ZERO);
        assert_eq!(parse_duration("-0").unwrap(), Duration::ZERO);
        assert_eq!(parse_duration("0s").unwrap(), Duration::ZERO);
        assert_eq!(parse_duration("+5h").unwrap(), 5 * H);
        assert_eq!(parse_duration(".5s").unwrap(), 500 * MS);
        assert_eq!(parse_duration("1.5h").unwrap(), 90 * M);
        assert_eq!(
            parse_duration("2h45m30.5s").unwrap(),
            (2 * H) + (45 * M) + (30 * S) + (500 * MS)
        );
        assert_eq!(
            parse_duration("1h1m1s1ms1us1ns").unwrap(),
            H + M + S + MS + (1000 * NS) + NS
        );
    }

    #[test]
    fn largest_representable_value() {
        assert_eq!(
            parse_duration("9223372036854775807ns").unwrap(),
            Duration::from_nanos(9_223_372_036_854_775_807)
        );
    }

    #[test]
    fn rejects_invalid_and_out_of_range() {
        // Bare integers (viper's nanosecond fallback) are not part of Go's grammar and are rejected here.
        assert!(matches!(parse_duration("10"), Err(ParseDurationError::Invalid { .. })));
        assert!(matches!(parse_duration(""), Err(ParseDurationError::Invalid { .. })));
        assert!(matches!(parse_duration("abc"), Err(ParseDurationError::Invalid { .. })));
        assert!(matches!(parse_duration("1d"), Err(ParseDurationError::Invalid { .. })));
        assert!(matches!(
            parse_duration("5m32sFOO"),
            Err(ParseDurationError::Invalid { .. })
        ));
        assert!(matches!(parse_duration("-1s"), Err(ParseDurationError::Negative)));
        assert!(matches!(
            parse_duration("9223372036854775808ns"),
            Err(ParseDurationError::Overflow)
        ));
    }

    #[test]
    fn viper_coercion_accepts_go_strings_and_bare_integers() {
        // Go-grammar strings still parse.
        assert_eq!(parse_viper_duration("10s").unwrap(), Duration::from_secs(10));
        assert_eq!(parse_viper_duration("0s").unwrap(), Duration::ZERO);
        // Bare integers are nanoseconds (viper's fallback), not part of the Go grammar.
        assert_eq!(parse_viper_duration("5").unwrap(), Duration::from_nanos(5));
        assert_eq!(parse_viper_duration("0").unwrap(), Duration::ZERO);
        // Surrounding whitespace is trimmed before either interpretation.
        assert_eq!(parse_viper_duration(" 5s").unwrap(), Duration::from_secs(5));
        assert_eq!(parse_viper_duration("  5  ").unwrap(), Duration::from_nanos(5));
    }

    #[test]
    fn viper_coercion_rejects_negative_overflow_and_junk() {
        // Negative and overflow are rejected in both the Go-grammar and the bare-integer path.
        assert!(matches!(parse_viper_duration("-1s"), Err(ParseDurationError::Negative)));
        assert!(matches!(parse_viper_duration("-5"), Err(ParseDurationError::Negative)));
        assert!(matches!(
            parse_viper_duration("9223372036854775808"),
            Err(ParseDurationError::Overflow)
        ));
        assert_eq!(
            parse_viper_duration("9223372036854775807").unwrap(),
            Duration::from_nanos(9_223_372_036_854_775_807)
        );
        // Non-integer junk surfaces the original Go-grammar error rather than the fallback's.
        assert!(matches!(
            parse_viper_duration("abc"),
            Err(ParseDurationError::Invalid { .. })
        ));
    }

    #[test]
    fn checked_numeric_conversions() {
        let max = MAX_NANOS_U64;
        assert_eq!(
            checked_duration_from_nanos_i128(max as i128).unwrap(),
            Duration::from_nanos(max)
        );
        assert!(matches!(
            checked_duration_from_nanos_i128(-1),
            Err(ParseDurationError::Negative)
        ));
        assert!(matches!(
            checked_duration_from_nanos_i128(max as i128 + 1),
            Err(ParseDurationError::Overflow)
        ));
        assert!(matches!(
            checked_duration_from_nanos_u128(max as u128 + 1),
            Err(ParseDurationError::Overflow)
        ));
        assert_eq!(checked_duration_from_nanos_f64(1.9).unwrap(), Duration::from_nanos(1));
        assert!(matches!(
            checked_duration_from_nanos_f64(-0.5),
            Err(ParseDurationError::Negative)
        ));
        assert!(matches!(
            checked_duration_from_nanos_f64(f64::INFINITY),
            Err(ParseDurationError::Invalid { .. })
        ));
        assert!(matches!(
            checked_duration_from_nanos_f64(f64::NAN),
            Err(ParseDurationError::Invalid { .. })
        ));
    }

    #[test]
    fn error_display_messages() {
        assert!(parse_duration("").unwrap_err().to_string().contains("empty duration"));
        assert!(parse_duration(".s")
            .unwrap_err()
            .to_string()
            .contains("expected digits"));
        assert!(parse_duration("5ns5").unwrap_err().to_string().contains("missing unit"));
        assert!(parse_duration("1d")
            .unwrap_err()
            .to_string()
            .contains("unknown unit 'd'"));
        assert!(parse_duration("-1s").unwrap_err().to_string().contains("negative"));
        assert!(parse_duration("9223372036854775808ns")
            .unwrap_err()
            .to_string()
            .contains("exceeds"));
    }
}
