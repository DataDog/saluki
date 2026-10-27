use std::time::Duration;

use saluki_core::data_model::origin::OriginTagCardinality;
use saluki_error::{generic_error, GenericError};

/// How OTLP histograms are reported.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HistogramMode {
    /// Report no bucket metrics.
    NoBuckets,

    /// Report each bucket as a counter.
    Counters,

    /// Report histograms as distributions.
    Distributions,
}

/// How cumulative monotonic OTLP sums are reported.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CumulativeMonotonicMode {
    /// Report the difference between consecutive points.
    ToDelta,

    /// Report the raw cumulative value.
    RawValue,
}

/// How the first point of a cumulative monotonic OTLP sum is reported.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InitialCumulativeMonotonicValue {
    /// Keep the first point only when the series began after the process started and the point is not at the series
    /// start time. Otherwise drop it.
    Auto,

    /// Always drop the first point.
    Drop,

    /// Always keep the first point.
    Keep,
}

/// How OTLP summaries are reported.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SummaryMode {
    /// Report quantiles as gauges.
    Gauges,

    /// Report no quantiles.
    NoQuantiles,
}

/// Settings of OTLP metrics translation.
#[derive(Clone, Debug, PartialEq)]
pub struct OtlpMetricsSettings {
    /// Cardinality of entity and global tags resolved from an OTLP resource.
    pub tag_cardinality: OriginTagCardinality,

    /// How histograms are reported.
    pub histogram_mode: HistogramMode,

    /// Whether histograms also report count, sum, min, and max aggregations.
    pub send_histogram_aggregations: bool,

    /// Whether scalar resource attributes are added as tags, in addition to the recognized mappings that are always
    /// applied.
    pub resource_attributes_as_tags: bool,

    /// Whether instrumentation scope metadata is added as tags.
    pub instrumentation_scope_metadata_as_tags: bool,

    /// How cumulative monotonic sums are reported.
    pub cumulative_monotonic_mode: CumulativeMonotonicMode,

    /// How the first point of a cumulative monotonic sum is reported.
    pub initial_cumulative_monotonic_value: InitialCumulativeMonotonicValue,

    /// Comma-separated tags added to every metric. Empty for none.
    pub tags: String,

    /// How summaries are reported.
    pub summary_mode: SummaryMode,

    /// How long cumulative points are cached to compute deltas.
    pub delta_ttl: Duration,
}

#[derive(Debug, Clone, Copy)]
#[allow(dead_code)]
pub struct OtlpMetricsTranslatorConfig {
    /// Cardinality of entity and global tags resolved from an OTLP resource.
    pub tag_cardinality: OriginTagCardinality,
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
    pub infer_delta_interval: bool,
}

#[allow(dead_code)]
impl OtlpMetricsTranslatorConfig {
    /// Creates a translator configuration from the source's metrics settings.
    pub fn from_settings(settings: &OtlpMetricsSettings) -> Self {
        Self {
            tag_cardinality: settings.tag_cardinality,
            hist_mode: settings.histogram_mode,
            send_histogram_aggregations: settings.send_histogram_aggregations,
            cumulative_monotonic_mode: settings.cumulative_monotonic_mode,
            initial_cumulative_monotonic_value: settings.initial_cumulative_monotonic_value,
            instrumentation_scope_metadata_as_tags: settings.instrumentation_scope_metadata_as_tags,
            instrumentation_library_metadata_as_tags: false,
            resource_attributes_as_tags: settings.resource_attributes_as_tags,
            with_remapping: false,
            with_otel_prefix: false,
            quantiles: matches!(settings.summary_mode, SummaryMode::Gauges),
            delta_ttl: settings.delta_ttl,
            infer_delta_interval: false,
        }
    }

    /// Returns a translator configuration with the configuration defaults, except that quantiles are not reported.
    #[cfg(test)]
    pub fn for_tests() -> Self {
        Self {
            tag_cardinality: OriginTagCardinality::Low,
            hist_mode: HistogramMode::Distributions,
            send_histogram_aggregations: false,
            cumulative_monotonic_mode: CumulativeMonotonicMode::ToDelta,
            initial_cumulative_monotonic_value: InitialCumulativeMonotonicValue::Auto,
            instrumentation_scope_metadata_as_tags: true,
            instrumentation_library_metadata_as_tags: false,
            resource_attributes_as_tags: false,
            with_remapping: false,
            with_otel_prefix: false,
            quantiles: false,
            infer_delta_interval: false,
            delta_ttl: Duration::from_secs(3600),
        }
    }

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

    pub fn with_histogram_mode(mut self, hist_mode: HistogramMode) -> Self {
        self.hist_mode = hist_mode;
        self
    }

    pub fn with_send_histogram_aggregations(mut self, send_histogram_aggregations: bool) -> Self {
        self.send_histogram_aggregations = send_histogram_aggregations;
        self
    }

    pub fn with_infer_delta_interval(mut self, infer_delta_interval: bool) -> Self {
        self.infer_delta_interval = infer_delta_interval;
        self
    }

    pub fn with_instrumentation_library_metadata_as_tags(
        mut self, instrumentation_library_metadata_as_tags: bool,
    ) -> Self {
        self.instrumentation_library_metadata_as_tags = instrumentation_library_metadata_as_tags;
        self
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
        let config = OtlpMetricsTranslatorConfig::for_tests()
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
        let config = OtlpMetricsTranslatorConfig::for_tests()
            .with_histogram_mode(HistogramMode::NoBuckets)
            .with_send_histogram_aggregations(true);

        assert!(config.validate().is_ok());
    }

    #[test]
    fn validate_allows_bucketed_modes_regardless_of_aggregations() {
        // Only the `NoBuckets`/`!send_histogram_aggregations` pair is incompatible; the bucketed
        // modes are accepted even without aggregations.
        for hist_mode in [HistogramMode::Counters, HistogramMode::Distributions] {
            let config = OtlpMetricsTranslatorConfig::for_tests()
                .with_histogram_mode(hist_mode)
                .with_send_histogram_aggregations(false);

            assert!(config.validate().is_ok(), "expected {hist_mode:?} to validate");
        }
    }
}
