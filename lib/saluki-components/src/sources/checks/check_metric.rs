use std::time::Duration;

use saluki_context::{
    tags::{Tag, TagSet},
    Context,
};
use saluki_core::data_model::event::metric::{Metric, MetricMetadata, MetricValues};
use saluki_core::data_model::event::Event;
use tracing::warn;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MetricType {
    Gauge,
    Rate,
    Count,
    MonotonicCount,
    Counter,
    Histogram,
    Historate,
}

impl From<i32> for MetricType {
    fn from(v: i32) -> Self {
        match v {
            0 => MetricType::Gauge,
            1 => MetricType::Rate,
            2 => MetricType::Count,
            3 => MetricType::MonotonicCount,
            4 => MetricType::Counter,
            5 => MetricType::Histogram,
            6 => MetricType::Historate,
            _ => {
                warn!("Unknown metric type: {}, considering it as a gauge", v);
                MetricType::Gauge
            }
        }
    }
}

/// CheckMetric are used to transmit metrics from python check execution results
/// to forward in the saluki's pipeline.
#[derive(Debug, PartialEq)]
pub struct CheckMetric {
    name: String,
    metric_type: MetricType,
    value: f64,
    tags: Vec<String>,
}

impl CheckMetric {
    pub fn new(name: String, metric_type: MetricType, value: f64, tags: Vec<String>) -> Self {
        Self {
            name,
            metric_type,
            value,
            tags,
        }
    }
}

impl From<CheckMetric> for Event {
    fn from(check_metric: CheckMetric) -> Self {
        let tags = check_metric
            .tags
            .into_iter()
            .map(Tag::from)
            .collect::<TagSet>()
            .into_shared();

        let context = Context::from_parts(check_metric.name, tags);
        let metadata = MetricMetadata::default();

        let values = match check_metric.metric_type {
            MetricType::Gauge => MetricValues::gauge(check_metric.value),
            MetricType::Counter => MetricValues::counter(check_metric.value),
            MetricType::Histogram => MetricValues::histogram(check_metric.value),
            // TODO what is historate? what do I do with it?
            MetricType::Historate => MetricValues::gauge(check_metric.value),
            // TODO incorrect handling of monotonic count
            MetricType::MonotonicCount => MetricValues::counter(check_metric.value),
            MetricType::Rate => MetricValues::rate(check_metric.value, Duration::from_secs(1)),
            // TODO incorrect handling of count
            MetricType::Count => MetricValues::counter(check_metric.value),
        };

        Event::Metric(Metric::from_parts(context, values, metadata))
    }
}
