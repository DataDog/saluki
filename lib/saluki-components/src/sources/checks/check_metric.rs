use std::time::Duration;

use saluki_context::{
    tags::{Tag, TagSet},
    Context,
};
use saluki_core::data_model::event::metric::{Metric, MetricMetadata, MetricValues};
use saluki_core::data_model::event::Event;
use tracing::warn;

#[derive(Debug)]
pub enum AggregatorError {
    UnsupportedType,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MetricType {
    Gauge = 0,
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

impl TryInto<Event> for CheckMetric {
    type Error = AggregatorError;

    fn try_into(self) -> Result<Event, Self::Error> {
        // Convert Vec<String> to Vec<Tag>
        let tags: Vec<Tag> = self.tags.into_iter().map(Tag::from).collect();

        // Convert Vec<Tag> to TagSet
        let tagset: TagSet = tags.into_iter().collect();

        let context = Context::from_parts(self.name, tagset);
        let metadata = MetricMetadata::default();

        match self.metric_type {
            MetricType::Gauge => Ok(Event::Metric(Metric::from_parts(
                context,
                MetricValues::gauge(self.value),
                metadata,
            ))),
            MetricType::Counter => Ok(Event::Metric(Metric::from_parts(
                context,
                MetricValues::counter(self.value),
                metadata,
            ))),
            MetricType::Histogram => Ok(Event::Metric(Metric::from_parts(
                context,
                MetricValues::histogram(self.value),
                metadata,
            ))),
            MetricType::Historate => Ok(Event::Metric(Metric::from_parts(
                context,
                // TODO what is historate? what do I do with it?
                MetricValues::gauge(self.value),
                metadata,
            ))),
            MetricType::MonotonicCount => Ok(Event::Metric(Metric::from_parts(
                context,
                // TODO incorrect handling of monotonic count
                MetricValues::counter(self.value),
                metadata,
            ))),
            // TODO: The Agent tracks rate of a metric over 2 successive flushes
            MetricType::Rate => Ok(Event::Metric(Metric::from_parts(
                context,
                MetricValues::rate(self.value, Duration::from_secs(1)),
                metadata,
            ))),
            MetricType::Count => Ok(Event::Metric(Metric::from_parts(
                context,
                // TODO incorrect handling of count
                MetricValues::counter(self.value),
                metadata,
            ))),
        }
    }
}
