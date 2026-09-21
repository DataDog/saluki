//! Go-compatible float formatting for metric tag values.

use std::fmt;

/// Formats a float using Go semantics directly into the caller's output buffer.
pub(super) struct GoFloat(pub(super) f64);

impl fmt::Display for GoFloat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let v = self.0;
        if v == f64::INFINITY {
            return f.write_str("inf");
        } else if v == f64::NEG_INFINITY {
            return f.write_str("-inf");
        } else if v.is_nan() {
            return f.write_str("nan");
        } else if v == 0.0 {
            return f.write_str("0");
        }

        // Mirror Go's strconv.FormatFloat(f, 'g', -1, 64): switch to scientific
        // notation below 1e-4 and at/above 1e6, with a zero-padded two-digit
        // signed exponent.
        if v.abs() < 1e-4 || v.abs() >= 1e6 {
            let s = format!("{v:e}");
            let (mantissa, exp_part) = s.split_once('e').expect("{:e} always contains 'e'");
            let (sign, digits) = if let Some(d) = exp_part.strip_prefix('-') {
                ("-", d)
            } else {
                ("+", exp_part.strip_prefix('+').unwrap_or(exp_part))
            };
            if digits.len() < 2 {
                write!(f, "{mantissa}e{sign}0{digits}")?;
            } else {
                write!(f, "{mantissa}e{sign}{digits}")?;
            }
            if v == v.floor() {
                f.write_str(".0")?;
            }
            return Ok(());
        }

        write!(f, "{v}")?;
        if v == v.floor() {
            f.write_str(".0")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // https://github.com/DataDog/datadog-agent/blob/main/pkg/opentelemetry-mapping-go/otlp/metrics/metrics_translator_test.go#L2225
    #[test]
    fn format_float_matches_go_strconv_formatting() {
        let tests = [
            (0.0, "0"),
            (0.001, "0.001"),
            (0.9, "0.9"),
            (0.95, "0.95"),
            (0.99, "0.99"),
            (0.999, "0.999"),
            (1.0, "1.0"),
            (2.0, "2.0"),
            (f64::INFINITY, "inf"),
            (f64::NEG_INFINITY, "-inf"),
            (f64::NAN, "nan"),
            (1e-10, "1e-10"),
            (1e-5, "1e-05"),
            (0.0001, "0.0001"),
            (999_999.0, "999999.0"),
            (1e6, "1e+06.0"),
            (-1e6, "-1e+06.0"),
            (1_000_000.5, "1.0000005e+06"),
            (1_234_567.0, "1.234567e+06.0"),
            (1_200_000.0, "1.2e+06.0"),
        ];

        for (input, expected) in tests {
            assert_eq!(GoFloat(input).to_string(), expected);
        }
    }

    #[test]
    fn bucket_bound_tags_use_go_float_format() {
        assert_eq!(
            format!("lower_bound:{}", GoFloat(f64::NEG_INFINITY)),
            "lower_bound:-inf"
        );
        assert_eq!(format!("upper_bound:{}", GoFloat(f64::INFINITY)), "upper_bound:inf");
        assert_eq!(format!("lower_bound:{}", GoFloat(0.0)), "lower_bound:0");
        assert_eq!(format!("lower_bound:{}", GoFloat(1.0)), "lower_bound:1.0");
        assert_eq!(format!("lower_bound:{}", GoFloat(0.001)), "lower_bound:0.001");
        assert_eq!(format!("upper_bound:{}", GoFloat(1e6)), "upper_bound:1e+06.0");
    }
}
