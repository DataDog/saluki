//! Translation of OTLP exponential histogram data points.

use agent_data_plane_config::domains::otlp::HistogramMode;
use otlp_protos::opentelemetry::proto::metrics::v1::ExponentialHistogramDataPoint as OtlpExponentialHistogramDataPoint;
use saluki_core::data_model::event::Event;
use tracing::debug;

use super::histogram::HistogramInfo;
use super::sketch::{convert_ddsketch_into_sketch, exponential_histogram_to_ddsketch};
use super::{infer_delta_interval, is_skippable, DataType, Dimensions, OtlpMetricsTranslator, TranslationContext};

impl OtlpMetricsTranslator {
    pub(super) fn map_exponential_histogram_metrics(
        &mut self, base_dims: Dimensions, data_points: Vec<OtlpExponentialHistogramDataPoint>, delta: bool,
        context: &TranslationContext,
    ) -> Vec<Event> {
        let mut events = Vec::new();
        for dp in data_points.iter() {
            let start_ts = dp.start_time_unix_nano;
            let ts = dp.time_unix_nano;
            let shadowing_resource_attributes = self
                .config
                .resource_attributes_as_tags
                .then_some(context.resource_attributes);
            let point_dims = base_dims.with_attribute_map(&dp.attributes, shadowing_resource_attributes);

            let mut hist_info = HistogramInfo {
                ok: true,
                ..Default::default()
            };

            let count_dims = point_dims.with_suffix("count");

            let count_val = dp.count as f64;

            if delta {
                hist_info.count = dp.count;
            } else {
                let (delta, ok) = self.prev_pts.diff(&count_dims, start_ts, ts, count_val);

                if ok {
                    hist_info.count = delta as u64;
                } else {
                    hist_info.ok = false;
                }
            }

            let sum_dims = point_dims.with_suffix("sum");

            let sum = dp.sum.unwrap_or(0.0);

            if !is_skippable(sum) {
                if delta {
                    hist_info.sum = sum;
                } else {
                    let (delta, ok) = self
                        .prev_pts
                        .diff(&sum_dims, dp.start_time_unix_nano, dp.time_unix_nano, sum);

                    if ok {
                        hist_info.sum = delta;
                    } else {
                        hist_info.ok = false;
                    }
                }
            } else {
                hist_info.ok = false;
                self.translator_metrics.dropped_invalid_value().increment(1);
            }

            let min_dims = point_dims.with_suffix("min");
            let max_dims = point_dims.with_suffix("max");

            if self.config.send_histogram_aggregations && hist_info.ok {
                self.record_metric_event(
                    &count_dims,
                    hist_info.count as f64,
                    ts,
                    DataType::Count,
                    &mut events,
                    context,
                );

                self.record_metric_event(&sum_dims, hist_info.sum, ts, DataType::Count, &mut events, context);

                if delta {
                    if let Some(min) = dp.min {
                        self.record_metric_event(&min_dims, min, ts, DataType::Gauge, &mut events, context);
                    }

                    if let Some(max) = dp.max {
                        self.record_metric_event(&max_dims, max, ts, DataType::Gauge, &mut events, context);
                    }
                }
            }

            if self.config.hist_mode == HistogramMode::NoBuckets {
                continue;
            }

            let exp_hist_dd_sketch = match exponential_histogram_to_ddsketch(dp, delta) {
                Ok(sketch) => sketch,
                Err(e) => {
                    debug!(
                        metric_name = base_dims.name,
                        error = %e,
                        "Failed to convert ExponentialHistogram into DDSketch"
                    );
                    self.translator_metrics.dropped_histogram_conversion().increment(1);
                    continue;
                }
            };

            let mut agent_sketch = match convert_ddsketch_into_sketch(exp_hist_dd_sketch) {
                Ok(sketch) => sketch,
                Err(e) => {
                    debug!(
                        metric_name = base_dims.name,
                        error = %e,
                        "Failed to convert DDSketch into agent sketch"
                    );
                    self.translator_metrics.dropped_histogram_conversion().increment(1);
                    continue;
                }
            };

            if hist_info.ok {
                agent_sketch.set_count(hist_info.count);
                agent_sketch.set_sum(hist_info.sum);
                agent_sketch.set_avg(if hist_info.count > 0 {
                    hist_info.sum / hist_info.count as f64
                } else {
                    0.0
                });

                if hist_info.count == 1 {
                    agent_sketch.set_min(hist_info.sum);
                    agent_sketch.set_max(hist_info.sum);
                }
            }

            if delta {
                if let Some(min) = dp.min {
                    agent_sketch.set_min(min);
                }
                if let Some(max) = dp.max {
                    agent_sketch.set_max(max);
                }
            }

            let mut interval: i64 = 0;
            if self.config.infer_delta_interval && delta {
                interval = infer_delta_interval(start_ts, ts);
            }

            self.record_sketch_event(&point_dims, agent_sketch, ts, &mut events, context, interval);
        }
        events
    }
}

#[cfg(test)]
mod tests {
    use otlp_protos::opentelemetry::proto::metrics::v1::{
        exponential_histogram_data_point::Buckets as OtlpExponentialHistogramBuckets, metric::Data as OtlpMetricData,
        AggregationTemporality, ExponentialHistogram as OtlpExponentialHistogram, Metric as OtlpMetric,
    };
    use saluki_core::data_model::event::metric::MetricValues;

    use super::super::tests::{distribution_sketch, nanos_from_seconds, resource_metrics_with_metric};
    use super::*;
    use crate::sources::otlp::Metrics;

    #[test]
    fn exponential_histogram_nobuckets_emits_aggregations_without_distribution() {
        let metrics = Metrics::for_tests();
        let context = TranslationContext {
            resource_attributes: &[],
            metrics: &metrics,
        };
        let dims = Dimensions {
            name: "exponential.histogram".to_string(),
            ..Default::default()
        };
        let mut translator = OtlpMetricsTranslator::for_tests();
        translator.config.hist_mode = HistogramMode::NoBuckets;
        translator.config.send_histogram_aggregations = true;

        let point = OtlpExponentialHistogramDataPoint {
            count: 2,
            sum: Some(3.0),
            positive: Some(OtlpExponentialHistogramBuckets {
                bucket_counts: vec![2],
                ..Default::default()
            }),
            min: Some(1.0),
            max: Some(2.0),
            time_unix_nano: nanos_from_seconds(1),
            ..Default::default()
        };

        let events = translator.map_exponential_histogram_metrics(dims, vec![point], true, &context);

        let metric_names: Vec<_> = events
            .iter()
            .map(|event| {
                event
                    .try_as_metric()
                    .expect("event should be a metric")
                    .context()
                    .name()
            })
            .collect();
        assert_eq!(
            metric_names,
            vec![
                "exponential.histogram.count",
                "exponential.histogram.sum",
                "exponential.histogram.min",
                "exponential.histogram.max",
            ]
        );
        assert!(events.iter().all(|event| {
            !matches!(
                event.try_as_metric().expect("event should be a metric").values(),
                MetricValues::Distribution(_)
            )
        }));
    }

    fn run_exponential_histogram(
        translator: &mut OtlpMetricsTranslator, name: &str, data_points: Vec<OtlpExponentialHistogramDataPoint>,
        delta: bool, metrics: &Metrics,
    ) -> Vec<Event> {
        let dims = Dimensions {
            name: name.to_string(),
            ..Default::default()
        };
        let context = TranslationContext {
            resource_attributes: &[],
            metrics,
        };
        translator.map_exponential_histogram_metrics(dims, data_points, delta, &context)
    }

    fn exp_histogram_dp(count: u64, sum: f64, min: Option<f64>, max: Option<f64>) -> OtlpExponentialHistogramDataPoint {
        OtlpExponentialHistogramDataPoint {
            count,
            sum: Some(sum),
            min,
            max,
            scale: 0,
            zero_count: 0,
            positive: Some(OtlpExponentialHistogramBuckets {
                offset: 0,
                bucket_counts: vec![1, 1, 1],
            }),
            negative: None,
            time_unix_nano: nanos_from_seconds(1),
            ..Default::default()
        }
    }

    #[test]
    fn exponential_histogram_delta_emits_single_sketch() {
        let metrics = Metrics::for_tests();
        let mut translator = OtlpMetricsTranslator::for_tests();

        let dp = exp_histogram_dp(3, 6.0, None, None);
        let events = run_exponential_histogram(&mut translator, "otlp.exphist", vec![dp], true, &metrics);

        assert_eq!(events.len(), 1, "delta exponential histogram emits a single sketch");
        let metric = events[0].try_as_metric().unwrap();
        assert_eq!(metric.context().name(), "otlp.exphist");
        assert_eq!(
            distribution_sketch(metric).count(),
            3,
            "sketch count should match the histogram count"
        );
    }

    #[test]
    fn exponential_histogram_aggregations_emit_count_sum_min_max_and_sketch() {
        let metrics = Metrics::for_tests();
        let mut translator = OtlpMetricsTranslator::for_tests();
        translator.config = translator.config.with_send_histogram_aggregations(true);

        let dp = exp_histogram_dp(3, 6.0, Some(0.5), Some(4.0));
        let events = run_exponential_histogram(&mut translator, "otlp.exphist", vec![dp], true, &metrics);

        // count, sum, min, max, then the sketch.
        assert_eq!(events.len(), 5);
        let count = events[0].try_as_metric().unwrap();
        assert_eq!(count.context().name(), "otlp.exphist.count");
        assert_eq!(count.values(), &MetricValues::counter((1, 3.0)));

        let sum = events[1].try_as_metric().unwrap();
        assert_eq!(sum.context().name(), "otlp.exphist.sum");
        assert_eq!(sum.values(), &MetricValues::counter((1, 6.0)));

        let min = events[2].try_as_metric().unwrap();
        assert_eq!(min.context().name(), "otlp.exphist.min");
        assert_eq!(min.values(), &MetricValues::gauge((1, 0.5)));

        let max = events[3].try_as_metric().unwrap();
        assert_eq!(max.context().name(), "otlp.exphist.max");
        assert_eq!(max.values(), &MetricValues::gauge((1, 4.0)));

        let sketch = events[4].try_as_metric().unwrap();
        assert_eq!(sketch.context().name(), "otlp.exphist");
        assert_eq!(distribution_sketch(sketch).count(), 3);
    }

    #[test]
    fn exponential_histogram_cumulative_is_dropped() {
        // Only delta temporality is supported; cumulative exponential histograms are dropped.
        let metrics = Metrics::for_tests();
        let mut translator = OtlpMetricsTranslator::for_tests();

        let dp = exp_histogram_dp(3, 6.0, None, None);
        let events = run_exponential_histogram(&mut translator, "otlp.exphist", vec![dp], false, &metrics);

        assert!(events.is_empty());
    }

    #[test]
    fn exponential_histogram_cumulative_temporality_is_dropped_via_translate_metrics() {
        let metrics = Metrics::for_tests();
        let mut translator = OtlpMetricsTranslator::for_tests();

        let metric = OtlpMetric {
            name: "otlp.exphist.cumulative".to_string(),
            data: Some(OtlpMetricData::ExponentialHistogram(OtlpExponentialHistogram {
                aggregation_temporality: AggregationTemporality::Cumulative as i32,
                data_points: vec![exp_histogram_dp(3, 6.0, None, None)],
            })),
            ..Default::default()
        };

        let (events_iter, _) = translator
            .translate_metrics(resource_metrics_with_metric(metric), &metrics)
            .expect("translation should succeed");
        let events = events_iter.collect::<Vec<_>>();

        assert!(
            events.is_empty(),
            "cumulative exponential histogram must be dropped with no events emitted"
        );
    }
}
