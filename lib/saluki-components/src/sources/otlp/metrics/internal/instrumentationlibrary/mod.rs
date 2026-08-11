//! OTLP `instrumentation_library` metadata.

use otlp_protos::opentelemetry::proto::common::v1 as otlp_common;

use crate::sources::otlp::metrics::internal::utils;

const LIBRARY_NAME_TAG: &str = "instrumentation_library";
const LIBRARY_VERSION_TAG: &str = "instrumentation_library_version";

/// Creates a new slice of tags from an OTLP `InstrumentationLibrary` (now Scope).
pub fn tags_from_instrumentation_library_metadata(scope: &otlp_common::InstrumentationScope) -> Vec<String> {
    vec![
        utils::format_key_value_tag(LIBRARY_NAME_TAG, &scope.name),
        utils::format_key_value_tag(LIBRARY_VERSION_TAG, &scope.version),
    ]
}

#[cfg(test)]
mod tests {
    use otlp_protos::opentelemetry::proto::common::v1::InstrumentationScope;

    use super::*;

    #[test]
    fn library_tags_use_scope_name_and_version() {
        let scope = InstrumentationScope {
            name: "lib.name".to_string(),
            version: "0.9".to_string(),
            ..Default::default()
        };

        assert_eq!(
            tags_from_instrumentation_library_metadata(&scope),
            vec![
                "instrumentation_library:lib.name".to_string(),
                "instrumentation_library_version:0.9".to_string(),
            ]
        );
    }

    #[test]
    fn library_tags_replace_empty_values_with_na() {
        let scope = InstrumentationScope::default();

        assert_eq!(
            tags_from_instrumentation_library_metadata(&scope),
            vec![
                "instrumentation_library:n/a".to_string(),
                "instrumentation_library_version:n/a".to_string(),
            ]
        );
    }
}
