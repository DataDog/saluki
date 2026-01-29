use saluki_context::tags::SharedTagSet;
use saluki_core::data_model::event::metric::{Metric, MetricValues};
use saluki_io::compression::CompressionScheme;

/// Metrics intake endpoint.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MetricsEndpoint {
    /// Series metrics.
    ///
    /// Includes counters, gauges, rates, and sets.
    Series,

    /// Sketch metrics.
    ///
    /// Includes histograms and distributions.
    Sketches,
}

impl MetricsEndpoint {
    /// Creates a new `MetricsEndpoint` from the given metric.
    pub fn from_metric(metric: &Metric) -> Self {
        match metric.values() {
            MetricValues::Counter(..) | MetricValues::Rate(..) | MetricValues::Gauge(..) | MetricValues::Set(..) => {
                Self::Series
            }
            MetricValues::Histogram(..) | MetricValues::Distribution(..) => Self::Sketches,
        }
    }
}

pub struct EndpointConfiguration {
    compression_scheme: CompressionScheme,
    max_metrics_per_payload: usize,
    additional_tags: SharedTagSet,
}

impl EndpointConfiguration {
    pub fn new(
        compression_scheme: CompressionScheme, max_metrics_per_payload: usize, additional_tags: Option<SharedTagSet>,
    ) -> Self {
        Self {
            compression_scheme,
            max_metrics_per_payload,
            additional_tags: additional_tags.unwrap_or_default(),
        }
    }

    pub fn compression_scheme(&self) -> CompressionScheme {
        self.compression_scheme
    }

    pub fn max_metrics_per_payload(&self) -> usize {
        self.max_metrics_per_payload
    }

    pub fn additional_tags(&self) -> &SharedTagSet {
        &self.additional_tags
    }
}
