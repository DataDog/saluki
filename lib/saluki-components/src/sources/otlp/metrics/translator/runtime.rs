//! Duplication of OTLP runtime metrics under their Datadog-conventional names.

use otlp_protos::opentelemetry::proto::common::v1::any_value::Value as OtlpAnyValueValue;
use otlp_protos::opentelemetry::proto::metrics::v1::{
    metric::Data as OtlpMetricData, Histogram as OtlpHistogram, Metric as OtlpMetric,
};
use saluki_common::collections::FastHashSet;

use crate::sources::otlp::metrics::runtime_metrics::RuntimeMetricMapping;

pub(super) fn map_sum_runtime_metric_with_attributes(
    metric: &OtlpMetric, new_metrics: &mut Vec<OtlpMetric>, mapping: &RuntimeMetricMapping,
) {
    if let Some(OtlpMetricData::Sum(sum)) = &metric.data {
        for dp in &sum.data_points {
            // Check if the data point's attributes match all the required attributes from the mapping.
            let mut matches_attributes = true;
            for required_attr in mapping.attributes {
                let key_to_find = required_attr.key;
                let allowed_values = required_attr.values;

                let has_matching_attribute = dp.attributes.iter().any(|kv| {
                    if kv.key == key_to_find {
                        if let Some(any_value) = &kv.value {
                            if let Some(otlp_protos::opentelemetry::proto::common::v1::any_value::Value::StringValue(
                                s_val,
                            )) = &any_value.value
                            {
                                return allowed_values.contains(&s_val.as_str());
                            }
                        }
                    }
                    false
                });

                if !has_matching_attribute {
                    matches_attributes = false;
                    break;
                }
            }

            if matches_attributes {
                // Create a new metric with a single data point.
                let mut new_metric = OtlpMetric::default();
                let mut new_sum = otlp_protos::opentelemetry::proto::metrics::v1::Sum {
                    aggregation_temporality: sum.aggregation_temporality,
                    is_monotonic: sum.is_monotonic,
                    data_points: vec![],
                };

                let mut new_dp = dp.clone();

                // Remove the attributes that were used for matching.
                let keys_to_remove: std::collections::HashSet<&str> =
                    mapping.attributes.iter().map(|a| a.key).collect();
                new_dp.attributes.retain(|kv| !keys_to_remove.contains(kv.key.as_str()));

                new_sum.data_points.push(new_dp);
                new_metric.data = Some(OtlpMetricData::Sum(new_sum));
                new_metric.name = mapping.mapped_name.to_string();
                new_metrics.push(new_metric);
            }
        }
    }
}

pub(super) fn map_gauge_runtime_metric_with_attributes(
    metric: &OtlpMetric, new_metrics: &mut Vec<OtlpMetric>, mapping: &RuntimeMetricMapping,
) {
    if let Some(OtlpMetricData::Gauge(gauge)) = &metric.data {
        for dp in &gauge.data_points {
            // Check if the data point's attributes match all the required attributes from the mapping.
            let mut matches_attributes = true;
            for required_attr in mapping.attributes {
                let key_to_find = required_attr.key;
                let allowed_values = required_attr.values;

                let has_matching_attribute = dp.attributes.iter().any(|kv| {
                    if kv.key == key_to_find {
                        if let Some(any_value) = &kv.value {
                            if let Some(otlp_protos::opentelemetry::proto::common::v1::any_value::Value::StringValue(
                                s_val,
                            )) = &any_value.value
                            {
                                return allowed_values.contains(&s_val.as_str());
                            }
                        }
                    }
                    false
                });

                if !has_matching_attribute {
                    matches_attributes = false;
                    break;
                }
            }

            if matches_attributes {
                // Create a new metric with a single data point.
                let mut new_metric = OtlpMetric::default();
                let mut new_gauge = otlp_protos::opentelemetry::proto::metrics::v1::Gauge::default();

                let mut new_dp = dp.clone();

                // Remove the attributes that were used for matching.
                let keys_to_remove: std::collections::HashSet<&str> =
                    mapping.attributes.iter().map(|a| a.key).collect();
                new_dp.attributes.retain(|kv| !keys_to_remove.contains(kv.key.as_str()));

                new_gauge.data_points.push(new_dp);
                new_metric.data = Some(OtlpMetricData::Gauge(new_gauge));
                new_metric.name = mapping.mapped_name.to_string();
                new_metrics.push(new_metric);
            }
        }
    }
}

pub(super) fn map_histogram_runtime_metric_with_attributes(
    metric: &OtlpMetric, new_metrics: &mut Vec<OtlpMetric>, mapping: &RuntimeMetricMapping,
) {
    if let Some(OtlpMetricData::Histogram(histogram)) = &metric.data {
        for dp in &histogram.data_points {
            // Check if the data point's attributes match all the required attributes from the mapping.
            let mut matches_attributes = true;
            for required_attr in mapping.attributes {
                let has_matching_attribute = dp.attributes.iter().any(|kv| {
                    kv.key == required_attr.key
                        && matches!(
                            kv.value.as_ref().and_then(|any_value| any_value.value.as_ref()),
                            Some(
                                OtlpAnyValueValue::StringValue(value)
                            ) if required_attr.values.contains(&value.as_str())
                        )
                });

                if !has_matching_attribute {
                    matches_attributes = false;
                    break;
                }
            }

            if matches_attributes {
                // Create a new metric with a single data point.
                let mut new_metric = OtlpMetric::default();
                let mut new_histogram = OtlpHistogram {
                    aggregation_temporality: histogram.aggregation_temporality,
                    data_points: vec![],
                };
                let mut new_dp = dp.clone();

                // Remove the attributes that were used for matching.
                let keys_to_remove: FastHashSet<&str> =
                    mapping.attributes.iter().map(|attribute| attribute.key).collect();
                new_dp.attributes.retain(|kv| !keys_to_remove.contains(kv.key.as_str()));

                new_histogram.data_points.push(new_dp);
                new_metric.data = Some(OtlpMetricData::Histogram(new_histogram));
                new_metric.name = mapping.mapped_name.to_string();
                new_metrics.push(new_metric);

                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use otlp_protos::opentelemetry::proto::common::v1::KeyValue as OtlpKeyValue;
    use otlp_protos::opentelemetry::proto::metrics::v1::{
        number_data_point::Value as OtlpNumberDataPointValue, AggregationTemporality, Gauge,
        HistogramDataPoint as OtlpHistogramDataPoint, NumberDataPoint as OtlpNumberDataPoint, Sum,
    };
    use saluki_core::data_model::event::metric::MetricValues;

    use super::super::tests::{gauge_metric_named, metric_by_name, nanos_from_seconds, resource_metrics_with_metric};
    use super::super::OtlpMetricsTranslator;
    use super::*;
    use crate::sources::otlp::metrics::runtime_metrics::RUNTIME_METRICS_MAPPINGS;
    use crate::sources::otlp::Metrics;

    // Exercises the `runtime_metrics.rs` tables consumed by `translate_metrics` and the
    // `map_*_runtime_metric_with_attributes` helpers.

    fn int_dp_with_attr(value: i64, key: &str, attr_value: &str) -> OtlpNumberDataPoint {
        OtlpNumberDataPoint {
            value: Some(OtlpNumberDataPointValue::AsInt(value)),
            time_unix_nano: nanos_from_seconds(1),
            attributes: vec![OtlpKeyValue {
                key: key.to_string(),
                value: Some(otlp_protos::opentelemetry::proto::common::v1::AnyValue {
                    value: Some(
                        otlp_protos::opentelemetry::proto::common::v1::any_value::Value::StringValue(
                            attr_value.to_string(),
                        ),
                    ),
                }),
            }],
            ..Default::default()
        }
    }

    #[test]
    fn translate_metrics_forwards_runtime_metric_under_original_name() {
        let metrics = Metrics::for_tests();
        let mut translator = OtlpMetricsTranslator::for_tests();

        let metric = gauge_metric_named("process.runtime.go.goroutines");

        let (events_iter, languages) = translator
            .translate_metrics(resource_metrics_with_metric(metric), &metrics)
            .expect("translation should succeed");
        let events = events_iter.collect::<Vec<_>>();

        let original = metric_by_name(&events, "process.runtime.go.goroutines");
        assert_eq!(original.values(), &MetricValues::gauge((1, 1.0)));

        assert!(
            events
                .iter()
                .filter_map(|e| e.try_as_metric())
                .all(|m| m.context().name() != "runtime.go.num_goroutine"),
            "no remapped runtime metric should be emitted"
        );

        assert_eq!(languages.len(), 1);
        assert!(languages.contains("go"));
    }

    #[test]
    fn translate_metrics_forwards_runtime_gauge_with_attributes_under_original_name() {
        let metrics = Metrics::for_tests();
        let mut translator = OtlpMetricsTranslator::for_tests();

        let metric = OtlpMetric {
            name: "process.runtime.jvm.memory.usage".to_string(),
            data: Some(OtlpMetricData::Gauge(Gauge {
                data_points: vec![
                    int_dp_with_attr(100, "type", "heap"),
                    int_dp_with_attr(50, "type", "non_heap"),
                ],
            })),
            ..Default::default()
        };

        let (events_iter, languages) = translator
            .translate_metrics(resource_metrics_with_metric(metric), &metrics)
            .expect("translation should succeed");
        let events = events_iter.collect::<Vec<_>>();

        assert!(
            events
                .iter()
                .filter_map(|e| e.try_as_metric())
                .any(|m| m.context().name() == "process.runtime.jvm.memory.usage"),
            "the original runtime metric should be emitted under its OpenTelemetry name"
        );

        assert!(
            events
                .iter()
                .filter_map(|e| e.try_as_metric())
                .all(|m| m.context().name() != "jvm.heap_memory" && m.context().name() != "jvm.non_heap_memory"),
            "no remapped runtime metrics should be emitted"
        );

        assert_eq!(languages.len(), 1);
        assert!(languages.contains("jvm"));
    }

    #[test]
    fn sum_runtime_metric_maps_matching_attribute_and_strips_it() {
        // `process.runtime.dotnet.gc.heap.size` fans out by the `generation` attribute.
        let mapping_set = RUNTIME_METRICS_MAPPINGS
            .get("process.runtime.dotnet.gc.heap.size")
            .expect("runtime mapping should exist");
        let gen0_mapping = mapping_set
            .iter()
            .find(|m| m.mapped_name == "runtime.dotnet.gc.size.gen0")
            .expect("gen0 mapping should exist");

        let metric = OtlpMetric {
            name: "process.runtime.dotnet.gc.heap.size".to_string(),
            data: Some(OtlpMetricData::Sum(Sum {
                data_points: vec![
                    int_dp_with_attr(42, "generation", "gen0"),
                    int_dp_with_attr(7, "generation", "gen1"),
                ],
                is_monotonic: true,
                aggregation_temporality: AggregationTemporality::Cumulative as i32,
            })),
            ..Default::default()
        };

        let mut new_metrics = Vec::new();
        map_sum_runtime_metric_with_attributes(&metric, &mut new_metrics, gen0_mapping);

        assert_eq!(new_metrics.len(), 1, "only the gen0 data point should match");
        assert_eq!(new_metrics[0].name, "runtime.dotnet.gc.size.gen0");
        let Some(OtlpMetricData::Sum(sum)) = &new_metrics[0].data else {
            panic!("expected a sum metric");
        };
        assert_eq!(sum.data_points.len(), 1);
        assert_eq!(
            sum.data_points[0].value,
            Some(OtlpNumberDataPointValue::AsInt(42)),
            "the matched data point's value should be preserved"
        );
        assert!(
            sum.data_points[0].attributes.is_empty(),
            "the matched `generation` attribute should be stripped"
        );
    }

    #[test]
    fn histogram_runtime_metric_maps_first_matching_attribute_and_strips_it() {
        let mapping_set = RUNTIME_METRICS_MAPPINGS
            .get("process.runtime.dotnet.gc.heap.size")
            .expect("runtime mapping should exist");
        let gen0_mapping = mapping_set
            .iter()
            .find(|mapping| mapping.mapped_name == "runtime.dotnet.gc.size.gen0")
            .expect("gen0 mapping should exist");
        let string_attribute = |key: &str, value: &str| OtlpKeyValue {
            key: key.to_string(),
            value: Some(otlp_protos::opentelemetry::proto::common::v1::AnyValue {
                value: Some(
                    otlp_protos::opentelemetry::proto::common::v1::any_value::Value::StringValue(value.to_string()),
                ),
            }),
        };
        let histogram_data_point = |count, attributes| OtlpHistogramDataPoint {
            count,
            sum: Some(count as f64),
            bucket_counts: vec![count],
            attributes,
            ..Default::default()
        };
        let metric = OtlpMetric {
            name: "process.runtime.dotnet.gc.heap.size".to_string(),
            data: Some(OtlpMetricData::Histogram(OtlpHistogram {
                aggregation_temporality: AggregationTemporality::Delta as i32,
                data_points: vec![
                    histogram_data_point(1, vec![string_attribute("generation", "gen1")]),
                    histogram_data_point(
                        2,
                        vec![
                            string_attribute("generation", "gen0"),
                            string_attribute("region", "us-east"),
                        ],
                    ),
                    histogram_data_point(3, vec![string_attribute("generation", "gen0")]),
                ],
            })),
            ..Default::default()
        };

        let mut new_metrics = Vec::new();
        map_histogram_runtime_metric_with_attributes(&metric, &mut new_metrics, gen0_mapping);

        assert_eq!(
            new_metrics.len(),
            1,
            "only the first matching data point should be copied"
        );
        assert_eq!(new_metrics[0].name, "runtime.dotnet.gc.size.gen0");
        let Some(OtlpMetricData::Histogram(histogram)) = &new_metrics[0].data else {
            panic!("expected a histogram metric");
        };
        assert_eq!(histogram.aggregation_temporality, AggregationTemporality::Delta as i32);
        assert_eq!(histogram.data_points.len(), 1);
        assert_eq!(
            histogram.data_points[0].count, 2,
            "the first matching data point should be preserved"
        );
        assert_eq!(histogram.data_points[0].attributes.len(), 1);
        assert_eq!(histogram.data_points[0].attributes[0].key, "region");
    }
}
