#![allow(dead_code)]
use std::collections::BTreeMap;
use std::sync::Arc;

use futures::stream::StreamExt as _;
use papaya::HashMap;
use prometheus_exposition::{MetricType, PrometheusRenderer};
use saluki_context::Context;
use saluki_core::data_model::event::{metric::MetricValues, Event};
use saluki_core::{
    observability::metrics::MetricsStream,
    state::reflector::{Processor, Reflector},
};
use stringtheory::MetaString;
use tokio::sync::OnceCell;

mod rules;
pub use self::rules::{get_datadog_agent_remappings, RemapperRule};

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

/// Merges two aggregated metric values.
///
/// Counters are summed, gauges use last-write-wins (the incoming value is kept).
fn merge_aggregated_values(existing: AggregatedMetricValue, incoming: AggregatedMetricValue) -> AggregatedMetricValue {
    match (existing, incoming) {
        (AggregatedMetricValue::Counter(a), AggregatedMetricValue::Counter(b)) => AggregatedMetricValue::Counter(a + b),
        // For gauges (and mixed types), take the incoming value.
        (_, other) => other,
    }
}

/// Renders the RAR-relevant subset of internal metrics in Prometheus text exposition format.
///
/// Iterates the aggregated metrics state, matches each metric against the remapper rules, and renders only the
/// matching metrics (in their Agent-compatible remapped form) using the provided renderer. The remapper rules act as
/// both the filter (only matched metrics are included) and the name translator.
pub fn render_rar_telemetry(
    state: &AggregatedMetricsState, rules: &[RemapperRule], renderer: &mut PrometheusRenderer,
) -> String {
    renderer.clear();

    // Collect, deduplicate, and group matched metrics by their remapped name and tags.
    //
    // Multiple source metrics can remap to the same (name, tags) identity. We aggregate these by summing counters
    // and taking the latest value for gauges. We also sort tags to normalize their order so that identical tag sets
    // in different orders are correctly deduplicated.
    //
    // The outer BTreeMap groups by metric name (required because Prometheus text format needs all series with the
    // same metric name under one TYPE/HELP header), and the inner BTreeMap deduplicates by tag set.
    let mut groups: BTreeMap<&'static str, BTreeMap<Vec<MetaString>, AggregatedMetricValue>> = BTreeMap::new();

    state.visit_metrics(|context, value| {
        for rule in rules {
            if let Some(mut remapped) = rule.try_match_no_context(context) {
                remapped.tags.sort();

                let series = groups.entry(remapped.name).or_default();
                series
                    .entry(remapped.tags)
                    .and_modify(|existing| *existing = merge_aggregated_values(*existing, *value))
                    .or_insert(*value);
                return;
            }
        }
    });

    // Render each group.
    for (name, series) in &groups {
        // Determine metric type from the first series value.
        let metric_type = match series.values().next() {
            Some(AggregatedMetricValue::Counter(_)) => MetricType::Counter,
            Some(AggregatedMetricValue::Gauge(_)) => MetricType::Gauge,
            None => continue,
        };

        let help_text = get_help_text(name);

        // Tags are split into key/value pairs at render time, skipping bare tags (those without a `:`).
        // We pre-collect labels as owned strings since the MetaString tags are consumed by the iterator.
        let rendered_series: Vec<(Vec<(String, String)>, f64)> = series
            .iter()
            .map(|(tags, value)| {
                let labels: Vec<(String, String)> = tags
                    .iter()
                    .filter_map(|tag| {
                        tag.as_ref()
                            .split_once(':')
                            .map(|(k, v)| (k.to_string(), v.to_string()))
                    })
                    .collect();
                (labels, value.value())
            })
            .collect();

        renderer.render_scalar_group(name, metric_type, help_text, rendered_series);
    }

    renderer.output().to_string()
}

fn get_help_text(metric_name: &str) -> Option<&'static str> {
    // The HELP text for overlapped metrics MUST match the agent's HELP text exactly or else an error will occur on the
    // agent's side when parsing the metrics. Keys here use the raw (pre-normalization) metric names as produced by the
    // remapper rules; the renderer handles normalization when writing the output.
    match metric_name {
        "no_aggregation.flush" => Some("Count the number of flushes done by the no-aggregation pipeline worker"),
        "no_aggregation.processed" => {
            Some("Count the number of samples processed by the no-aggregation pipeline worker")
        }
        "aggregator.dogstatsd_contexts_by_mtype" => {
            Some("Count the number of dogstatsd contexts in the aggregator, by metric type")
        }
        "aggregator.flush" => Some("Number of metrics/service checks/events flushed"),
        "aggregator.dogstatsd_contexts_bytes_by_mtype" => {
            Some("Estimated count of bytes taken by contexts in the aggregator, by metric type")
        }
        "aggregator.dogstatsd_contexts" => Some("Count the number of dogstatsd contexts in the aggregator"),
        "aggregator.processed" => Some("Amount of metrics/services_checks/events processed by the aggregator"),
        "dogstatsd.processed" => Some("Count of service checks/events/metrics processed by dogstatsd"),
        "dogstatsd.packet_pool_get" => Some("Count of get done in the packet pool"),
        "dogstatsd.packet_pool_put" => Some("Count of put done in the packet pool"),
        "dogstatsd.packet_pool" => Some("Usage of the packet pool in dogstatsd"),
        "transactions.errors" => Some("Count of transactions errored grouped by type of error"),
        "transactions.http_errors" => Some("Count of transactions http errors per http code"),
        "transactions.dropped" => Some("Transaction drop count"),
        "transactions.success" => Some("Successful transaction count"),
        "transactions.success_bytes" => Some("Successful transaction sizes in bytes"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use saluki_core::data_model::event::metric::Metric;

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
    fn test_render_rar_telemetry() {
        let processor = AggregatedMetricsProcessor;
        let state = processor.build_initial_state();

        // Simulate internal metrics that match remapper rules.
        let metrics = vec![
            Event::Metric(Metric::counter(
                Context::from_static_parts("adp.object_pool_acquired", &["pool_name:dsd_packet_bufs"]),
                42.0,
            )),
            Event::Metric(Metric::gauge(
                Context::from_static_parts("adp.object_pool_in_use", &["pool_name:dsd_packet_bufs"]),
                7.0,
            )),
            // This metric should NOT appear in output (no matching rule).
            Event::Metric(Metric::counter(
                Context::from_static_parts("adp.some_unrelated_metric", &[]),
                100.0,
            )),
        ];

        for metric in metrics {
            processor.process(metric, &state);
        }

        let rules = get_datadog_agent_remappings();
        let mut renderer = PrometheusRenderer::new();
        let output = render_rar_telemetry(&state, &rules, &mut renderer);

        // Matched metrics should appear with remapped names.
        assert!(output.contains("dogstatsd__packet_pool_get "));
        assert!(output.contains("dogstatsd__packet_pool "));

        // Unmatched metrics should NOT appear.
        assert!(!output.contains("some_unrelated_metric"));

        // Should have TYPE headers.
        assert!(output.contains("# TYPE dogstatsd__packet_pool_get counter"));
        assert!(output.contains("# TYPE dogstatsd__packet_pool gauge"));
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

    #[test]
    fn test_match_context() {
        let rules = get_datadog_agent_remappings();

        let context = Context::from_static_parts("adp.object_pool_acquired", &["pool_name:dsd_packet_bufs"]);
        let matched = rules.iter().find_map(|r| r.try_match_no_context(&context));
        let remapped = matched.expect("should have matched");
        assert_eq!(remapped.name, "dogstatsd.packet_pool_get");

        // Should not match without the required tag.
        let context = Context::from_static_parts("adp.object_pool_acquired", &["pool_name:other"]);
        let matched = rules.iter().find_map(|r| r.try_match_no_context(&context));
        assert!(matched.is_none());

        // Test tag remapping.
        let context = Context::from_static_parts(
            "adp.component_events_received_total",
            &["component_id:dsd_in", "message_type:metrics"],
        );
        let matched = rules.iter().find_map(|r| r.try_match_no_context(&context));
        let remapped = matched.expect("should have matched");
        assert_eq!(remapped.name, "dogstatsd.processed");
        assert!(remapped.tags.iter().any(|t| t.as_ref() == "message_type:metrics"));
        assert!(remapped.tags.iter().any(|t| t.as_ref() == "state:ok"));
    }

    #[test]
    fn test_rar_telemetry_deduplicates_remapped_metrics() {
        // Two source metrics with different source tags that remap to the same (name, tags) identity
        // should be deduplicated (counters summed) in the RAR output.
        let processor = AggregatedMetricsProcessor;
        let state = processor.build_initial_state();

        // These two metrics have different listener_type tags, but both remap to
        // dogstatsd.processed{message_type:metrics, state:ok}.
        let metrics = vec![
            Event::Metric(Metric::counter(
                Context::from_static_parts(
                    "adp.component_events_received_total",
                    &["component_id:dsd_in", "message_type:metrics", "listener_type:udp"],
                ),
                10.0,
            )),
            Event::Metric(Metric::counter(
                Context::from_static_parts(
                    "adp.component_events_received_total",
                    &["component_id:dsd_in", "message_type:metrics", "listener_type:unixgram"],
                ),
                25.0,
            )),
        ];

        for metric in metrics {
            processor.process(metric, &state);
        }

        let rules = get_datadog_agent_remappings();
        let mut renderer = PrometheusRenderer::new();
        let output = render_rar_telemetry(&state, &rules, &mut renderer);

        // Should only have one dogstatsd__processed series with state="ok" and message_type="metrics",
        // with the summed value of 35.
        let processed_lines: Vec<&str> = output
            .lines()
            .filter(|line| {
                line.starts_with("dogstatsd__processed{")
                    && line.contains("state=\"ok\"")
                    && line.contains("message_type=\"metrics\"")
            })
            .collect();
        assert_eq!(
            processed_lines.len(),
            1,
            "expected exactly one deduplicated series, got: {processed_lines:?}"
        );
        assert!(
            processed_lines[0].ends_with(" 35"),
            "expected summed value of 35, got: {}",
            processed_lines[0]
        );
    }

    #[test]
    fn prom_get_help_text() {
        // Ensure that we catch when the help text changes for these metrics.
        assert_eq!(
            get_help_text("no_aggregation.flush"),
            Some("Count the number of flushes done by the no-aggregation pipeline worker")
        );
        assert_eq!(
            get_help_text("no_aggregation.processed"),
            Some("Count the number of samples processed by the no-aggregation pipeline worker")
        );
        assert_eq!(
            get_help_text("aggregator.dogstatsd_contexts_by_mtype"),
            Some("Count the number of dogstatsd contexts in the aggregator, by metric type")
        );
        assert_eq!(
            get_help_text("aggregator.flush"),
            Some("Number of metrics/service checks/events flushed")
        );
        assert_eq!(
            get_help_text("aggregator.dogstatsd_contexts_bytes_by_mtype"),
            Some("Estimated count of bytes taken by contexts in the aggregator, by metric type")
        );
        assert_eq!(
            get_help_text("aggregator.dogstatsd_contexts"),
            Some("Count the number of dogstatsd contexts in the aggregator")
        );
        assert_eq!(
            get_help_text("aggregator.processed"),
            Some("Amount of metrics/services_checks/events processed by the aggregator")
        );
    }
}
