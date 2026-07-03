use std::collections::HashMap;

use serde_json::Value;

use super::{ClassifierEntry, DefaultValue, PipelineAffinity, SupportLevel, CLASSIFIER_ENTRIES};

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
            is_default: is_default_value(entry.default, value),
            pipeline_affinity: entry.pipeline_affinity,
        })
    }
}

fn is_default_value(default: DefaultValue, value: &Value) -> bool {
    match default {
        DefaultValue::Json(default_str) => serde_json::from_str::<Value>(default_str)
            .map(|default_value| *value == default_value)
            .unwrap_or(false),

        // Duration defaults are canonicalized to nanoseconds at build time. The Agent transmits
        // durations as integer nanoseconds (and occasionally as a Go duration string), so normalize
        // the incoming value the same way before comparing.
        DefaultValue::DurationNanos(default_ns) => duration_value_as_nanos(value)
            .map(|value_ns| value_ns == default_ns)
            .unwrap_or(false),

        DefaultValue::Missing => match value {
            Value::Null => true,
            Value::String(s) => s.is_empty(),
            _ => false,
        },
    }
}

/// Normalizes a duration-typed config value to nanoseconds.
///
/// Accepts a JSON number already expressed in nanoseconds (the form the Datadog Agent sends over
/// the config stream) or a Go duration string (for example, `"10s"`). Returns `None` for any other
/// shape or an out-of-range/invalid value.
fn duration_value_as_nanos(value: &Value) -> Option<u64> {
    match value {
        Value::Number(n) => n.as_u64(),
        Value::String(s) => go_duration::parse_duration(s).ok().map(|d| d.as_nanos() as u64),
        _ => None,
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
        // tls_handshake_timeout has a duration default (10s); a null value can't be normalized.
        let result = c.classify("tls_handshake_timeout", &Value::Null).unwrap();
        assert!(!result.is_default);
    }

    #[test]
    fn duration_default_non_duration_string_is_not_default() {
        let c = classifier();
        // Neither an empty string nor arbitrary text parses as a duration, so neither matches.
        assert!(
            !c.classify("tls_handshake_timeout", &Value::String("".into()))
                .unwrap()
                .is_default
        );
        assert!(
            !c.classify("tls_handshake_timeout", &Value::String("something".into()))
                .unwrap()
                .is_default
        );
    }

    #[test]
    fn duration_default_matches_go_duration_string() {
        let c = classifier();
        // The default is also matched when supplied as a Go duration string rather than nanoseconds.
        let result = c
            .classify("tls_handshake_timeout", &Value::String("10s".into()))
            .unwrap();
        assert!(result.is_default);
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
    fn is_default_value_by_variant() {
        // Json: structural equality against the decoded JSON literal.
        assert!(is_default_value(
            DefaultValue::Json("\"tlsv1.2\""),
            &Value::String("tlsv1.2".into())
        ));
        assert!(!is_default_value(
            DefaultValue::Json("\"tlsv1.2\""),
            &Value::String("tlsv1.3".into())
        ));
        assert!(is_default_value(DefaultValue::Json("1"), &Value::Number(1.into())));

        // DurationNanos: matches both the nanosecond number and the equivalent Go duration string.
        assert!(is_default_value(
            DefaultValue::DurationNanos(10_000_000_000),
            &Value::Number(10_000_000_000u64.into())
        ));
        assert!(is_default_value(
            DefaultValue::DurationNanos(10_000_000_000),
            &Value::String("10s".into())
        ));
        assert!(!is_default_value(
            DefaultValue::DurationNanos(10_000_000_000),
            &Value::Number(5_000_000_000u64.into())
        ));
        assert!(!is_default_value(
            DefaultValue::DurationNanos(10_000_000_000),
            &Value::String("nope".into())
        ));

        // Missing: only null or an empty string counts as "default".
        assert!(is_default_value(DefaultValue::Missing, &Value::Null));
        assert!(is_default_value(DefaultValue::Missing, &Value::String("".into())));
        assert!(!is_default_value(DefaultValue::Missing, &Value::String("x".into())));
    }
}
