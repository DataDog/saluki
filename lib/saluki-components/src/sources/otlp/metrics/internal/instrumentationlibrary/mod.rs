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
