#![allow(dead_code)]

use std::collections::HashSet;
use std::sync::LazyLock;
use std::time::{SystemTime, UNIX_EPOCH};

use otlp_protos::opentelemetry::proto::common::v1::KeyValue as OtlpKeyValue;
use otlp_protos::opentelemetry::proto::metrics::v1::{
    metric::Data as OtlpMetricData, AggregationTemporality, DataPointFlags,
    HistogramDataPoint as OtlpHistogramDataPoint, Metric as OtlpMetric, NumberDataPoint as OtlpNumberDataPoint,
    ResourceMetrics as OtlpResourceMetrics,
};
use saluki_context::tags::{SharedTagSet, TagSet};
use saluki_context::ContextResolver;
use saluki_core::data_model::event::metric::{Metric, MetricMetadata, MetricValues};
use saluki_core::data_model::event::Event;
use saluki_error::GenericError;
use tracing::warn;

use super::super::attributes::source::{Source, SourceKind};
use super::super::attributes::translator::AttributeTranslator;
use super::cache::Cache;
use super::config::{HistogramMode, NumberMode, OtlpTranslatorConfig};
use super::dimensions::Dimensions;
use super::internal::{instrumentationlibrary, instrumentationscope};
use super::remap;
use super::runtime_metrics::{RuntimeMetricMapping, RUNTIME_METRICS_MAPPINGS};
use crate::sources::otlp::metrics::config::InitialCumulMonoValueMode;
use crate::sources::otlp::Metrics;

// https://github.com/DataDog/datadog-agent/blob/main/pkg/opentelemetry-mapping-go/otlp/metrics/metrics_translator.go#L48-L63
static RATE_AS_GAUGE_METRICS: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    let mut m = HashSet::new();
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

#[derive(Clone, Copy, Debug, PartialEq)]
enum DataType {
    Gauge,
    Count,
}

/// A translator for converting OTLP metrics into Saluki `Event::Metric`s.
pub struct OtlpTranslator {
    config: OtlpTranslatorConfig,
    context_resolver: ContextResolver,
    prev_pts: Cache,
    process_start_time_ns: u64, // Used for initial value consumption.
    attribute_translator: AttributeTranslator,
}

#[derive(Debug, Default)]
struct HistogramInfo {
    sum: f64,
    count: u64,
    has_min_from_last_time_window: bool,
    has_max_from_last_time_window: bool,
    ok: bool,
}

impl OtlpTranslator {
    /// Creates a new, empty `OtlpTranslator`.
    pub fn new(config: OtlpTranslatorConfig, context_resolver: ContextResolver) -> Self {
        let process_start_time_ns = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("System time is before the UNIX epoch, this should not happen.")
            .as_nanos() as u64;

        Self {
            config,
            context_resolver,
            prev_pts: Cache::new(),
            process_start_time_ns,
            attribute_translator: AttributeTranslator::new(),
        }
    }

    /// Translates a batch of OTLP `ResourceMetrics` into a collection of Saluki `Event`s.
    /// This is the Rust equivalent of the Go `MapMetrics` function.
    pub fn map_metrics(
        &mut self, resource_metrics: OtlpResourceMetrics, metrics: &Metrics,
    ) -> Result<Vec<Event>, GenericError> {
        let mut events = Vec::new();
        let resource = resource_metrics.resource.unwrap_or_default();
        let source = self.attribute_translator.resource_to_source(&resource);

        let attribute_tags = self.attribute_translator.tags_from_attributes(&resource.attributes);

        // TODO: https://github.com/DataDog/datadog-agent/blob/main/pkg/opentelemetry-mapping-go/otlp/metrics/metrics_translator.go#L736-L753
        let host = if let Some(Source {
            kind: SourceKind::HostnameKind,
            identifier,
        }) = &source
        {
            Some(identifier.clone())
        } else {
            None
        };

        for scope_metrics in resource_metrics.scope_metrics {
            let tags = {
                let mut mutable_tags = TagSet::default();
                for tag in &attribute_tags {
                    mutable_tags.insert_tag(tag.clone());
                }

                if self.config.instrumentation_scope_metadata_as_tags {
                    if let Some(scope) = &scope_metrics.scope {
                        for tag in instrumentationscope::tags_from_instrumentation_scope_metadata(scope) {
                            mutable_tags.insert_tag(tag);
                        }
                    }
                } else if self.config.instrumentation_library_metadata_as_tags {
                    if let Some(scope) = &scope_metrics.scope {
                        for tag in instrumentationlibrary::tags_from_instrumentation_library_metadata(scope) {
                            mutable_tags.insert_tag(tag);
                        }
                    }
                }
                mutable_tags.into_shared()
            };

            let mut new_metrics: Vec<OtlpMetric> = Vec::new();
            for mut metric in scope_metrics.metrics {
                if let Some(mappings) = RUNTIME_METRICS_MAPPINGS.get(metric.name.as_str()) {
                    for mapping in mappings {
                        if mapping.attributes.is_empty() {
                            // If there are no attributes to match, just duplicate the metric with the new name.
                            let mut new_metric = metric.clone();
                            new_metric.name = mapping.mapped_name.to_string();
                            new_metrics.push(new_metric);
                            break;
                        }
                        if let Some(ref data) = metric.data {
                            match data {
                                OtlpMetricData::Sum(_) => {
                                    map_sum_runtime_metric_with_attributes(&metric, &mut new_metrics, mapping);
                                }
                                OtlpMetricData::Gauge(_) => {
                                    map_gauge_runtime_metric_with_attributes(&metric, &mut new_metrics, mapping);
                                }
                                OtlpMetricData::Histogram(_) => {
                                    map_histogram_runtime_metric_with_attributes(&metric, &mut new_metrics, mapping);
                                }
                                _ => {}
                            }
                        }
                    }
                }

                if self.config.with_remapping {
                    remap::remap_metrics(&mut new_metrics, &metric);
                }

                if self.config.with_otel_prefix {
                    remap::rename_metric(&mut metric);
                }

                let mut translated_events =
                    self.map_to_dd_format(metric, &tags, host.as_deref(), &resource.attributes, metrics);
                events.append(&mut translated_events);
            }

            for metric in new_metrics {
                let mut translated_events =
                    self.map_to_dd_format(metric, &tags, host.as_deref(), &resource.attributes, metrics);
                events.append(&mut translated_events);
            }
        }

        // TODO: Handle source
        // if let Some(source) = source {
        //     if let SourceKind::AwsEcsFargateKind = source.kind {
        //         let mut tag_set = TagSet::default();
        //         tag_set.insert_tag(source.tag());
        //         events.push(Event::new_with_kind(None, EventKind::Tags(tag_set.into_shared())));
        //     }
        // }

        Ok(events)
    }

    /// Translates a single OTLP `Metric` into a collection of Saluki `Event`s.
    fn map_to_dd_format(
        &mut self, metric: OtlpMetric, attribute_tags: &SharedTagSet, host: Option<&str>,
        resource_attributes: &[OtlpKeyValue], metrics: &Metrics,
    ) -> Vec<Event> {
        let origin_id = self.attribute_translator.origin_id_from_attributes(resource_attributes);
        let base_dims = Dimensions {
            name: metric.name,
            tags: attribute_tags.clone(),
            host: host.map(|h| h.to_string()),
            origin_id,
        };

        if let Some(data) = metric.data {
            match data {
                OtlpMetricData::Gauge(gauge) => {
                    self.map_number_metrics(base_dims, gauge.data_points, DataType::Gauge, metrics)
                }
                OtlpMetricData::Sum(sum) => match AggregationTemporality::try_from(sum.aggregation_temporality) {
                    Ok(AggregationTemporality::Cumulative) => {
                        if sum.is_monotonic {
                            match self.config.number_mode {
                                NumberMode::CumulativeToDelta => {
                                    self.map_number_monotonic_metrics(base_dims, sum.data_points, metrics)
                                }
                                NumberMode::RawValue => {
                                    self.map_number_metrics(base_dims, sum.data_points, DataType::Gauge, metrics)
                                }
                            }
                        } else {
                            // Cumulative non-monotonic sums are handled as gauges.
                            self.map_number_metrics(base_dims, sum.data_points, DataType::Gauge, metrics)
                        }
                    }
                    Ok(AggregationTemporality::Delta) => {
                        self.map_number_metrics(base_dims, sum.data_points, DataType::Count, metrics)
                    }
                    _ => {
                        warn!(
                            metric_name = base_dims.name,
                            temporality = sum.aggregation_temporality,
                            "Unsupported or unknown aggregation temporality for Sum metric."
                        );
                        Vec::new()
                    }
                },
                OtlpMetricData::Histogram(histogram) => {
                    match AggregationTemporality::try_from(histogram.aggregation_temporality) {
                        Ok(AggregationTemporality::Cumulative) => {
                            self.map_histogram_metrics(base_dims, histogram.data_points, false, metrics)
                        }
                        Ok(AggregationTemporality::Delta) => {
                            self.map_histogram_metrics(base_dims, histogram.data_points, true, metrics)
                        }
                        _ => {
                            warn!(
                                metric_name = base_dims.name,
                                temporality = histogram.aggregation_temporality,
                                "Unsupported or unknown aggregation temporality for Histogram metric."
                            );
                            Vec::new()
                        }
                    }
                }
                _ => {
                    // TODO: Handle Summary, etc.
                    Vec::new()
                }
            }
        } else {
            Vec::new()
        }
    }

    /// Centralized helper to create a metric event and push it to the events vector.
    /// TODO: how do we handle timestamp, hostname, origin?
    fn record_metric_event(
        &mut self, dims: &Dimensions, value: f64, timestamp_ns: u64, data_type: DataType, events: &mut Vec<Event>,
        metrics: &Metrics,
    ) {
        metrics.metrics_received().increment(1);

        // TODO: Handle origin
        match self.context_resolver.resolve(&dims.name, &dims.tags, None) {
            Some(context) => {
                let values = match data_type {
                    DataType::Gauge => MetricValues::gauge((timestamp_ns, value)),
                    DataType::Count => MetricValues::counter((timestamp_ns, value)),
                };

                let metric = Metric::from_parts(context, values, MetricMetadata::default());
                events.push(Event::Metric(metric));
            }
            None => {
                warn!("Failed to resolve context for metric: {}", dims.name);
            }
        }
    }

    /// Maps a slice of OTLP numeric data points to Saluki `Event`s.
    fn map_number_metrics(
        &mut self, base_dims: Dimensions, data_points: Vec<OtlpNumberDataPoint>, data_type: DataType, metrics: &Metrics,
    ) -> Vec<Event> {
        let mut events = Vec::new();
        for dp in data_points {
            // Skip if the data point has no recorded value.
            if dp.flags & (DataPointFlags::NoRecordedValueMask as u32) != 0 {
                continue;
            }

            let point_dims = base_dims.with_attribute_map(&dp.attributes);
            let value = get_number_data_point_value(&dp);
            if is_skippable(value) {
                warn!(
                    metric_name = point_dims.name,
                    value, "Skipping metric with unsupported value (NaN or Infinity)."
                );
                continue;
            }

            let ts = dp.time_unix_nano;

            self.record_metric_event(&point_dims, value, ts, data_type, &mut events, metrics);
        }
        events
    }

    /// Maps a slice of OTLP cumulative monotonic `Sum` data points to Saluki `Event`s.
    fn map_number_monotonic_metrics(
        &mut self, base_dims: Dimensions, data_points: Vec<OtlpNumberDataPoint>, metrics: &Metrics,
    ) -> Vec<Event> {
        let mut events = Vec::new();
        for (i, dp) in data_points.iter().enumerate() {
            // Skip if the data point has no recorded value.
            if dp.flags & (DataPointFlags::NoRecordedValueMask as u32) != 0 {
                continue;
            }

            let point_dims = base_dims.with_attribute_map(&dp.attributes);
            let value = get_number_data_point_value(dp);
            if is_skippable(value) {
                warn!(
                    metric_name = point_dims.name,
                    value, "Skipping metric with unsupported value (NaN or Infinity)."
                );
                continue;
            }

            if RATE_AS_GAUGE_METRICS.contains(point_dims.name.as_str()) {
                let (rate, is_first_point, should_drop_point) =
                    self.prev_pts
                        .monotonic_rate(&point_dims, dp.start_time_unix_nano, dp.time_unix_nano, value);

                if should_drop_point {
                    warn!(
                        metric_name = point_dims.name,
                        "Dropping cumulative monotonic data point (rate) due to reset or out-of-order timestamp."
                    );
                    continue;
                }

                if !is_first_point {
                    self.record_metric_event(
                        &point_dims,
                        rate,
                        dp.time_unix_nano,
                        DataType::Gauge,
                        &mut events,
                        metrics,
                    );
                }
                continue;
            }

            // Default behavior: calculate delta and consume as a Counter.
            let (delta, is_first_point, should_drop_point) =
                self.prev_pts
                    .monotonic_diff(&point_dims, dp.start_time_unix_nano, dp.time_unix_nano, value);

            if should_drop_point {
                // warn!(
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
                    metrics,
                );
            } else if i == 0 && self.should_consume_initial_value(dp.start_time_unix_nano, dp.time_unix_nano) {
                // We only compute the first point in the timeseries if it is the first value in the datapoint slice.
                self.record_metric_event(
                    &point_dims,
                    value,
                    dp.time_unix_nano,
                    DataType::Count,
                    &mut events,
                    metrics,
                );
            }
        }
        events
    }

    fn map_histogram_metrics(
        &mut self, base_dims: Dimensions, data_points: Vec<OtlpHistogramDataPoint>, delta: bool, metrics: &Metrics,
    ) -> Vec<Event> {
        let mut events = Vec::new();

        for dp in data_points {
            if dp.flags & (DataPointFlags::NoRecordedValueMask as u32) != 0 {
                continue;
            }

            let mut hist_info = HistogramInfo {
                ok: true,
                ..Default::default()
            };

            let point_dims = base_dims.with_attribute_map(&dp.attributes);

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
            if let Some(sum) = dp.sum {
                if !is_skippable(sum) {
                    if delta {
                        hist_info.sum = sum;
                    } else {
                        let (delta, ok) =
                            self.prev_pts
                                .diff(&sum_dims, dp.start_time_unix_nano, dp.time_unix_nano, sum);
                        if ok {
                            hist_info.sum = delta;
                        } else {
                            hist_info.ok = false;
                        }
                    }
                } else {
                    hist_info.ok = false;
                }
            } else {
                hist_info.ok = false;
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
                    metrics,
                );

                self.record_metric_event(&sum_dims, hist_info.sum, ts, DataType::Count, &mut events, metrics);

                if delta {
                    if let Some(min) = dp.min {
                        self.record_metric_event(&min_dims, min, ts, DataType::Gauge, &mut events, metrics);
                    }
                    if let Some(max) = dp.max {
                        self.record_metric_event(&max_dims, max, ts, DataType::Gauge, &mut events, metrics);
                    }
                }
            }

            // TODO: Implement bucket-to-sketch conversion.
            match self.config.hist_mode {
                HistogramMode::NoBuckets => {
                    // Do nothing for buckets.
                }
                HistogramMode::Counters => {
                    // TODO: Implement legacy bucket conversion as counters.
                }
                HistogramMode::Distributions => {
                    // TODO: Implement bucket-to-sketch conversion.
                }
            }
        }
        events
    }

    /// Determines if the initial value of a cumulative monotonic metric should be consumed.
    fn should_consume_initial_value(&self, start_ts: u64, ts: u64) -> bool {
        match self.config.initial_cumul_mono_value_mode {
            InitialCumulMonoValueMode::Auto => {
                // We report the first value if the timeseries started after the translator process started.
                self.process_start_time_ns < start_ts && start_ts != ts
            }
            InitialCumulMonoValueMode::Keep => true,
            InitialCumulMonoValueMode::Drop => false,
        }
    }
}

fn map_sum_runtime_metric_with_attributes(
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

fn map_gauge_runtime_metric_with_attributes(
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

fn map_histogram_runtime_metric_with_attributes(
    _metric: &OtlpMetric, _new_metrics: &mut [OtlpMetric], _mapping: &RuntimeMetricMapping,
) {
    // TODO: Implement attribute matching and metric duplication for histograms.
    // https://github.com/DataDog/datadog-agent/blob/main/pkg/opentelemetry-mapping-go/otlp/metrics/metrics_translator.go#L698-L727
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

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use metrics::Counter;
    use otlp_protos::opentelemetry::proto::metrics::v1::{
        number_data_point::Value as OtlpNumberDataPointValue, NumberDataPoint as OtlpNumberDataPoint,
    };
    use saluki_context::{tags::Tag, ContextResolverBuilder};

    use super::*;
    use crate::sources::otlp::metrics::dimensions::Dimensions;

    fn build_metrics() -> Metrics {
        Metrics {
            metrics_received: Counter::noop(),
        }
    }

    fn seconds_from_nanos(ns: u64) -> u64 {
        ns * 1_000_000_000
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
                time_unix_nano: seconds_from_nanos((i * 10) as u64),
                ..Default::default()
            });
        }
        slice
    }

    /// A helper function to build a series of cumulative monotonic integer data points that includes a reset.
    /// Mimics the `buildMonotonicIntRebootPoints` helper in the Go tests.
    fn build_monotonic_int_reboot_points() -> Vec<OtlpNumberDataPoint> {
        let values = [0, 30, 0, 20];
        let mut slice = Vec::with_capacity(values.len());

        for (i, val) in values.iter().enumerate() {
            slice.push(OtlpNumberDataPoint {
                value: Some(OtlpNumberDataPointValue::AsInt(*val)),
                time_unix_nano: seconds_from_nanos((i * 10) as u64),
                ..Default::default()
            });
        }
        slice
    }

    // https://github.com/DataDog/datadog-agent/blob/main/pkg/opentelemetry-mapping-go/otlp/metrics/metrics_translator_test.go#L296
    #[test]
    fn test_map_int_monotonic_metrics() {
        let metrics = build_metrics();
        let deltas = vec![1, 2, 200, 3, 7, 0];

        // Test Case 1: "diff" mode (standard cumulative to delta)
        {
            let context_resolver = ContextResolverBuilder::for_tests().build();
            let mut translator = OtlpTranslator::new(Default::default(), context_resolver);
            let slice = build_monotonic_int_points(&deltas);
            let dims = Dimensions {
                name: "metric.example".to_string(),
                ..Default::default()
            };

            let events = translator.map_number_monotonic_metrics(dims, slice, &metrics);

            assert_eq!(events.len(), deltas.len(), "Expected one event for each delta");

            for (i, event) in events.iter().enumerate() {
                let metric = event.try_as_metric().unwrap();
                assert_eq!(
                    metric.values(),
                    &MetricValues::counter((seconds_from_nanos(((i + 1) * 10) as u64), deltas[i] as f64))
                );
            }
        }

        // Test Case 2: "rate" mode
        {
            let context_resolver = ContextResolverBuilder::for_tests().build();
            let mut translator = OtlpTranslator::new(Default::default(), context_resolver);
            let slice = build_monotonic_int_points(&deltas);
            let dims = Dimensions {
                name: "kafka.net.bytes_out.rate".to_string(),
                ..Default::default()
            };

            let events = translator.map_number_monotonic_metrics(dims, slice, &metrics);

            assert_eq!(
                events.len(),
                deltas.len(),
                "Expected one event for each delta in rate mode"
            );

            for (i, event) in events.iter().enumerate() {
                let metric = event.try_as_metric().unwrap();
                // The rate is delta / 10s interval
                assert_eq!(
                    metric.values(),
                    &MetricValues::gauge((seconds_from_nanos(((i + 1) * 10) as u64), deltas[i] as f64 / 10.0),)
                );
            }
        }
    }

    // https://github.com/DataDog/datadog-agent/blob/main/pkg/opentelemetry-mapping-go/otlp/metrics/metrics_translator_test.go#L549
    #[test]
    fn test_map_int_monotonic_drop_point_within_slice() {
        let metrics = build_metrics();

        // Test Case 1: "equal" timestamp.
        // Verifies that a point is dropped if its timestamp is the same as the previous point.
        {
            let context_resolver = ContextResolverBuilder::for_tests().build();
            let mut translator = OtlpTranslator::new(Default::default(), context_resolver);
            let start_ts = translator.process_start_time_ns + 1;

            let slice = vec![
                // First point
                OtlpNumberDataPoint {
                    value: Some(OtlpNumberDataPointValue::AsInt(10)),
                    start_time_unix_nano: start_ts,
                    time_unix_nano: start_ts + seconds_from_nanos(2),
                    ..Default::default()
                },
                // Second point - DUPLICATE timestamp. This should be dropped.
                OtlpNumberDataPoint {
                    value: Some(OtlpNumberDataPointValue::AsInt(20)),
                    start_time_unix_nano: start_ts,
                    time_unix_nano: start_ts + seconds_from_nanos(2),
                    ..Default::default()
                },
                // Third point
                OtlpNumberDataPoint {
                    value: Some(OtlpNumberDataPointValue::AsInt(40)),
                    start_time_unix_nano: start_ts,
                    time_unix_nano: start_ts + seconds_from_nanos(4),
                    ..Default::default()
                },
            ];

            let dims = Dimensions {
                name: "metric.example".to_string(),
                ..Default::default()
            };
            let events = translator.map_number_monotonic_metrics(dims, slice, &metrics);

            assert_eq!(events.len(), 2, "Expected two metrics after dropping a point");

            // First metric: the initial value of the counter.
            let metric = events[0].try_as_metric().unwrap();
            assert_eq!(
                metric.values(),
                &MetricValues::counter((start_ts + seconds_from_nanos(2), 10.0))
            );

            // Second metric: the delta between the third and first points.
            let metric = events[1].try_as_metric().unwrap();
            assert_eq!(
                metric.values(),
                &MetricValues::counter((start_ts + seconds_from_nanos(4), 30.0))
            );
        }

        // Test Case 2: "equal-rate" timestamp.
        {
            let context_resolver = ContextResolverBuilder::for_tests().build();
            let mut translator = OtlpTranslator::new(Default::default(), context_resolver);
            let start_ts = translator.process_start_time_ns + 1;

            let slice = vec![
                // First point
                OtlpNumberDataPoint {
                    value: Some(OtlpNumberDataPointValue::AsInt(10)),
                    start_time_unix_nano: start_ts,
                    time_unix_nano: start_ts + seconds_from_nanos(2),
                    ..Default::default()
                },
                // Second point - DUPLICATE timestamp. This should be dropped.
                OtlpNumberDataPoint {
                    value: Some(OtlpNumberDataPointValue::AsInt(20)),
                    start_time_unix_nano: start_ts,
                    time_unix_nano: start_ts + seconds_from_nanos(2),
                    ..Default::default()
                },
                // Third point
                OtlpNumberDataPoint {
                    value: Some(OtlpNumberDataPointValue::AsInt(40)),
                    start_time_unix_nano: start_ts,
                    time_unix_nano: start_ts + seconds_from_nanos(4),
                    ..Default::default()
                },
            ];

            let dims = Dimensions {
                name: "kafka.net.bytes_out.rate".to_string(),
                ..Default::default()
            };
            let events = translator.map_number_monotonic_metrics(dims, slice, &metrics);

            // The first point is consumed but produces no metric for rates.
            // The second is dropped.
            // The third produces a rate based on the delta from the first.
            assert_eq!(events.len(), 1, "Expected one metric for equal-rate test");

            let metric = events[0].try_as_metric().unwrap();
            // rate is (40-10) / (4s-2s) = 30 / 2 = 15
            assert_eq!(
                metric.values(),
                &MetricValues::gauge((start_ts + seconds_from_nanos(4), 15.0))
            );
        }

        // Test Case 3: "older" timestamp.
        // Verifies that a point is dropped if its timestamp is older than the previous point.
        {
            let context_resolver = ContextResolverBuilder::for_tests().build();
            let mut translator = OtlpTranslator::new(Default::default(), context_resolver);
            let start_ts = translator.process_start_time_ns + 1;

            let slice = vec![
                // First point
                OtlpNumberDataPoint {
                    value: Some(OtlpNumberDataPointValue::AsInt(10)),
                    start_time_unix_nano: start_ts,
                    time_unix_nano: start_ts + seconds_from_nanos(3),
                    ..Default::default()
                },
                // Second point - OLDER timestamp. This should be dropped.
                OtlpNumberDataPoint {
                    value: Some(OtlpNumberDataPointValue::AsInt(25)),
                    start_time_unix_nano: start_ts,
                    time_unix_nano: start_ts + seconds_from_nanos(2),
                    ..Default::default()
                },
                // Third point
                OtlpNumberDataPoint {
                    value: Some(OtlpNumberDataPointValue::AsInt(40)),
                    start_time_unix_nano: start_ts,
                    time_unix_nano: start_ts + seconds_from_nanos(5),
                    ..Default::default()
                },
            ];

            let dims = Dimensions {
                name: "metric.example".to_string(),
                ..Default::default()
            };
            let events = translator.map_number_monotonic_metrics(dims, slice, &metrics);
            assert_eq!(events.len(), 2, "Expected two metrics after dropping an older point");

            let metric = events[0].try_as_metric().unwrap();
            assert_eq!(
                metric.values(),
                &MetricValues::counter((start_ts + seconds_from_nanos(3), 10.0))
            );

            let metric = events[1].try_as_metric().unwrap();
            assert_eq!(
                metric.values(),
                &MetricValues::counter((start_ts + seconds_from_nanos(5), 30.0))
            );
        }

        // Test Case 4: "older-rate" timestamp.
        {
            let context_resolver = ContextResolverBuilder::for_tests().build();
            let mut translator = OtlpTranslator::new(Default::default(), context_resolver);
            let start_ts = translator.process_start_time_ns + 1;

            let slice = vec![
                // First point
                OtlpNumberDataPoint {
                    value: Some(OtlpNumberDataPointValue::AsInt(10)),
                    start_time_unix_nano: start_ts,
                    time_unix_nano: start_ts + seconds_from_nanos(3),
                    ..Default::default()
                },
                // Second point - OLDER timestamp. This should be dropped.
                OtlpNumberDataPoint {
                    value: Some(OtlpNumberDataPointValue::AsInt(25)),
                    start_time_unix_nano: start_ts,
                    time_unix_nano: start_ts + seconds_from_nanos(2),
                    ..Default::default()
                },
                // Third point
                OtlpNumberDataPoint {
                    value: Some(OtlpNumberDataPointValue::AsInt(40)),
                    start_time_unix_nano: start_ts,
                    time_unix_nano: start_ts + seconds_from_nanos(5),
                    ..Default::default()
                },
            ];

            let dims = Dimensions {
                name: "kafka.net.bytes_out.rate".to_string(),
                ..Default::default()
            };
            let events = translator.map_number_monotonic_metrics(dims, slice, &metrics);
            assert_eq!(
                events.len(),
                1,
                "Expected one metric after dropping an older rate point"
            );

            let metric = events[0].try_as_metric().unwrap();
            // rate is (40-10) / (5s-3s) = 30 / 2 = 15
            assert_eq!(
                metric.values(),
                &MetricValues::gauge((start_ts + seconds_from_nanos(5), 15.0))
            );
        }
    }

    // https://github.com/DataDog/datadog-agent/blob/main/pkg/opentelemetry-mapping-go/otlp/metrics/metrics_translator_test.go#L1411
    #[test]
    fn test_map_int_monotonic_out_of_order() {
        let metrics = build_metrics();
        let context_resolver = ContextResolverBuilder::for_tests().build();
        let mut translator = OtlpTranslator::new(Default::default(), context_resolver);

        let timestamps = [1, 0, 2, 3];
        let values = [0, 1, 2, 3];

        let mut slice = Vec::with_capacity(values.len());
        for i in 0..values.len() {
            slice.push(OtlpNumberDataPoint {
                value: Some(OtlpNumberDataPointValue::AsInt(values[i])),
                time_unix_nano: seconds_from_nanos(timestamps[i]),
                ..Default::default()
            });
        }

        let dims = Dimensions {
            name: "metric.example".to_string(),
            ..Default::default()
        };
        let events = translator.map_number_monotonic_metrics(dims, slice, &metrics);

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
        assert_eq!(metric.values(), &MetricValues::counter((seconds_from_nanos(2), 2.0)));

        let metric = events[1].try_as_metric().unwrap();
        assert_eq!(metric.values(), &MetricValues::counter((seconds_from_nanos(3), 1.0)));
    }

    // https://github.com/DataDog/datadog-agent/blob/main/pkg/opentelemetry-mapping-go/otlp/metrics/metrics_translator_test.go#L332
    #[test]
    fn test_map_int_monotonic_different_dimensions() {
        let metrics = build_metrics();
        let context_resolver = ContextResolverBuilder::for_tests().build();
        let mut translator = OtlpTranslator::new(Default::default(), context_resolver);

        let mut slice = Vec::new();

        // Series with no tags
        slice.push(OtlpNumberDataPoint {
            time_unix_nano: seconds_from_nanos(0),
            ..Default::default()
        });
        slice.push(OtlpNumberDataPoint {
            value: Some(OtlpNumberDataPointValue::AsInt(20)),
            time_unix_nano: seconds_from_nanos(1),
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
            time_unix_nano: seconds_from_nanos(0),
            attributes: attributes_a.clone(),
            ..Default::default()
        });
        slice.push(OtlpNumberDataPoint {
            value: Some(OtlpNumberDataPointValue::AsInt(30)),
            time_unix_nano: seconds_from_nanos(1),
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
            time_unix_nano: seconds_from_nanos(0),
            attributes: attributes_b.clone(),
            ..Default::default()
        });
        slice.push(OtlpNumberDataPoint {
            value: Some(OtlpNumberDataPointValue::AsInt(40)),
            time_unix_nano: seconds_from_nanos(1),
            attributes: attributes_b,
            ..Default::default()
        });

        let dims = Dimensions {
            name: "metric.example".to_string(),
            ..Default::default()
        };
        let events = translator.map_number_monotonic_metrics(dims, slice, &metrics);
        assert_eq!(events.len(), 3, "Expected three distinct metrics");

        assert_eq!(
            events[0].try_as_metric().unwrap().values(),
            &MetricValues::counter((seconds_from_nanos(1), 20.0))
        );
        let metric2 = events[1].try_as_metric().unwrap();
        assert_eq!(
            metric2.context().tags().get_single_tag("key1"),
            Some(&Tag::from("key1:valA"))
        );
        assert_eq!(metric2.values(), &MetricValues::counter((seconds_from_nanos(1), 30.0)));
        let metric3 = events[2].try_as_metric().unwrap();
        assert_eq!(
            metric3.context().tags().get_single_tag("key1"),
            Some(&Tag::from("key1:valB"))
        );
        assert_eq!(metric3.values(), &MetricValues::counter((seconds_from_nanos(1), 40.0)));
    }

    // https://github.com/DataDog/datadog-agent/blob/main/pkg/opentelemetry-mapping-go/otlp/metrics/metrics_translator_test.go#L201
    #[test]
    fn test_map_number_metrics() {
        let metrics = build_metrics();
        // Setup test data that will be reused
        let ts = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos() as u64;

        let slice = vec![OtlpNumberDataPoint {
            value: Some(OtlpNumberDataPointValue::AsInt(17)),
            time_unix_nano: ts,
            ..Default::default()
        }];

        // Test Case 1: Gauge
        {
            let context_resolver = ContextResolverBuilder::for_tests().build();
            let mut translator = OtlpTranslator::new(Default::default(), context_resolver);
            let dims = Dimensions {
                name: "int64.test".to_string(),
                ..Default::default()
            };
            let events = translator.map_number_metrics(dims, slice.clone(), DataType::Gauge, &metrics);

            assert_eq!(events.len(), 1, "Expected one event for the gauge test");
            let metric = events[0].try_as_metric().unwrap();
            assert_eq!(metric.context().name(), "int64.test");
            assert_eq!(metric.values(), &MetricValues::gauge((ts, 17.0)));
            assert!(
                metric.context().tags().is_empty(),
                "Expected no tags for the simple gauge test"
            );
        }

        // Test Case 2: Count
        {
            let context_resolver = ContextResolverBuilder::for_tests().build();
            let mut translator = OtlpTranslator::new(Default::default(), context_resolver);
            let dims = Dimensions {
                name: "int64.delta.test".to_string(),
                ..Default::default()
            };
            let events = translator.map_number_metrics(dims, slice.clone(), DataType::Count, &metrics);

            assert_eq!(events.len(), 1, "Expected one event for the count test");
            let metric = events[0].try_as_metric().unwrap();
            assert_eq!(metric.context().name(), "int64.delta.test");
            assert_eq!(metric.values(), &MetricValues::counter((ts, 17.0)));
            assert!(
                metric.context().tags().is_empty(),
                "Expected no tags for the simple count test"
            );
        }

        // Test Case 3: Gauge with Tags
        {
            let context_resolver = ContextResolverBuilder::for_tests().build();
            let mut translator = OtlpTranslator::new(Default::default(), context_resolver);
            let mut tags = TagSet::default();
            tags.insert_tag("attribute_tag:attribute_value");
            let dims = Dimensions {
                name: "int64.test".to_string(),
                tags: tags.into_shared(),
                ..Default::default()
            };
            let events = translator.map_number_metrics(dims, slice.clone(), DataType::Gauge, &metrics);

            assert_eq!(events.len(), 1, "Expected one event for the gauge with tags test");
            let metric = events[0].try_as_metric().unwrap();
            assert_eq!(metric.context().name(), "int64.test");
            assert_eq!(metric.values(), &MetricValues::gauge((ts, 17.0)));
            assert_eq!(
                metric.context().tags().get_single_tag("attribute_tag"),
                Some(&Tag::from("attribute_tag:attribute_value"))
            );
        }
    }

    // https://github.com/DataDog/datadog-agent/blob/main/pkg/opentelemetry-mapping-go/otlp/metrics/metrics_translator_test.go#L395
    #[test]
    fn test_map_int_monotonic_with_reboot_within_slice() {
        let metrics = build_metrics();

        // Test Case 1: "diff" mode with reset
        {
            let context_resolver = ContextResolverBuilder::for_tests().build();
            let mut translator = OtlpTranslator::new(Default::default(), context_resolver);
            let slice = build_monotonic_int_reboot_points();
            let dims = Dimensions {
                name: "metric.example".to_string(),
                ..Default::default()
            };

            let events = translator.map_number_monotonic_metrics(dims, slice, &metrics);

            assert_eq!(events.len(), 2, "Expected two metrics after a reboot");

            let metric = events[0].try_as_metric().unwrap();
            assert_eq!(metric.values(), &MetricValues::counter((seconds_from_nanos(10), 30.0)));

            let metric = events[1].try_as_metric().unwrap();
            assert_eq!(metric.values(), &MetricValues::counter((seconds_from_nanos(30), 20.0)));
        }

        // Test Case 2: "rate" mode with reset
        {
            let context_resolver = ContextResolverBuilder::for_tests().build();
            let mut translator = OtlpTranslator::new(Default::default(), context_resolver);
            let slice = build_monotonic_int_reboot_points();
            let dims = Dimensions {
                name: "kafka.net.bytes_out.rate".to_string(),
                ..Default::default()
            };

            let events = translator.map_number_monotonic_metrics(dims, slice, &metrics);

            assert_eq!(events.len(), 2, "Expected two metrics for rate after a reboot");

            let metric = events[0].try_as_metric().unwrap();
            assert_eq!(
                metric.values(),
                &MetricValues::gauge((seconds_from_nanos(10), 3.0)) // 30 / 10s
            );

            let metric = events[1].try_as_metric().unwrap();
            assert_eq!(
                metric.values(),
                &MetricValues::gauge((seconds_from_nanos(30), 2.0)) // 20 / 10s
            );
        }
    }
}
