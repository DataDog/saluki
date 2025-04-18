#![allow(dead_code)]
use std::sync::Arc;

use futures::stream::StreamExt as _;
use papaya::HashMap;
use saluki_context::Context;
use saluki_core::{
    observability::metrics::MetricsStream,
    state::reflector::{Processor, Reflector},
};
use saluki_event::{metric::MetricValues, Event};
use tokio::sync::OnceCell;

/// Aggregated metric value.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum AggregatedMetricValue {
    /// A counter.
    Counter(f64),

    /// A gauge.
    Gauge(f64),
}

impl AggregatedMetricValue {
    /// Returns the value of the metric.
    pub const fn value(&self) -> f64 {
        match self {
            AggregatedMetricValue::Counter(value) => *value,
            AggregatedMetricValue::Gauge(value) => *value,
        }
    }
}

#[derive(Clone, Copy)]
struct AggregatedMetric {
    timestamp: Option<u64>,
    value: AggregatedMetricValue,
}

impl AggregatedMetric {
    fn counter(value: f64) -> Self {
        Self {
            timestamp: None,
            value: AggregatedMetricValue::Counter(value),
        }
    }

    fn gauge(timestamp: u64, value: f64) -> Self {
        Self {
            timestamp: Some(timestamp),
            value: AggregatedMetricValue::Gauge(value),
        }
    }

    fn merge(&self, other: Self) -> Self {
        match (self.value, other.value) {
            (AggregatedMetricValue::Counter(a), AggregatedMetricValue::Counter(b)) => Self {
                timestamp: None,
                value: AggregatedMetricValue::Counter(a + b),
            },
            (AggregatedMetricValue::Gauge(a), AggregatedMetricValue::Gauge(b)) => {
                let ts_a = self.timestamp.unwrap_or(0);
                let ts_b = other.timestamp.unwrap_or(0);
                let (new_ts, new_value) = if ts_a > ts_b { (ts_a, a) } else { (ts_b, b) };

                Self {
                    timestamp: Some(new_ts),
                    value: AggregatedMetricValue::Gauge(new_value),
                }
            }
            (_, _) => other,
        }
    }
}

struct Inner {
    metrics: HashMap<Context, AggregatedMetric>,
}

/// Aggregated metrics state.
pub struct AggregatedMetricsState {
    inner: Arc<Inner>,
}

impl AggregatedMetricsState {
    /// Visits all metrics.
    pub fn visit_metrics<F>(&self, mut visitor: F)
    where
        F: FnMut(&Context, &AggregatedMetricValue),
    {
        self.inner
            .metrics
            .pin()
            .iter()
            .for_each(|(context, value)| visitor(context, &value.value));
    }

    /// Searches the state for a single metric with a matching name.
    ///
    /// The latest value for the metric is returned.
    ///
    /// If no metric is found with a matching name, or if multiple metrics are found with a matching name, `None` is
    /// returned.
    pub fn find_single(&self, name: &str) -> Option<f64> {
        self.find_single_with_tags(name, &[])
    }

    /// Searches the state for a single metric with a matching name and tags.
    ///
    /// The latest value for the metric is returned.
    ///
    /// If no metric is found with a matching name and tags, or if multiple metrics are found with a matching name and
    /// tags, `None` is returned. Tags are matched on a partial basis: a metric must at least have the tags provided,
    /// but may have additional tags as well.
    pub fn find_single_with_tags(&self, name: &str, tags: &[&str]) -> Option<f64> {
        let mut had_existing = false;
        let mut maybe_metric = None;

        self.visit_metrics(|context, value| {
            if context.name() == name {
                for tag in tags {
                    if !context.tags().has_tag(tag) {
                        return;
                    }
                }

                had_existing = maybe_metric.is_some();
                maybe_metric = Some(value.value());
            }
        });

        if had_existing {
            None
        } else {
            maybe_metric
        }
    }

    /// Searches the state for all counter metrics with a matching name and returns their aggregated value.
    ///
    /// This method allows rolling up counter metrics that share a common name. For example, a metric may be emitted
    /// with the same name, but N different values for a specific tag, potentially as a way to break down the metric by
    /// a certain facet. This method can allow re-aggregating the value of each different facet value into a single
    /// value.
    ///
    /// If multiple metrics are found with a matching name, but they are not all counter metrics, or if no metrics are
    /// found with a matching name, `0.0` is returned.
    pub fn get_aggregated(&self, name: &str) -> f64 {
        self.get_aggregated_with_tags(name, &[])
    }

    /// Searches the state for all counter metrics with a matching name and returns their aggregated value.
    ///
    /// This method allows rolling up counter metrics that share a common name. For example, a metric may be emitted
    /// with the same name, but N different values for a specific tag, potentially as a way to break down the metric by
    /// a certain facet. This method can allow re-aggregating the value of each different facet value into a single
    /// value.
    ///
    /// If multiple metrics are found with a matching name, but they are not all counter metrics, or if no metrics are
    /// found with a matching name, `0.0` is returned.
    pub fn get_aggregated_with_tags(&self, name: &str, tags: &[&str]) -> f64 {
        let mut total = 0.0;

        self.visit_metrics(|context, value| {
            if context.name() == name {
                for tag in tags {
                    if !context.tags().has_tag(tag) {
                        return;
                    }
                }

                if let AggregatedMetricValue::Counter(value) = value {
                    total += *value;
                }
            }
        });

        total
    }
}

/// Aggregated metrics processor.
///
/// This processor maintains a map of aggregated metrics, where each metric is represented by its context (name and
/// tags), and the aggregated value of the metric. Only counters and gauges are supported, and all other metric types
/// are ignored.
///
/// Aggregation follows the following rules:
///
/// - Counters are summed together.
/// - Gauges are represented by the latest value.
///
/// Aggregated have no concept of "points" -- specific values at a specific timestamped -- or multiple data points: each metric simply has a single value that represents
/// the aggregated amount since the processor was created.
#[derive(Clone)]
pub struct AggregatedMetricsProcessor;

impl Processor for AggregatedMetricsProcessor {
    type Input = Event;
    type State = AggregatedMetricsState;

    fn build_initial_state(&self) -> Self::State {
        AggregatedMetricsState {
            inner: Arc::new(Inner {
                metrics: HashMap::new(),
            }),
        }
    }

    fn process(&self, input: Self::Input, state: &Self::State) {
        if let Some(metric) = input.try_into_metric() {
            let (context, values, _) = metric.into_parts();
            if let Some(agg_metric) = metric_values_to_aggregated(values) {
                state.inner.metrics.pin().update_or_insert_with(
                    context,
                    |existing| existing.merge(agg_metric),
                    || agg_metric,
                );
            }
        }
    }
}

fn metric_values_to_aggregated(values: MetricValues) -> Option<AggregatedMetric> {
    match values {
        MetricValues::Counter(points) => {
            // Extract all points and sum their values together.
            let value = points.into_iter().map(|(_, value)| value).sum();
            Some(AggregatedMetric::counter(value))
        }
        MetricValues::Gauge(points) => {
            // Take the last point value, which will be the latest timestamped point, to represent the "current" value.
            points
                .into_iter()
                .last()
                .map(|(ts, value)| AggregatedMetric::gauge(ts.map(|ts| ts.get()).unwrap_or(0), value))
        }
        _ => None,
    }
}

/// Gets the shared metrics state, which provides unified access to internal metrics in a simplified interface.
///
/// This is lazily initialized and will only be created when it is first accessed.
pub async fn get_shared_metrics_state() -> Reflector<AggregatedMetricsProcessor> {
    static REFLECTOR: OnceCell<Reflector<AggregatedMetricsProcessor>> = OnceCell::const_new();
    REFLECTOR
        .get_or_init(|| async {
            let metrics_stream = MetricsStream::register().map(Arc::unwrap_or_clone);
            Reflector::new(metrics_stream, AggregatedMetricsProcessor).await
        })
        .await
        .clone()
}

#[cfg(test)]
mod tests {
    use saluki_event::metric::Metric;

    use super::*;

    fn process_metrics(metrics: Vec<Event>) -> Vec<(String, AggregatedMetricValue)> {
        let processor = AggregatedMetricsProcessor;
        let state = processor.build_initial_state();

        for metric in metrics {
            processor.process(metric, &state);
        }

        let mut result = Vec::new();
        state.visit_metrics(|context, value| {
            result.push((context.name().to_string(), *value));
        });

        result.sort_by(|(name_a, _), (name_b, _)| name_a.cmp(name_b));

        result
    }

    #[test]
    fn test_aggregate_multiple() {
        // We should be able to process multiple metric types.
        let input_metrics = vec![
            Event::Metric(Metric::counter("counter", 14.0)),
            Event::Metric(Metric::gauge("gauge", 28.0)),
        ];

        let aggregated_metrics = process_metrics(input_metrics);
        assert_eq!(
            aggregated_metrics,
            vec![
                ("counter".to_string(), AggregatedMetricValue::Counter(14.0)),
                ("gauge".to_string(), AggregatedMetricValue::Gauge(28.0)),
            ]
        );
    }

    #[test]
    fn test_aggregate_counters() {
        // We should see all counter values summed together, regardless of the number of points per metric or whether or
        // not the points are timestamped.
        let input_metrics = vec![
            Event::Metric(Metric::counter("counter", 14.0)),
            Event::Metric(Metric::counter("counter", [(123456, 22.0)])),
            Event::Metric(Metric::counter("counter", [(123456, 67.0), (123457, 44.0)])),
        ];

        let aggregated_metrics = process_metrics(input_metrics);
        assert_eq!(
            aggregated_metrics,
            vec![("counter".to_string(), AggregatedMetricValue::Counter(147.0))]
        );
    }

    #[test]
    fn test_aggregate_gauges() {
        // We should see that the gauge value from the latest (highest) timestamp is kept.
        let input_metrics = vec![
            Event::Metric(Metric::gauge("gauge", 14.0)),
            Event::Metric(Metric::gauge("gauge", [(123458, 44.0)])),
            Event::Metric(Metric::gauge("gauge", [(123455, 67.0), (123457, 88.0)])),
        ];

        let aggregated_metrics = process_metrics(input_metrics);
        assert_eq!(
            aggregated_metrics,
            vec![("gauge".to_string(), AggregatedMetricValue::Gauge(44.0))]
        );
    }

    #[test]
    fn test_aggregate_gauges_bias_incoming() {
        // We should see that the gauge value from the latest (highest) timestamp is kept, but when we have two points
        // with an identical timestamp, we should bias to keeping the incoming value.
        let input_metrics = vec![
            Event::Metric(Metric::gauge("gauge", [(123456, 33.0)])),
            Event::Metric(Metric::gauge("gauge", [(123456, 66.0)])),
        ];

        let aggregated_metrics = process_metrics(input_metrics);
        assert_eq!(
            aggregated_metrics,
            vec![("gauge".to_string(), AggregatedMetricValue::Gauge(66.0))]
        );
    }

    #[test]
    fn test_aggregate_type_change() {
        // When aggregating metrics with identical contexts but different types (i.e. counter vs gauge), the aggregated
        // metric should be the last type seen.
        let input_metrics = vec![
            Event::Metric(Metric::gauge("my_metric", 33.0)),
            Event::Metric(Metric::counter("my_metric", 42.0)),
        ];

        let aggregated_metrics = process_metrics(input_metrics);
        assert_eq!(
            aggregated_metrics,
            vec![("my_metric".to_string(), AggregatedMetricValue::Counter(42.0))]
        );
    }
}
