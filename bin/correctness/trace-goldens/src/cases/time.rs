//! Resolution of the dynamic span times used by case specs.
//!
//! Case specs express span times relative to the test clock so that the generated payload always lands in the
//! concentrator's current bucket. A span `start` is either a Go duration string, interpreted as a signed offset from
//! the test clock (for example, `"-2s"`), or a JSON integer holding an absolute Unix timestamp in nanoseconds (used by
//! the pre-year-2000 repair cases). A span `duration` is either a Go duration string (for example, `"80ms"`) or a JSON
//! integer holding raw nanoseconds.

use go_duration::parse_duration;
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use serde::Deserialize;

/// A time value in a case spec: either a Go duration string or a raw nanosecond count.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(untagged)]
pub enum TimeSpec {
    /// A Go duration string, such as `"-2s"` or `"80ms"`.
    Text(String),

    /// A raw, non-negative nanosecond count.
    ///
    /// Negative and non-integer JSON numbers are rejected while parsing.
    Nanos(u64),
}

/// Parses a Go duration string that may carry a leading sign, returning signed nanoseconds.
///
/// `go_duration::parse_duration` only accepts non-negative durations, so the sign is stripped, the magnitude is
/// parsed, and the sign is reapplied.
fn parse_signed_offset(s: &str) -> Result<i128, GenericError> {
    let s = s.trim();
    let (negative, magnitude) = match s.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, s.strip_prefix('+').unwrap_or(s)),
    };
    let nanos = parse_duration(magnitude)
        .with_error_context(|| format!("Invalid start offset '{}'.", s))?
        .as_nanos() as i128;
    Ok(if negative { -nanos } else { nanos })
}

/// Resolves a span start to an absolute Unix timestamp in nanoseconds.
///
/// A [`TimeSpec::Text`] value is a signed offset from `now_unix_nanos`; a [`TimeSpec::Nanos`] value is an absolute
/// timestamp and is returned unchanged, regardless of `now_unix_nanos`.
///
/// # Errors
///
/// Returns an error if the offset is not a valid Go duration, or if applying it to `now_unix_nanos` produces a value
/// that is negative or does not fit in a `u64`.
pub fn resolve_start(spec: &TimeSpec, now_unix_nanos: u64) -> Result<u64, GenericError> {
    match spec {
        TimeSpec::Nanos(n) => Ok(*n),
        TimeSpec::Text(s) => {
            let resolved = now_unix_nanos as i128 + parse_signed_offset(s)?;
            u64::try_from(resolved).map_err(|_| {
                generic_error!(
                    "Start offset '{}' applied to clock {} gives {}, which is not a valid timestamp.",
                    s,
                    now_unix_nanos,
                    resolved
                )
            })
        }
    }
}

/// Resolves a span duration to nanoseconds.
///
/// A [`TimeSpec::Nanos`] value is returned unchanged. It deliberately bypasses the `i64::MAX` limit of Go duration
/// strings, because the `malformed_times` case relies on a duration above `i64::MAX`.
///
/// # Errors
///
/// Returns an error if a [`TimeSpec::Text`] value is not a valid, non-negative Go duration.
pub fn resolve_duration(spec: &TimeSpec) -> Result<u64, GenericError> {
    match spec {
        TimeSpec::Nanos(n) => Ok(*n),
        TimeSpec::Text(s) => {
            let nanos = parse_duration(s.trim())
                .with_error_context(|| format!("Invalid duration '{}'.", s))?
                .as_nanos();
            u64::try_from(nanos).map_err(|_| generic_error!("Duration '{}' does not fit in 64 bits.", s))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_700_000_000_000_000_000;

    fn text(s: &str) -> TimeSpec {
        TimeSpec::Text(s.to_string())
    }

    #[test]
    fn negative_offsets_resolve_against_clock() {
        assert_eq!(resolve_start(&text("-2s"), NOW).unwrap(), NOW - 2_000_000_000);
        assert_eq!(resolve_start(&text("-1.99s"), NOW).unwrap(), NOW - 1_990_000_000);
    }

    #[test]
    fn positive_offsets_resolve_against_clock() {
        assert_eq!(resolve_start(&text("+40ms"), NOW).unwrap(), NOW + 40_000_000);
        assert_eq!(resolve_start(&text("1s"), NOW).unwrap(), NOW + 1_000_000_000);
    }

    #[test]
    fn absolute_start_ignores_clock() {
        assert_eq!(resolve_start(&TimeSpec::Nanos(1000), NOW).unwrap(), 1000);
        assert_eq!(resolve_start(&TimeSpec::Nanos(1000), 0).unwrap(), 1000);
    }

    #[test]
    fn start_before_epoch_is_an_error() {
        assert!(resolve_start(&text("-2s"), 1_000).is_err());
        assert!(resolve_start(&text("bogus"), NOW).is_err());
    }

    #[test]
    fn durations_resolve() {
        assert_eq!(resolve_duration(&text("80ms")).unwrap(), 80_000_000);
        assert_eq!(
            resolve_duration(&TimeSpec::Nanos(9_500_000_000_000_000_000)).unwrap(),
            9_500_000_000_000_000_000
        );
        assert!(resolve_duration(&text("-1s")).is_err());
    }

    #[test]
    fn time_spec_rejects_negative_and_fractional_numbers() {
        assert!(serde_json::from_str::<TimeSpec>("-5").is_err());
        assert!(serde_json::from_str::<TimeSpec>("1.5").is_err());
        assert_eq!(serde_json::from_str::<TimeSpec>("1000").unwrap(), TimeSpec::Nanos(1000));
    }
}
