use std::collections::HashMap;

use serde_json::Value;

use super::{PipelineAffinity, SchemaEntry, SupportLevel, ALL_ANNOTATIONS};

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
/// Only knows about annotated keys (`ALL_ANNOTATIONS`). Keys not in the registry - whether ignored,
/// unrecognized, or anything else - return `None` from [`classify`](Self::classify).
pub struct ConfigClassifier {
    lookup: HashMap<&'static str, (&'static SchemaEntry, SupportLevel, PipelineAffinity)>,
}

impl ConfigClassifier {
    /// Builds a classifier from all annotated config keys.
    pub fn new() -> Self {
        let mut lookup = HashMap::new();

        for &annotation in ALL_ANNOTATIONS.iter() {
            let entry = (
                annotation.schema,
                annotation.support_level,
                annotation.pipeline_affinity,
            );
            lookup.insert(annotation.yaml_path(), entry);
            for alias in annotation.additional_yaml_paths {
                lookup.insert(alias, entry);
            }
        }

        Self { lookup }
    }

    /// Classifies a single config key/value pair against the registry.
    ///
    /// Returns `None` for keys not in `ALL_ANNOTATIONS` (ignored, unrecognized, etc.).
    pub fn classify(&self, key: &str, value: &Value) -> Option<Classification> {
        let &(schema, support_level, pipeline_affinity) = self.lookup.get(key)?;
        Some(Classification {
            support_level,
            is_default: is_default_value(schema, value),
            pipeline_affinity,
        })
    }
}

/// Determines whether the runtime `value` matches the default value according to config registry.
/// We rely on the schema's default and the runtime value deserializing to the same `serde_json::Value`.
fn is_default_value(schema: &SchemaEntry, value: &Value) -> bool {
    match schema.default {
        Some(default_str) => match serde_json::from_str::<Value>(default_str) {
            Ok(default_value) => *value == default_value,
            Err(_) => false,
        },
        None => match value {
            Value::Null => true,
            // Empty string is considered equivalent to null in config.
            Value::String(s) => s.is_empty(),
            _ => false,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config_registry::datadog::unsupported;
    use crate::config_registry::{Pipeline, SUPPORTED_ANNOTATIONS};

    fn classifier() -> ConfigClassifier {
        ConfigClassifier::new()
    }

    #[test]
    fn full_non_default() {
        let c = classifier();
        let result = c.classify("dogstatsd_port", &Value::Number(9999.into())).unwrap();
        assert_eq!(result.support_level, SupportLevel::Full);
        assert!(!result.is_default);
    }

    #[test]
    fn full_default() {
        let c = classifier();
        let result = c.classify("dogstatsd_port", &Value::Number(8125.into())).unwrap();
        assert_eq!(result.support_level, SupportLevel::Full);
        assert!(result.is_default);
    }

    #[test]
    fn partial_non_default() {
        let c = classifier();
        let partial = SUPPORTED_ANNOTATIONS
            .iter()
            .find(|a| a.support_level == SupportLevel::Partial)
            .expect("need at least one Partial annotation for this test");
        let result = c
            .classify(partial.yaml_path(), &Value::String("non_default_value".into()))
            .unwrap();
        assert_eq!(result.support_level, SupportLevel::Partial);
    }

    #[test]
    // To whoever implements this config in the future: sorry! We just wanted to make sure this is
    // working correctly by giving it a currently unsupported key. You can delete the unsupported
    // tests or choose a different key.
    fn incompatible_non_default() {
        let c = classifier();
        let key = unsupported::TLS_HANDSHAKE_TIMEOUT.yaml_path();
        let result = c.classify(key, &Value::Number(999.into())).unwrap();
        assert!(matches!(result.support_level, SupportLevel::Incompatible(_)));
        assert!(!result.is_default);
    }

    #[test]
    fn incompatible_default() {
        let c = classifier();
        let ann = &unsupported::TLS_HANDSHAKE_TIMEOUT;
        let result = c.classify(ann.yaml_path(), &Value::String("".into())).unwrap();
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
    fn alias_resolves_to_annotation() {
        let c = classifier();
        let result = c
            .classify("dogstatsd_expiry_seconds", &Value::Number(999.into()))
            .unwrap();
        assert_eq!(result.support_level, SupportLevel::Full);
        assert!(!result.is_default);
    }

    #[test]
    fn alias_default_matches_canonical() {
        let c = classifier();
        let canonical = c.classify("counter_expiry_seconds", &Value::Null).unwrap();
        let alias = c.classify("dogstatsd_expiry_seconds", &Value::Null).unwrap();
        assert!(canonical.is_default);
        assert!(alias.is_default);
    }

    #[test]
    fn log_payloads_is_supported() {
        let c = classifier();
        let result = c.classify("log_payloads", &Value::Bool(true)).unwrap();
        assert_eq!(result.support_level, SupportLevel::Full);
        assert!(!result.is_default);
    }

    #[test]
    fn config_id_is_supported() {
        let c = classifier();
        let result = c.classify("config_id", &Value::String("test-config01".into())).unwrap();
        assert_eq!(result.support_level, SupportLevel::Full);
        assert_eq!(
            result.pipeline_affinity,
            PipelineAffinity::Pipelines(&[Pipeline::DogStatsD, Pipeline::Checks, Pipeline::Otlp])
        );
        assert!(!result.is_default);
    }

    #[test]
    fn none_default_null_is_default() {
        let c = classifier();
        let ann = ALL_ANNOTATIONS
            .iter()
            .find(|a| a.schema.default.is_none())
            .expect("need an annotation with default: None");
        let result = c.classify(ann.yaml_path(), &Value::Null).unwrap();
        assert!(result.is_default);
    }

    #[test]
    fn none_default_empty_string_is_default() {
        let c = classifier();
        let ann = ALL_ANNOTATIONS
            .iter()
            .find(|a| a.schema.default.is_none())
            .expect("need an annotation with default: None");
        let result = c.classify(ann.yaml_path(), &Value::String("".into())).unwrap();
        assert!(result.is_default);
    }

    #[test]
    fn none_default_non_empty_is_not_default() {
        let c = classifier();
        let ann = ALL_ANNOTATIONS
            .iter()
            .find(|a| a.schema.default.is_none())
            .expect("need an annotation with default: None");
        let result = c.classify(ann.yaml_path(), &Value::String("something".into())).unwrap();
        assert!(!result.is_default);
    }
}
