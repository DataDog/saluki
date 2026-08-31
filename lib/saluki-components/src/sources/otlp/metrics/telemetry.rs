//! Self-telemetry for the OTLP metrics translator.
//!
//! These metrics are distinct from the server-level volume counters in [`crate::common::otlp::Metrics`], which
//! track bytes and events received. All counters use a bounded `reason` tag:
//!
//! - `translate`: the translator returned an error for an entire resource batch.
//! - `unsupported_temporality`: a metric point was dropped because its aggregation temporality is not supported.
//! - `histogram_conversion`: a histogram or exponential histogram data point could not be converted.
//! - `invalid_value`: a metric point was dropped because its value was `NaN` or `Infinity`.

use ::metrics::{Counter, Histogram};
use saluki_core::components::ComponentContext;
use saluki_core::observability::ComponentMetricsExt;
use saluki_metrics::MetricsBuilder;

#[derive(Clone)]
pub struct OtlpMetricsTranslatorMetrics {
    errors_translate: Counter,
    dropped_unsupported_temporality: Counter,
    dropped_histogram_conversion: Counter,
    dropped_invalid_value: Counter,
    processing_duration: Histogram,
}

impl OtlpMetricsTranslatorMetrics {
    pub fn from_component_context(component_context: &ComponentContext) -> Self {
        let builder = MetricsBuilder::from_component_context(component_context);

        Self {
            errors_translate: builder
                .register_counter_with_tags("otlp_metrics_errors_total", [("reason", "translate")]),
            dropped_unsupported_temporality: builder.register_counter_with_tags(
                "otlp_metrics_dropped_points_total",
                [("reason", "unsupported_temporality")],
            ),
            dropped_histogram_conversion: builder.register_counter_with_tags(
                "otlp_metrics_dropped_points_total",
                [("reason", "histogram_conversion")],
            ),
            dropped_invalid_value: builder
                .register_counter_with_tags("otlp_metrics_dropped_points_total", [("reason", "invalid_value")]),
            processing_duration: builder.register_histogram("otlp_metrics_processing_duration_seconds"),
        }
    }

    pub fn for_tests() -> Self {
        Self {
            errors_translate: Counter::noop(),
            dropped_unsupported_temporality: Counter::noop(),
            dropped_histogram_conversion: Counter::noop(),
            dropped_invalid_value: Counter::noop(),
            processing_duration: Histogram::noop(),
        }
    }

    pub fn errors_translate(&self) -> &Counter {
        &self.errors_translate
    }

    pub fn dropped_unsupported_temporality(&self) -> &Counter {
        &self.dropped_unsupported_temporality
    }

    pub fn dropped_histogram_conversion(&self) -> &Counter {
        &self.dropped_histogram_conversion
    }

    pub fn dropped_invalid_value(&self) -> &Counter {
        &self.dropped_invalid_value
    }

    pub fn processing_duration(&self) -> &Histogram {
        &self.processing_duration
    }
}
