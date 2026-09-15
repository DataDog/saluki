//! Metric-type overrides driven by the `datadog.metric.as_type` data point attribute.

use otlp_protos::opentelemetry::proto::common::v1::KeyValue as OtlpKeyValue;
use saluki_common::collections::FastHashSet;
use tracing::warn;

use super::DataType;

// Outlines whether a metric can be overriden from one metric type into another
pub(super) enum MetricTypeOverride<'a> {
    Known(DataType),
    Unsupported(&'a str),
}

// Different outcomes attributed to overriding a metric type
#[derive(Clone, Copy, Eq, Hash, PartialEq)]
pub(super) enum MetricTypeOverrideWarningKind {
    InvalidType,
    ZeroInterval,
    UnsupportedValue,
}

const METRIC_TYPE_ATTRIBUTE_KEY: &str = "datadog.metric.as_type";

pub(super) fn metric_type_override(attributes: &[OtlpKeyValue]) -> Option<MetricTypeOverride<'_>> {
    let attribute = attributes
        .iter()
        .find(|attribute| attribute.key == METRIC_TYPE_ATTRIBUTE_KEY)?;
    let value = attribute
        .value
        .as_ref()
        .and_then(|value| value.value.as_ref())
        .and_then(|value| match value {
            otlp_protos::opentelemetry::proto::common::v1::any_value::Value::StringValue(value) => Some(value.as_str()),
            _ => None,
        })
        .unwrap_or_default();

    let data_type = if value.eq_ignore_ascii_case("rate") {
        DataType::Rate
    } else if value.eq_ignore_ascii_case("count") {
        DataType::Count
    } else if value.eq_ignore_ascii_case("gauge") {
        DataType::Gauge
    } else {
        return Some(MetricTypeOverride::Unsupported(value));
    };

    Some(MetricTypeOverride::Known(data_type))
}

pub(super) fn warn_metric_type_override_once(
    warnings: &mut FastHashSet<(String, MetricTypeOverrideWarningKind)>, metric_name: &str,
    kind: MetricTypeOverrideWarningKind, attribute_value: Option<&str>,
) {
    if !warnings.insert((metric_name.to_owned(), kind)) {
        return;
    }

    match kind {
        MetricTypeOverrideWarningKind::InvalidType => warn!(
            metric_name,
            "Ignoring `datadog.metric.as_type=rate`: it is only supported on OTLP delta Sum metrics."
        ),
        MetricTypeOverrideWarningKind::ZeroInterval => warn!(
            metric_name,
            "Emitting OTLP delta Sum with `datadog.metric.as_type=rate` as an unnormalized Rate because no delta interval is available."
        ),
        MetricTypeOverrideWarningKind::UnsupportedValue => warn!(
            metric_name,
            attribute_value = attribute_value.unwrap_or_default(),
            "Ignoring unsupported `datadog.metric.as_type` value; accepted values are `rate`, `count`, and `gauge`."
        ),
    }
}

#[cfg(test)]
mod tests {
    use otlp_protos::opentelemetry::proto::metrics::v1::{
        metric::Data as OtlpMetricData, number_data_point::Value as OtlpNumberDataPointValue, AggregationTemporality,
        Metric as OtlpMetric, NumberDataPoint as OtlpNumberDataPoint,
    };
    use saluki_context::tags::{SharedTagSet, Tag};
    use saluki_core::data_model::event::metric::MetricValues;

    use super::super::tests::{nanos_from_seconds, string_attribute};
    use super::super::OtlpMetricsTranslator;
    use super::*;
    use crate::sources::otlp::Metrics;

    fn delta_sum_with_as_type(value: i64, as_type: &str) -> OtlpMetric {
        OtlpMetric {
            name: "delta.sum".to_string(),
            data: Some(OtlpMetricData::Sum(
                otlp_protos::opentelemetry::proto::metrics::v1::Sum {
                    aggregation_temporality: AggregationTemporality::Delta as i32,
                    data_points: vec![OtlpNumberDataPoint {
                        value: Some(OtlpNumberDataPointValue::AsInt(value)),
                        time_unix_nano: nanos_from_seconds(2),
                        attributes: vec![string_attribute(METRIC_TYPE_ATTRIBUTE_KEY, as_type)],
                        ..Default::default()
                    }],
                    ..Default::default()
                },
            )),
            ..Default::default()
        }
    }

    #[test]
    fn delta_sum_rate_as_type_emits_rate_and_preserves_attribute_tag() {
        let metrics = Metrics::for_tests();
        let mut translator = OtlpMetricsTranslator::for_tests();
        let events = translator.map_to_dd_format(
            delta_sum_with_as_type(42, "RaTe"),
            &SharedTagSet::default(),
            None,
            &[],
            &metrics,
        );

        assert_eq!(events.len(), 1);
        let metric = events[0].try_as_metric().expect("metric event");
        assert_eq!(
            metric.values(),
            &MetricValues::rate((2, 42.0), std::time::Duration::ZERO)
        );
        assert_eq!(
            metric.context().tags().get_single_tag(METRIC_TYPE_ATTRIBUTE_KEY),
            Some(&Tag::from("datadog.metric.as_type:RaTe"))
        );
    }

    #[test]
    fn delta_sum_rate_as_type_warns_once_per_metric() {
        let metrics = Metrics::for_tests();
        let mut translator = OtlpMetricsTranslator::for_tests();
        let mut metric = delta_sum_with_as_type(42, "rate");
        let Some(OtlpMetricData::Sum(sum)) = metric.data.as_mut() else {
            unreachable!("expected delta Sum metric");
        };
        sum.data_points.push(sum.data_points[0].clone());

        let events = translator.map_to_dd_format(metric, &SharedTagSet::default(), None, &[], &metrics);

        assert_eq!(events.len(), 2);
        assert!(translator
            .metric_type_override_warnings
            .contains(&("delta.sum".to_string(), MetricTypeOverrideWarningKind::ZeroInterval,)));
    }

    #[test]
    fn delta_sum_non_rate_as_type_values_emit_counts() {
        let metrics = Metrics::for_tests();

        for as_type in ["count", "gauge"] {
            let mut translator = OtlpMetricsTranslator::for_tests();
            let events = translator.map_to_dd_format(
                delta_sum_with_as_type(42, as_type),
                &SharedTagSet::default(),
                None,
                &[],
                &metrics,
            );

            assert_eq!(events.len(), 1, "expected one event for {as_type}");
            assert_eq!(
                events[0].try_as_metric().expect("metric event").values(),
                &MetricValues::counter((2, 42.0)),
                "expected Count for {as_type}"
            );
        }
    }

    #[test]
    fn unsupported_rate_as_type_value_warns_once_per_metric() {
        let metrics = Metrics::for_tests();
        let mut translator = OtlpMetricsTranslator::for_tests();
        let mut metric = delta_sum_with_as_type(42, "unsupported");
        let Some(OtlpMetricData::Sum(sum)) = metric.data.as_mut() else {
            unreachable!("expected delta Sum metric");
        };
        sum.data_points.push(sum.data_points[0].clone());

        let events = translator.map_to_dd_format(metric, &SharedTagSet::default(), None, &[], &metrics);

        assert_eq!(events.len(), 2);
        assert!(translator
            .metric_type_override_warnings
            .contains(&("delta.sum".to_string(), MetricTypeOverrideWarningKind::UnsupportedValue,)));
    }

    #[test]
    fn rate_as_type_does_not_convert_gauges() {
        let metrics = Metrics::for_tests();
        let mut translator = OtlpMetricsTranslator::for_tests();
        let events = translator.map_to_dd_format(
            OtlpMetric {
                name: "gauge".to_string(),
                data: Some(OtlpMetricData::Gauge(
                    otlp_protos::opentelemetry::proto::metrics::v1::Gauge {
                        data_points: vec![OtlpNumberDataPoint {
                            value: Some(OtlpNumberDataPointValue::AsInt(42)),
                            time_unix_nano: nanos_from_seconds(2),
                            attributes: vec![string_attribute(METRIC_TYPE_ATTRIBUTE_KEY, "rate")],
                            ..Default::default()
                        }],
                    },
                )),
                ..Default::default()
            },
            &SharedTagSet::default(),
            None,
            &[],
            &metrics,
        );

        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0].try_as_metric().expect("metric event").values(),
            &MetricValues::gauge((2, 42.0))
        );
        assert!(translator
            .metric_type_override_warnings
            .contains(&("gauge".to_string(), MetricTypeOverrideWarningKind::InvalidType,)));
    }
}
