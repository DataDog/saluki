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
            is_default: is_default_value(entry.default, value),
            pipeline_affinity: entry.pipeline_affinity,
        })
    }
}

fn is_default_value(default: Option<&str>, value: &Value) -> bool {
    match default {
        Some(default_str) => match serde_json::from_str::<Value>(default_str) {
            Ok(default_value) => *value == default_value,
            Err(_) => false,
        },
        None => match value {
            Value::Null => true,
            Value::String(s) => s.is_empty(),
            _ => false,
        },
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
        let result = c.classify("tls_handshake_timeout", &Value::String("".into())).unwrap();
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
    fn none_default_null_is_default() {
        let c = classifier();
        // tls_handshake_timeout is unsupported with no schema default
        let result = c.classify("tls_handshake_timeout", &Value::Null).unwrap();
        assert!(result.is_default);
    }

    #[test]
    fn none_default_empty_string_is_default() {
        let c = classifier();
        let result = c.classify("tls_handshake_timeout", &Value::String("".into())).unwrap();
        assert!(result.is_default);
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
}
