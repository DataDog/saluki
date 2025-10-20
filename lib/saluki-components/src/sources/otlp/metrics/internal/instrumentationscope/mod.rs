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

/// Creates tags for when instrumentation scope is not present in the OTLP payload.
/// This matches DD Agent behavior which adds "n/a" values for missing scope.
pub fn tags_from_empty_instrumentation_scope() -> Vec<String> {
    vec![
        utils::format_key_value_tag(SCOPE_NAME_TAG, ""),
        utils::format_key_value_tag(SCOPE_VERSION_TAG, ""),
    ]
}
