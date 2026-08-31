//! Self-telemetry for the OTLP metrics translator.
//!
//! Holds counters and histograms that measure translation errors, dropped metric points, and processing latency for
//! OTLP metrics batches. These metrics are distinct from the server-level volume counters in
//! [`crate::common::otlp::Metrics`], which track bytes and events received.
//!
//! All counters use a bounded `reason` tag so that the cardinality of emitted series stays fixed regardless of input.
//! The supported reasons are:
//!
//! - `translate`: a batch-level translation failure (the translator returned an error for the entire resource).
//! - `unsupported_temporality`: a metric point was dropped because its aggregation temporality is not supported.
//! - `histogram_conversion`: a histogram or exponential histogram data point could not be converted.
//! - `invalid_value`: a metric point was dropped because its value was `NaN` or `Infinity`.

use std::time::Duration;

use ::metrics::{Counter, Histogram};
use saluki_core::components::ComponentContext;
use saluki_core::observability::ComponentMetricsExt;
use saluki_metrics::MetricsBuilder;

/// The counter name for OTLP metrics translation errors.
const OTLP_METRICS_ERRORS_TOTAL: &str = "otlp_metrics_errors_total";

/// The counter name for OTLP metric points dropped during translation.
const OTLP_METRICS_DROPPED_POINTS_TOTAL: &str = "otlp_metrics_dropped_points_total";

/// The histogram name for OTLP metrics processing latency, in seconds.
const OTLP_METRICS_PROCESSING_DURATION_SECONDS: &str = "otlp_metrics_processing_duration_seconds";

/// Reason tag value for a batch-level translation error.
const REASON_TRANSLATE: &str = "translate";

/// Reason tag value for an unsupported aggregation temporality drop.
const REASON_UNSUPPORTED_TEMPORALITY: &str = "unsupported_temporality";

/// Reason tag value for a histogram or exponential histogram conversion failure.
const REASON_HISTOGRAM_CONVERSION: &str = "histogram_conversion";

/// Reason tag value for a metric point with an invalid value (`NaN` or `Infinity`).
const REASON_INVALID_VALUE: &str = "invalid_value";

/// Self-telemetry for the OTLP metrics translator.
///
/// Holds translation-specific error and dropped-point counters, plus a processing-latency histogram. The counters
/// use a bounded `reason` tag so the cardinality of emitted series is fixed.
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
    ///
    /// Each counter is pre-registered with its fixed `reason` tag so that the emitted series cardinality is bounded
    /// and does not grow with input.
    pub fn from_component_context(component_context: &ComponentContext) -> Self {
        let builder = MetricsBuilder::from_component_context(component_context);

        Self {
            errors_translate: builder
                .register_counter_with_tags(OTLP_METRICS_ERRORS_TOTAL, [("reason", REASON_TRANSLATE)]),
            dropped_unsupported_temporality: builder.register_counter_with_tags(
                OTLP_METRICS_DROPPED_POINTS_TOTAL,
                [("reason", REASON_UNSUPPORTED_TEMPORALITY)],
            ),
            dropped_histogram_conversion: builder.register_counter_with_tags(
                OTLP_METRICS_DROPPED_POINTS_TOTAL,
                [("reason", REASON_HISTOGRAM_CONVERSION)],
            ),
            dropped_invalid_value: builder
                .register_counter_with_tags(OTLP_METRICS_DROPPED_POINTS_TOTAL, [("reason", REASON_INVALID_VALUE)]),
            processing_duration: builder.register_histogram(OTLP_METRICS_PROCESSING_DURATION_SECONDS),
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

    /// Increments the batch-level translation error counter.
    ///
    /// Call this when an entire resource batch fails to translate and `translate_metrics` returns an error.
    pub fn increment_translate_error(&self) {
        self.errors_translate.increment(1);
    }

    /// Increments the dropped-point counter for an unsupported aggregation temporality.
    ///
    /// Call this when a metric point is dropped because its aggregation temporality is not supported (for example,
    /// an unknown temporality value on a `Sum`, `Histogram`, or `ExponentialHistogram`).
    pub fn increment_dropped_unsupported_temporality(&self) {
        self.dropped_unsupported_temporality.increment(1);
    }

    /// Increments the dropped-point counter for a histogram conversion failure.
    ///
    /// Call this when a histogram or exponential histogram data point could not be converted (for example, a
    /// cumulative exponential histogram, a malformed bucket/bound count mismatch, or a DDSketch remapping failure).
    pub fn increment_dropped_histogram_conversion(&self) {
        self.dropped_histogram_conversion.increment(1);
    }

    /// Increments the dropped-point counter for an invalid metric value.
    ///
    /// Call this when a metric point is dropped because its value is `NaN` or `Infinity`.
    pub fn increment_dropped_invalid_value(&self) {
        self.dropped_invalid_value.increment(1);
    }

    /// Records a processing-latency sample, in seconds.
    ///
    /// Call this for every translated batch, whether the translation succeeded or failed, so the histogram captures
    /// the full distribution of processing times.
    pub fn record_processing_duration(&self, duration: Duration) {
        self.processing_duration.record(duration.as_secs_f64());
    }
}
