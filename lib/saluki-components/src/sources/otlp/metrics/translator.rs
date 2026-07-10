#![allow(dead_code)]

use std::collections::BTreeMap;
use std::collections::HashSet;
use std::fmt;
use std::sync::LazyLock;
use std::time::{SystemTime, UNIX_EPOCH};
use std::vec::IntoIter;

use ::ddsketch::canonical::mapping::IndexMapping;
use agent_data_plane_config::domains::otlp::{CumulativeMonotonicMode, HistogramMode, InitialCumulativeMonotonicValue};
use datadog_protos::metrics::Dogsketch;
use ddsketch::canonical::mapping::LogarithmicMapping;
use ddsketch::canonical::store::DenseStore;
use ddsketch::canonical::DDSketch as CanonicalDDSketch;
use ddsketch::canonical::Store as DdStore;
use ddsketch::{Bucket, DDSketch};
use otlp_protos::opentelemetry::proto::common::v1::KeyValue as OtlpKeyValue;
use otlp_protos::opentelemetry::proto::metrics::v1::{
    exponential_histogram_data_point::Buckets as OtlpExponentialHistogramBuckets, metric::Data as OtlpMetricData,
    AggregationTemporality, DataPointFlags, ExponentialHistogramDataPoint as OtlpExponentialHistogramDataPoint,
    HistogramDataPoint as OtlpHistogramDataPoint, Metric as OtlpMetric, NumberDataPoint as OtlpNumberDataPoint,
    ResourceMetrics as OtlpResourceMetrics, SummaryDataPoint as OtlpSummaryDataPoint,
};
use saluki_context::tags::{SharedTagSet, TagSet};
use saluki_context::{ContextResolver, ContextResolverBuilder};
use saluki_core::data_model::event::metric::{Metric, MetricMetadata, MetricValues};
use saluki_core::data_model::event::Event;
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use stringtheory::MetaString;
use tracing::{debug, trace, warn};

use super::cache::PointsCache;
use super::config::OtlpMetricsTranslatorConfig;
use super::dimensions::Dimensions;
use super::internal::{instrumentationlibrary, instrumentationscope};
use super::remap;
use super::runtime_metrics::{RuntimeMetricMapping, RUNTIME_METRICS_MAPPINGS};
use crate::common::otlp::attributes::translator::AttributeTranslator;
use crate::common::otlp::attributes::{raw_origin_from_attributes, ResourceAttributeTagMode};
use crate::common::otlp::util::{Source, SourceKind};
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

struct TranslationContext<'a> {
    resource_attributes: &'a [OtlpKeyValue],
    metrics: &'a Metrics,
}

/// A translator for converting OTLP metrics into Saluki `Event::Metric`s.
pub struct OtlpMetricsTranslator {
    config: OtlpMetricsTranslatorConfig,
    default_hostname: MetaString,
    context_resolver: ContextResolver,
    prev_pts: PointsCache,
    process_start_time_ns: u64, // Used for initial value consumption.
    attribute_translator: AttributeTranslator,
    // Configured tags (`otlp_config.metrics.tags`) added to every emitted metric.
    metric_tags: SharedTagSet,
}

#[derive(Debug, Default)]
struct HistogramInfo {
    sum: f64,
    count: u64,
    has_min_from_last_time_window: bool,
    has_max_from_last_time_window: bool,
    ok: bool,
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

fn infer_delta_interval(start_ts: u64, ts: u64) -> i64 {
    if start_ts == 0 || start_ts > ts {
        return 0;
    }
    // Convert OTLP nanosecond timestamps to seconds because Datadog interval inference operates on whole-second intervals.
    let delta = (ts - start_ts) as f64 / 1e9;
    let rounded_delta = f64::round(delta);

    if f64::abs(rounded_delta - delta) < 0.05 {
        return rounded_delta as i64;
    }
    0
}

/// Formats a float using Go semantics directly into the caller's output buffer.
struct GoFloat(f64);

impl fmt::Display for GoFloat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let v = self.0;
        if v == f64::INFINITY {
            return f.write_str("inf");
        } else if v == f64::NEG_INFINITY {
            return f.write_str("-inf");
        } else if v.is_nan() {
            return f.write_str("nan");
        } else if v == 0.0 {
            return f.write_str("0");
        }

        // Mirror Go's strconv.FormatFloat(f, 'g', -1, 64): switch to scientific
        // notation below 1e-4 and at/above 1e6, with a zero-padded two-digit
        // signed exponent.
        if v.abs() < 1e-4 || v.abs() >= 1e6 {
            let s = format!("{v:e}");
            let (mantissa, exp_part) = s.split_once('e').expect("{:e} always contains 'e'");
            let (sign, digits) = if let Some(d) = exp_part.strip_prefix('-') {
                ("-", d)
            } else {
                ("+", exp_part.strip_prefix('+').unwrap_or(exp_part))
            };
            if digits.len() < 2 {
                write!(f, "{mantissa}e{sign}0{digits}")?;
            } else {
                write!(f, "{mantissa}e{sign}{digits}")?;
            }
            if v == v.floor() {
                f.write_str(".0")?;
            }
            return Ok(());
        }

        write!(f, "{v}")?;
        if v == v.floor() {
            f.write_str(".0")?;
        }
        Ok(())
    }
}

fn format_quantile_tag(quantile: f64) -> String {
    format!("quantile:{}", GoFloat(quantile))
}

fn to_store(b: Option<&OtlpExponentialHistogramBuckets>) -> DenseStore {
    let mut store = DenseStore::new();
    if let Some(buckets) = b {
        let offset = buckets.offset;
        let bucket_counts = &buckets.bucket_counts;

        for (j, &count) in bucket_counts.iter().enumerate() {
            let index = offset + j as i32;
            store.add(index, count);
        }
    }
    store
}

fn exponential_histogram_to_ddsketch(
    dp: &OtlpExponentialHistogramDataPoint, delta: bool,
) -> Result<CanonicalDDSketch<LogarithmicMapping, DenseStore>, GenericError> {
    if !delta {
        return Err(generic_error!("cumulative exponential histograms are not supported"));
    }

    let positive_store = to_store(dp.positive.as_ref());
    let negative_store = to_store(dp.negative.as_ref());

    let gamma = 2_f64.powf(2_f64.powi(-dp.scale));
    let mapping = LogarithmicMapping::new_with_gamma(gamma)
        .map_err(|e| generic_error!("Failed to create index mapping for DDSketch: {}", e))?;

    let mut sketch = CanonicalDDSketch::new(mapping, positive_store, negative_store);
    sketch.add_n(0.0, dp.zero_count);

    Ok(sketch)
}

fn remap_store_bins_to_agent_space(
    source_mapping: &LogarithmicMapping, target_mapping: &LogarithmicMapping, store: &DenseStore, sign: i32,
    remapped_counts: &mut BTreeMap<i32, f64>, zeroes: &mut f64,
) -> Result<(), GenericError> {
    const MAX_AGENT_INDEX: i32 = i16::MAX as i32;

    for (index, count) in store.iter_non_zero_bins() {
        let count = count as f64;
        if count <= 0.0 {
            return Err(generic_error!("negative counts are not supported: got {}", count));
        }

        let input_lower_bound = source_mapping.lower_bound(index);
        let input_upper_bound = source_mapping.lower_bound(index + 1);
        let input_size = input_upper_bound - input_lower_bound;
        let mut output_index = target_mapping.index(input_lower_bound);

        while target_mapping.lower_bound(output_index) < input_upper_bound {
            if output_index >= MAX_AGENT_INDEX {
                return Err(generic_error!(
                    "index value {} exceeds the maximum supported index value ({})",
                    output_index,
                    MAX_AGENT_INDEX
                ));
            }

            let output_lower_bound = target_mapping.lower_bound(output_index);
            let output_upper_bound = target_mapping.lower_bound(output_index + 1);
            let lower_intersection_bound = output_lower_bound.max(input_lower_bound);
            let upper_intersection_bound = output_upper_bound.min(input_upper_bound);
            let intersection_size = upper_intersection_bound - lower_intersection_bound;

            if intersection_size > 0.0 {
                let remapped_count = count * (intersection_size / input_size);
                if output_index <= 0 {
                    *zeroes += remapped_count;
                } else {
                    *remapped_counts.entry(sign * output_index).or_default() += remapped_count;
                }
            }

            output_index += 1;
        }
    }

    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct RemappedSketchSummary {
    min: f64,
    max: f64,
    sum: f64,
}

fn sketch_value_for_key(target_mapping: &LogarithmicMapping, key: i32) -> f64 {
    if key == 0 {
        0.0
    } else if key < 0 {
        -target_mapping.value(-key)
    } else {
        target_mapping.value(key)
    }
}

fn summarize_remapped_counts(
    float_key_counts: &BTreeMap<i32, f64>, target_mapping: &LogarithmicMapping,
) -> Option<RemappedSketchSummary> {
    let mut summary: Option<RemappedSketchSummary> = None;

    for (&key, &count) in float_key_counts {
        if count <= 0.0 {
            continue;
        }

        let value = sketch_value_for_key(target_mapping, key);

        match &mut summary {
            Some(summary) => {
                summary.max = value;
                summary.sum += value * count;
            }
            None => {
                summary = Some(RemappedSketchSummary {
                    min: value,
                    max: value,
                    sum: value * count,
                });
            }
        }
    }

    summary
}

fn convert_float_counts_to_int_counts(
    float_key_counts: BTreeMap<i32, f64>,
) -> Result<(Vec<(i16, u64)>, u64), GenericError> {
    let mut key_counts = Vec::new();
    let mut float_total = 0.0;
    let mut int_total = 0_u64;

    for (key, count) in float_key_counts {
        let key = i16::try_from(key).map_err(|_| generic_error!("bin key {} overflows i16", key))?;
        float_total += count;

        let rounded_total = float_total.round() as u64;
        let rounded = rounded_total.saturating_sub(int_total);
        int_total += rounded;

        if rounded > 0 {
            key_counts.push((key, rounded));
        }
    }

    Ok((key_counts, int_total))
}

fn build_agent_sketch_from_key_counts(
    key_counts: &[(i16, u64)], total_count: u64, summary: Option<RemappedSketchSummary>,
) -> Result<DDSketch, GenericError> {
    let mut dogsketch = Dogsketch::new();
    dogsketch.set_cnt(i64::try_from(total_count).map_err(|_| generic_error!("sketch count overflows i64"))?);

    if total_count == 0 {
        return DDSketch::try_from(dogsketch).map_err(|e| generic_error!("failed to build sketch: {}", e));
    }

    let mut k = Vec::new();
    let mut n = Vec::new();

    for &(key, count) in key_counts {
        let mut remaining = count;
        while remaining > u64::from(u16::MAX) {
            k.push(i32::from(key));
            n.push(u32::from(u16::MAX));
            remaining -= u64::from(u16::MAX);
        }

        if remaining > 0 {
            k.push(i32::from(key));
            n.push(remaining as u32);
        }
    }

    let summary = summary.ok_or_else(|| generic_error!("missing sketch summary for non-empty sketch"))?;

    dogsketch.set_min(summary.min);
    dogsketch.set_max(summary.max);
    dogsketch.set_sum(summary.sum);
    dogsketch.set_avg(summary.sum / total_count as f64);
    dogsketch.set_k(k);
    dogsketch.set_n(n);

    DDSketch::try_from(dogsketch).map_err(|e| generic_error!("failed to build sketch: {}", e))
}

fn convert_ddsketch_into_sketch(
    canonical: CanonicalDDSketch<LogarithmicMapping, DenseStore>,
) -> Result<DDSketch, GenericError> {
    let target_mapping = DDSketch::remap_mapping();
    let source_mapping = canonical.mapping();
    let mut remapped_counts = BTreeMap::new();
    let mut zeroes = canonical.zero_count() as f64;

    remap_store_bins_to_agent_space(
        source_mapping,
        &target_mapping,
        canonical.positive_store(),
        1,
        &mut remapped_counts,
        &mut zeroes,
    )?;
    remap_store_bins_to_agent_space(
        source_mapping,
        &target_mapping,
        canonical.negative_store(),
        -1,
        &mut remapped_counts,
        &mut zeroes,
    )?;

    if zeroes != 0.0 {
        *remapped_counts.entry(0).or_default() += zeroes;
    }

    let summary = summarize_remapped_counts(&remapped_counts, &target_mapping);
    let (key_counts, total_count) = convert_float_counts_to_int_counts(remapped_counts)?;
    build_agent_sketch_from_key_counts(&key_counts, total_count, summary)
}

impl OtlpMetricsTranslator {
    /// Creates an `OtlpMetricsTranslator` from the given configuration, context resolver, and
    /// configured metric tags.
    pub fn new(
        config: OtlpMetricsTranslatorConfig, default_hostname: MetaString, context_resolver: ContextResolver,
        metric_tags: SharedTagSet,
    ) -> Result<Self, GenericError> {
        config
            .validate()
            .error_context("Failed to validate OTLP metrics translator configuration.")?;

        let process_start_time_ns = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos() as u64;

        Ok(Self {
            config,
            default_hostname,
            context_resolver,
            prev_pts: PointsCache::from_config(config),
            process_start_time_ns,
            attribute_translator: AttributeTranslator::new(),
            metric_tags,
        })
    }

    /// Translates a batch of OTLP `ResourceMetrics` into Saluki `Event`s.
    /// This is the Rust equivalent of the Go `MapMetrics` function.
    pub fn translate_metrics(
        &mut self, resource_metrics: OtlpResourceMetrics, metrics: &Metrics,
    ) -> Result<IntoIter<Event>, GenericError> {
        let mut events = Vec::new();
        let resource = resource_metrics.resource.unwrap_or_default();
        let source = self.attribute_translator.resource_to_source(&resource);

        let resource_tag_mode = if self.config.resource_attributes_as_tags {
            ResourceAttributeTagMode::All
        } else {
            ResourceAttributeTagMode::Mapped
        };
        let resource_attribute_tags = self
            .attribute_translator
            .tags_from_attributes(&resource.attributes, resource_tag_mode)
            .into_shared();

        // Combine configured and resource-derived tags once per resource, then reuse them for every
        // instrumentation scope.
        let mut resource_tags = self.metric_tags.clone();
        resource_tags.extend_from_shared(&resource_attribute_tags);

        // TODO: https://github.com/DataDog/datadog-agent/blob/main/pkg/opentelemetry-mapping-go/otlp/metrics/metrics_translator.go#L736-L753
        let host = match source {
            Some(Source {
                kind: SourceKind::HostnameKind,
                identifier,
            }) => MetaString::from(identifier),
            _ => self.default_hostname.clone(),
        };

        for scope_metrics in resource_metrics.scope_metrics {
            let scope_tags = {
                let mut tags = TagSet::default();

                if self.config.instrumentation_scope_metadata_as_tags {
                    // Always add instrumentation scope tags, even if scope is `None`
                    // to match the datadog agent's behavior which adds "n/a" values
                    let scope_tags = match &scope_metrics.scope {
                        Some(scope) => instrumentationscope::tags_from_instrumentation_scope_metadata(scope),
                        None => instrumentationscope::tags_from_empty_instrumentation_scope(),
                    };
                    for tag in scope_tags {
                        tags.insert_tag(tag);
                    }
                } else if self.config.instrumentation_library_metadata_as_tags {
                    if let Some(scope) = &scope_metrics.scope {
                        for tag in instrumentationlibrary::tags_from_instrumentation_library_metadata(scope) {
                            tags.insert_tag(tag);
                        }
                    }
                }

                tags.into_shared()
            };

            let mut tags = resource_tags.clone();
            tags.extend_from_shared(&scope_tags);

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
                    self.map_to_dd_format(metric, &tags, Some(host.clone()), &resource.attributes, metrics);
                events.append(&mut translated_events);
            }

            for metric in new_metrics {
                let mut translated_events =
                    self.map_to_dd_format(metric, &tags, Some(host.clone()), &resource.attributes, metrics);
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

        metrics.metrics_received().increment(events.len() as u64);

        Ok(events.into_iter())
    }

    /// Creates a new `OtlpMetricsTranslator` for tests.
    pub fn for_tests() -> OtlpMetricsTranslator {
        let process_start_time_ns = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("System time is before the UNIX epoch, this should not happen.")
            .as_nanos() as u64;

        OtlpMetricsTranslator {
            config: Default::default(),
            default_hostname: MetaString::from_static("default-host"),
            context_resolver: ContextResolverBuilder::for_tests().build(),
            prev_pts: PointsCache::for_tests(),
            process_start_time_ns,
            attribute_translator: AttributeTranslator::new(),
            metric_tags: SharedTagSet::default(),
        }
    }

    /// Translates a single OTLP `Metric` into a collection of Saluki `Event`s.
    fn map_to_dd_format(
        &mut self, metric: OtlpMetric, attribute_tags: &SharedTagSet, host: Option<MetaString>,
        resource_attributes: &[OtlpKeyValue], metrics: &Metrics,
    ) -> Vec<Event> {
        let origin_id = self.attribute_translator.origin_id_from_attributes(resource_attributes);
        let base_dims = Dimensions {
            name: metric.name,
            tags: attribute_tags.clone(),
            host,
            origin_id,
        };

        let context = TranslationContext {
            resource_attributes,
            metrics,
        };

        if let Some(data) = metric.data {
            match data {
                OtlpMetricData::Gauge(gauge) => {
                    self.map_number_metrics(base_dims, gauge.data_points, DataType::Gauge, &context)
                }
                OtlpMetricData::Sum(sum) => match AggregationTemporality::try_from(sum.aggregation_temporality) {
                    Ok(AggregationTemporality::Cumulative) => {
                        if sum.is_monotonic {
                            match self.config.cumulative_monotonic_mode {
                                CumulativeMonotonicMode::ToDelta => {
                                    self.map_number_monotonic_metrics(base_dims, sum.data_points, &context)
                                }
                                CumulativeMonotonicMode::RawValue => {
                                    self.map_number_metrics(base_dims, sum.data_points, DataType::Gauge, &context)
                                }
                            }
                        } else {
                            // Cumulative non-monotonic sums are handled as gauges.
                            self.map_number_metrics(base_dims, sum.data_points, DataType::Gauge, &context)
                        }
                    }
                    Ok(AggregationTemporality::Delta) => {
                        self.map_number_metrics(base_dims, sum.data_points, DataType::Count, &context)
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
                            self.map_histogram_metrics(base_dims, histogram.data_points, false, &context)
                        }
                        Ok(AggregationTemporality::Delta) => {
                            self.map_histogram_metrics(base_dims, histogram.data_points, true, &context)
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
                OtlpMetricData::Summary(summary) => self.map_summary_metrics(base_dims, summary.data_points, &context),
                OtlpMetricData::ExponentialHistogram(exponential_histogram) => {
                    match AggregationTemporality::try_from(exponential_histogram.aggregation_temporality) {
                        Ok(AggregationTemporality::Delta) => self.map_exponential_histogram_metrics(
                            base_dims,
                            exponential_histogram.data_points,
                            true,
                            &context,
                        ),
                        _ => {
                            warn!(
                                metric_name = base_dims.name,
                                temporality = exponential_histogram.aggregation_temporality,
                                "Unknown or unsupported aggregation temporality"
                            );
                            Vec::new()
                        }
                    }
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
        context: &TranslationContext,
    ) {
        context.metrics.metrics_received().increment(1);

        let raw_origin = raw_origin_from_attributes(context.resource_attributes);
        match self.context_resolver.resolve_with_optional_host(
            &dims.name,
            dims.host.as_deref(),
            &dims.tags,
            Some(raw_origin),
        ) {
            Some(resolved_context) => {
                let timestamp_s = timestamp_ns / 1_000_000_000;
                let values = match data_type {
                    DataType::Gauge => MetricValues::gauge((timestamp_s, value)),
                    DataType::Count => MetricValues::counter((timestamp_s, value)),
                };

                let metric = Metric::from_parts(resolved_context, values, MetricMetadata::default());
                events.push(Event::Metric(metric));
            }
            None => {
                warn!("Failed to resolve context for metric: {}", dims.name);
            }
        }
    }

    // Maps a monotonic OTLP Summary metric to Saluki 'Event's.
    fn map_summary_metrics(
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
                }
            }

            if self.config.quantiles {
                let base_quantile_dims = point_dims.with_suffix("quantile");
                let quantiles = &dp.quantile_values;
                for quantile in quantiles {
                    if is_skippable(quantile.value) {
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
    /// Maps a slice of OTLP numeric data points to Saluki `Event`s.
    fn map_number_metrics(
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
                continue;
            }

            let ts = dp.time_unix_nano;

            self.record_metric_event(&point_dims, value, ts, data_type, &mut events, context);
        }
        events
    }

    /// Maps a slice of OTLP cumulative monotonic `Sum` data points to Saluki `Event`s.
    fn map_number_monotonic_metrics(
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

    fn record_sketch_event(
        &mut self, dims: &Dimensions, sketch: DDSketch, timestamp_ns: u64, events: &mut Vec<Event>,
        context: &TranslationContext, interval: i64,
    ) {
        context.metrics.metrics_received().increment(1);
        let raw_origin = raw_origin_from_attributes(context.resource_attributes);
        match self.context_resolver.resolve_with_optional_host(
            &dims.name,
            dims.host.as_deref(),
            &dims.tags,
            Some(raw_origin),
        ) {
            Some(resolved_context) => {
                if interval != 0 {
                    trace!(
                        metric = %dims.name,
                        interval,
                        "Inferred delta interval for OTLP metric."
                    );
                }
                let timestamp_s = timestamp_ns / 1_000_000_000;
                let values = MetricValues::distribution((timestamp_s, sketch));
                let metric = Metric::from_parts(resolved_context, values, MetricMetadata::default());
                events.push(Event::Metric(metric));
            }
            None => {
                warn!("Failed to resolve context for metric: {}", dims.name);
            }
        }
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

    fn map_histogram_metrics(
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
                    }
                }
                HistogramMode::Distributions => {
                    if let Err(e) = self.get_sketch_buckets(context, point_dims, &dp, delta, &mut events, hist_info) {
                        warn!(error = %e, "Failed to convert histogram buckets to sketch, dropping data point.");
                    }
                }
            }
        }
        events
    }

    fn map_exponential_histogram_metrics(
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

    /// Determines if the initial value of a cumulative monotonic metric should be consumed.
    fn should_consume_initial_value(&self, start_ts: u64, ts: u64) -> bool {
        match self.config.initial_cumulative_monotonic_value {
            InitialCumulativeMonotonicValue::Auto => {
                // We report the first value if the timeseries started after the translator process started.
                self.process_start_time_ns < start_ts && start_ts != ts
            }
            InitialCumulativeMonotonicValue::Keep => true,
            InitialCumulativeMonotonicValue::Drop => false,
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
    use std::collections::BTreeMap;
    use std::time::{SystemTime, UNIX_EPOCH};

    use otlp_protos::opentelemetry::proto::metrics::v1::{
        number_data_point::Value as OtlpNumberDataPointValue, summary_data_point::ValueAtQuantile, Gauge,
        NumberDataPoint as OtlpNumberDataPoint, ScopeMetrics, Sum,
    };
    use saluki_context::tags::Tag;

    use super::*;
    use crate::sources::otlp::metrics::dimensions::Dimensions;

    fn nanos_from_seconds(s: u64) -> u64 {
        s * 1_000_000_000
    }

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

    // https://github.com/DataDog/datadog-agent/blob/main/pkg/opentelemetry-mapping-go/otlp/metrics/metrics_translator_test.go#L2225
    #[test]
    fn format_float_matches_go_strconv_formatting() {
        let tests = [
            (0.0, "0"),
            (0.001, "0.001"),
            (0.9, "0.9"),
            (0.95, "0.95"),
            (0.99, "0.99"),
            (0.999, "0.999"),
            (1.0, "1.0"),
            (2.0, "2.0"),
            (f64::INFINITY, "inf"),
            (f64::NEG_INFINITY, "-inf"),
            (f64::NAN, "nan"),
            (1e-10, "1e-10"),
            (1e-5, "1e-05"),
            (0.0001, "0.0001"),
            (999_999.0, "999999.0"),
            (1e6, "1e+06.0"),
            (-1e6, "-1e+06.0"),
            (1_000_000.5, "1.0000005e+06"),
            (1_234_567.0, "1.234567e+06.0"),
            (1_200_000.0, "1.2e+06.0"),
        ];

        for (input, expected) in tests {
            assert_eq!(GoFloat(input).to_string(), expected);
        }
    }

    #[test]
    fn bucket_bound_tags_use_go_float_format() {
        assert_eq!(
            format!("lower_bound:{}", GoFloat(f64::NEG_INFINITY)),
            "lower_bound:-inf"
        );
        assert_eq!(format!("upper_bound:{}", GoFloat(f64::INFINITY)), "upper_bound:inf");
        assert_eq!(format!("lower_bound:{}", GoFloat(0.0)), "lower_bound:0");
        assert_eq!(format!("lower_bound:{}", GoFloat(1.0)), "lower_bound:1.0");
        assert_eq!(format!("lower_bound:{}", GoFloat(0.001)), "lower_bound:0.001");
        assert_eq!(format!("upper_bound:{}", GoFloat(1e6)), "upper_bound:1e+06.0");
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

    fn single_gauge_resource_metrics(resource_host: Option<&str>) -> OtlpResourceMetrics {
        let resource = resource_host.map(|host| otlp_protos::opentelemetry::proto::resource::v1::Resource {
            attributes: vec![OtlpKeyValue {
                key: "host.name".to_string(),
                value: Some(otlp_protos::opentelemetry::proto::common::v1::AnyValue {
                    value: Some(
                        otlp_protos::opentelemetry::proto::common::v1::any_value::Value::StringValue(host.to_string()),
                    ),
                }),
            }],
            ..Default::default()
        });

        OtlpResourceMetrics {
            resource,
            scope_metrics: vec![otlp_protos::opentelemetry::proto::metrics::v1::ScopeMetrics {
                metrics: vec![OtlpMetric {
                    name: "otlp.host.metric".to_string(),
                    data: Some(OtlpMetricData::Gauge(
                        otlp_protos::opentelemetry::proto::metrics::v1::Gauge {
                            data_points: vec![OtlpNumberDataPoint {
                                value: Some(OtlpNumberDataPointValue::AsDouble(1.0)),
                                time_unix_nano: nanos_from_seconds(1),
                                ..Default::default()
                            }],
                        },
                    )),
                    ..Default::default()
                }],
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    #[test]
    fn translate_metrics_uses_default_host_when_resource_host_is_unset() {
        let metrics = Metrics::for_tests();
        let mut translator = OtlpMetricsTranslator::for_tests();

        let events = translator
            .translate_metrics(single_gauge_resource_metrics(None), &metrics)
            .expect("translation should succeed")
            .collect::<Vec<_>>();

        let metric = events[0].try_as_metric().expect("metric event");
        assert_eq!(metric.context().host(), Some("default-host"));
    }

    #[test]
    fn translate_metrics_preserves_resource_host() {
        let metrics = Metrics::for_tests();
        let mut translator = OtlpMetricsTranslator::for_tests();

        let events = translator
            .translate_metrics(single_gauge_resource_metrics(Some("resource-host")), &metrics)
            .expect("translation should succeed")
            .collect::<Vec<_>>();

        let metric = events[0].try_as_metric().expect("metric event");
        assert_eq!(metric.context().host(), Some("resource-host"));
    }

    #[test]
    fn raw_value_mode_emits_cumulative_monotonic_sums_as_gauges() {
        let metrics = Metrics::for_tests();
        let mut translator = OtlpMetricsTranslator::for_tests();
        translator.config.cumulative_monotonic_mode = CumulativeMonotonicMode::RawValue;

        let events = translator.map_to_dd_format(
            OtlpMetric {
                name: "cumulative.sum".to_string(),
                data: Some(OtlpMetricData::Sum(
                    otlp_protos::opentelemetry::proto::metrics::v1::Sum {
                        aggregation_temporality: AggregationTemporality::Cumulative as i32,
                        is_monotonic: true,
                        data_points: vec![OtlpNumberDataPoint {
                            value: Some(OtlpNumberDataPointValue::AsInt(42)),
                            start_time_unix_nano: nanos_from_seconds(1),
                            time_unix_nano: nanos_from_seconds(2),
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
    }

    fn initial_cumulative_monotonic_sum(value: i64, start_time_unix_nano: u64, time_unix_nano: u64) -> OtlpMetric {
        OtlpMetric {
            name: "cumulative.sum".to_string(),
            data: Some(OtlpMetricData::Sum(
                otlp_protos::opentelemetry::proto::metrics::v1::Sum {
                    aggregation_temporality: AggregationTemporality::Cumulative as i32,
                    is_monotonic: true,
                    data_points: vec![OtlpNumberDataPoint {
                        value: Some(OtlpNumberDataPointValue::AsInt(value)),
                        start_time_unix_nano,
                        time_unix_nano,
                        ..Default::default()
                    }],
                },
            )),
            ..Default::default()
        }
    }

    fn translate_initial_cumulative_monotonic_sum(
        translator: &mut OtlpMetricsTranslator, metrics: &Metrics, value: i64, start_time_unix_nano: u64,
        time_unix_nano: u64,
    ) -> Vec<Event> {
        translator.map_to_dd_format(
            initial_cumulative_monotonic_sum(value, start_time_unix_nano, time_unix_nano),
            &SharedTagSet::default(),
            None,
            &[],
            metrics,
        )
    }

    #[test]
    fn auto_initial_cumulative_monotonic_value_mode_reports_new_series() {
        let metrics = Metrics::for_tests();
        let mut translator = OtlpMetricsTranslator::for_tests();
        let start_time = translator.process_start_time_ns + 1;
        let timestamp = start_time + nanos_from_seconds(1);

        let events = translate_initial_cumulative_monotonic_sum(&mut translator, &metrics, 42, start_time, timestamp);

        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0].try_as_metric().expect("metric event").values(),
            &MetricValues::counter((timestamp / 1_000_000_000, 42.0))
        );
    }

    #[test]
    fn keep_initial_cumulative_monotonic_value_mode_reports_first_value() {
        let metrics = Metrics::for_tests();
        let mut translator = OtlpMetricsTranslator::for_tests();
        translator.config.initial_cumulative_monotonic_value = InitialCumulativeMonotonicValue::Keep;
        let timestamp = translator.process_start_time_ns;

        let events = translate_initial_cumulative_monotonic_sum(&mut translator, &metrics, 42, timestamp, timestamp);

        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0].try_as_metric().expect("metric event").values(),
            &MetricValues::counter((timestamp / 1_000_000_000, 42.0))
        );
    }

    #[test]
    fn drop_initial_cumulative_monotonic_value_mode_drops_new_series() {
        let metrics = Metrics::for_tests();
        let mut translator = OtlpMetricsTranslator::for_tests();
        translator.config.initial_cumulative_monotonic_value = InitialCumulativeMonotonicValue::Drop;
        let start_time = translator.process_start_time_ns + 1;
        let timestamp = start_time + nanos_from_seconds(1);

        let events = translate_initial_cumulative_monotonic_sum(&mut translator, &metrics, 42, start_time, timestamp);

        assert!(events.is_empty());
    }

    fn string_attribute(key: &str, value: &str) -> OtlpKeyValue {
        OtlpKeyValue {
            key: key.to_string(),
            value: Some(otlp_protos::opentelemetry::proto::common::v1::AnyValue {
                value: Some(
                    otlp_protos::opentelemetry::proto::common::v1::any_value::Value::StringValue(value.to_string()),
                ),
            }),
        }
    }

    fn single_gauge_with_resource_attributes(attributes: Vec<OtlpKeyValue>) -> OtlpResourceMetrics {
        OtlpResourceMetrics {
            resource: Some(otlp_protos::opentelemetry::proto::resource::v1::Resource {
                attributes,
                ..Default::default()
            }),
            scope_metrics: vec![otlp_protos::opentelemetry::proto::metrics::v1::ScopeMetrics {
                metrics: vec![OtlpMetric {
                    name: "otlpresource.metric".to_string(),
                    data: Some(OtlpMetricData::Gauge(
                        otlp_protos::opentelemetry::proto::metrics::v1::Gauge {
                            data_points: vec![OtlpNumberDataPoint {
                                value: Some(OtlpNumberDataPointValue::AsDouble(1.0)),
                                time_unix_nano: nanos_from_seconds(1),
                                ..Default::default()
                            }],
                        },
                    )),
                    ..Default::default()
                }],
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    #[test]
    fn resource_attributes_as_tags_disabled_keeps_only_semantic_mapping() {
        let metrics = Metrics::for_tests();
        let mut translator = OtlpMetricsTranslator::for_tests();
        assert!(!translator.config.resource_attributes_as_tags);

        let resource_metrics = single_gauge_with_resource_attributes(vec![
            string_attribute("service.name", "otlp-test"),
            string_attribute("custom.resource.attribute", "present"),
        ]);

        let events = translator
            .translate_metrics(resource_metrics, &metrics)
            .expect("translation should succeed")
            .collect::<Vec<_>>();

        let tags = events[0].try_as_metric().expect("metric event").context().tags();

        // The semantic-convention mapping is always applied.
        assert_eq!(tags.get_single_tag("service"), Some(&Tag::from("service:otlp-test")));

        // The raw resource attributes are not added when the flag is disabled.
        assert_eq!(tags.get_single_tag("service.name"), None);
        assert_eq!(tags.get_single_tag("custom.resource.attribute"), None);
    }

    #[test]
    fn resource_attributes_as_tags_enabled_adds_raw_attributes() {
        let metrics = Metrics::for_tests();
        let mut translator = OtlpMetricsTranslator::for_tests();
        translator.config.resource_attributes_as_tags = true;

        // Configured tags and resource tags may be stored in separate shared chunks. Exact
        // duplicates must still resolve to one tag in the metric context.
        let mut configured_tags = TagSet::default();
        configured_tags.insert_tag("service:otlp-test");
        configured_tags.insert_tag("custom.resource.attribute:present");
        translator.metric_tags = configured_tags.into_shared();

        let resource_metrics = single_gauge_with_resource_attributes(vec![
            string_attribute("service.name", "otlp-test"),
            string_attribute("custom.resource.attribute", "present"),
        ]);

        let events = translator
            .translate_metrics(resource_metrics, &metrics)
            .expect("translation should succeed")
            .collect::<Vec<_>>();

        let tags = events[0].try_as_metric().expect("metric event").context().tags();

        // The semantic-convention mapping remains intact.
        assert_eq!(tags.get_single_tag("service"), Some(&Tag::from("service:otlp-test")));

        // Every resource attribute is also emitted as a raw tag.
        assert_eq!(
            tags.get_single_tag("service.name"),
            Some(&Tag::from("service.name:otlp-test"))
        );
        assert_eq!(
            tags.get_single_tag("custom.resource.attribute"),
            Some(&Tag::from("custom.resource.attribute:present"))
        );
        assert_eq!(tags.into_iter().filter(|tag| tag.name() == "service").count(), 1);
        assert_eq!(
            tags.into_iter()
                .filter(|tag| tag.name() == "custom.resource.attribute")
                .count(),
            1
        );
    }

    #[test]
    fn configured_metric_tags_are_added_to_every_metric() {
        let metrics = Metrics::for_tests();
        let mut translator = OtlpMetricsTranslator::for_tests();

        let mut configured = TagSet::default();
        configured.insert_tag("correctness:configured");
        translator.metric_tags = configured.into_shared();

        let events = translator
            .translate_metrics(single_gauge_with_resource_attributes(vec![]), &metrics)
            .expect("translation should succeed")
            .collect::<Vec<_>>();

        let tags = events[0].try_as_metric().expect("metric event").context().tags();
        assert_eq!(
            tags.get_single_tag("correctness"),
            Some(&Tag::from("correctness:configured"))
        );
    }

    #[test]
    fn conflicting_service_values_all_coexist() {
        let metrics = Metrics::for_tests();
        let mut translator = OtlpMetricsTranslator::for_tests();
        translator.config.resource_attributes_as_tags = true;

        // A configured `service` tag with a value that differs from the resource's `service.name`.
        let mut configured = TagSet::default();
        configured.insert_tag("service:configured");
        translator.metric_tags = configured.into_shared();

        let resource_metrics =
            single_gauge_with_resource_attributes(vec![string_attribute("service.name", "resource")]);

        let events = translator
            .translate_metrics(resource_metrics, &metrics)
            .expect("translation should succeed")
            .collect::<Vec<_>>();

        let tags = events[0].try_as_metric().expect("metric event").context().tags();

        // Distinct values for the same tag name coexist; tags are not keyed by name alone.
        let service_values: Vec<&str> = tags
            .into_iter()
            .filter(|tag| tag.name() == "service")
            .map(|tag| tag.value().unwrap_or(""))
            .collect();
        assert!(service_values.contains(&"configured"), "got {service_values:?}");
        assert!(service_values.contains(&"resource"), "got {service_values:?}");

        // The raw resource attribute is also present under its original key.
        assert_eq!(
            tags.get_single_tag("service.name"),
            Some(&Tag::from("service.name:resource"))
        );
    }

    #[test]
    fn resource_attribute_shadows_colliding_datapoint_attribute() {
        let metrics = Metrics::for_tests();
        let mut translator = OtlpMetricsTranslator::for_tests();
        translator.config.resource_attributes_as_tags = true;

        // A data-point attribute collides with a resource attribute of the same key.
        let resource = otlp_protos::opentelemetry::proto::resource::v1::Resource {
            attributes: vec![string_attribute("custom.key", "resource")],
            ..Default::default()
        };
        let resource_metrics = OtlpResourceMetrics {
            resource: Some(resource),
            scope_metrics: vec![otlp_protos::opentelemetry::proto::metrics::v1::ScopeMetrics {
                metrics: vec![OtlpMetric {
                    name: "otlpresource.metric".to_string(),
                    data: Some(OtlpMetricData::Gauge(
                        otlp_protos::opentelemetry::proto::metrics::v1::Gauge {
                            data_points: vec![OtlpNumberDataPoint {
                                value: Some(OtlpNumberDataPointValue::AsDouble(1.0)),
                                time_unix_nano: nanos_from_seconds(1),
                                attributes: vec![string_attribute("custom.key", "datapoint")],
                                ..Default::default()
                            }],
                        },
                    )),
                    ..Default::default()
                }],
                ..Default::default()
            }],
            ..Default::default()
        };

        let events = translator
            .translate_metrics(resource_metrics, &metrics)
            .expect("translation should succeed")
            .collect::<Vec<_>>();

        let tags = events[0].try_as_metric().expect("metric event").context().tags();

        // The resource value wins; the data-point value is dropped.
        let values: Vec<&str> = tags
            .into_iter()
            .filter(|tag| tag.name() == "custom.key")
            .map(|tag| tag.value().unwrap_or(""))
            .collect();
        assert_eq!(values, vec!["resource"]);
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

    #[test]
    fn remapped_summary_uses_target_mapping_values_for_signed_keys() {
        let target_mapping = DDSketch::remap_mapping();
        let mut remapped_counts = BTreeMap::new();
        remapped_counts.insert(-10, 1.5);
        remapped_counts.insert(0, 2.0);
        remapped_counts.insert(12, 3.0);

        let summary = summarize_remapped_counts(&remapped_counts, &target_mapping).expect("summary should exist");
        let expected_min = -target_mapping.value(10);
        let expected_max = target_mapping.value(12);
        let expected_sum = expected_min * 1.5 + expected_max * 3.0;

        assert_eq!(summary.min, expected_min);
        assert_eq!(summary.max, expected_max);
        assert!((summary.sum - expected_sum).abs() < f64::EPSILON);
    }

    #[test]
    fn ddsketch_conversion_splits_coarse_bins_across_multiple_agent_bins() {
        let source_mapping = LogarithmicMapping::new_with_gamma(2.0).expect("source mapping should be valid");
        let mut positive_store = DenseStore::new();
        positive_store.add(0, 256);

        let canonical = CanonicalDDSketch::new(source_mapping, positive_store, DenseStore::new());
        let sketch = convert_ddsketch_into_sketch(canonical).expect("conversion should succeed");

        assert_eq!(sketch.count(), 256);
        assert!(sketch.bin_count() > 1);
        assert!(sketch.min().unwrap() < sketch.max().unwrap());
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

    // -----------------------------------------------------------------------------------------------
    // Histogram translation (`map_histogram_metrics`).
    //
    // Mirrors the Go `mapHistogramMetrics` histogram-mode dispatch:
    // https://github.com/DataDog/datadog-agent/blob/main/pkg/opentelemetry-mapping-go/otlp/metrics/metrics_translator.go
    // -----------------------------------------------------------------------------------------------

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

    fn distribution_sketch(metric: &Metric) -> &DDSketch {
        match metric.values() {
            MetricValues::Distribution(points) => {
                points
                    .into_iter()
                    .next()
                    .expect("distribution should carry one sketch point")
                    .1
            }
            _ => panic!("expected a distribution metric"),
        }
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

    // -----------------------------------------------------------------------------------------------
    // Summary translation (`map_summary_metrics`).
    // -----------------------------------------------------------------------------------------------

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

    // -----------------------------------------------------------------------------------------------
    // ExponentialHistogram translation (`map_exponential_histogram_metrics`).
    // -----------------------------------------------------------------------------------------------

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

    // -----------------------------------------------------------------------------------------------
    // Runtime metric mapping (`runtime_metrics.rs` tables consumed by `translate_metrics` and the
    // `map_*_runtime_metric_with_attributes` helpers).
    // -----------------------------------------------------------------------------------------------

    fn resource_metrics_with_metric(metric: OtlpMetric) -> OtlpResourceMetrics {
        OtlpResourceMetrics {
            resource: None,
            scope_metrics: vec![ScopeMetrics {
                metrics: vec![metric],
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    fn metric_by_name<'a>(events: &'a [Event], name: &str) -> &'a Metric {
        events
            .iter()
            .filter_map(|e| e.try_as_metric())
            .find(|m| m.context().name() == name)
            .unwrap_or_else(|| panic!("no metric named {name}"))
    }

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
    fn translate_metrics_renames_runtime_metric_without_attributes() {
        let metrics = Metrics::for_tests();
        let mut translator = OtlpMetricsTranslator::for_tests();

        // `process.runtime.go.goroutines` maps (with no attribute matching) to `runtime.go.num_goroutine`.
        let metric = OtlpMetric {
            name: "process.runtime.go.goroutines".to_string(),
            data: Some(OtlpMetricData::Gauge(Gauge {
                data_points: vec![OtlpNumberDataPoint {
                    value: Some(OtlpNumberDataPointValue::AsInt(5)),
                    time_unix_nano: nanos_from_seconds(1),
                    ..Default::default()
                }],
            })),
            ..Default::default()
        };

        let events = translator
            .translate_metrics(resource_metrics_with_metric(metric), &metrics)
            .expect("translation should succeed")
            .collect::<Vec<_>>();

        let renamed = metric_by_name(&events, "runtime.go.num_goroutine");
        assert_eq!(renamed.values(), &MetricValues::gauge((1, 5.0)));
    }

    #[test]
    fn translate_metrics_maps_runtime_gauge_by_attribute() {
        let metrics = Metrics::for_tests();
        let mut translator = OtlpMetricsTranslator::for_tests();

        // `process.runtime.jvm.memory.usage` fans out by the `type` attribute: heap -> jvm.heap_memory,
        // non_heap -> jvm.non_heap_memory. The `type` attribute is stripped from the mapped metric.
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

        let events = translator
            .translate_metrics(resource_metrics_with_metric(metric), &metrics)
            .expect("translation should succeed")
            .collect::<Vec<_>>();

        let heap = metric_by_name(&events, "jvm.heap_memory");
        assert_eq!(heap.values(), &MetricValues::gauge((1, 100.0)));
        assert!(
            heap.context().tags().get_single_tag("type").is_none(),
            "the matched `type` attribute should be stripped from the mapped metric"
        );

        let non_heap = metric_by_name(&events, "jvm.non_heap_memory");
        assert_eq!(non_heap.values(), &MetricValues::gauge((1, 50.0)));
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
}
