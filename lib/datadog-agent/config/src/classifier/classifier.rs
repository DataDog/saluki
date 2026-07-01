use std::collections::HashMap;

use serde_json::Value;

use super::{ClassifierEntry, PipelineAffinity, SupportLevel, CLASSIFIER_ENTRIES};

/// Result of classifying a single config key/value pair against the registry.
pub struct Classification {
    /// The level at which this config key is supported by Saluki.
    pub support_level: SupportLevel,
    /// Whether the value matches the schema default.
    pub is_default: bool,
    /// Which pipelines this key's incompatibility warning applies to.
    pub pipeline_affinity: PipelineAffinity,
}

/// Classifies the support level of config keys and determines if a value is default.
///
/// Only knows about annotated keys (supported + unsupported). Keys not in the registry - whether
/// ignored, unrecognized, or anything else - return `None` from
/// [`classify`](Self::classify).
pub struct ConfigClassifier {
    lookup: HashMap<&'static str, &'static ClassifierEntry>,
}

impl ConfigClassifier {
    /// Builds a classifier from all annotated config keys.
    pub fn new() -> Self {
        let mut lookup = HashMap::new();

        for entry in CLASSIFIER_ENTRIES {
            lookup.insert(entry.yaml_path, entry);
            for alias in entry.aliases {
                lookup.insert(alias, entry);
            }
        }

        Self { lookup }
    }

    /// Classifies a single config key/value pair against the registry.
    ///
    /// Returns `None` for keys not in the registry (ignored, unrecognized, etc.).
    pub fn classify(&self, key: &str, value: &Value) -> Option<Classification> {
        let entry = self.lookup.get(key)?;
        Some(Classification {
            support_level: entry.support_level,
            is_default: is_default_value(entry.default, entry.is_duration, value),
            pipeline_affinity: entry.pipeline_affinity,
        })
    }
}

fn is_default_value(default: Option<&str>, is_duration: bool, value: &Value) -> bool {
    match default {
        Some(default_str) => match serde_json::from_str::<Value>(default_str) {
            Ok(default_value) => {
                if *value == default_value {
                    return true;
                }
                // Duration keys need special handling: the schema default is a Go duration string
                // (for example, `"10s"`), but the Datadog Agent transmits durations as integer
                // nanoseconds. Normalize both sides to nanoseconds before comparing so a value at
                // its default doesn't get flagged as an override.
                if is_duration {
                    if let (Some(default_ns), Some(value_ns)) =
                        (duration_value_as_nanos(&default_value), duration_value_as_nanos(value))
                    {
                        return default_ns == value_ns;
                    }
                }
                false
            }
            Err(_) => false,
        },
        None => match value {
            Value::Null => true,
            Value::String(s) => s.is_empty(),
            _ => false,
        },
    }
}

/// Normalizes a duration-typed config value to nanoseconds.
///
/// Accepts a Go duration string (for example, `"10s"`, `"1m30s"`) or a JSON number already
/// expressed in nanoseconds (the form the Datadog Agent sends over the config stream). Returns
/// `None` for any other shape.
fn duration_value_as_nanos(value: &Value) -> Option<i128> {
    match value {
        Value::String(s) => parse_go_duration_nanos(s),
        Value::Number(n) => n.as_i64().map(i128::from),
        _ => None,
    }
}

/// Parses a Go `time.Duration` string (for example, `"10s"`, `"500ms"`, `"1h30m0s"`) into
/// nanoseconds.
///
/// Supports the unit suffixes Go's `time.ParseDuration` accepts (`ns`, `us`/`µs`/`μs`, `ms`, `s`,
/// `m`, `h`), an optional leading sign, and fractional components. Returns `None` if the string is
/// not a well-formed duration.
fn parse_go_duration_nanos(input: &str) -> Option<i128> {
    let mut s = input.trim();
    if s.is_empty() {
        return None;
    }

    let negative = match s.as_bytes()[0] {
        b'-' => {
            s = &s[1..];
            true
        }
        b'+' => {
            s = &s[1..];
            false
        }
        _ => false,
    };

    // Go accepts a bare "0" (no unit) as a special case.
    if s == "0" {
        return Some(0);
    }

    let mut total_nanos: f64 = 0.0;
    let mut chars = s.char_indices().peekable();
    let mut consumed_any = false;

    while let Some(&(start, _)) = chars.peek() {
        // Parse the numeric portion (digits with an optional single decimal point).
        let mut end = start;
        let mut seen_digit = false;
        let mut seen_dot = false;
        while let Some(&(idx, c)) = chars.peek() {
            if c.is_ascii_digit() {
                seen_digit = true;
                end = idx + c.len_utf8();
                chars.next();
            } else if c == '.' && !seen_dot {
                seen_dot = true;
                end = idx + c.len_utf8();
                chars.next();
            } else {
                break;
            }
        }
        if !seen_digit {
            return None;
        }
        let number: f64 = s[start..end].parse().ok()?;

        // Parse the unit portion (letters, including the micro sign variants).
        let unit_start = end;
        let mut unit_end = end;
        while let Some(&(idx, c)) = chars.peek() {
            if c.is_ascii_alphabetic() || c == 'µ' || c == 'μ' {
                unit_end = idx + c.len_utf8();
                chars.next();
            } else {
                break;
            }
        }
        let unit = &s[unit_start..unit_end];
        let unit_nanos = match unit {
            "ns" => 1.0,
            "us" | "µs" | "μs" => 1_000.0,
            "ms" => 1_000_000.0,
            "s" => 1_000_000_000.0,
            "m" => 60.0 * 1_000_000_000.0,
            "h" => 3_600.0 * 1_000_000_000.0,
            _ => return None,
        };

        total_nanos += number * unit_nanos;
        consumed_any = true;
    }

    if !consumed_any {
        return None;
    }

    let total_nanos = total_nanos.round();
    if negative {
        Some(-(total_nanos as i128))
    } else {
        Some(total_nanos as i128)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classifier::Severity;

    fn classifier() -> ConfigClassifier {
        ConfigClassifier::new()
    }

    #[test]
    fn full_support_keys_not_in_classifier() {
        // Full-support keys are not actionable at the call site; the classifier omits them.
        // The call site treats None identically to Full (silently continues).
        let c = classifier();
        assert!(c.classify("dogstatsd_port", &Value::Number(9999.into())).is_none());
        assert!(c.classify("log_payloads", &Value::Bool(true)).is_none());
    }

    #[test]
    fn partial_non_default() {
        let c = classifier();
        let result = c
            .classify("min_tls_version", &Value::String("non_default_value".into()))
            .unwrap();
        assert_eq!(result.support_level, SupportLevel::Partial);
    }

    #[test]
    fn incompatible_non_default() {
        let c = classifier();
        let result = c.classify("tls_handshake_timeout", &Value::Number(999.into())).unwrap();
        assert!(matches!(result.support_level, SupportLevel::Incompatible(_)));
        assert!(!result.is_default);
    }

    #[test]
    fn incompatible_default() {
        let c = classifier();
        // config_id has schema default "" (empty string)
        let result = c.classify("config_id", &Value::String("".into())).unwrap();
        assert!(matches!(result.support_level, SupportLevel::Incompatible(_)));
        assert!(result.is_default);
    }

    #[test]
    fn not_in_registry_returns_none() {
        let c = classifier();
        assert!(c.classify("totally_made_up_key", &Value::Bool(true)).is_none());
        assert!(c.classify("GUI_host", &Value::String("localhost".into())).is_none());
    }

    #[test]
    fn duration_default_null_is_not_default() {
        let c = classifier();
        // tls_handshake_timeout now has default "10s" (a duration string); null != "10s"
        let result = c.classify("tls_handshake_timeout", &Value::Null).unwrap();
        assert!(!result.is_default);
    }

    #[test]
    fn duration_default_empty_string_is_not_default() {
        let c = classifier();
        // tls_handshake_timeout has default "10s"; empty string does not match
        let result = c.classify("tls_handshake_timeout", &Value::String("".into())).unwrap();
        assert!(!result.is_default);
    }

    #[test]
    fn none_default_non_empty_is_not_default() {
        let c = classifier();
        let result = c
            .classify("tls_handshake_timeout", &Value::String("something".into()))
            .unwrap();
        assert!(!result.is_default);
    }

    #[test]
    fn incompatible_severity_levels() {
        let c = classifier();
        let result = c.classify("tls_handshake_timeout", &Value::Number(30.into())).unwrap();
        assert!(matches!(
            result.support_level,
            SupportLevel::Incompatible(Severity::Medium)
        ));
    }

    #[test]
    fn duration_default_as_nanoseconds_is_default() {
        let c = classifier();
        // The Agent transmits tls_handshake_timeout (schema default "10s") as integer nanoseconds.
        // The classifier must recognize this as the default and not flag it as an override.
        let result = c
            .classify("tls_handshake_timeout", &Value::Number(10_000_000_000i64.into()))
            .unwrap();
        assert!(result.is_default);
    }

    #[test]
    fn duration_non_default_nanoseconds_is_not_default() {
        let c = classifier();
        // 5s in nanoseconds is not the 10s default.
        let result = c
            .classify("tls_handshake_timeout", &Value::Number(5_000_000_000i64.into()))
            .unwrap();
        assert!(!result.is_default);
    }

    #[test]
    fn parse_go_duration_nanos_handles_common_forms() {
        assert_eq!(parse_go_duration_nanos("10s"), Some(10_000_000_000));
        assert_eq!(parse_go_duration_nanos("500ms"), Some(500_000_000));
        assert_eq!(parse_go_duration_nanos("1m0s"), Some(60_000_000_000));
        assert_eq!(parse_go_duration_nanos("1h30m0s"), Some(5_400_000_000_000));
        assert_eq!(parse_go_duration_nanos("24h0m0s"), Some(86_400_000_000_000));
        assert_eq!(parse_go_duration_nanos("0s"), Some(0));
        assert_eq!(parse_go_duration_nanos("0"), Some(0));
        assert_eq!(parse_go_duration_nanos("1.5h"), Some(5_400_000_000_000));
        assert_eq!(parse_go_duration_nanos("250µs"), Some(250_000));
        assert_eq!(parse_go_duration_nanos("-5s"), Some(-5_000_000_000));
        assert_eq!(parse_go_duration_nanos("nonsense"), None);
        assert_eq!(parse_go_duration_nanos("10"), None);
        assert_eq!(parse_go_duration_nanos(""), None);
    }
}
