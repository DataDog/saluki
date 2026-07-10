use std::time::Duration;

use agent_data_plane_config::domains::otlp::{
    CumulativeMonotonicMode, HistogramMode, InitialCumulativeMonotonicValue, SummaryMode,
};
use saluki_error::{generic_error, GenericError};

const DEFAULT_DELTA_TTL: Duration = Duration::from_secs(3600);
const DEFAULT_SWEEP_INTERVAL: Duration = Duration::from_secs(1800);

#[derive(Debug, Clone, Copy)]
#[allow(dead_code)]
pub struct OtlpMetricsTranslatorConfig {
    pub hist_mode: HistogramMode,
    pub send_histogram_aggregations: bool,
    pub cumulative_monotonic_mode: CumulativeMonotonicMode,
    pub initial_cumulative_monotonic_value: InitialCumulativeMonotonicValue,
    pub instrumentation_scope_metadata_as_tags: bool,
    pub instrumentation_library_metadata_as_tags: bool,
    // Whether scalar resource attributes should be added as raw tags on emitted metrics, in
    // addition to the recognized mappings that are always applied.
    pub resource_attributes_as_tags: bool,
    // Reports whether certain metrics that are only available when using
    // the Datadog Agent should be obtained by remapping from OTEL counterparts (for example,
    // container.* and system.* metrics). This configuration also enables with_otel_prefix.
    pub with_remapping: bool,
    //  Reports whether some OpenTelemetry metrics (ex: host metrics) should be
    // renamed with the `otel.` prefix. This prevents the Collector and Datadog
    // Agent from computing metrics with the same names.
    pub with_otel_prefix: bool,
    pub quantiles: bool,
    // Points cache settings
    pub delta_ttl: Duration,
    pub sweep_interval: Duration,
    pub infer_delta_interval: bool,
}

#[allow(dead_code)]
impl OtlpMetricsTranslatorConfig {
    pub fn validate(&self) -> Result<(), GenericError> {
        if self.hist_mode == HistogramMode::NoBuckets && !self.send_histogram_aggregations {
            return Err(generic_error!("no buckets mode and no send count sum are incompatible"));
        }
        Ok(())
    }

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

    pub fn with_initial_cumulative_monotonic_value(
        mut self, initial_cumulative_monotonic_value: InitialCumulativeMonotonicValue,
    ) -> Self {
        self.initial_cumulative_monotonic_value = initial_cumulative_monotonic_value;
        self
    }

    pub fn with_histogram_mode(mut self, hist_mode: HistogramMode) -> Self {
        self.hist_mode = hist_mode;
        self
    }

    pub fn with_cumulative_monotonic_mode(mut self, cumulative_monotonic_mode: CumulativeMonotonicMode) -> Self {
        self.cumulative_monotonic_mode = cumulative_monotonic_mode;
        self
    }

    pub fn with_send_histogram_aggregations(mut self, send_histogram_aggregations: bool) -> Self {
        self.send_histogram_aggregations = send_histogram_aggregations;
        self
    }

    /// Sets how OTLP summary quantiles are reported.
    pub fn with_summary_mode(mut self, summary_mode: SummaryMode) -> Self {
        self.quantiles = matches!(summary_mode, SummaryMode::Gauges);
        self
    }

    pub fn with_infer_delta_interval(mut self, infer_delta_interval: bool) -> Self {
        self.infer_delta_interval = infer_delta_interval;
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

    pub fn with_resource_attributes_as_tags(mut self, resource_attributes_as_tags: bool) -> Self {
        self.resource_attributes_as_tags = resource_attributes_as_tags;
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

impl Default for OtlpMetricsTranslatorConfig {
    fn default() -> Self {
        Self {
            hist_mode: HistogramMode::default(),
            send_histogram_aggregations: false,
            cumulative_monotonic_mode: CumulativeMonotonicMode::default(),
            initial_cumulative_monotonic_value: InitialCumulativeMonotonicValue::default(),
            instrumentation_scope_metadata_as_tags: true,
            instrumentation_library_metadata_as_tags: false,
            resource_attributes_as_tags: false,
            with_remapping: false,
            with_otel_prefix: false,
            quantiles: false,
            infer_delta_interval: false,
            delta_ttl: DEFAULT_DELTA_TTL,
            sweep_interval: DEFAULT_SWEEP_INTERVAL,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Ports the `TestConfig`/`(*Config).validate` rejection rule from the Go
    // opentelemetry-mapping-go implementation: "no buckets" histogram mode is only valid when
    // count/sum aggregations are also being sent.
    // https://github.com/DataDog/datadog-agent/blob/main/pkg/opentelemetry-mapping-go/otlp/metrics/config.go
    #[test]
    fn validate_rejects_no_buckets_mode_without_histogram_aggregations() {
        let config = OtlpMetricsTranslatorConfig::default()
            .with_histogram_mode(HistogramMode::NoBuckets)
            .with_send_histogram_aggregations(false);

        let err = config
            .validate()
            .expect_err("no-buckets histogram mode without aggregations must be rejected");
        assert_eq!(
            err.to_string(),
            "no buckets mode and no send count sum are incompatible"
        );
    }

    #[test]
    fn validate_allows_no_buckets_mode_with_histogram_aggregations() {
        // The rejection is specific to the missing aggregations: enabling them makes the same
        // histogram mode valid.
        let config = OtlpMetricsTranslatorConfig::default()
            .with_histogram_mode(HistogramMode::NoBuckets)
            .with_send_histogram_aggregations(true);

        assert!(config.validate().is_ok());
    }

    #[test]
    fn validate_allows_bucketed_modes_regardless_of_aggregations() {
        // Only the `NoBuckets`/`!send_histogram_aggregations` pair is incompatible; the bucketed
        // modes are accepted even without aggregations.
        for hist_mode in [HistogramMode::Counters, HistogramMode::Distributions] {
            let config = OtlpMetricsTranslatorConfig::default()
                .with_histogram_mode(hist_mode)
                .with_send_histogram_aggregations(false);

            assert!(config.validate().is_ok(), "expected {hist_mode:?} to validate");
        }
    }
}
