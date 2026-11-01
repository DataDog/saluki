//! Positive case durations with an explicit unit for every number.

use std::{str::FromStr, time::Duration};

use serde::{Deserialize, Deserializer};

/// A positive duration parsed from unit-qualified numbers, such as `1m30s`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CaseDuration(Duration);

impl From<CaseDuration> for Duration {
    fn from(duration: CaseDuration) -> Self {
        duration.0
    }
}

impl FromStr for CaseDuration {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        if s.is_empty() {
            return Err("empty duration string".to_string());
        }

        let mut total = Duration::ZERO;
        let mut current_num = String::new();
        let mut chars = s.chars().peekable();
        let overflow = || format!("duration '{s}' is too large");
        let no_unit = |number: &str| {
            format!("duration '{s}' ends with '{number}', which has no unit; write one, as in '{number}s'")
        };

        while let Some(c) = chars.next() {
            if c.is_ascii_digit() {
                current_num.push(c);
            } else if c.is_alphabetic() {
                if current_num.is_empty() {
                    return Err(format!("unexpected unit '{}' without a number", c));
                }

                let num: u64 = current_num
                    .parse()
                    .map_err(|_| format!("invalid number: {}", current_num))?;
                current_num.clear();

                let mut unit = String::from(c);
                while chars.peek().map(|c| c.is_alphabetic()).unwrap_or(false) {
                    unit.push(chars.next().unwrap());
                }

                let duration = match unit.as_str() {
                    "ns" => Duration::from_nanos(num),
                    "us" | "µs" => Duration::from_micros(num),
                    "ms" => Duration::from_millis(num),
                    "s" => Duration::from_secs(num),
                    "m" => Duration::from_secs(num.checked_mul(60).ok_or_else(overflow)?),
                    "h" => Duration::from_secs(num.checked_mul(3600).ok_or_else(overflow)?),
                    _ => return Err(format!("unknown duration unit: {}", unit)),
                };

                total = total.checked_add(duration).ok_or_else(overflow)?;
            } else if c.is_whitespace() {
                if !current_num.is_empty() {
                    while chars.peek().is_some_and(|c| c.is_whitespace()) {
                        chars.next();
                    }
                    if chars.peek().is_some_and(|c| c.is_ascii_digit()) {
                        return Err(no_unit(&current_num));
                    }
                }
            } else {
                return Err(format!("unexpected character: {}", c));
            }
        }

        if !current_num.is_empty() {
            return Err(no_unit(&current_num));
        }
        if total.is_zero() {
            return Err("duration must be greater than zero".to_string());
        }

        Ok(Self(total))
    }
}

impl<'de> Deserialize<'de> for CaseDuration {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_duration_needs_a_unit_after_every_number() {
        for (text, number) in [
            ("30", "30"),
            ("1m30", "30"),
            (" 5 ", "5"),
            // Joining digits across whitespace turns `1 30s` into a 130-second deadline.
            ("1 30s", "1"),
            ("1 0s", "1"),
            ("1 \t\n 30s", "1"),
        ] {
            assert_eq!(
                text.parse::<CaseDuration>().unwrap_err(),
                format!(
                    "duration '{}' ends with '{number}', which has no unit; write one, as in '{number}s'",
                    text.trim()
                )
            );
        }
    }

    #[test]
    fn durations_combine_units_and_skip_whitespace() {
        for (text, expected) in [
            ("1m30s", Duration::from_secs(90)),
            ("500ms", Duration::from_millis(500)),
            ("1h", Duration::from_secs(3600)),
            ("2 m", Duration::from_secs(120)),
            ("30 s", Duration::from_secs(30)),
            ("10us", Duration::from_micros(10)),
            ("1µs", Duration::from_micros(1)),
            ("5ns", Duration::from_nanos(5)),
            (" \t1m 30s\n", Duration::from_secs(90)),
            ("18446744073709551615s", Duration::from_secs(u64::MAX)),
        ] {
            assert_eq!(
                Duration::from(text.parse::<CaseDuration>().unwrap()),
                expected,
                "{text}"
            );
        }
    }

    #[test]
    fn an_empty_zero_or_malformed_duration_is_rejected() {
        for (text, expected) in [
            ("", "empty duration string"),
            ("0s", "duration must be greater than zero"),
            ("s", "unexpected unit 's' without a number"),
            ("1x", "unknown duration unit: x"),
            ("1s-", "unexpected character: -"),
            ("1.5s", "unexpected character: ."),
            ("-1s", "unexpected character: -"),
            ("18446744073709551616s", "invalid number: 18446744073709551616"),
        ] {
            assert_eq!(text.parse::<CaseDuration>().unwrap_err(), expected, "{text}");
        }
    }

    #[test]
    fn an_overflowing_duration_is_an_error_not_a_panic() {
        for text in [
            "18446744073709551615h",
            "18446744073709551615m",
            "18446744073709551615s 1s",
            "18446744073709551615s 999999999ns 1ns",
        ] {
            assert_eq!(
                text.parse::<CaseDuration>().unwrap_err(),
                format!("duration '{text}' is too large")
            );
        }
    }
}
