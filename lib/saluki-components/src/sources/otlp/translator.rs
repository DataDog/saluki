#![allow(dead_code)]

use otlp_protos::opentelemetry::proto::common::v1::any_value::Value;
use otlp_protos::opentelemetry::proto::common::v1::KeyValue;
use otlp_protos::opentelemetry::proto::metrics::v1::{
    metric::Data as OtlpMetricData, AggregationTemporality, DataPointFlags, Metric as OtlpMetric,
    NumberDataPoint as OtlpNumberDataPoint, ResourceMetrics as OtlpResourceMetrics,
};
use saluki_context::tags::{SharedTagSet, TagSet};
use saluki_context::ContextResolver;
use saluki_core::data_model::event::metric::{Metric, MetricMetadata, MetricValues};
use saluki_core::data_model::event::Event;
use saluki_error::GenericError;
use tracing::warn;

use super::cache::Cache;

#[derive(Clone, Copy, Debug, PartialEq)]
enum DataType {
    Gauge,
    Count,
}

/// A translator for converting OTLP metrics into Saluki `Event::Metric`s.
pub struct OtlpTranslator {
    context_resolver: ContextResolver,
    prev_pts: Cache,
}

impl OtlpTranslator {
    /// Creates a new, empty `OtlpTranslator`.
    pub fn new(context_resolver: ContextResolver) -> Self {
        Self {
            context_resolver,
            prev_pts: Cache::new(),
        }
    }

    /// Translates a batch of OTLP `ResourceMetrics` into a collection of Saluki `Event`s.
    /// This is the Rust equivalent of the Go `MapMetrics` function.
    pub fn map_metrics(&mut self, resource_metrics: OtlpResourceMetrics) -> Result<Vec<Event>, GenericError> {
        let mut events = Vec::new();

        let attribute_tags = resource_metrics
            .resource
            .map(|res| attributes_to_tag_set(&res.attributes))
            .unwrap_or_default();

        // TODO: https://github.com/DataDog/datadog-agent/blob/main/pkg/opentelemetry-mapping-go/otlp/metrics/metrics_translator.go#L736-L753

        for scope_metrics in resource_metrics.scope_metrics {
            for metric in scope_metrics.metrics {
                let mut translated_events = self.map_to_dd_format(metric, &attribute_tags);
                events.append(&mut translated_events);
            }
        }

        Ok(events)
    }

    /// Translates a single OTLP `Metric` into a collection of Saluki `Event`s.
    fn map_to_dd_format(&mut self, metric: OtlpMetric, attribute_tags: &SharedTagSet) -> Vec<Event> {
        let metric_name = metric.name;
        println!("rz6300 mapping metric: {:?}", metric_name);
        if let Some(data) = metric.data {
            match data {
                OtlpMetricData::Gauge(gauge) => {
                    self.map_number_data_points(&metric_name, attribute_tags, gauge.data_points, DataType::Gauge)
                }
                OtlpMetricData::Sum(sum) => match AggregationTemporality::try_from(sum.aggregation_temporality) {
                    Ok(AggregationTemporality::Cumulative) => {
                        if !sum.is_monotonic {
                            println!("rz6300 mapping gauge from non monotonic sum: {:?}", metric_name);
                            self.map_number_data_points(&metric_name, attribute_tags, sum.data_points, DataType::Gauge)
                        } else {
                            println!("rz6300 mapping sum: {:?}", metric_name);
                            self.map_sum_data_points(&metric_name, attribute_tags, sum.data_points)
                        }
                    }
                    Ok(AggregationTemporality::Delta) => {
                        println!("rz6300 mapping delta: {:?}", metric_name);
                        self.map_number_data_points(&metric_name, attribute_tags, sum.data_points, DataType::Count)
                    }
                    _ => {
                        warn!(
                            metric_name,
                            temporality = sum.aggregation_temporality,
                            "Unsupported or unknown aggregation temporality for Sum metric."
                        );
                        Vec::new()
                    }
                },
                _ => {
                    // TODO: Handle Histogram, Summary, etc.
                    Vec::new()
                }
            }
        } else {
            Vec::new()
        }
    }

    /// Maps a slice of OTLP numeric data points to Saluki `Event`s.
    fn map_number_data_points(
        &mut self, metric_name: &str, attribute_tags: &SharedTagSet, data_points: Vec<OtlpNumberDataPoint>,
        data_type: DataType,
    ) -> Vec<Event> {
        let mut events = Vec::new();
        for dp in data_points {
            // Skip if the data point has no recorded value.
            if dp.flags & (DataPointFlags::NoRecordedValueMask as u32) != 0 {
                continue;
            }

            let value = get_number_data_point_value(&dp);
            if is_skippable(value) {
                warn!(
                    metric_name,
                    value, "Skipping metric with unsupported value (NaN or Infinity)."
                );
                continue;
            }

            // TODO: how do we handle the timestamp?
            let _ts = dp.time_unix_nano;

            let data_point_tags = attributes_to_tag_set(&dp.attributes);
            let mut final_tags = attribute_tags.clone();
            final_tags.extend_from_shared(&data_point_tags);

            match self.context_resolver.resolve(metric_name, final_tags.into_iter(), None) {
                Some(context) => {
                    let values = match data_type {
                        DataType::Gauge => MetricValues::Gauge(value.into()),
                        DataType::Count => MetricValues::Counter(value.into()),
                    };
                    let metadata = MetricMetadata::default();
                    let metric = Metric::from_parts(context, values, metadata);
                    events.push(Event::Metric(metric));
                }
                None => {
                    println!("Failed to resolve context for metric: {}", metric_name);
                    warn!("Failed to resolve context for metric: {}", metric_name);
                }
            }
        }
        events
    }

    /// Maps a slice of OTLP cumulative monotonic `Sum` data points to Saluki `Event`s.
    fn map_sum_data_points(
        &mut self, metric_name: &str, attribute_tags: &SharedTagSet, data_points: Vec<OtlpNumberDataPoint>,
    ) -> Vec<Event> {
        println!(
            "rz6300 mapping metric_name: {:?}, attribute_tags: {:?}, data_points: {:?}",
            metric_name, attribute_tags, data_points
        );
        let mut events = Vec::new();
        for dp in data_points {
            // Skip if the data point has no recorded value.
            if dp.flags & (DataPointFlags::NoRecordedValueMask as u32) != 0 {
                continue;
            }

            let value = get_number_data_point_value(&dp);
            if is_skippable(value) {
                warn!(
                    metric_name,
                    value, "Skipping metric with unsupported value (NaN or Infinity)."
                );
                continue;
            }

            // TODO: Rate as Gauge metrics
            // https://github.com/DataDog/datadog-agent/blob/main/pkg/opentelemetry-mapping-go/otlp/metrics/metrics_translator.go#L233-L241

            let data_point_tags = attributes_to_tag_set(&dp.attributes);
            let mut final_tags = attribute_tags.clone();
            final_tags.extend_from_shared(&data_point_tags);

            let (delta, is_first_point, should_drop_point) = self.prev_pts.monotonic_diff(
                metric_name,
                &final_tags,
                dp.start_time_unix_nano,
                dp.time_unix_nano,
                value,
            );
            println!(
                "rz6300 delta: {:?}, is_first_point: {:?}, should_drop_point: {:?}",
                delta, is_first_point, should_drop_point
            );

            if should_drop_point {
                warn!(
                    metric_name,
                    "Dropping cumulative monotonic data point due to reset or out-of-order timestamp."
                );
                continue;
            }

            if !is_first_point {
                match self.context_resolver.resolve(metric_name, final_tags.into_iter(), None) {
                    Some(context) => {
                        let values = MetricValues::Counter(delta.into());
                        let metadata = MetricMetadata::default();
                        let metric = Metric::from_parts(context, values, metadata);
                        events.push(Event::Metric(metric));
                    }
                    None => {
                        warn!("Failed to resolve context for metric: {}", metric_name);
                    }
                }
            }
            // If it's the first point, we don't emit a metric, as we can't calculate a delta. The point is now cached.
        }
        println!("rz6300 events: {:?}", events);
        events
    }
}

/// Converts a slice of OTLP `KeyValue` attributes to a `SharedTagSet`.
fn attributes_to_tag_set(attributes: &[KeyValue]) -> SharedTagSet {
    // TODO: Implement semantic attribute mappings.
    // https://github.com/DataDog/datadog-agent/blob/main/pkg/opentelemetry-mapping-go/otlp/attributes/attributes.go#L193
    let mut tag_set = TagSet::default();
    for kv in attributes {
        let value = kv.value.as_ref().and_then(|v| v.value.as_ref());
        if let Some(Value::StringValue(value)) = value {
            tag_set.insert_tag(format!("{}:{}", kv.key, value));
        }
    }
    tag_set.into_shared()
}

/// Extracts the f64 value from an OTLP `NumberDataPoint`.
fn get_number_data_point_value(dp: &OtlpNumberDataPoint) -> f64 {
    match dp.value {
        Some(otlp_protos::opentelemetry::proto::metrics::v1::number_data_point::Value::AsDouble(d)) => d,
        Some(otlp_protos::opentelemetry::proto::metrics::v1::number_data_point::Value::AsInt(i)) => i as f64,
        None => 0.0,
    }
}

/// Checks if a metric value is `NaN` or `Infinity`.
fn is_skippable(value: f64) -> bool {
    value.is_nan() || value.is_infinite()
}
