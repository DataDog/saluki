//! Translation of OTLP explicit-bucket histogram data points.

use agent_data_plane_config::domains::otlp::HistogramMode;
use ddsketch::{Bucket, DDSketch};
use otlp_protos::opentelemetry::proto::metrics::v1::{DataPointFlags, HistogramDataPoint as OtlpHistogramDataPoint};
use saluki_core::data_model::event::Event;
use saluki_error::{generic_error, GenericError};
use tracing::warn;

use super::go_float::GoFloat;
use super::{infer_delta_interval, is_skippable, DataType, Dimensions, OtlpMetricsTranslator, TranslationContext};

#[derive(Debug, Default)]
pub(super) struct HistogramInfo {
    pub(super) sum: f64,
    pub(super) count: u64,
    pub(super) has_min_from_last_time_window: bool,
    pub(super) has_max_from_last_time_window: bool,
    pub(super) ok: bool,
}

fn get_bounds(explicit_bounds: &[f64], idx: usize) -> (f64, f64) {
    let lower = if idx > 0 {
        explicit_bounds[idx - 1]
    } else {
        f64::NEG_INFINITY
    };
    let upper = if idx < explicit_bounds.len() {
        explicit_bounds[idx]
    } else {
        f64::INFINITY
    };
    (lower, upper)
}

fn validate_histogram_buckets(point_dims: &Dimensions, p: &OtlpHistogramDataPoint) -> Result<(), GenericError> {
    let bucket_count = p.bucket_counts.len();
    let bound_count = p.explicit_bounds.len();
    if bucket_count == 0 && bound_count != 0 {
        return Err(generic_error!(
            "Histogram '{}' has no bucket counts but {} explicit bounds; explicit bounds must be empty when bucket counts are empty.",
            point_dims.name,
            bound_count
        ));
    }
    if bucket_count > 0 && bucket_count != bound_count + 1 {
        return Err(generic_error!(
            "Histogram '{}' has {} bucket counts but {} explicit bounds; bucket count must equal bound count plus one.",
            point_dims.name,
            bucket_count,
            bound_count
        ));
    }
    Ok(())
}

impl OtlpMetricsTranslator {
    fn get_sketch_buckets(
        &mut self, context: &TranslationContext, point_dims: Dimensions, p: &OtlpHistogramDataPoint, delta: bool,
        events: &mut Vec<Event>, hist_info: HistogramInfo,
    ) -> Result<(), GenericError> {
        let start_ts = p.start_time_unix_nano;
        let ts = p.time_unix_nano;

        let mut qa = DDSketch::default();
        let mut bucket_counts = p.bucket_counts.clone();
        let mut explicit_bounds = p.explicit_bounds.clone();

        if bucket_counts.is_empty() && hist_info.ok {
            explicit_bounds.clear();

            if hist_info.has_min_from_last_time_window {
                bucket_counts.push(0);
                explicit_bounds.push(p.min.unwrap_or(0.0));
            }

            bucket_counts.push(hist_info.count);

            if hist_info.has_max_from_last_time_window {
                bucket_counts.push(0);
                explicit_bounds.push(p.max.unwrap_or(0.0));
            }
        }

        let (mut min_bound, mut max_bound) = (0.0, 0.0);
        let mut min_bound_set: bool = false;
        let mut buckets: Vec<Bucket> = Vec::new();
        for (j, &count) in bucket_counts.iter().enumerate() {
            let (lower_bound, upper_bound) = get_bounds(&explicit_bounds, j);
            let (original_lower_bound, original_upper_bound) = (lower_bound, upper_bound);

            let bucket_dims = point_dims.add_tags([
                format!("lower_bound:{}", GoFloat(lower_bound)),
                format!("upper_bound:{}", GoFloat(upper_bound)),
            ]);

            let (dx, ok) = self.prev_pts.diff(&bucket_dims, start_ts, ts, count as f64);

            let non_zero_bucket: bool;
            if delta {
                non_zero_bucket = count > 0u64;
                buckets.push(Bucket {
                    upper_limit: upper_bound,
                    count,
                });
            } else {
                non_zero_bucket = ok && dx > 0f64;
                if ok {
                    buckets.push(Bucket {
                        upper_limit: upper_bound,
                        count: dx as u64,
                    });
                }
            }

            if non_zero_bucket {
                if !min_bound_set {
                    min_bound = original_lower_bound;
                    min_bound_set = true;
                }
                max_bound = original_upper_bound
            }
        }
        qa.insert_interpolate_buckets(buckets)
            .map_err(|e| generic_error!("Failed to insert interpolated buckets: {}", e))?;

        if qa.is_empty() {
            return Ok(());
        }

        if hist_info.ok {
            qa.set_count(hist_info.count);

            if hist_info.count == 0 {
                self.record_sketch_event(&point_dims, qa, ts, events, context, 0);
                return Ok(());
            }

            qa.set_sum(hist_info.sum);
            qa.set_avg(hist_info.sum / hist_info.count as f64);
        }

        if min_bound_set {
            if !min_bound.is_infinite() {
                qa.set_min(min_bound);
            }
            if !max_bound.is_infinite() {
                qa.set_max(max_bound);
            }
        }

        if hist_info.has_min_from_last_time_window {
            qa.set_min(p.min.unwrap_or(0.0));
        } else if let Some(min) = p.min {
            qa.set_min(f64::max(min, qa.min().unwrap()));
        }

        if hist_info.has_max_from_last_time_window {
            qa.set_max(p.max.unwrap_or(0.0));
        } else if let Some(max) = p.max {
            qa.set_max(f64::min(max, qa.max().unwrap()));
        }

        let mut interval: i64 = 0;
        if self.config.infer_delta_interval && delta {
            interval = infer_delta_interval(start_ts, ts);
        }

        self.record_sketch_event(&point_dims, qa, ts, events, context, interval);

        Ok(())
    }

    fn get_legacy_buckets(
        &mut self, context: &TranslationContext, point_dims: Dimensions, p: OtlpHistogramDataPoint, delta: bool,
        events: &mut Vec<Event>,
    ) -> Result<(), GenericError> {
        let start_ts = p.start_time_unix_nano;
        let ts = p.time_unix_nano;

        let base_bucket_dims = &point_dims.with_suffix("bucket");
        for idx in 0..p.bucket_counts.len() {
            let (lower_bound, upper_bound) = get_bounds(&p.explicit_bounds, idx);

            let bucket_dims = base_bucket_dims.add_tags([
                format!("lower_bound:{}", GoFloat(lower_bound)),
                format!("upper_bound:{}", GoFloat(upper_bound)),
            ]);
            let count = p.bucket_counts[idx];
            let (dx, ok) = self.prev_pts.diff(&bucket_dims, start_ts, ts, count as f64);
            if delta {
                self.record_metric_event(&bucket_dims, count as f64, ts, DataType::Count, events, context);
            } else if ok {
                self.record_metric_event(&bucket_dims, dx, ts, DataType::Count, events, context);
            }
        }
        Ok(())
    }

    pub(super) fn map_histogram_metrics(
        &mut self, base_dims: Dimensions, data_points: Vec<OtlpHistogramDataPoint>, delta: bool,
        context: &TranslationContext,
    ) -> Vec<Event> {
        let mut events = Vec::new();

        for dp in data_points {
            if dp.flags & (DataPointFlags::NoRecordedValueMask as u32) != 0 {
                continue;
            }

            let shadowing_resource_attributes = self
                .config
                .resource_attributes_as_tags
                .then_some(context.resource_attributes);
            let point_dims = base_dims.with_attribute_map(&dp.attributes, shadowing_resource_attributes);

            // Validate before updating cumulative state.
            if let Err(e) = validate_histogram_buckets(&point_dims, &dp) {
                warn!(error = %e, "Failed to validate histogram buckets, dropping data point.");
                self.translator_metrics.dropped_histogram_conversion().increment(1);
                continue;
            }

            let mut hist_info = HistogramInfo {
                ok: true,
                ..Default::default()
            };

            let count_dims = point_dims.with_suffix("count");
            let sum_dims = point_dims.with_suffix("sum");
            let min_dims = point_dims.with_suffix("min");
            let max_dims = point_dims.with_suffix("max");

            // Handle the histogram's total count.
            let count_val = dp.count as f64;

            if delta {
                hist_info.count = dp.count;
            } else {
                let (delta, ok) =
                    self.prev_pts
                        .diff(&count_dims, dp.start_time_unix_nano, dp.time_unix_nano, count_val);

                if ok {
                    hist_info.count = delta as u64;
                } else {
                    hist_info.ok = false;
                }
            }

            // Handle the histogram's total sum.
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

            if let Some(min) = dp.min {
                hist_info.has_min_from_last_time_window = delta
                    || self
                        .prev_pts
                        .put_and_check_min(&min_dims, dp.start_time_unix_nano, dp.time_unix_nano, min);
            }

            if let Some(max) = dp.max {
                hist_info.has_max_from_last_time_window = delta
                    || self
                        .prev_pts
                        .put_and_check_max(&max_dims, dp.start_time_unix_nano, dp.time_unix_nano, max);
            }

            // Only proceed if both sum and count were processed correctly.
            if self.config.send_histogram_aggregations && hist_info.ok {
                let ts = dp.time_unix_nano;
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

            match self.config.hist_mode {
                HistogramMode::NoBuckets => {
                    continue;
                }
                HistogramMode::Counters => {
                    if let Err(e) = self.get_legacy_buckets(context, point_dims, dp, delta, &mut events) {
                        warn!(error = %e, "Failed to convert histogram buckets to counters, dropping data point.");
                        self.translator_metrics.dropped_histogram_conversion().increment(1);
                    }
                }
                HistogramMode::Distributions => {
                    if let Err(e) = self.get_sketch_buckets(context, point_dims, &dp, delta, &mut events, hist_info) {
                        warn!(error = %e, "Failed to convert histogram buckets to sketch, dropping data point.");
                        self.translator_metrics.dropped_histogram_conversion().increment(1);
                    }
                }
            }
        }
        events
    }
}

#[cfg(test)]
mod tests {
    use saluki_context::tags::Tag;
    use saluki_core::data_model::event::metric::MetricValues;

    use super::super::tests::{distribution_sketch, nanos_from_seconds};
    use super::*;
    use crate::sources::otlp::Metrics;

    // Mirrors the Go `mapHistogramMetrics` histogram-mode dispatch:
    // https://github.com/DataDog/datadog-agent/blob/main/pkg/opentelemetry-mapping-go/otlp/metrics/metrics_translator.go

    // Matches the Agent's rejection of malformed explicit histograms:
    // https://github.com/DataDog/datadog-agent/blob/087bbbe6d66864dbc8374ed2c66f71f3c1259c36/pkg/opentelemetry-mapping-go/otlp/metrics/default_mapper.go#L348-L352
    fn map_malformed_histogram(mode: HistogramMode, bucket_counts: Vec<u64>, explicit_bounds: Vec<f64>) -> Vec<Event> {
        let metrics = Metrics::for_tests();
        let mut translator = OtlpMetricsTranslator::for_tests();
        translator.config.hist_mode = mode;
        let context = TranslationContext {
            resource_attributes: &[],
            metrics: &metrics,
        };
        let dims = Dimensions {
            name: "malformed.histogram".to_string(),
            ..Default::default()
        };
        let point = OtlpHistogramDataPoint {
            count: 1,
            sum: Some(0.5),
            bucket_counts,
            explicit_bounds,
            time_unix_nano: nanos_from_seconds(1),
            ..Default::default()
        };

        translator.map_histogram_metrics(dims, vec![point], true, &context)
    }

    #[test]
    fn mismatched_bucket_and_bound_lengths_emit_no_distribution() {
        assert!(map_malformed_histogram(HistogramMode::Distributions, vec![1], vec![1.0, 2.0]).is_empty());
    }

    #[test]
    fn mismatched_bucket_and_bound_lengths_emit_no_bucket_counters() {
        assert!(map_malformed_histogram(HistogramMode::Counters, vec![1], vec![1.0, 2.0]).is_empty());
    }

    #[test]
    fn empty_bucket_counts_with_bounds_emit_no_distribution() {
        assert!(map_malformed_histogram(HistogramMode::Distributions, vec![], vec![1.0]).is_empty());
    }

    #[test]
    fn malformed_cumulative_histogram_does_not_update_aggregation_caches() {
        let metrics = Metrics::for_tests();
        let context = TranslationContext {
            resource_attributes: &[],
            metrics: &metrics,
        };
        let dims = Dimensions {
            name: "malformed.histogram".to_string(),
            ..Default::default()
        };
        let mut translator = OtlpMetricsTranslator::for_tests();
        translator.config.hist_mode = HistogramMode::Counters;
        translator.config.send_histogram_aggregations = true;

        let malformed = OtlpHistogramDataPoint {
            count: 5,
            sum: Some(8.0),
            bucket_counts: vec![1],
            explicit_bounds: vec![1.0, 2.0],
            start_time_unix_nano: nanos_from_seconds(1),
            time_unix_nano: nanos_from_seconds(2),
            ..Default::default()
        };
        let valid = OtlpHistogramDataPoint {
            count: 12,
            sum: Some(20.0),
            bucket_counts: vec![5, 7],
            explicit_bounds: vec![1.0],
            start_time_unix_nano: nanos_from_seconds(1),
            time_unix_nano: nanos_from_seconds(3),
            ..Default::default()
        };

        let events = translator.map_histogram_metrics(dims, vec![malformed, valid], false, &context);
        assert!(events.is_empty());
    }

    #[track_caller]
    fn assert_bucket_events(events: &[Event], values: &[f64], timestamp: u64) {
        let bounds = [("-inf", "1.0"), ("1.0", "inf")];
        assert_eq!(events.len(), values.len());
        for ((event, value), (lower, upper)) in events.iter().zip(values).zip(bounds) {
            let metric = event.try_as_metric().expect("event should be a metric");
            assert_eq!(metric.context().name(), "valid.histogram.bucket");
            assert_eq!(metric.values(), &MetricValues::counter((timestamp, *value)));
            assert_eq!(
                metric.context().tags().get_single_tag("lower_bound").unwrap().value(),
                Some(lower)
            );
            assert_eq!(
                metric.context().tags().get_single_tag("upper_bound").unwrap().value(),
                Some(upper)
            );
        }
    }

    #[test]
    fn valid_bucket_counters_preserve_bounds_and_cumulative_diffs() {
        let metrics = Metrics::for_tests();
        let context = TranslationContext {
            resource_attributes: &[],
            metrics: &metrics,
        };
        let dims = Dimensions {
            name: "valid.histogram".to_string(),
            ..Default::default()
        };

        let mut delta_translator = OtlpMetricsTranslator::for_tests();
        delta_translator.config.hist_mode = HistogramMode::Counters;
        let delta_point = OtlpHistogramDataPoint {
            count: 5,
            sum: Some(8.0),
            bucket_counts: vec![2, 3],
            explicit_bounds: vec![1.0],
            time_unix_nano: nanos_from_seconds(1),
            ..Default::default()
        };
        let delta_events = delta_translator.map_histogram_metrics(dims.clone(), vec![delta_point], true, &context);
        assert_bucket_events(&delta_events, &[2.0, 3.0], 1);

        let mut cumulative_translator = OtlpMetricsTranslator::for_tests();
        cumulative_translator.config.hist_mode = HistogramMode::Counters;
        let initial_point = OtlpHistogramDataPoint {
            count: 5,
            sum: Some(8.0),
            bucket_counts: vec![2, 3],
            explicit_bounds: vec![1.0],
            start_time_unix_nano: nanos_from_seconds(1),
            time_unix_nano: nanos_from_seconds(2),
            ..Default::default()
        };
        let next_point = OtlpHistogramDataPoint {
            count: 12,
            sum: Some(20.0),
            bucket_counts: vec![5, 7],
            explicit_bounds: vec![1.0],
            start_time_unix_nano: nanos_from_seconds(1),
            time_unix_nano: nanos_from_seconds(3),
            ..Default::default()
        };
        let initial_events =
            cumulative_translator.map_histogram_metrics(dims.clone(), vec![initial_point], false, &context);
        assert!(initial_events.is_empty());
        let cumulative_events = cumulative_translator.map_histogram_metrics(dims, vec![next_point], false, &context);
        assert_bucket_events(&cumulative_events, &[3.0, 4.0], 3);
    }

    #[test]
    fn cumulative_histogram_preserves_interpolated_summary_when_exact_count_is_zero() {
        let metrics = Metrics::for_tests();
        let mut translator = OtlpMetricsTranslator::for_tests();
        let dims = Dimensions {
            name: "metric.example".to_string(),
            ..Default::default()
        };
        let context = TranslationContext {
            resource_attributes: &[],
            metrics: &metrics,
        };
        let start_ts = translator.process_start_time_ns + 1;
        let mut events = Vec::new();

        let seed = OtlpHistogramDataPoint {
            start_time_unix_nano: start_ts,
            time_unix_nano: start_ts + nanos_from_seconds(1),
            count: 0,
            sum: Some(0.0),
            bucket_counts: vec![0, 0],
            explicit_bounds: vec![100.0],
            min: Some(0.0),
            max: Some(100.0),
            ..Default::default()
        };
        translator
            .get_sketch_buckets(
                &context,
                dims.clone(),
                &seed,
                false,
                &mut events,
                HistogramInfo::default(),
            )
            .expect("seeding cumulative bucket state should succeed");
        assert!(events.is_empty());

        let point = OtlpHistogramDataPoint {
            start_time_unix_nano: start_ts,
            time_unix_nano: start_ts + nanos_from_seconds(2),
            count: 0,
            sum: Some(0.0),
            bucket_counts: vec![0, 10],
            explicit_bounds: vec![100.0],
            min: Some(0.0),
            max: Some(100.0),
            ..Default::default()
        };
        translator
            .get_sketch_buckets(
                &context,
                dims,
                &point,
                false,
                &mut events,
                HistogramInfo {
                    ok: true,
                    count: 0,
                    sum: 0.0,
                    ..Default::default()
                },
            )
            .expect("translating cumulative bucket deltas should succeed");

        assert_eq!(events.len(), 1);
        let metric = events[0].try_as_metric().expect("event should be a metric");
        let MetricValues::Distribution(points) = metric.values() else {
            panic!("cumulative histogram should produce a distribution");
        };
        let (_, sketch) = points.into_iter().next().expect("distribution should contain a sketch");
        assert_eq!(sketch.count(), 0);
        assert!(!sketch.bins().is_empty());
        assert!(sketch.stored_min() > 0.0);
        assert!(sketch.stored_max() >= sketch.stored_min());
        assert!(sketch.stored_sum() > 0.0);
        assert!(sketch.stored_avg() > 0.0);
    }

    fn run_histogram(
        translator: &mut OtlpMetricsTranslator, name: &str, data_points: Vec<OtlpHistogramDataPoint>, delta: bool,
        metrics: &Metrics,
    ) -> Vec<Event> {
        let dims = Dimensions {
            name: name.to_string(),
            ..Default::default()
        };
        let context = TranslationContext {
            resource_attributes: &[],
            metrics,
        };
        translator.map_histogram_metrics(dims, data_points, delta, &context)
    }

    #[test]
    fn histogram_counters_mode_emits_one_counter_per_bucket() {
        let metrics = Metrics::for_tests();
        let mut translator = OtlpMetricsTranslator::for_tests();
        translator.config = translator.config.with_histogram_mode(HistogramMode::Counters);

        let dp = OtlpHistogramDataPoint {
            count: 6,
            sum: Some(10.0),
            bucket_counts: vec![1, 2, 3],
            explicit_bounds: vec![1.0, 2.0],
            time_unix_nano: nanos_from_seconds(1),
            ..Default::default()
        };

        let events = run_histogram(&mut translator, "otlp.histogram", vec![dp], true, &metrics);
        assert_eq!(events.len(), 3, "expected one counter per explicit bucket");

        // (-inf, 1.0], (1.0, 2.0], (2.0, +inf) with counts 1, 2, 3.
        let expected = [("-inf", "1.0", 1.0), ("1.0", "2.0", 2.0), ("2.0", "inf", 3.0)];
        for (event, (lower, upper, value)) in events.iter().zip(expected) {
            let metric = event.try_as_metric().unwrap();
            assert_eq!(metric.context().name(), "otlp.histogram.bucket");
            assert_eq!(
                metric.context().tags().get_single_tag("lower_bound"),
                Some(&Tag::from(format!("lower_bound:{lower}").as_str()))
            );
            assert_eq!(
                metric.context().tags().get_single_tag("upper_bound"),
                Some(&Tag::from(format!("upper_bound:{upper}").as_str()))
            );
            assert_eq!(metric.values(), &MetricValues::counter((1, value)));
        }
    }

    #[test]
    fn histogram_aggregations_emit_count_sum_min_max() {
        let metrics = Metrics::for_tests();
        let mut translator = OtlpMetricsTranslator::for_tests();
        translator.config = translator
            .config
            .with_histogram_mode(HistogramMode::NoBuckets)
            .with_send_histogram_aggregations(true);

        let dp = OtlpHistogramDataPoint {
            count: 6,
            sum: Some(12.0),
            min: Some(0.5),
            max: Some(9.0),
            bucket_counts: vec![1, 2, 3],
            explicit_bounds: vec![1.0, 2.0],
            time_unix_nano: nanos_from_seconds(1),
            ..Default::default()
        };

        // NoBuckets mode emits only the aggregations (count/sum as counters, min/max as gauges for delta).
        let events = run_histogram(&mut translator, "otlp.histogram", vec![dp], true, &metrics);
        assert_eq!(events.len(), 4, "expected count, sum, min, and max aggregations");

        let count = events[0].try_as_metric().unwrap();
        assert_eq!(count.context().name(), "otlp.histogram.count");
        assert_eq!(count.values(), &MetricValues::counter((1, 6.0)));

        let sum = events[1].try_as_metric().unwrap();
        assert_eq!(sum.context().name(), "otlp.histogram.sum");
        assert_eq!(sum.values(), &MetricValues::counter((1, 12.0)));

        let min = events[2].try_as_metric().unwrap();
        assert_eq!(min.context().name(), "otlp.histogram.min");
        assert_eq!(min.values(), &MetricValues::gauge((1, 0.5)));

        let max = events[3].try_as_metric().unwrap();
        assert_eq!(max.context().name(), "otlp.histogram.max");
        assert_eq!(max.values(), &MetricValues::gauge((1, 9.0)));
    }

    #[test]
    fn histogram_distributions_mode_emits_single_sketch() {
        let metrics = Metrics::for_tests();
        // `for_tests` defaults to `HistogramMode::Distributions`.
        let mut translator = OtlpMetricsTranslator::for_tests();

        let dp = OtlpHistogramDataPoint {
            count: 6,
            sum: Some(10.0),
            bucket_counts: vec![1, 2, 3],
            explicit_bounds: vec![1.0, 2.0],
            time_unix_nano: nanos_from_seconds(1),
            ..Default::default()
        };

        let events = run_histogram(&mut translator, "otlp.histogram", vec![dp], true, &metrics);
        assert_eq!(events.len(), 1, "distributions mode emits a single sketch");

        let metric = events[0].try_as_metric().unwrap();
        assert_eq!(metric.context().name(), "otlp.histogram");
        assert_eq!(
            distribution_sketch(metric).count(),
            6,
            "sketch count should match the histogram count"
        );
    }

    #[test]
    fn histogram_cumulative_first_point_emits_nothing() {
        // A cumulative histogram's first point has no previous value to diff against, so neither a
        // sketch nor aggregations can be produced.
        let metrics = Metrics::for_tests();
        let mut translator = OtlpMetricsTranslator::for_tests();

        let dp = OtlpHistogramDataPoint {
            count: 6,
            sum: Some(10.0),
            bucket_counts: vec![1, 2, 3],
            explicit_bounds: vec![1.0, 2.0],
            time_unix_nano: nanos_from_seconds(1),
            ..Default::default()
        };

        let events = run_histogram(&mut translator, "otlp.histogram", vec![dp], false, &metrics);
        assert!(events.is_empty());
    }
}
