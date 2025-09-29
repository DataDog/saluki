// https://github.com/DataDog/datadog-agent/blob/main/pkg/opentelemetry-mapping-go/otlp/metrics/config.go#L131-L140
#[derive(Debug, Clone, Copy, PartialEq)]
#[allow(dead_code)]
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
#[allow(dead_code)]
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
#[allow(dead_code)]
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

use std::time::Duration;

#[derive(Debug, Clone, Copy)]
#[allow(dead_code)]
pub struct OtlpTranslatorConfig {
    pub hist_mode: HistogramMode,
    pub send_histogram_aggregations: bool,
    pub number_mode: NumberMode,
    pub initial_cumul_mono_value_mode: InitialCumulMonoValueMode,
    pub instrumentation_scope_metadata_as_tags: bool,
    pub instrumentation_library_metadata_as_tags: bool,
    // Reports whether certain metrics that are only available when using
    // the Datadog Agent should be obtained by remapping from OTEL counterparts (e.g.
    // container.* and system.* metrics). This configuration also enables with_otel_prefix.
    pub with_remapping: bool,
    //  Reports whether some OpenTelemetry metrics (ex: host metrics) should be
    // renamed with the `otel.` prefix. This prevents the Collector and Datadog
    // Agent from computing metrics with the same names.
    pub with_otel_prefix: bool,
    // Points cache settings
    pub delta_ttl: Duration,
    pub sweep_interval: Duration,
}

#[allow(dead_code)]
impl OtlpTranslatorConfig {
    pub fn with_remapping(mut self, with_remapping: bool) -> Self {
        self.with_remapping = with_remapping;
        if with_remapping {
            self.with_otel_prefix = true;
        }
        self
    }

    pub fn with_otel_prefix(mut self, with_otel_prefix: bool) -> Self {
        self.with_otel_prefix = with_otel_prefix;
        self
    }

    pub fn with_initial_cumul_mono_value_mode(
        mut self, initial_cumul_mono_value_mode: InitialCumulMonoValueMode,
    ) -> Self {
        self.initial_cumul_mono_value_mode = initial_cumul_mono_value_mode;
        self
    }

    pub fn with_histogram_mode(mut self, hist_mode: HistogramMode) -> Self {
        self.hist_mode = hist_mode;
        self
    }

    pub fn with_number_mode(mut self, number_mode: NumberMode) -> Self {
        self.number_mode = number_mode;
        self
    }

    pub fn with_send_histogram_aggregations(mut self, send_histogram_aggregations: bool) -> Self {
        self.send_histogram_aggregations = send_histogram_aggregations;
        self
    }

    pub fn with_instrumentation_scope_metadata_as_tags(mut self, instrumentation_scope_metadata_as_tags: bool) -> Self {
        self.instrumentation_scope_metadata_as_tags = instrumentation_scope_metadata_as_tags;
        self
    }

    pub fn with_instrumentation_library_metadata_as_tags(
        mut self, instrumentation_library_metadata_as_tags: bool,
    ) -> Self {
        self.instrumentation_library_metadata_as_tags = instrumentation_library_metadata_as_tags;
        self
    }

    pub fn with_delta_ttl(mut self, ttl: Duration) -> Self {
        self.delta_ttl = ttl;
        self
    }

    pub fn with_sweep_interval(mut self, interval: Duration) -> Self {
        self.sweep_interval = interval;
        self
    }
}

impl Default for OtlpTranslatorConfig {
    fn default() -> Self {
        Self {
            hist_mode: HistogramMode::default(),
            send_histogram_aggregations: true,
            number_mode: NumberMode::default(),
            initial_cumul_mono_value_mode: InitialCumulMonoValueMode::default(),
            instrumentation_scope_metadata_as_tags: false,
            instrumentation_library_metadata_as_tags: false,
            with_remapping: false,
            with_otel_prefix: false,
            delta_ttl: Duration::from_secs(60),
            sweep_interval: Duration::from_secs(30),
        }
    }
}
