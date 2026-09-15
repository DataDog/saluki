//! Translation of OTLP gauge and delta sum data points.

use otlp_protos::opentelemetry::proto::metrics::v1::{DataPointFlags, NumberDataPoint as OtlpNumberDataPoint};
use saluki_core::data_model::event::Event;
use tracing::warn;

use super::metric_type::{
    metric_type_override, warn_metric_type_override_once, MetricTypeOverride, MetricTypeOverrideWarningKind,
};
use super::{is_skippable, DataType, Dimensions, OtlpMetricsTranslator, TranslationContext};

/// Extracts the f64 value from an OTLP `NumberDataPoint`.
pub(super) fn get_number_data_point_value(dp: &OtlpNumberDataPoint) -> f64 {
    match dp.value {
        Some(otlp_protos::opentelemetry::proto::metrics::v1::number_data_point::Value::AsDouble(d)) => d,
        Some(otlp_protos::opentelemetry::proto::metrics::v1::number_data_point::Value::AsInt(i)) => i as f64,
        None => 0.0,
    }
}

impl OtlpMetricsTranslator {
    /// Maps a slice of OTLP numeric data points to Saluki `Event`s.
    pub(super) fn map_number_metrics(
        &mut self, base_dims: Dimensions, data_points: Vec<OtlpNumberDataPoint>, data_type: DataType,
        context: &TranslationContext,
    ) -> Vec<Event> {
        let mut events = Vec::new();
        for dp in data_points {
            // Skip if the data point has no recorded value.
            if dp.flags & (DataPointFlags::NoRecordedValueMask as u32) != 0 {
                continue;
            }

            let shadowing_resource_attributes = self
                .config
                .resource_attributes_as_tags
                .then_some(context.resource_attributes);
            let point_dims = base_dims.with_attribute_map(&dp.attributes, shadowing_resource_attributes);
            let value = get_number_data_point_value(&dp);
            if is_skippable(value) {
                warn!(
                    metric_name = point_dims.name,
                    value, "Skipping metric with unsupported value (NaN or Infinity)."
                );
                self.translator_metrics.dropped_invalid_value().increment(1);
                continue;
            }

            let ts = dp.time_unix_nano;
            let data_type = match metric_type_override(&dp.attributes) {
                Some(MetricTypeOverride::Known(DataType::Rate)) if data_type == DataType::Count => {
                    warn_metric_type_override_once(
                        &mut self.metric_type_override_warnings,
                        point_dims.name.as_str(),
                        MetricTypeOverrideWarningKind::ZeroInterval,
                        None,
                    );
                    DataType::Rate
                }
                Some(MetricTypeOverride::Known(DataType::Rate)) => {
                    warn_metric_type_override_once(
                        &mut self.metric_type_override_warnings,
                        point_dims.name.as_str(),
                        MetricTypeOverrideWarningKind::InvalidType,
                        None,
                    );
                    data_type
                }
                Some(MetricTypeOverride::Unsupported(attribute_value)) => {
                    warn_metric_type_override_once(
                        &mut self.metric_type_override_warnings,
                        point_dims.name.as_str(),
                        MetricTypeOverrideWarningKind::UnsupportedValue,
                        Some(attribute_value),
                    );
                    data_type
                }
                None | Some(MetricTypeOverride::Known(_)) => data_type,
            };

            self.record_metric_event(&point_dims, value, ts, data_type, &mut events, context);
        }
        events
    }
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use otlp_protos::opentelemetry::proto::metrics::v1::number_data_point::Value as OtlpNumberDataPointValue;
    use saluki_context::tags::{Tag, TagSet};
    use saluki_core::data_model::event::metric::MetricValues;

    use super::*;
    use crate::sources::otlp::Metrics;

    /// Drives `map_number_metrics` for the given `dims`/`data_type`, hiding the repeated
    /// `TranslationContext` construction shared across the number-mapping tests.
    fn run_number(
        translator: &mut OtlpMetricsTranslator, dims: Dimensions, data_points: Vec<OtlpNumberDataPoint>,
        data_type: DataType, metrics: &Metrics,
    ) -> Vec<Event> {
        let context = TranslationContext {
            resource_attributes: &[],
            metrics,
        };
        translator.map_number_metrics(dims, data_points, data_type, &context)
    }

    /// A single integer data point at "now" and its second-resolution timestamp.
    fn single_int_point() -> (Vec<OtlpNumberDataPoint>, u64) {
        let ts = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos() as u64;
        (
            vec![OtlpNumberDataPoint {
                value: Some(OtlpNumberDataPointValue::AsInt(17)),
                time_unix_nano: ts,
                ..Default::default()
            }],
            ts / 1_000_000_000,
        )
    }

    // https://github.com/DataDog/datadog-agent/blob/main/pkg/opentelemetry-mapping-go/otlp/metrics/metrics_translator_test.go#L201
    #[test]
    fn number_gauge_emits_untagged_gauge() {
        let metrics = Metrics::for_tests();
        let (slice, ts_s) = single_int_point();
        let mut translator = OtlpMetricsTranslator::for_tests();
        let dims = Dimensions {
            name: "int64.test".to_string(),
            ..Default::default()
        };

        let events = run_number(&mut translator, dims, slice, DataType::Gauge, &metrics);

        assert_eq!(events.len(), 1, "Expected one event for the gauge test");
        let metric = events[0].try_as_metric().unwrap();
        assert_eq!(metric.context().name(), "int64.test");
        assert_eq!(metric.values(), &MetricValues::gauge((ts_s, 17.0)));
        assert!(
            metric.context().tags().is_empty(),
            "Expected no tags for the simple gauge test"
        );
    }

    // https://github.com/DataDog/datadog-agent/blob/main/pkg/opentelemetry-mapping-go/otlp/metrics/metrics_translator_test.go#L201
    #[test]
    fn number_count_emits_untagged_counter() {
        let metrics = Metrics::for_tests();
        let (slice, ts_s) = single_int_point();
        let mut translator = OtlpMetricsTranslator::for_tests();
        let dims = Dimensions {
            name: "int64.delta.test".to_string(),
            ..Default::default()
        };

        let events = run_number(&mut translator, dims, slice, DataType::Count, &metrics);

        assert_eq!(events.len(), 1, "Expected one event for the count test");
        let metric = events[0].try_as_metric().unwrap();
        assert_eq!(metric.context().name(), "int64.delta.test");
        assert_eq!(metric.values(), &MetricValues::counter((ts_s, 17.0)));
        assert!(
            metric.context().tags().is_empty(),
            "Expected no tags for the simple count test"
        );
    }

    // https://github.com/DataDog/datadog-agent/blob/main/pkg/opentelemetry-mapping-go/otlp/metrics/metrics_translator_test.go#L201
    #[test]
    fn number_gauge_preserves_dimension_tags() {
        let metrics = Metrics::for_tests();
        let (slice, ts_s) = single_int_point();
        let mut translator = OtlpMetricsTranslator::for_tests();
        let mut tags = TagSet::default();
        tags.insert_tag("attribute_tag:attribute_value");
        let dims = Dimensions {
            name: "int64.test".to_string(),
            tags: tags.into_shared(),
            ..Default::default()
        };

        let events = run_number(&mut translator, dims, slice, DataType::Gauge, &metrics);

        assert_eq!(events.len(), 1, "Expected one event for the gauge with tags test");
        let metric = events[0].try_as_metric().unwrap();
        assert_eq!(metric.context().name(), "int64.test");
        assert_eq!(metric.values(), &MetricValues::gauge((ts_s, 17.0)));
        assert_eq!(
            metric.context().tags().get_single_tag("attribute_tag"),
            Some(&Tag::from("attribute_tag:attribute_value"))
        );
    }

    /// A single double data point (`PI`) at "now" and its second-resolution timestamp.
    fn single_double_point() -> (Vec<OtlpNumberDataPoint>, u64) {
        let ts = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos() as u64;
        (
            vec![OtlpNumberDataPoint {
                value: Some(OtlpNumberDataPointValue::AsDouble(std::f64::consts::PI)),
                time_unix_nano: ts,
                ..Default::default()
            }],
            ts / 1_000_000_000,
        )
    }

    // https://github.com/DataDog/datadog-agent/blob/main/pkg/opentelemetry-mapping-go/otlp/metrics/metrics_translator_test.go#L236
    #[test]
    fn double_gauge_emits_gauge() {
        let metrics = Metrics::for_tests();
        let (slice, ts_s) = single_double_point();
        let mut translator = OtlpMetricsTranslator::for_tests();
        let dims = Dimensions {
            name: "float64.test".to_string(),
            ..Default::default()
        };

        let events = run_number(&mut translator, dims, slice, DataType::Gauge, &metrics);

        assert_eq!(events.len(), 1, "Expected one event for the gauge test");
        let metric = events[0].try_as_metric().unwrap();
        assert_eq!(metric.context().name(), "float64.test");
        assert_eq!(metric.values(), &MetricValues::gauge((ts_s, std::f64::consts::PI)));
    }

    // https://github.com/DataDog/datadog-agent/blob/main/pkg/opentelemetry-mapping-go/otlp/metrics/metrics_translator_test.go#L236
    #[test]
    fn double_count_emits_counter() {
        let metrics = Metrics::for_tests();
        let (slice, ts_s) = single_double_point();
        let mut translator = OtlpMetricsTranslator::for_tests();
        let dims = Dimensions {
            name: "float64.delta.test".to_string(),
            ..Default::default()
        };

        let events = run_number(&mut translator, dims, slice, DataType::Count, &metrics);

        assert_eq!(events.len(), 1, "Expected one event for the count test");
        let metric = events[0].try_as_metric().unwrap();
        assert_eq!(metric.context().name(), "float64.delta.test");
        assert_eq!(metric.values(), &MetricValues::counter((ts_s, std::f64::consts::PI)));
    }

    // https://github.com/DataDog/datadog-agent/blob/main/pkg/opentelemetry-mapping-go/otlp/metrics/metrics_translator_test.go#L236
    #[test]
    fn double_gauge_preserves_dimension_tags() {
        let metrics = Metrics::for_tests();
        let (slice, ts_s) = single_double_point();
        let mut translator = OtlpMetricsTranslator::for_tests();
        let mut tags = TagSet::default();
        tags.insert_tag("attribute_tag:attribute_value");
        let dims = Dimensions {
            name: "float64.test".to_string(),
            tags: tags.into_shared(),
            ..Default::default()
        };

        let events = run_number(&mut translator, dims, slice, DataType::Gauge, &metrics);

        assert_eq!(events.len(), 1, "Expected one event for the gauge with tags test");
        let metric = events[0].try_as_metric().unwrap();
        assert_eq!(
            metric.context().tags().get_single_tag("attribute_tag"),
            Some(&Tag::from("attribute_tag:attribute_value"))
        );
        assert_eq!(metric.values(), &MetricValues::gauge((ts_s, std::f64::consts::PI)));
    }
}
