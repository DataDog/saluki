#[derive(Debug, Clone, Copy)]
pub struct OtlpTracesTranslatorConfig {
    pub ignore_missing_datadog_fields: bool,
    pub compute_top_level_by_span_kind: bool,
}

impl Default for OtlpTracesTranslatorConfig {
    fn default() -> Self {
        Self {
            ignore_missing_datadog_fields: false,
            compute_top_level_by_span_kind: false,
        }
    }
}

impl OtlpTracesTranslatorConfig {
    pub fn with_ignore_missing_datadog_fields(mut self, ignore_missing_datadog_fields: bool) -> Self {
        self.ignore_missing_datadog_fields = ignore_missing_datadog_fields;
        self
    }

    pub fn with_compute_top_level_by_span_kind(mut self, compute_top_level_by_span_kind: bool) -> Self {
        self.compute_top_level_by_span_kind = compute_top_level_by_span_kind;
        self
    }
}

