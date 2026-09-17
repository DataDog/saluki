//! Translation of OTLP cumulative monotonic sum data points into deltas or rates.

use std::sync::LazyLock;

use otlp_protos::opentelemetry::proto::metrics::v1::{DataPointFlags, NumberDataPoint as OtlpNumberDataPoint};
use saluki_common::collections::FastHashSet;
use saluki_core::data_model::event::Event;
use tracing::debug;

use super::number::get_number_data_point_value;
use super::{is_skippable, DataType, Dimensions, OtlpMetricsTranslator, TranslationContext};

// https://github.com/DataDog/datadog-agent/blob/main/pkg/opentelemetry-mapping-go/otlp/metrics/metrics_translator.go#L48-L63
static RATE_AS_GAUGE_METRICS: LazyLock<FastHashSet<&'static str>> = LazyLock::new(|| {
    let mut m = FastHashSet::default();
    m.insert("kafka.net.bytes_out.rate");
    m.insert("kafka.net.bytes_in.rate");
    m.insert("kafka.replication.isr_shrinks.rate");
    m.insert("kafka.replication.isr_expands.rate");
    m.insert("kafka.replication.leader_elections.rate");
    m.insert("jvm.gc.minor_collection_count");
    m.insert("jvm.gc.major_collection_count");
    m.insert("jvm.gc.minor_collection_time");
    m.insert("jvm.gc.major_collection_time");
    m.insert("kafka.messages_in.rate");
    m.insert("kafka.request.produce.failed.rate");
    m.insert("kafka.request.fetch.failed.rate");
    m.insert("kafka.replication.unclean_leader_elections.rate");
    m.insert("kafka.log.flush_rate.rate");
    m.insert("raymond.test.cumulative.as.gauge");
    m
});

impl OtlpMetricsTranslator {
    /// Maps a slice of OTLP cumulative monotonic `Sum` data points to Saluki `Event`s.
    pub(super) fn map_number_monotonic_metrics(
        &mut self, base_dims: Dimensions, data_points: Vec<OtlpNumberDataPoint>, context: &TranslationContext,
    ) -> Vec<Event> {
        let mut events = Vec::new();
        for (i, dp) in data_points.iter().enumerate() {
            // Skip if the data point has no recorded value.
            if dp.flags & (DataPointFlags::NoRecordedValueMask as u32) != 0 {
                continue;
            }

            let shadowing_resource_attributes = self
                .config
                .resource_attributes_as_tags
                .then_some(context.resource_attributes);
            let point_dims = base_dims.with_attribute_map(&dp.attributes, shadowing_resource_attributes);
            let value = get_number_data_point_value(dp);
            if is_skippable(value) {
                debug!(
                    metric_name = point_dims.name,
                    value, "Skipping metric with unsupported value (NaN or Infinity)."
                );
                self.translator_metrics.dropped_invalid_value().increment(1);
                continue;
            }

            if RATE_AS_GAUGE_METRICS.contains(point_dims.name.as_str()) {
                let (rate, is_first_point, should_drop_point) =
                    self.prev_pts
                        .monotonic_rate(&point_dims, dp.start_time_unix_nano, dp.time_unix_nano, value);

                if should_drop_point {
                    // debug!(
                    //     metric_name = point_dims.name,
                    //     "Dropping cumulative monotonic data point (rate) due to reset or out-of-order timestamp."
                    // );
                    continue;
                }

                if !is_first_point {
                    self.record_metric_event(
                        &point_dims,
                        rate,
                        dp.time_unix_nano,
                        DataType::Gauge,
                        &mut events,
                        context,
                    );
                }
                continue;
            }

            // Default behavior: calculate delta and consume as a Counter.
            let (delta, is_first_point, should_drop_point) =
                self.prev_pts
                    .monotonic_diff(&point_dims, dp.start_time_unix_nano, dp.time_unix_nano, value);

            if should_drop_point {
                // debug!(
                //     metric_name = point_dims.name,
                //     "Dropping cumulative monotonic data point due to reset or out-of-order timestamp."
                // );
                continue;
            }

            if !is_first_point {
                self.record_metric_event(
                    &point_dims,
                    delta,
                    dp.time_unix_nano,
                    DataType::Count,
                    &mut events,
                    context,
                );
            } else if i == 0 && self.should_consume_initial_value(dp.start_time_unix_nano, dp.time_unix_nano) {
                // We only compute the first point in the timeseries if it is the first value in the datapoint slice.
                self.record_metric_event(
                    &point_dims,
                    value,
                    dp.time_unix_nano,
                    DataType::Count,
                    &mut events,
                    context,
                );
            }
        }
        events
    }
}

#[cfg(test)]
mod tests {
    use otlp_protos::opentelemetry::proto::common::v1::KeyValue as OtlpKeyValue;
    use otlp_protos::opentelemetry::proto::metrics::v1::number_data_point::Value as OtlpNumberDataPointValue;
    use saluki_context::tags::Tag;
    use saluki_core::data_model::event::metric::MetricValues;

    use super::super::tests::nanos_from_seconds;
    use super::*;
    use crate::sources::otlp::Metrics;

    /// Drives `map_number_monotonic_metrics` for a metric named `name`, hiding the repeated
    /// `Dimensions` + `TranslationContext` construction shared across the monotonic-mapping tests.
    fn run_monotonic(
        translator: &mut OtlpMetricsTranslator, name: &str, data_points: Vec<OtlpNumberDataPoint>, metrics: &Metrics,
    ) -> Vec<Event> {
        let dims = Dimensions {
            name: name.to_string(),
            ..Default::default()
        };
        let context = TranslationContext {
            resource_attributes: &[],
            metrics,
        };
        translator.map_number_monotonic_metrics(dims, data_points, &context)
    }

    /// A helper function to build a series of cumulative monotonic integer data points from deltas.
    /// Mimics the `buildMonotonicIntPoints` helper in the Go tests.
    fn build_monotonic_int_points(deltas: &[i64]) -> Vec<OtlpNumberDataPoint> {
        let mut cumulative = Vec::with_capacity(deltas.len() + 1);
        cumulative.push(0);
        for (i, delta) in deltas.iter().enumerate() {
            let next_val = cumulative[i] + delta;
            cumulative.push(next_val);
        }

        let mut slice = Vec::with_capacity(cumulative.len());
        for (i, val) in cumulative.iter().enumerate() {
            slice.push(OtlpNumberDataPoint {
                value: Some(OtlpNumberDataPointValue::AsInt(*val)),
                time_unix_nano: nanos_from_seconds((i * 10) as u64),
                ..Default::default()
            });
        }
        slice
    }

    fn build_test_cumulative_monotonic_double_points(
        translator: &OtlpMetricsTranslator, values: &[f64], ts_match: bool,
    ) -> Vec<OtlpNumberDataPoint> {
        let start_ts = translator.process_start_time_ns + 1;
        values
            .iter()
            .enumerate()
            .map(|(i, &val)| {
                let timestamp = if ts_match {
                    start_ts
                } else {
                    start_ts + nanos_from_seconds((i + 2) as u64)
                };
                OtlpNumberDataPoint {
                    value: Some(OtlpNumberDataPointValue::AsDouble(val)),
                    start_time_unix_nano: start_ts,
                    time_unix_nano: timestamp,
                    ..Default::default()
                }
            })
            .collect()
    }

    /// A helper function to build a series of cumulative monotonic integer data points that includes a reset.
    /// Mimics the `buildMonotonicIntRebootPoints` helper in the Go tests.
    fn build_monotonic_int_reboot_points() -> Vec<OtlpNumberDataPoint> {
        let values = [0, 30, 0, 20];
        let mut slice = Vec::with_capacity(values.len());

        for (i, val) in values.iter().enumerate() {
            slice.push(OtlpNumberDataPoint {
                value: Some(OtlpNumberDataPointValue::AsInt(*val)),
                time_unix_nano: nanos_from_seconds((i * 10) as u64),
                ..Default::default()
            });
        }
        slice
    }

    /// A helper function to build a series of cumulative monotonic double data points from deltas.
    /// Mimics the `buildMonotonicDoublePoints` helper in the Go tests.
    fn build_monotonic_double_points(deltas: &[f64]) -> Vec<OtlpNumberDataPoint> {
        let mut cumulative = Vec::with_capacity(deltas.len() + 1);
        cumulative.push(0.0);
        for (i, delta) in deltas.iter().enumerate() {
            let next_val = cumulative[i] + delta;
            cumulative.push(next_val);
        }

        let mut slice = Vec::with_capacity(cumulative.len());
        for (i, val) in cumulative.iter().enumerate() {
            slice.push(OtlpNumberDataPoint {
                value: Some(OtlpNumberDataPointValue::AsDouble(*val)),
                time_unix_nano: nanos_from_seconds((i * 10) as u64),
                ..Default::default()
            });
        }
        slice
    }

    /// A helper function to build a series of cumulative monotonic double data points that includes a reset.
    /// Mimics the `buildMonotonicDoubleRebootPoints` helper in the Go tests.
    fn build_monotonic_double_reboot_points() -> Vec<OtlpNumberDataPoint> {
        let values = [0.0, 30.0, 0.0, 20.0];
        let mut slice = Vec::with_capacity(values.len());

        for (i, val) in values.iter().enumerate() {
            slice.push(OtlpNumberDataPoint {
                value: Some(OtlpNumberDataPointValue::AsDouble(*val)),
                time_unix_nano: nanos_from_seconds((i * 10) as u64),
                ..Default::default()
            });
        }
        slice
    }

    fn build_test_cumulative_monotonic_int_points(
        translator: &OtlpMetricsTranslator, values: &[i64], ts_match: bool,
    ) -> Vec<OtlpNumberDataPoint> {
        let start_ts = translator.process_start_time_ns + 1;
        values
            .iter()
            .enumerate()
            .map(|(i, &val)| {
                let timestamp = if ts_match {
                    start_ts
                } else {
                    start_ts + nanos_from_seconds((i + 2) as u64)
                };
                OtlpNumberDataPoint {
                    value: Some(OtlpNumberDataPointValue::AsInt(val)),
                    start_time_unix_nano: start_ts,
                    time_unix_nano: timestamp,
                    ..Default::default()
                }
            })
            .collect()
    }

    // https://github.com/DataDog/datadog-agent/blob/main/pkg/opentelemetry-mapping-go/otlp/metrics/metrics_translator_test.go#L296
    #[test]
    fn int_monotonic_diff_emits_one_counter_delta_per_point() {
        let metrics = Metrics::for_tests();
        let deltas = vec![1, 2, 200, 3, 7, 0];
        let mut translator = OtlpMetricsTranslator::for_tests();

        let events = run_monotonic(
            &mut translator,
            "metric.example",
            build_monotonic_int_points(&deltas),
            &metrics,
        );

        assert_eq!(events.len(), deltas.len(), "Expected one event for each delta");
        for (i, event) in events.iter().enumerate() {
            let metric = event.try_as_metric().unwrap();
            assert_eq!(
                metric.values(),
                &MetricValues::counter((((i + 1) * 10) as u64, deltas[i] as f64))
            );
        }
    }

    // https://github.com/DataDog/datadog-agent/blob/main/pkg/opentelemetry-mapping-go/otlp/metrics/metrics_translator_test.go#L296
    #[test]
    fn int_monotonic_rate_emits_per_second_gauge_per_point() {
        let metrics = Metrics::for_tests();
        let deltas = vec![1, 2, 200, 3, 7, 0];
        let mut translator = OtlpMetricsTranslator::for_tests();

        let events = run_monotonic(
            &mut translator,
            "kafka.net.bytes_out.rate",
            build_monotonic_int_points(&deltas),
            &metrics,
        );

        assert_eq!(
            events.len(),
            deltas.len(),
            "Expected one event for each delta in rate mode"
        );
        for (i, event) in events.iter().enumerate() {
            let metric = event.try_as_metric().unwrap();
            // The rate is delta / 10s interval.
            assert_eq!(
                metric.values(),
                &MetricValues::gauge((((i + 1) * 10) as u64, deltas[i] as f64 / 10.0))
            );
        }
    }

    /// Three int data points where the second duplicates the first point's timestamp (dropped).
    fn int_equal_timestamp_drop_slice(start_ts: u64) -> Vec<OtlpNumberDataPoint> {
        vec![
            OtlpNumberDataPoint {
                value: Some(OtlpNumberDataPointValue::AsInt(10)),
                start_time_unix_nano: start_ts,
                time_unix_nano: start_ts + nanos_from_seconds(2),
                ..Default::default()
            },
            OtlpNumberDataPoint {
                value: Some(OtlpNumberDataPointValue::AsInt(20)),
                start_time_unix_nano: start_ts,
                time_unix_nano: start_ts + nanos_from_seconds(2),
                ..Default::default()
            },
            OtlpNumberDataPoint {
                value: Some(OtlpNumberDataPointValue::AsInt(40)),
                start_time_unix_nano: start_ts,
                time_unix_nano: start_ts + nanos_from_seconds(4),
                ..Default::default()
            },
        ]
    }

    /// Three int data points where the second is older than the first point's timestamp (dropped).
    fn int_older_timestamp_drop_slice(start_ts: u64) -> Vec<OtlpNumberDataPoint> {
        vec![
            OtlpNumberDataPoint {
                value: Some(OtlpNumberDataPointValue::AsInt(10)),
                start_time_unix_nano: start_ts,
                time_unix_nano: start_ts + nanos_from_seconds(3),
                ..Default::default()
            },
            OtlpNumberDataPoint {
                value: Some(OtlpNumberDataPointValue::AsInt(25)),
                start_time_unix_nano: start_ts,
                time_unix_nano: start_ts + nanos_from_seconds(2),
                ..Default::default()
            },
            OtlpNumberDataPoint {
                value: Some(OtlpNumberDataPointValue::AsInt(40)),
                start_time_unix_nano: start_ts,
                time_unix_nano: start_ts + nanos_from_seconds(5),
                ..Default::default()
            },
        ]
    }

    // https://github.com/DataDog/datadog-agent/blob/main/pkg/opentelemetry-mapping-go/otlp/metrics/metrics_translator_test.go#L549
    #[test]
    fn int_monotonic_diff_drops_equal_timestamp_point() {
        let metrics = Metrics::for_tests();
        let mut translator = OtlpMetricsTranslator::for_tests();
        let start_ts = translator.process_start_time_ns + 1;

        let events = run_monotonic(
            &mut translator,
            "metric.example",
            int_equal_timestamp_drop_slice(start_ts),
            &metrics,
        );
        assert_eq!(events.len(), 2, "Expected two metrics after dropping a point");

        // First metric: the initial value of the counter.
        let metric = events[0].try_as_metric().unwrap();
        let expected_ts_s = (start_ts + nanos_from_seconds(2)) / 1_000_000_000;
        assert_eq!(metric.values(), &MetricValues::counter((expected_ts_s, 10.0)));

        // Second metric: the delta between the third and first points.
        let metric = events[1].try_as_metric().unwrap();
        let expected_ts_s = (start_ts + nanos_from_seconds(4)) / 1_000_000_000;
        assert_eq!(metric.values(), &MetricValues::counter((expected_ts_s, 30.0)));
    }

    // https://github.com/DataDog/datadog-agent/blob/main/pkg/opentelemetry-mapping-go/otlp/metrics/metrics_translator_test.go#L549
    #[test]
    fn int_monotonic_rate_drops_equal_timestamp_point() {
        let metrics = Metrics::for_tests();
        let mut translator = OtlpMetricsTranslator::for_tests();
        let start_ts = translator.process_start_time_ns + 1;

        // The first point is consumed but produces no rate; the second is dropped; the third
        // produces a rate based on the delta from the first.
        let events = run_monotonic(
            &mut translator,
            "kafka.net.bytes_out.rate",
            int_equal_timestamp_drop_slice(start_ts),
            &metrics,
        );
        assert_eq!(events.len(), 1, "Expected one metric for equal-rate test");

        let metric = events[0].try_as_metric().unwrap();
        // rate is (40-10) / (4s-2s) = 30 / 2 = 15
        let expected_ts_s = (start_ts + nanos_from_seconds(4)) / 1_000_000_000;
        assert_eq!(metric.values(), &MetricValues::gauge((expected_ts_s, 15.0)));
    }

    // https://github.com/DataDog/datadog-agent/blob/main/pkg/opentelemetry-mapping-go/otlp/metrics/metrics_translator_test.go#L549
    #[test]
    fn int_monotonic_diff_drops_older_timestamp_point() {
        let metrics = Metrics::for_tests();
        let mut translator = OtlpMetricsTranslator::for_tests();
        let start_ts = translator.process_start_time_ns + 1;

        let events = run_monotonic(
            &mut translator,
            "metric.example",
            int_older_timestamp_drop_slice(start_ts),
            &metrics,
        );
        assert_eq!(events.len(), 2, "Expected two metrics after dropping an older point");

        let metric = events[0].try_as_metric().unwrap();
        let expected_ts_s = (start_ts + nanos_from_seconds(3)) / 1_000_000_000;
        assert_eq!(metric.values(), &MetricValues::counter((expected_ts_s, 10.0)));

        let metric = events[1].try_as_metric().unwrap();
        let expected_ts_s = (start_ts + nanos_from_seconds(5)) / 1_000_000_000;
        assert_eq!(metric.values(), &MetricValues::counter((expected_ts_s, 30.0)));
    }

    // https://github.com/DataDog/datadog-agent/blob/main/pkg/opentelemetry-mapping-go/otlp/metrics/metrics_translator_test.go#L549
    #[test]
    fn int_monotonic_rate_drops_older_timestamp_point() {
        let metrics = Metrics::for_tests();
        let mut translator = OtlpMetricsTranslator::for_tests();
        let start_ts = translator.process_start_time_ns + 1;

        let events = run_monotonic(
            &mut translator,
            "kafka.net.bytes_out.rate",
            int_older_timestamp_drop_slice(start_ts),
            &metrics,
        );
        assert_eq!(
            events.len(),
            1,
            "Expected one metric after dropping an older rate point"
        );

        let metric = events[0].try_as_metric().unwrap();
        // rate is (40-10) / (5s-3s) = 30 / 2 = 15
        let expected_ts_s = (start_ts + nanos_from_seconds(5)) / 1_000_000_000;
        assert_eq!(metric.values(), &MetricValues::gauge((expected_ts_s, 15.0)));
    }

    // https://github.com/DataDog/datadog-agent/blob/main/pkg/opentelemetry-mapping-go/otlp/metrics/metrics_translator_test.go#L884
    #[test]
    fn int_monotonic_reports_first_value_from_new_series() {
        let metrics = Metrics::for_tests();
        let mut translator = OtlpMetricsTranslator::for_tests();

        let slice = build_test_cumulative_monotonic_int_points(&translator, &[10, 15, 20], false);
        let start_ts = slice[0].start_time_unix_nano;

        let events = run_monotonic(&mut translator, "metric.example", slice, &metrics);

        assert_eq!(events.len(), 3, "Expected three metrics for a new cumulative series");

        // First point is the raw value
        let metric1 = events[0].try_as_metric().unwrap();
        let expected_ts_s_1 = (start_ts + nanos_from_seconds(2)) / 1_000_000_000;
        assert_eq!(metric1.values(), &MetricValues::counter((expected_ts_s_1, 10.0)));

        // Second point is a delta from the first to the second value
        let metric2 = events[1].try_as_metric().unwrap();
        let expected_ts_s_2 = (start_ts + nanos_from_seconds(3)) / 1_000_000_000;
        assert_eq!(metric2.values(), &MetricValues::counter((expected_ts_s_2, 5.0)));

        // Third point is a delta from the second to the third value
        let metric3 = events[2].try_as_metric().unwrap();
        let expected_ts_s_3 = (start_ts + nanos_from_seconds(4)) / 1_000_000_000;
        assert_eq!(metric3.values(), &MetricValues::counter((expected_ts_s_3, 5.0)));
    }

    // https://github.com/DataDog/datadog-agent/blob/main/pkg/opentelemetry-mapping-go/otlp/metrics/metrics_translator_test.go#L1411
    #[test]
    fn int_monotonic_drops_out_of_order_point() {
        let metrics = Metrics::for_tests();
        let mut translator = OtlpMetricsTranslator::for_tests();

        let timestamps = [1, 0, 2, 3];
        let values = [0, 1, 2, 3];

        let mut slice = Vec::with_capacity(values.len());
        for i in 0..values.len() {
            slice.push(OtlpNumberDataPoint {
                value: Some(OtlpNumberDataPointValue::AsInt(values[i])),
                time_unix_nano: nanos_from_seconds(timestamps[i]),
                ..Default::default()
            });
        }

        let events = run_monotonic(&mut translator, "metric.example", slice, &metrics);

        // Expected metrics:
        // 1. First valid point is (ts: 1, val: 0). The point at ts: 0 is dropped. The first point is
        //    consumed by the cache but does not produce a metric.
        // 2. Next valid point is (ts: 2, val: 2). Delta is 2 - 0 = 2.
        // 3. Next valid point is (ts: 3, val: 3). Delta is 3 - 2 = 1.
        assert_eq!(
            events.len(),
            2,
            "Expected two metrics after dropping one out-of-order point"
        );

        let metric = events[0].try_as_metric().unwrap();
        assert_eq!(metric.values(), &MetricValues::counter((2, 2.0)));

        let metric = events[1].try_as_metric().unwrap();
        assert_eq!(metric.values(), &MetricValues::counter((3, 1.0)));
    }

    // https://github.com/DataDog/datadog-agent/blob/main/pkg/opentelemetry-mapping-go/otlp/metrics/metrics_translator_test.go#L332
    #[test]
    fn int_monotonic_separates_series_by_dimensions() {
        let metrics = Metrics::for_tests();
        let mut translator = OtlpMetricsTranslator::for_tests();

        let mut slice = Vec::new();

        // Series with no tags
        slice.push(OtlpNumberDataPoint {
            time_unix_nano: nanos_from_seconds(0),
            ..Default::default()
        });
        slice.push(OtlpNumberDataPoint {
            value: Some(OtlpNumberDataPointValue::AsInt(20)),
            time_unix_nano: nanos_from_seconds(1),
            ..Default::default()
        });

        // Series with tag key1:valA
        let attributes_a = vec![OtlpKeyValue {
            key: "key1".to_string(),
            value: Some(otlp_protos::opentelemetry::proto::common::v1::AnyValue {
                value: Some(
                    otlp_protos::opentelemetry::proto::common::v1::any_value::Value::StringValue("valA".to_string()),
                ),
            }),
        }];
        slice.push(OtlpNumberDataPoint {
            time_unix_nano: nanos_from_seconds(0),
            attributes: attributes_a.clone(),
            ..Default::default()
        });
        slice.push(OtlpNumberDataPoint {
            value: Some(OtlpNumberDataPointValue::AsInt(30)),
            time_unix_nano: nanos_from_seconds(1),
            attributes: attributes_a,
            ..Default::default()
        });

        // Series with tag key1:valB
        let attributes_b = vec![OtlpKeyValue {
            key: "key1".to_string(),
            value: Some(otlp_protos::opentelemetry::proto::common::v1::AnyValue {
                value: Some(
                    otlp_protos::opentelemetry::proto::common::v1::any_value::Value::StringValue("valB".to_string()),
                ),
            }),
        }];
        slice.push(OtlpNumberDataPoint {
            time_unix_nano: nanos_from_seconds(0),
            attributes: attributes_b.clone(),
            ..Default::default()
        });
        slice.push(OtlpNumberDataPoint {
            value: Some(OtlpNumberDataPointValue::AsInt(40)),
            time_unix_nano: nanos_from_seconds(1),
            attributes: attributes_b,
            ..Default::default()
        });

        let events = run_monotonic(&mut translator, "metric.example", slice, &metrics);
        assert_eq!(events.len(), 3, "Expected three distinct metrics");

        assert_eq!(
            events[0].try_as_metric().unwrap().values(),
            &MetricValues::counter((1, 20.0))
        );
        let metric2 = events[1].try_as_metric().unwrap();
        assert_eq!(
            metric2.context().tags().get_single_tag("key1"),
            Some(&Tag::from("key1:valA"))
        );
        assert_eq!(metric2.values(), &MetricValues::counter((1, 30.0)));
        let metric3 = events[2].try_as_metric().unwrap();
        assert_eq!(
            metric3.context().tags().get_single_tag("key1"),
            Some(&Tag::from("key1:valB"))
        );
        assert_eq!(metric3.values(), &MetricValues::counter((1, 40.0)));
    }

    // https://github.com/DataDog/datadog-agent/blob/main/pkg/opentelemetry-mapping-go/otlp/metrics/metrics_translator_test.go#L395
    #[test]
    fn int_monotonic_diff_handles_reboot_within_slice() {
        let metrics = Metrics::for_tests();
        let mut translator = OtlpMetricsTranslator::for_tests();

        let events = run_monotonic(
            &mut translator,
            "metric.example",
            build_monotonic_int_reboot_points(),
            &metrics,
        );

        assert_eq!(events.len(), 2, "Expected two metrics after a reboot");
        assert_eq!(
            events[0].try_as_metric().unwrap().values(),
            &MetricValues::counter((10, 30.0))
        );
        assert_eq!(
            events[1].try_as_metric().unwrap().values(),
            &MetricValues::counter((30, 20.0))
        );
    }

    // https://github.com/DataDog/datadog-agent/blob/main/pkg/opentelemetry-mapping-go/otlp/metrics/metrics_translator_test.go#L395
    #[test]
    fn int_monotonic_rate_handles_reboot_within_slice() {
        let metrics = Metrics::for_tests();
        let mut translator = OtlpMetricsTranslator::for_tests();

        let events = run_monotonic(
            &mut translator,
            "kafka.net.bytes_out.rate",
            build_monotonic_int_reboot_points(),
            &metrics,
        );

        assert_eq!(events.len(), 2, "Expected two metrics for rate after a reboot");
        // 30 / 10s and 20 / 10s.
        assert_eq!(
            events[0].try_as_metric().unwrap().values(),
            &MetricValues::gauge((10, 3.0))
        );
        assert_eq!(
            events[1].try_as_metric().unwrap().values(),
            &MetricValues::gauge((30, 2.0))
        );
    }

    // https://github.com/DataDog/datadog-agent/blob/main/pkg/opentelemetry-mapping-go/otlp/metrics/metrics_translator_test.go#L1456
    #[test]
    fn double_monotonic_diff_emits_one_counter_delta_per_point() {
        let metrics = Metrics::for_tests();
        let deltas = vec![1.0, 2.0, 200.0, 3.0, 7.0, 0.0];
        let mut translator = OtlpMetricsTranslator::for_tests();

        let events = run_monotonic(
            &mut translator,
            "metric.example",
            build_monotonic_double_points(&deltas),
            &metrics,
        );

        assert_eq!(events.len(), deltas.len());
        for (i, event) in events.iter().enumerate() {
            let metric = event.try_as_metric().unwrap();
            assert_eq!(
                metric.values(),
                &MetricValues::counter((((i + 1) * 10) as u64, deltas[i]))
            );
        }
    }

    // https://github.com/DataDog/datadog-agent/blob/main/pkg/opentelemetry-mapping-go/otlp/metrics/metrics_translator_test.go#L1456
    #[test]
    fn double_monotonic_rate_emits_per_second_gauge_per_point() {
        let metrics = Metrics::for_tests();
        let deltas = vec![1.0, 2.0, 200.0, 3.0, 7.0, 0.0];
        let mut translator = OtlpMetricsTranslator::for_tests();

        let events = run_monotonic(
            &mut translator,
            "kafka.net.bytes_out.rate",
            build_monotonic_double_points(&deltas),
            &metrics,
        );

        assert_eq!(events.len(), deltas.len());
        for (i, event) in events.iter().enumerate() {
            let metric = event.try_as_metric().unwrap();
            assert_eq!(
                metric.values(),
                &MetricValues::gauge((((i + 1) * 10) as u64, deltas[i] / 10.0))
            );
        }
    }

    // https://github.com/DataDog/datadog-agent/blob/main/pkg/opentelemetry-mapping-go/otlp/metrics/metrics_translator_test.go#L1493
    #[test]
    fn double_monotonic_separates_series_by_dimensions() {
        let metrics = Metrics::for_tests();
        let mut translator = OtlpMetricsTranslator::for_tests();

        let mut slice = Vec::new();

        // Series with no tags
        slice.push(OtlpNumberDataPoint {
            time_unix_nano: nanos_from_seconds(0),
            ..Default::default()
        });
        slice.push(OtlpNumberDataPoint {
            value: Some(OtlpNumberDataPointValue::AsDouble(20.0)),
            time_unix_nano: nanos_from_seconds(1),
            ..Default::default()
        });

        // Series with tag key1:valA
        let attributes_a = vec![OtlpKeyValue {
            key: "key1".to_string(),
            value: Some(otlp_protos::opentelemetry::proto::common::v1::AnyValue {
                value: Some(
                    otlp_protos::opentelemetry::proto::common::v1::any_value::Value::StringValue("valA".to_string()),
                ),
            }),
        }];
        slice.push(OtlpNumberDataPoint {
            time_unix_nano: nanos_from_seconds(0),
            attributes: attributes_a.clone(),
            ..Default::default()
        });
        slice.push(OtlpNumberDataPoint {
            value: Some(OtlpNumberDataPointValue::AsDouble(30.0)),
            time_unix_nano: nanos_from_seconds(1),
            attributes: attributes_a,
            ..Default::default()
        });

        // Series with tag key1:valB
        let attributes_b = vec![OtlpKeyValue {
            key: "key1".to_string(),
            value: Some(otlp_protos::opentelemetry::proto::common::v1::AnyValue {
                value: Some(
                    otlp_protos::opentelemetry::proto::common::v1::any_value::Value::StringValue("valB".to_string()),
                ),
            }),
        }];
        slice.push(OtlpNumberDataPoint {
            time_unix_nano: nanos_from_seconds(0),
            attributes: attributes_b.clone(),
            ..Default::default()
        });
        slice.push(OtlpNumberDataPoint {
            value: Some(OtlpNumberDataPointValue::AsDouble(40.0)),
            time_unix_nano: nanos_from_seconds(1),
            attributes: attributes_b,
            ..Default::default()
        });

        let events = run_monotonic(&mut translator, "metric.example", slice, &metrics);
        assert_eq!(events.len(), 3);

        assert_eq!(
            events[0].try_as_metric().unwrap().values(),
            &MetricValues::counter((1, 20.0))
        );
        let metric2 = events[1].try_as_metric().unwrap();
        assert_eq!(
            metric2.context().tags().get_single_tag("key1"),
            Some(&Tag::from("key1:valA"))
        );
        assert_eq!(metric2.values(), &MetricValues::counter((1, 30.0)));
        let metric3 = events[2].try_as_metric().unwrap();
        assert_eq!(
            metric3.context().tags().get_single_tag("key1"),
            Some(&Tag::from("key1:valB"))
        );
        assert_eq!(metric3.values(), &MetricValues::counter((1, 40.0)));
    }

    // https://github.com/DataDog/datadog-agent/blob/main/pkg/opentelemetry-mapping-go/otlp/metrics/metrics_translator_test.go#L1555
    #[test]
    fn double_monotonic_diff_handles_reboot_within_slice() {
        let metrics = Metrics::for_tests();
        let mut translator = OtlpMetricsTranslator::for_tests();

        let events = run_monotonic(
            &mut translator,
            "metric.example",
            build_monotonic_double_reboot_points(),
            &metrics,
        );

        assert_eq!(events.len(), 2);
        assert_eq!(
            events[0].try_as_metric().unwrap().values(),
            &MetricValues::counter((10, 30.0))
        );
        assert_eq!(
            events[1].try_as_metric().unwrap().values(),
            &MetricValues::counter((30, 20.0))
        );
    }

    // https://github.com/DataDog/datadog-agent/blob/main/pkg/opentelemetry-mapping-go/otlp/metrics/metrics_translator_test.go#L1555
    #[test]
    fn double_monotonic_rate_handles_reboot_within_slice() {
        let metrics = Metrics::for_tests();
        let mut translator = OtlpMetricsTranslator::for_tests();

        let events = run_monotonic(
            &mut translator,
            "kafka.net.bytes_out.rate",
            build_monotonic_double_reboot_points(),
            &metrics,
        );

        assert_eq!(events.len(), 2);
        assert_eq!(
            events[0].try_as_metric().unwrap().values(),
            &MetricValues::gauge((10, 3.0))
        );
        assert_eq!(
            events[1].try_as_metric().unwrap().values(),
            &MetricValues::gauge((30, 2.0))
        );
    }

    // https://github.com/DataDog/datadog-agent/blob/main/pkg/opentelemetry-mapping-go/otlp/metrics/metrics_translator_test.go#L1678
    #[test]
    fn double_monotonic_diff_drops_equal_timestamp_point() {
        let metrics = Metrics::for_tests();
        let mut translator = OtlpMetricsTranslator::for_tests();
        let start_ts = translator.process_start_time_ns + 1;

        // The second point duplicates the first point's timestamp and is dropped.
        let slice = vec![
            OtlpNumberDataPoint {
                value: Some(OtlpNumberDataPointValue::AsDouble(10.0)),
                start_time_unix_nano: start_ts,
                time_unix_nano: start_ts + nanos_from_seconds(2),
                ..Default::default()
            },
            OtlpNumberDataPoint {
                value: Some(OtlpNumberDataPointValue::AsDouble(20.0)),
                start_time_unix_nano: start_ts,
                time_unix_nano: start_ts + nanos_from_seconds(2),
                ..Default::default()
            },
            OtlpNumberDataPoint {
                value: Some(OtlpNumberDataPointValue::AsDouble(40.0)),
                start_time_unix_nano: start_ts,
                time_unix_nano: start_ts + nanos_from_seconds(4),
                ..Default::default()
            },
        ];

        let events = run_monotonic(&mut translator, "metric.example", slice, &metrics);
        assert_eq!(events.len(), 2);

        let expected_ts_s_1 = (start_ts + nanos_from_seconds(2)) / 1_000_000_000;
        assert_eq!(
            events[0].try_as_metric().unwrap().values(),
            &MetricValues::counter((expected_ts_s_1, 10.0))
        );
        let expected_ts_s_2 = (start_ts + nanos_from_seconds(4)) / 1_000_000_000;
        assert_eq!(
            events[1].try_as_metric().unwrap().values(),
            &MetricValues::counter((expected_ts_s_2, 30.0))
        );
    }

    // https://github.com/DataDog/datadog-agent/blob/main/pkg/opentelemetry-mapping-go/otlp/metrics/metrics_translator_test.go#L1678
    #[test]
    fn double_monotonic_diff_drops_older_timestamp_point() {
        let metrics = Metrics::for_tests();
        let mut translator = OtlpMetricsTranslator::for_tests();
        let start_ts = translator.process_start_time_ns + 1;

        // The second point is older than the first point's timestamp and is dropped.
        let slice = vec![
            OtlpNumberDataPoint {
                value: Some(OtlpNumberDataPointValue::AsDouble(10.0)),
                start_time_unix_nano: start_ts,
                time_unix_nano: start_ts + nanos_from_seconds(3),
                ..Default::default()
            },
            OtlpNumberDataPoint {
                value: Some(OtlpNumberDataPointValue::AsDouble(25.0)),
                start_time_unix_nano: start_ts,
                time_unix_nano: start_ts + nanos_from_seconds(2),
                ..Default::default()
            },
            OtlpNumberDataPoint {
                value: Some(OtlpNumberDataPointValue::AsDouble(40.0)),
                start_time_unix_nano: start_ts,
                time_unix_nano: start_ts + nanos_from_seconds(5),
                ..Default::default()
            },
        ];

        let events = run_monotonic(&mut translator, "metric.example", slice, &metrics);
        assert_eq!(events.len(), 2);

        let expected_ts_s_1 = (start_ts + nanos_from_seconds(3)) / 1_000_000_000;
        assert_eq!(
            events[0].try_as_metric().unwrap().values(),
            &MetricValues::counter((expected_ts_s_1, 10.0))
        );
        let expected_ts_s_2 = (start_ts + nanos_from_seconds(5)) / 1_000_000_000;
        assert_eq!(
            events[1].try_as_metric().unwrap().values(),
            &MetricValues::counter((expected_ts_s_2, 30.0))
        );
    }

    // https://github.com/DataDog/datadog-agent/blob/main/pkg/opentelemetry-mapping-go/otlp/metrics/metrics_translator_test.go#L2033
    #[test]
    fn double_monotonic_drops_out_of_order_point() {
        let metrics = Metrics::for_tests();
        let mut translator = OtlpMetricsTranslator::for_tests();

        let timestamps = [1, 0, 2, 3];
        let values = [0.0, 1.0, 2.0, 3.0];

        let mut slice = Vec::with_capacity(values.len());
        for i in 0..values.len() {
            slice.push(OtlpNumberDataPoint {
                value: Some(OtlpNumberDataPointValue::AsDouble(values[i])),
                time_unix_nano: nanos_from_seconds(timestamps[i]),
                ..Default::default()
            });
        }

        let events = run_monotonic(&mut translator, "metric.example", slice, &metrics);
        assert_eq!(events.len(), 2);

        assert_eq!(
            events[0].try_as_metric().unwrap().values(),
            &MetricValues::counter((2, 2.0))
        );
        assert_eq!(
            events[1].try_as_metric().unwrap().values(),
            &MetricValues::counter((3, 1.0))
        );
    }

    // https://github.com/DataDog/datadog-agent/blob/main/pkg/opentelemetry-mapping-go/otlp/metrics/metrics_translator_test.go#L464
    #[test]
    fn int_monotonic_diff_reports_reset_at_beginning_of_slice() {
        let metrics = Metrics::for_tests();
        let mut translator = OtlpMetricsTranslator::for_tests();
        let start_ts = translator.process_start_time_ns + 1;

        // Prime the cache with a previous point so the first point in the slice is a reset (5 < 10).
        let dims = Dimensions {
            name: "metric.example".to_string(),
            ..Default::default()
        };
        translator
            .prev_pts
            .monotonic_diff(&dims, start_ts, start_ts + nanos_from_seconds(2), 10.0);

        let slice = vec![
            OtlpNumberDataPoint {
                value: Some(OtlpNumberDataPointValue::AsInt(5)), // Reset: 5 < 10
                start_time_unix_nano: start_ts,
                time_unix_nano: start_ts + nanos_from_seconds(3),
                ..Default::default()
            },
            OtlpNumberDataPoint {
                value: Some(OtlpNumberDataPointValue::AsInt(30)),
                start_time_unix_nano: start_ts,
                time_unix_nano: start_ts + nanos_from_seconds(4),
                ..Default::default()
            },
        ];

        let events = run_monotonic(&mut translator, "metric.example", slice, &metrics);
        assert_eq!(events.len(), 2, "Expected two metrics after reset");

        // The reset point should be emitted as a new "first value".
        let expected_ts_s_1 = (start_ts + nanos_from_seconds(3)) / 1_000_000_000;
        assert_eq!(
            events[0].try_as_metric().unwrap().values(),
            &MetricValues::counter((expected_ts_s_1, 5.0))
        );

        // The next point should be a delta from the reset value: 30 - 5.
        let expected_ts_s_2 = (start_ts + nanos_from_seconds(4)) / 1_000_000_000;
        assert_eq!(
            events[1].try_as_metric().unwrap().values(),
            &MetricValues::counter((expected_ts_s_2, 25.0))
        );
    }

    // https://github.com/DataDog/datadog-agent/blob/main/pkg/opentelemetry-mapping-go/otlp/metrics/metrics_translator_test.go#L464
    #[test]
    fn int_monotonic_rate_reports_reset_at_beginning_of_slice() {
        let metrics = Metrics::for_tests();
        let mut translator = OtlpMetricsTranslator::for_tests();
        let start_ts = translator.process_start_time_ns + 1;

        // Prime the cache so the first point in the slice is a reset (5 < 10).
        let dims = Dimensions {
            name: "kafka.net.bytes_out.rate".to_string(),
            ..Default::default()
        };
        translator
            .prev_pts
            .monotonic_rate(&dims, start_ts, start_ts + nanos_from_seconds(2), 10.0);

        let slice = vec![
            OtlpNumberDataPoint {
                value: Some(OtlpNumberDataPointValue::AsInt(5)), // Reset: 5 < 10
                start_time_unix_nano: start_ts,
                time_unix_nano: start_ts + nanos_from_seconds(3),
                ..Default::default()
            },
            OtlpNumberDataPoint {
                value: Some(OtlpNumberDataPointValue::AsInt(30)),
                start_time_unix_nano: start_ts,
                time_unix_nano: start_ts + nanos_from_seconds(4),
                ..Default::default()
            },
        ];

        let events = run_monotonic(&mut translator, "kafka.net.bytes_out.rate", slice, &metrics);
        assert_eq!(events.len(), 1, "Expected one metric for rate after reset");

        let expected_ts_s = (start_ts + nanos_from_seconds(4)) / 1_000_000_000;
        // rate = (30 - 5) / (4s - 3s) = 25 / 1 = 25
        assert_eq!(
            events[0].try_as_metric().unwrap().values(),
            &MetricValues::gauge((expected_ts_s, 25.0))
        );
    }

    // https://github.com/DataDog/datadog-agent/blob/main/pkg/opentelemetry-mapping-go/otlp/metrics/metrics_translator_test.go#L901
    #[test]
    fn int_monotonic_rate_does_not_report_first_value() {
        let metrics = Metrics::for_tests();
        let mut translator = OtlpMetricsTranslator::for_tests();

        let slice = build_test_cumulative_monotonic_int_points(&translator, &[10, 15, 20], false);
        let start_ts_s = slice[0].start_time_unix_nano / 1_000_000_000;

        let events = run_monotonic(&mut translator, "kafka.net.bytes_out.rate", slice, &metrics);

        // For rates, the first value is consumed by the cache but doesn't produce a metric.
        assert_eq!(events.len(), 2, "Expected two metrics for a new rate series");

        // First metric is rate from point 1 to 2: (15-10)/(3-2) = 5
        let metric1 = events[0].try_as_metric().unwrap();
        let expected_ts_s_1 = start_ts_s + 3;
        assert_eq!(metric1.values(), &MetricValues::gauge((expected_ts_s_1, 5.0)));

        // Second metric is rate from point 2 to 3: (20-15)/(4-3) = 5
        let metric2 = events[1].try_as_metric().unwrap();
        let expected_ts_s_2 = start_ts_s + 4;
        assert_eq!(metric2.values(), &MetricValues::gauge((expected_ts_s_2, 5.0)));
    }

    // https://github.com/DataDog/datadog-agent/blob/main/pkg/opentelemetry-mapping-go/otlp/metrics/metrics_translator_test.go#L917
    #[test]
    fn int_monotonic_does_not_report_first_value_when_start_ts_matches_ts() {
        let metrics = Metrics::for_tests();
        let mut translator = OtlpMetricsTranslator::for_tests();

        // ts_match = true, so start_time_unix_nano will equal time_unix_nano.
        let slice = build_test_cumulative_monotonic_int_points(&translator, &[10, 15, 20], true);

        let events = run_monotonic(&mut translator, "metric.example", slice, &metrics);

        assert!(
            events.is_empty(),
            "Expected no metrics when start timestamp matches timestamp"
        );
    }

    // https://github.com/DataDog/datadog-agent/blob/main/pkg/opentelemetry-mapping-go/otlp/metrics/metrics_translator_test.go#L926
    #[test]
    fn int_monotonic_rate_does_not_report_first_value_when_start_ts_matches_ts() {
        let metrics = Metrics::for_tests();
        let mut translator = OtlpMetricsTranslator::for_tests();

        // ts_match = true, so start_time_unix_nano will equal time_unix_nano.
        let slice = build_test_cumulative_monotonic_int_points(&translator, &[10, 15, 20], true);

        let events = run_monotonic(&mut translator, "kafka.net.bytes_out.rate", slice, &metrics);

        assert!(
            events.is_empty(),
            "Expected no metrics when start timestamp matches timestamp for rates"
        );
    }

    // https://github.com/DataDog/datadog-agent/blob/main/pkg/opentelemetry-mapping-go/otlp/metrics/metrics_translator_test.go#L935
    #[test]
    fn int_monotonic_reports_diff_for_first_value_against_cached_point() {
        let metrics = Metrics::for_tests();
        let mut translator = OtlpMetricsTranslator::for_tests();

        let dims = Dimensions {
            name: "metric.example".to_string(),
            ..Default::default()
        };
        let start_ts = translator.process_start_time_ns + 1;

        // Pre-populate the cache with a previous point.
        translator
            .prev_pts
            .monotonic_diff(&dims, start_ts, start_ts + nanos_from_seconds(1), 1.0);

        let slice = build_test_cumulative_monotonic_int_points(&translator, &[10, 15, 20], false);
        let start_ts_s = slice[0].start_time_unix_nano / 1_000_000_000;

        let events = run_monotonic(&mut translator, "metric.example", slice, &metrics);

        assert_eq!(
            events.len(),
            3,
            "Expected three metrics when diffing from a pre-existing value"
        );

        // First point is diff from cached value: 10 - 1 = 9
        let metric1 = events[0].try_as_metric().unwrap();
        let expected_ts_s_1 = start_ts_s + 2;
        assert_eq!(metric1.values(), &MetricValues::counter((expected_ts_s_1, 9.0)));

        // Second point is delta: 15 - 10 = 5
        let metric2 = events[1].try_as_metric().unwrap();
        let expected_ts_s_2 = start_ts_s + 3;
        assert_eq!(metric2.values(), &MetricValues::counter((expected_ts_s_2, 5.0)));

        // Third point is delta: 20 - 15 = 5
        let metric3 = events[2].try_as_metric().unwrap();
        let expected_ts_s_3 = start_ts_s + 4;
        assert_eq!(metric3.values(), &MetricValues::counter((expected_ts_s_3, 5.0)));
    }

    // https://github.com/DataDog/datadog-agent/blob/main/pkg/opentelemetry-mapping-go/otlp/metrics/metrics_translator_test.go#L955
    #[test]
    fn int_monotonic_reports_rate_for_first_value_against_cached_point() {
        let metrics = Metrics::for_tests();
        let mut translator = OtlpMetricsTranslator::for_tests();

        let dims = Dimensions {
            name: "kafka.net.bytes_out.rate".to_string(),
            ..Default::default()
        };
        let start_ts = translator.process_start_time_ns + 1;

        // Pre-populate the cache with a previous point.
        translator
            .prev_pts
            .monotonic_diff(&dims, start_ts, start_ts + nanos_from_seconds(1), 1.0);

        let slice = build_test_cumulative_monotonic_int_points(&translator, &[10, 15, 20], false);
        let start_ts_s = slice[0].start_time_unix_nano / 1_000_000_000;

        let events = run_monotonic(&mut translator, "kafka.net.bytes_out.rate", slice, &metrics);

        assert_eq!(
            events.len(),
            3,
            "Expected three metrics when calculating rate from a pre-existing value"
        );

        // First point is rate from cached value: (10 - 1) / ((start_ts+2) - (start_ts+1)) = 9 / 1s = 9
        let metric1 = events[0].try_as_metric().unwrap();
        let expected_ts_s_1 = start_ts_s + 2;
        assert_eq!(metric1.values(), &MetricValues::gauge((expected_ts_s_1, 9.0)));

        // Second point is rate: (15 - 10) / ((start_ts+3) - (start_ts+2)) = 5 / 1s = 5
        let metric2 = events[1].try_as_metric().unwrap();
        let expected_ts_s_2 = start_ts_s + 3;
        assert_eq!(metric2.values(), &MetricValues::gauge((expected_ts_s_2, 5.0)));

        // Third point is rate: (20 - 15) / ((start_ts+4) - (start_ts+3)) = 5 / 1s = 5
        let metric3 = events[2].try_as_metric().unwrap();
        let expected_ts_s_3 = start_ts_s + 4;
        assert_eq!(metric3.values(), &MetricValues::gauge((expected_ts_s_3, 5.0)));
    }

    // https://github.com/DataDog/datadog-agent/blob/main/pkg/opentelemetry-mapping-go/otlp/metrics/metrics_translator_test.go#L429
    #[test]
    fn int_monotonic_skips_no_recorded_value_points() {
        let metrics = Metrics::for_tests();
        let mut translator = OtlpMetricsTranslator::for_tests();

        let start_ts = translator.process_start_time_ns;

        // This setup mimics the `buildMonotonicWithNoRecorded` helper in the Go test.
        let slice = vec![
            OtlpNumberDataPoint {
                value: Some(OtlpNumberDataPointValue::AsInt(0)),
                start_time_unix_nano: start_ts,
                time_unix_nano: start_ts + nanos_from_seconds(0),
                ..Default::default()
            },
            OtlpNumberDataPoint {
                value: Some(OtlpNumberDataPointValue::AsInt(30)),
                start_time_unix_nano: start_ts,
                time_unix_nano: start_ts + nanos_from_seconds(10),
                ..Default::default()
            },
            OtlpNumberDataPoint {
                value: Some(OtlpNumberDataPointValue::AsInt(0)),
                start_time_unix_nano: start_ts,
                time_unix_nano: start_ts + nanos_from_seconds(20),
                // The `NoRecordedValue` flag instructs the consumer to skip this point.
                flags: DataPointFlags::NoRecordedValueMask as u32,
                ..Default::default()
            },
            OtlpNumberDataPoint {
                value: Some(OtlpNumberDataPointValue::AsInt(40)),
                start_time_unix_nano: start_ts,
                time_unix_nano: start_ts + nanos_from_seconds(30),
                ..Default::default()
            },
        ];

        let events = run_monotonic(&mut translator, "metric.example", slice, &metrics);

        assert_eq!(
            events.len(),
            2,
            "Expected two metrics, skipping the one with NoRecordedValue flag"
        );

        // First metric is delta from point 0 to 1: 30 - 0 = 30
        let start_ts_s = start_ts / 1_000_000_000;
        let metric1 = events[0].try_as_metric().unwrap();
        assert_eq!(metric1.values(), &MetricValues::counter((start_ts_s + 10, 30.0)));

        // Second metric is delta from point 1 to 3 (skipping 2): 40 - 30 = 10
        let metric2 = events[1].try_as_metric().unwrap();
        assert_eq!(metric2.values(), &MetricValues::counter((start_ts_s + 30, 10.0)));
    }

    // https://github.com/DataDog/datadog-agent/blob/main/pkg/opentelemetry-mapping-go/otlp/metrics/metrics_translator_test.go#L1589
    #[test]
    fn double_monotonic_diff_reports_reset_at_beginning_of_slice() {
        let metrics = Metrics::for_tests();
        let mut translator = OtlpMetricsTranslator::for_tests();
        let start_ts = translator.process_start_time_ns + 1;

        // Pre-populate the cache to establish a previous value so the first point is a reset (5 < 10).
        let dims = Dimensions {
            name: "metric.example".to_string(),
            ..Default::default()
        };
        translator
            .prev_pts
            .monotonic_diff(&dims, start_ts, start_ts + nanos_from_seconds(2), 10.0);

        let slice = vec![
            OtlpNumberDataPoint {
                value: Some(OtlpNumberDataPointValue::AsDouble(5.0)),
                start_time_unix_nano: start_ts,
                time_unix_nano: start_ts + nanos_from_seconds(3),
                ..Default::default()
            },
            OtlpNumberDataPoint {
                value: Some(OtlpNumberDataPointValue::AsDouble(30.0)),
                start_time_unix_nano: start_ts,
                time_unix_nano: start_ts + nanos_from_seconds(4),
                ..Default::default()
            },
        ];

        let events = run_monotonic(&mut translator, "metric.example", slice, &metrics);
        assert_eq!(events.len(), 2, "Expected two metrics for reboot diff test");

        let start_ts_s = start_ts / 1_000_000_000;
        assert_eq!(
            events[0].try_as_metric().unwrap().values(),
            &MetricValues::counter((start_ts_s + 3, 5.0))
        );
        assert_eq!(
            events[1].try_as_metric().unwrap().values(),
            &MetricValues::counter((start_ts_s + 4, 25.0))
        );
    }

    // https://github.com/DataDog/datadog-agent/blob/main/pkg/opentelemetry-mapping-go/otlp/metrics/metrics_translator_test.go#L1589
    #[test]
    fn double_monotonic_rate_reports_reset_at_beginning_of_slice() {
        let metrics = Metrics::for_tests();
        let mut translator = OtlpMetricsTranslator::for_tests();
        let start_ts = translator.process_start_time_ns + 1;

        // Pre-populate the cache so the first point is a reset (5 < 10).
        let dims = Dimensions {
            name: "kafka.net.bytes_out.rate".to_string(),
            ..Default::default()
        };
        translator
            .prev_pts
            .monotonic_rate(&dims, start_ts, start_ts + nanos_from_seconds(2), 10.0);

        let slice = vec![
            OtlpNumberDataPoint {
                value: Some(OtlpNumberDataPointValue::AsDouble(5.0)),
                start_time_unix_nano: start_ts,
                time_unix_nano: start_ts + nanos_from_seconds(3),
                ..Default::default()
            },
            OtlpNumberDataPoint {
                value: Some(OtlpNumberDataPointValue::AsDouble(30.0)),
                start_time_unix_nano: start_ts,
                time_unix_nano: start_ts + nanos_from_seconds(4),
                ..Default::default()
            },
        ];

        let events = run_monotonic(&mut translator, "kafka.net.bytes_out.rate", slice, &metrics);
        assert_eq!(events.len(), 1, "Expected one metric for reboot rate test");

        let start_ts_s = start_ts / 1_000_000_000;
        // Rate is (30-5)/(4-3) = 25.
        assert_eq!(
            events[0].try_as_metric().unwrap().values(),
            &MetricValues::gauge((start_ts_s + 4, 25.0))
        );
    }

    // https://github.com/DataDog/datadog-agent/blob/main/pkg/opentelemetry-mapping-go/otlp/metrics/metrics_translator_test.go#L1848
    #[test]
    fn double_monotonic_diff_drops_equal_timestamp_at_beginning_of_slice() {
        let metrics = Metrics::for_tests();
        let mut translator = OtlpMetricsTranslator::for_tests();
        let start_ts = translator.process_start_time_ns + 1;

        // Pre-populate the cache; the first slice point duplicates the cached timestamp and is dropped.
        let dims = Dimensions {
            name: "metric.example".to_string(),
            ..Default::default()
        };
        translator
            .prev_pts
            .monotonic_diff(&dims, start_ts, start_ts + nanos_from_seconds(2), 10.0);

        let slice = vec![
            OtlpNumberDataPoint {
                value: Some(OtlpNumberDataPointValue::AsDouble(20.0)),
                start_time_unix_nano: start_ts,
                time_unix_nano: start_ts + nanos_from_seconds(2),
                ..Default::default()
            },
            OtlpNumberDataPoint {
                value: Some(OtlpNumberDataPointValue::AsDouble(40.0)),
                start_time_unix_nano: start_ts,
                time_unix_nano: start_ts + nanos_from_seconds(4),
                ..Default::default()
            },
        ];

        let events = run_monotonic(&mut translator, "metric.example", slice, &metrics);
        assert_eq!(events.len(), 1, "Expected one metric for drop equal test");

        let start_ts_s = start_ts / 1_000_000_000;
        // 40 - 10 (delta from the cached point).
        assert_eq!(
            events[0].try_as_metric().unwrap().values(),
            &MetricValues::counter((start_ts_s + 4, 30.0))
        );
    }

    // https://github.com/DataDog/datadog-agent/blob/main/pkg/opentelemetry-mapping-go/otlp/metrics/metrics_translator_test.go#L1848
    #[test]
    fn double_monotonic_diff_drops_older_timestamp_at_beginning_of_slice() {
        let metrics = Metrics::for_tests();
        let mut translator = OtlpMetricsTranslator::for_tests();
        let start_ts = translator.process_start_time_ns + 1;

        // Pre-populate the cache; the first slice point is older than the cached timestamp and is dropped.
        let dims = Dimensions {
            name: "metric.example".to_string(),
            ..Default::default()
        };
        translator
            .prev_pts
            .monotonic_diff(&dims, start_ts, start_ts + nanos_from_seconds(3), 10.0);

        let slice = vec![
            OtlpNumberDataPoint {
                value: Some(OtlpNumberDataPointValue::AsDouble(20.0)),
                start_time_unix_nano: start_ts,
                time_unix_nano: start_ts + nanos_from_seconds(2),
                ..Default::default()
            },
            OtlpNumberDataPoint {
                value: Some(OtlpNumberDataPointValue::AsDouble(40.0)),
                start_time_unix_nano: start_ts,
                time_unix_nano: start_ts + nanos_from_seconds(5),
                ..Default::default()
            },
        ];

        let events = run_monotonic(&mut translator, "metric.example", slice, &metrics);
        assert_eq!(events.len(), 1, "Expected one metric for drop older test");

        let start_ts_s = start_ts / 1_000_000_000;
        // 40 - 10 (delta from the cached point).
        assert_eq!(
            events[0].try_as_metric().unwrap().values(),
            &MetricValues::counter((start_ts_s + 5, 30.0))
        );
    }

    // https://github.com/DataDog/datadog-agent/blob/main/pkg/opentelemetry-mapping-go/otlp/metrics/metrics_translator_test.go#L1932
    #[test]
    fn double_monotonic_reports_first_value_from_new_series() {
        let metrics = Metrics::for_tests();
        let mut translator = OtlpMetricsTranslator::for_tests();
        let slice = build_test_cumulative_monotonic_double_points(&translator, &[10.0, 15.0, 20.0], false);
        let start_ts_s = slice[0].start_time_unix_nano / 1_000_000_000;
        let events = run_monotonic(&mut translator, "metric.example", slice, &metrics);
        assert_eq!(events.len(), 3);
        let metric1 = events[0].try_as_metric().unwrap();
        assert_eq!(metric1.values(), &MetricValues::counter((start_ts_s + 2, 10.0)));
        let metric2 = events[1].try_as_metric().unwrap();
        assert_eq!(metric2.values(), &MetricValues::counter((start_ts_s + 3, 5.0)));
        let metric3 = events[2].try_as_metric().unwrap();
        assert_eq!(metric3.values(), &MetricValues::counter((start_ts_s + 4, 5.0)));
    }

    // https://github.com/DataDog/datadog-agent/blob/main/pkg/opentelemetry-mapping-go/otlp/metrics/metrics_translator_test.go#L1948
    #[test]
    fn double_monotonic_rate_does_not_report_first_value() {
        let metrics = Metrics::for_tests();
        let mut translator = OtlpMetricsTranslator::for_tests();
        let slice = build_test_cumulative_monotonic_double_points(&translator, &[10.0, 15.0, 20.0], false);
        let start_ts_s = slice[0].start_time_unix_nano / 1_000_000_000;
        let events = run_monotonic(&mut translator, "kafka.net.bytes_out.rate", slice, &metrics);
        assert_eq!(events.len(), 2);
        let metric1 = events[0].try_as_metric().unwrap();
        assert_eq!(metric1.values(), &MetricValues::gauge((start_ts_s + 3, 5.0)));
        let metric2 = events[1].try_as_metric().unwrap();
        assert_eq!(metric2.values(), &MetricValues::gauge((start_ts_s + 4, 5.0)));
    }

    // https://github.com/DataDog/datadog-agent/blob/main/pkg/opentelemetry-mapping-go/otlp/metrics/metrics_translator_test.go#L1964
    #[test]
    fn double_monotonic_does_not_report_first_value_when_start_ts_matches_ts() {
        let metrics = Metrics::for_tests();
        let mut translator = OtlpMetricsTranslator::for_tests();
        let slice = build_test_cumulative_monotonic_double_points(&translator, &[10.0, 15.0, 20.0], true);
        let events = run_monotonic(&mut translator, "metric.example", slice, &metrics);
        assert!(events.is_empty());
    }

    // https://github.com/DataDog/datadog-agent/blob/main/pkg/opentelemetry-mapping-go/otlp/metrics/metrics_translator_test.go#L1994
    #[test]
    fn double_monotonic_reports_diff_for_first_value_against_cached_point() {
        let metrics = Metrics::for_tests();
        let mut translator = OtlpMetricsTranslator::for_tests();
        let dims = Dimensions {
            name: "metric.example".to_string(),
            ..Default::default()
        };
        let start_ts = translator.process_start_time_ns + 1;
        translator
            .prev_pts
            .monotonic_diff(&dims, start_ts, start_ts + nanos_from_seconds(1), 1.0);
        let slice = build_test_cumulative_monotonic_double_points(&translator, &[10.0, 15.0, 20.0], false);
        let start_ts_s = slice[0].start_time_unix_nano / 1_000_000_000;
        let events = run_monotonic(&mut translator, "metric.example", slice, &metrics);
        assert_eq!(events.len(), 3);
        let metric1 = events[0].try_as_metric().unwrap();
        assert_eq!(metric1.values(), &MetricValues::counter((start_ts_s + 2, 9.0)));
        let metric2 = events[1].try_as_metric().unwrap();
        assert_eq!(metric2.values(), &MetricValues::counter((start_ts_s + 3, 5.0)));
        let metric3 = events[2].try_as_metric().unwrap();
        assert_eq!(metric3.values(), &MetricValues::counter((start_ts_s + 4, 5.0)));
    }

    // https://github.com/DataDog/datadog-agent/blob/main/pkg/opentelemetry-mapping-go/otlp/metrics/metrics_translator_test.go#L2013
    #[test]
    fn double_monotonic_reports_rate_for_first_value_against_cached_point() {
        let metrics = Metrics::for_tests();
        let mut translator = OtlpMetricsTranslator::for_tests();
        let dims = Dimensions {
            name: "kafka.net.bytes_out.rate".to_string(),
            ..Default::default()
        };
        let start_ts = translator.process_start_time_ns + 1;
        translator
            .prev_pts
            .monotonic_diff(&dims, start_ts, start_ts + nanos_from_seconds(1), 1.0);
        let slice = build_test_cumulative_monotonic_double_points(&translator, &[10.0, 15.0, 20.0], false);
        let start_ts_s = slice[0].start_time_unix_nano / 1_000_000_000;
        let events = run_monotonic(&mut translator, "kafka.net.bytes_out.rate", slice, &metrics);
        assert_eq!(events.len(), 3);
        let metric1 = events[0].try_as_metric().unwrap();
        assert_eq!(metric1.values(), &MetricValues::gauge((start_ts_s + 2, 9.0)));
        let metric2 = events[1].try_as_metric().unwrap();
        assert_eq!(metric2.values(), &MetricValues::gauge((start_ts_s + 3, 5.0)));
        let metric3 = events[2].try_as_metric().unwrap();
        assert_eq!(metric3.values(), &MetricValues::gauge((start_ts_s + 4, 5.0)));
    }
}
