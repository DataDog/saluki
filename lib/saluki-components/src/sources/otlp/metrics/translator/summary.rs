//! Translation of OTLP summary data points.

use otlp_protos::opentelemetry::proto::metrics::v1::{DataPointFlags, SummaryDataPoint as OtlpSummaryDataPoint};
use saluki_core::data_model::event::Event;

use super::go_float::GoFloat;
use super::{is_skippable, DataType, Dimensions, OtlpMetricsTranslator, TranslationContext};

fn format_quantile_tag(quantile: f64) -> String {
    format!("quantile:{}", GoFloat(quantile))
}

impl OtlpMetricsTranslator {
    // Maps a monotonic OTLP Summary metric to Saluki 'Event's.
    pub(super) fn map_summary_metrics(
        &mut self, base_dims: Dimensions, data_points: Vec<OtlpSummaryDataPoint>, context: &TranslationContext,
    ) -> Vec<Event> {
        let mut events = Vec::new();
        for (i, dp) in data_points.iter().enumerate() {
            if dp.flags & (DataPointFlags::NoRecordedValueMask as u32) != 0 {
                continue;
            }

            let start_ts = dp.start_time_unix_nano;
            let ts = dp.time_unix_nano;
            let shadowing_resource_attributes = self
                .config
                .resource_attributes_as_tags
                .then_some(context.resource_attributes);
            let point_dims = base_dims.with_attribute_map(&dp.attributes, shadowing_resource_attributes);
            //Count will be treated as a cumulative monotonic metric
            {
                let count_dims = point_dims.with_suffix("count");
                let val = dp.count as f64;

                let (count_delta, is_first_point, should_drop_point) =
                    self.prev_pts.monotonic_diff(&count_dims, start_ts, ts, val);

                if !should_drop_point && !is_skippable(val) {
                    if !is_first_point {
                        self.record_metric_event(&count_dims, count_delta, ts, DataType::Count, &mut events, context);
                    } else if i == 0 && self.should_consume_initial_value(start_ts, ts) {
                        self.record_metric_event(&count_dims, val, ts, DataType::Count, &mut events, context);
                    }
                }
            }
            {
                let sum_dims = point_dims.with_suffix("sum");
                if !is_skippable(dp.sum) {
                    let (sum_delta, ok) = self.prev_pts.diff(&sum_dims, start_ts, ts, dp.sum);
                    if ok {
                        self.record_metric_event(&sum_dims, sum_delta, ts, DataType::Count, &mut events, context);
                    }
                } else {
                    self.translator_metrics.dropped_invalid_value().increment(1);
                }
            }

            if self.config.quantiles {
                let base_quantile_dims = point_dims.with_suffix("quantile");
                let quantiles = &dp.quantile_values;
                for quantile in quantiles {
                    if is_skippable(quantile.value) {
                        self.translator_metrics.dropped_invalid_value().increment(1);
                        continue;
                    }
                    let quantile_dims = base_quantile_dims.add_tags([format_quantile_tag(quantile.quantile)]);
                    self.record_metric_event(
                        &quantile_dims,
                        quantile.value,
                        ts,
                        DataType::Gauge,
                        &mut events,
                        context,
                    );
                }
            }
        }
        events
    }
}

#[cfg(test)]
mod tests {
    use otlp_protos::opentelemetry::proto::metrics::v1::summary_data_point::ValueAtQuantile;
    use saluki_context::tags::Tag;
    use saluki_core::data_model::event::metric::MetricValues;

    use super::super::tests::nanos_from_seconds;
    use super::*;
    use crate::sources::otlp::Metrics;

    fn run_summary(
        translator: &mut OtlpMetricsTranslator, name: &str, data_points: Vec<OtlpSummaryDataPoint>, metrics: &Metrics,
    ) -> Vec<Event> {
        let dims = Dimensions {
            name: name.to_string(),
            ..Default::default()
        };
        let context = TranslationContext {
            resource_attributes: &[],
            metrics,
        };
        translator.map_summary_metrics(dims, data_points, &context)
    }

    fn summary_dp(count: u64, sum: f64, ts_secs: u64, quantiles: &[(f64, f64)]) -> OtlpSummaryDataPoint {
        OtlpSummaryDataPoint {
            count,
            sum,
            time_unix_nano: nanos_from_seconds(ts_secs),
            quantile_values: quantiles
                .iter()
                .map(|&(quantile, value)| ValueAtQuantile { quantile, value })
                .collect(),
            ..Default::default()
        }
    }

    #[test]
    fn summary_count_and_sum_emit_counter_deltas() {
        let metrics = Metrics::for_tests();
        let mut translator = OtlpMetricsTranslator::for_tests();

        // Count is treated as a cumulative monotonic series and sum as a cumulative (non-monotonic)
        // diff, so the first point only primes the cache and the second yields the deltas.
        let data_points = vec![summary_dp(5, 100.0, 1, &[]), summary_dp(10, 250.0, 2, &[])];
        let events = run_summary(&mut translator, "otlp.summary", data_points, &metrics);

        assert_eq!(events.len(), 2);
        let count = events[0].try_as_metric().unwrap();
        assert_eq!(count.context().name(), "otlp.summary.count");
        assert_eq!(count.values(), &MetricValues::counter((2, 5.0))); // 10 - 5

        let sum = events[1].try_as_metric().unwrap();
        assert_eq!(sum.context().name(), "otlp.summary.sum");
        assert_eq!(sum.values(), &MetricValues::counter((2, 150.0))); // 250 - 100
    }

    #[test]
    fn summary_quantiles_emit_gauges_tagged_with_quantile() {
        let metrics = Metrics::for_tests();
        let mut translator = OtlpMetricsTranslator::for_tests();
        translator.config.quantiles = true;

        // A single data point produces no count/sum delta, leaving only the quantile gauges.
        let data_points = vec![summary_dp(3, 30.0, 1, &[(0.5, 1.0), (0.99, 9.0)])];
        let events = run_summary(&mut translator, "otlp.summary", data_points, &metrics);

        assert_eq!(events.len(), 2);
        let q50 = events[0].try_as_metric().unwrap();
        assert_eq!(q50.context().name(), "otlp.summary.quantile");
        assert_eq!(
            q50.context().tags().get_single_tag("quantile"),
            Some(&Tag::from("quantile:0.5"))
        );
        assert_eq!(q50.values(), &MetricValues::gauge((1, 1.0)));

        let q99 = events[1].try_as_metric().unwrap();
        assert_eq!(
            q99.context().tags().get_single_tag("quantile"),
            Some(&Tag::from("quantile:0.99"))
        );
        assert_eq!(q99.values(), &MetricValues::gauge((1, 9.0)));
    }

    #[test]
    fn summary_skips_no_recorded_value_points() {
        let metrics = Metrics::for_tests();
        let mut translator = OtlpMetricsTranslator::for_tests();
        translator.config.quantiles = true;

        let dp = OtlpSummaryDataPoint {
            count: 5,
            sum: 100.0,
            flags: DataPointFlags::NoRecordedValueMask as u32,
            time_unix_nano: nanos_from_seconds(1),
            quantile_values: vec![ValueAtQuantile {
                quantile: 0.5,
                value: 1.0,
            }],
            ..Default::default()
        };

        let events = run_summary(&mut translator, "otlp.summary", vec![dp], &metrics);
        assert!(events.is_empty());
    }
}
