use ::metrics::{Counter, Histogram};
use saluki_core::components::ComponentContext;
use saluki_core::observability::ComponentMetricsExt;
use saluki_metrics::MetricsBuilder;

/// Self-telemetry for the OTLP metrics translator.
///
/// Holds translation-specific error and dropped-point counters, plus a processing-latency histogram.
#[derive(Clone)]
pub struct OtlpMetricsTranslatorMetrics {
    errors_translate: Counter,
    dropped_unsupported_temporality: Counter,
    dropped_histogram_conversion: Counter,
    dropped_invalid_value: Counter,
    processing_duration: Histogram,
}

impl OtlpMetricsTranslatorMetrics {
    /// Builds the translator telemetry from the given component context.
    pub fn from_component_context(component_context: &ComponentContext) -> Self {
        let builder = MetricsBuilder::from_component_context(component_context);

        Self {
            errors_translate: builder.register_counter_with_tags("component_errors_total", [("reason", "translate")]),
            dropped_unsupported_temporality: builder.register_counter_with_tags(
                "component_events_dropped_total",
                [("reason", "unsupported_temporality")],
            ),
            dropped_histogram_conversion: builder
                .register_counter_with_tags("component_events_dropped_total", [("reason", "histogram_conversion")]),
            dropped_invalid_value: builder
                .register_counter_with_tags("component_events_dropped_total", [("reason", "invalid_value")]),
            processing_duration: builder.register_histogram("component_processing_duration_seconds"),
        }
    }

    /// Creates a no-op instance for use in tests where real metric recording is not needed.
    pub fn for_tests() -> Self {
        Self {
            errors_translate: Counter::noop(),
            dropped_unsupported_temporality: Counter::noop(),
            dropped_histogram_conversion: Counter::noop(),
            dropped_invalid_value: Counter::noop(),
            processing_duration: Histogram::noop(),
        }
    }

    /// Returns the counter for batch-level translation errors.
    pub fn errors_translate(&self) -> &Counter {
        &self.errors_translate
    }

    /// Returns the counter for points dropped due to unsupported aggregation temporality.
    pub fn dropped_unsupported_temporality(&self) -> &Counter {
        &self.dropped_unsupported_temporality
    }

    /// Returns the counter for points dropped due to histogram conversion failures.
    pub fn dropped_histogram_conversion(&self) -> &Counter {
        &self.dropped_histogram_conversion
    }

    /// Returns the counter for points dropped due to invalid values (`NaN` or `Infinity`).
    pub fn dropped_invalid_value(&self) -> &Counter {
        &self.dropped_invalid_value
    }

    /// Returns the histogram for OTLP metrics processing latency.
    pub fn processing_duration(&self) -> &Histogram {
        &self.processing_duration
    }
}
