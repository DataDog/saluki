use std::collections::HashMap;

use serde_json::Value;

use super::{SchemaEntry, SupportLevel, ALL_ANNOTATIONS, IGNORED_ENTRIES};

/// Result of classifying a single config key/value pair against the registry.
pub struct Classification {
    /// The level at which this config key is supported by Saluki.
    pub support_level: SupportLevel,
    /// Whether the value matches the schema default.
    pub is_default: bool,
}

/// A type used internally in our on-demand classification map.
enum RegistryEntry {
    Annotated {
        support_level: SupportLevel,
        schema: &'static SchemaEntry,
    },
    Ignored,
}

/// Classifies the support level of config keys and determines if a value is default.
pub struct ConfigClassifier {
    lookup: HashMap<&'static str, RegistryEntry>,
}

impl ConfigClassifier {
    /// Builds a classifier from all supported, unsupported, and ignored config keys.
    pub fn new() -> Self {
        let mut lookup = HashMap::new();

        // Insert ignored entries first so annotations win on overlap (e.g. dogstatsd_expiry_seconds
        // is both in ignored_keys.yaml and an alias for counter_expiry_seconds).
        for &key in IGNORED_ENTRIES.iter() {
            lookup.insert(key, RegistryEntry::Ignored);
        }

        for &annotation in ALL_ANNOTATIONS.iter() {
            lookup.insert(
                annotation.yaml_path(),
                RegistryEntry::Annotated {
                    support_level: annotation.support_level,
                    schema: annotation.schema,
                },
            );
            for alias in annotation.additional_yaml_paths {
                lookup.insert(
                    alias,
                    RegistryEntry::Annotated {
                        support_level: annotation.support_level,
                        schema: annotation.schema,
                    },
                );
            }
        }

        Self { lookup }
    }

    /// Classifies a single config key/value pair against the registry.
    pub fn classify(&self, key: &str, value: &Value) -> Classification {
        match self.lookup.get(key) {
            Some(RegistryEntry::Annotated { support_level, schema }) => Classification {
                support_level: *support_level,
                is_default: is_default_value(schema, value),
            },
            Some(RegistryEntry::Ignored) => Classification {
                support_level: SupportLevel::NotApplicable,
                is_default: true,
            },
            None => Classification {
                support_level: SupportLevel::Unrecognized,
                is_default: false,
            },
        }
    }
}

/// Determines whether the runtime `value` matches the default value according to config registry.
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
    use crate::config_registry::SUPPORTED_ANNOTATIONS;

    fn classifier() -> ConfigClassifier {
        ConfigClassifier::new()
    }

    #[test]
    fn full_non_default() {
        let c = classifier();
        let result = c.classify("dogstatsd_port", &Value::Number(9999.into()));
        assert_eq!(result.support_level, SupportLevel::Full);
        assert!(!result.is_default);
    }

    #[test]
    fn full_default() {
        let c = classifier();
        let result = c.classify("dogstatsd_port", &Value::Number(8125.into()));
        assert_eq!(result.support_level, SupportLevel::Full);
        assert!(result.is_default);
    }

    #[test]
    fn partial_non_default() {
        let c = classifier();
        // Find a Partial annotation
        let partial = SUPPORTED_ANNOTATIONS
            .iter()
            .find(|a| a.support_level == SupportLevel::Partial)
            .expect("need at least one Partial annotation for this test");
        let result = c.classify(partial.yaml_path(), &Value::String("non_default_value".into()));
        assert_eq!(result.support_level, SupportLevel::Partial);
    }

    #[test]
    // To whoever implemented this config: sorry! We just wanted to make sure this is working with correctly by giving
    // it a currently unsupported key. You can delete the usupported tests or choose a different unsupported key.
    fn incompatible_non_default() {
        let c = classifier();
        let key = unsupported::TLS_HANDSHAKE_TIMEOUT.yaml_path();
        let result = c.classify(key, &Value::Number(999.into()));
        assert_eq!(result.support_level, SupportLevel::Incompatible);
        assert!(!result.is_default);
    }

    #[test]
    fn incompatible_default() {
        let c = classifier();
        let ann = &unsupported::TLS_HANDSHAKE_TIMEOUT;
        let result = c.classify(ann.yaml_path(), &Value::String("".into()));
        assert_eq!(result.support_level, SupportLevel::Incompatible);
        assert!(result.is_default);
    }

    #[test]
    fn not_applicable() {
        let c = classifier();
        // GUI_host is in ignored_keys.yaml
        let result = c.classify("GUI_host", &Value::String("localhost".into()));
        assert_eq!(result.support_level, SupportLevel::NotApplicable);
        assert!(result.is_default);
    }

    #[test]
    fn unrecognized() {
        let c = classifier();
        let result = c.classify("totally_made_up_key", &Value::Bool(true));
        assert_eq!(result.support_level, SupportLevel::Unrecognized);
        assert!(!result.is_default);
    }

    #[test]
    fn alias_resolves_to_annotation() {
        let c = classifier();
        // dogstatsd_expiry_seconds is an alias for counter_expiry_seconds (Full support)
        let result = c.classify("dogstatsd_expiry_seconds", &Value::Number(999.into()));
        assert_eq!(result.support_level, SupportLevel::Full);
        assert!(!result.is_default);
    }

    #[test]
    fn alias_default_matches_canonical() {
        let c = classifier();
        // counter_expiry_seconds has default: None (Saluki-only schema entry), so both
        // canonical and alias should treat Value::Null as default.
        let canonical = c.classify("counter_expiry_seconds", &Value::Null);
        let alias = c.classify("dogstatsd_expiry_seconds", &Value::Null);
        assert!(canonical.is_default);
        assert!(alias.is_default);
    }

    #[test]
    fn none_default_null_is_default() {
        let c = classifier();
        // GUI_port has default: None in the schema - but it's in ignored_keys.
        // Find an annotated key with default: None.
        let ann = ALL_ANNOTATIONS
            .iter()
            .find(|a| a.schema.default.is_none())
            .expect("need an annotation with default: None");
        let result = c.classify(ann.yaml_path(), &Value::Null);
        assert!(result.is_default);
    }

    #[test]
    fn none_default_empty_string_is_default() {
        let c = classifier();
        let ann = ALL_ANNOTATIONS
            .iter()
            .find(|a| a.schema.default.is_none())
            .expect("need an annotation with default: None");
        let result = c.classify(ann.yaml_path(), &Value::String("".into()));
        assert!(result.is_default);
    }

    #[test]
    fn none_default_non_empty_is_not_default() {
        let c = classifier();
        let ann = ALL_ANNOTATIONS
            .iter()
            .find(|a| a.schema.default.is_none())
            .expect("need an annotation with default: None");
        let result = c.classify(ann.yaml_path(), &Value::String("something".into()));
        assert!(!result.is_default);
    }
}
