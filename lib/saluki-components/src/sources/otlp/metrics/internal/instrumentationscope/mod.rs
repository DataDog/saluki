//! OTLP `instrumentation_scope` metadata.

use otlp_protos::opentelemetry::proto::common::v1 as otlp_common;
use otlp_protos::opentelemetry::proto::common::v1::any_value::Value;

use crate::sources::otlp::metrics::internal::utils;

const SCOPE_NAME_TAG: &str = "instrumentation_scope";
const SCOPE_VERSION_TAG: &str = "instrumentation_scope_version";

/// Creates a new slice of tags from an OTLP `InstrumentationScope`.
pub fn tags_from_instrumentation_scope_metadata(scope: &otlp_common::InstrumentationScope) -> Vec<String> {
    let mut tags = Vec::new();
    tags.push(utils::format_key_value_tag(SCOPE_NAME_TAG, &scope.name));
    tags.push(utils::format_key_value_tag(SCOPE_VERSION_TAG, &scope.version));
    for kv in &scope.attributes {
        if let Some(Value::StringValue(v)) = kv.value.as_ref().and_then(|av| av.value.as_ref()) {
            tags.push(utils::format_key_value_tag(&kv.key, v));
        }
    }
    tags
}

/// Creates tags for when instrumentation scope isn't present in the OTLP payload.
/// This matches DD Agent behavior which adds "n/a" values for missing scope.
pub fn tags_from_empty_instrumentation_scope() -> Vec<String> {
    vec![
        utils::format_key_value_tag(SCOPE_NAME_TAG, ""),
        utils::format_key_value_tag(SCOPE_VERSION_TAG, ""),
    ]
}

#[cfg(test)]
mod tests {
    use otlp_protos::opentelemetry::proto::common::v1::{AnyValue, InstrumentationScope, KeyValue};

    use super::*;

    fn string_kv(key: &str, value: &str) -> KeyValue {
        KeyValue {
            key: key.to_string(),
            value: Some(AnyValue {
                value: Some(Value::StringValue(value.to_string())),
            }),
        }
    }

    fn int_kv(key: &str, value: i64) -> KeyValue {
        KeyValue {
            key: key.to_string(),
            value: Some(AnyValue {
                value: Some(Value::IntValue(value)),
            }),
        }
    }

    #[test]
    fn populated_scope_emits_name_version_and_string_attributes() {
        let scope = InstrumentationScope {
            name: "my.instrumentation".to_string(),
            version: "1.2.3".to_string(),
            // The int attribute is deliberately included to confirm non-string attributes are skipped,
            // matching the Go implementation which only converts string-valued scope attributes to tags.
            attributes: vec![string_kv("build.id", "abc"), int_kv("workers", 4)],
            ..Default::default()
        };

        let tags = tags_from_instrumentation_scope_metadata(&scope);
        assert_eq!(
            tags,
            vec![
                "instrumentation_scope:my.instrumentation".to_string(),
                "instrumentation_scope_version:1.2.3".to_string(),
                "build.id:abc".to_string(),
            ]
        );
    }

    #[test]
    fn populated_scope_with_empty_name_and_version_uses_na_placeholders() {
        let scope = InstrumentationScope::default();

        let tags = tags_from_instrumentation_scope_metadata(&scope);
        assert_eq!(
            tags,
            vec![
                "instrumentation_scope:n/a".to_string(),
                "instrumentation_scope_version:n/a".to_string(),
            ]
        );
    }

    #[test]
    fn empty_scope_fallback_uses_na_placeholders() {
        assert_eq!(
            tags_from_empty_instrumentation_scope(),
            vec![
                "instrumentation_scope:n/a".to_string(),
                "instrumentation_scope_version:n/a".to_string(),
            ]
        );
    }
}
