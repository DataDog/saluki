// https://github.com/DataDog/datadog-agent/blob/main/pkg/opentelemetry-mapping-go/otlp/metrics/config.go#L131-L140
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum HistogramMode {
    NoBuckets,
    Counters,
    Distributions,
}

impl Default for HistogramMode {
    fn default() -> Self {
        Self::Distributions
    }
}

// https://github.com/DataDog/datadog-agent/blob/main/pkg/opentelemetry-mapping-go/otlp/metrics/config.go#L178-L190
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum NumberMode {
    CumulativeToDelta,
    RawValue,
}

impl Default for NumberMode {
    fn default() -> Self {
        Self::CumulativeToDelta
    }
}

// https://github.com/DataDog/datadog-agent/blob/main/pkg/opentelemetry-mapping-go/otlp/metrics/config.go#L209-L224
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum InitialCumulMonoValueMode {
    Auto,
    Drop,
    Keep,
}

impl Default for InitialCumulMonoValueMode {
    fn default() -> Self {
        Self::Auto
    }
}

#[derive(Debug, Clone, Copy)]
pub struct OtlpTranslatorConfig {
    pub hist_mode: HistogramMode,
    pub send_histogram_aggregations: bool,
    pub number_mode: NumberMode,
    pub initial_cumul_mono_value_mode: InitialCumulMonoValueMode,
    pub instrumentation_scope_metadata_as_tags: bool,
}

impl Default for OtlpTranslatorConfig {
    fn default() -> Self {
        Self {
            hist_mode: HistogramMode::default(),
            send_histogram_aggregations: true,
            number_mode: NumberMode::default(),
            initial_cumul_mono_value_mode: InitialCumulMonoValueMode::default(),
            instrumentation_scope_metadata_as_tags: false,
        }
    }
}
