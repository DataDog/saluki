//! OTLP `os` semantic conventions.
//!
//! <https://github.com/open-telemetry/opentelemetry-specification/blob/main/specification/resource/semantic_conventions/os.md>

use opentelemetry_semantic_conventions::resource::OS_TYPE;

#[derive(Debug, Default)]
pub(super) struct SystemAttributes {
    pub os_type: String,
}

impl SystemAttributes {
    /// Extracts a list of Datadog tags from the system attributes.
    pub fn extract_tags(&self) -> Vec<String> {
        let mut tags = Vec::new();
        if !self.os_type.is_empty() {
            tags.push(format!("{}:{}", OS_TYPE, self.os_type));
        }
        tags
    }
}
