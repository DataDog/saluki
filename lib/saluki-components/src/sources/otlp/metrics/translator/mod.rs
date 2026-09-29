#![allow(dead_code)]

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use std::vec::IntoIter;

use agent_data_plane_config::domains::otlp::{CumulativeMonotonicMode, InitialCumulativeMonotonicValue};
use ddsketch::DDSketch;
use otlp_protos::opentelemetry::proto::common::v1::KeyValue as OtlpKeyValue;
use otlp_protos::opentelemetry::proto::metrics::v1::{
    metric::Data as OtlpMetricData, AggregationTemporality, Metric as OtlpMetric,
    ResourceMetrics as OtlpResourceMetrics,
};
use saluki_common::collections::FastHashSet;
use saluki_context::tags::{SharedTagSet, TagSet};
use saluki_context::{ContextResolver, ContextResolverBuilder};
use saluki_core::data_model::event::metric::{Metric, MetricMetadata, MetricValues};
use saluki_core::data_model::event::Event;
use saluki_error::{ErrorContext as _, GenericError};
use stringtheory::MetaString;
use tracing::{debug, trace, warn};

use self::metric_type::MetricTypeOverrideWarningKind;
use self::runtime::{
    map_gauge_runtime_metric_with_attributes, map_histogram_runtime_metric_with_attributes,
    map_sum_runtime_metric_with_attributes,
};
use super::cache::PointsCache;
use super::config::OtlpMetricsTranslatorConfig;
use super::dimensions::Dimensions;
use super::internal::{instrumentationlibrary, instrumentationscope};
use super::remap;
use super::runtime_metrics::{RUNTIME_METRICS_MAPPINGS, RUNTIME_METRIC_PREFIX_LANGUAGE_MAP};
use super::telemetry::OtlpMetricsTranslatorMetrics;
use crate::common::otlp::attributes::translator::AttributeTranslator;
use crate::common::otlp::attributes::ResourceAttributeTagMode;
use crate::common::otlp::origin::OtlpOriginTagResolver;
use crate::common::otlp::util::{Source, SourceKind};
use crate::sources::otlp::Metrics;

mod exponential_histogram;
mod go_float;
mod histogram;
mod metric_type;
mod monotonic;
mod number;
mod runtime;
mod sketch;
mod summary;
mod usage_beacons;

/// Whether OpenTelemetry runtime metrics are remapped to their Datadog-conventional names.
const RUNTIME_REMAPPING_ENABLED: bool = false;

#[derive(Clone, Copy, Debug, PartialEq)]
enum DataType {
    Gauge,
    Count,
    Rate,
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
    origin_tag_resolver: OtlpOriginTagResolver,
    resolved_origin_tags: SharedTagSet,
    prev_pts: PointsCache,
    process_start_time_ns: u64, // Used for initial value consumption.
    attribute_translator: AttributeTranslator,
    // One-shot warnings emitted for each metric and type-override error kind.
    metric_type_override_warnings: FastHashSet<(String, MetricTypeOverrideWarningKind)>,
    // Configured tags (`otlp_config.metrics.tags`) added to every emitted metric.
    metric_tags: SharedTagSet,
    // Self-telemetry for translation errors, dropped points, and processing latency.
    translator_metrics: OtlpMetricsTranslatorMetrics,
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

/// Checks if a metric value is `NaN` or `Infinity`.
fn is_skippable(value: f64) -> bool {
    value.is_nan() || value.is_infinite()
}

impl OtlpMetricsTranslator {
    /// Creates an `OtlpMetricsTranslator` from the given configuration, context resolver, and
    /// configured metric tags.
    pub fn new(
        config: OtlpMetricsTranslatorConfig, default_hostname: MetaString, context_resolver: ContextResolver,
        origin_tag_resolver: OtlpOriginTagResolver, metric_tags: SharedTagSet,
        translator_metrics: OtlpMetricsTranslatorMetrics,
    ) -> Result<Self, GenericError> {
        config
            .validate()
            .error_context("Failed to validate OTLP metrics translator configuration.")?;

        let process_start_time_ns = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos() as u64;

        Ok(Self {
            config,
            default_hostname,
            context_resolver,
            origin_tag_resolver,
            resolved_origin_tags: SharedTagSet::default(),
            prev_pts: PointsCache::from_config(config),
            process_start_time_ns,
            attribute_translator: AttributeTranslator::new(),
            metric_type_override_warnings: FastHashSet::default(),
            metric_tags,
            translator_metrics,
        })
    }

    /// Translates a batch of OTLP `ResourceMetrics` into Saluki `Event`s.
    /// This is the Rust equivalent of the Go `MapMetrics` function.
    ///
    /// Returns the translated events and the set of runtime languages detected from metric names
    /// in this `ResourceMetrics`.
    ///
    /// The processing duration is recorded on the translator's latency histogram regardless of whether the
    /// translation succeeds or fails, so the histogram captures the full distribution of processing times.
    pub fn translate_metrics(
        &mut self, resource_metrics: OtlpResourceMetrics, metrics: &Metrics,
    ) -> Result<(IntoIter<Event>, FastHashSet<&'static str>), GenericError> {
        let start = Instant::now();
        let result = self.translate_metrics_inner(resource_metrics, metrics);
        self.translator_metrics
            .processing_duration()
            .record(start.elapsed().as_secs_f64());
        if result.is_err() {
            self.translator_metrics.errors_translate().increment(1);
        }
        result
    }

    fn translate_metrics_inner(
        &mut self, resource_metrics: OtlpResourceMetrics, metrics: &Metrics,
    ) -> Result<(IntoIter<Event>, FastHashSet<&'static str>), GenericError> {
        let mut events = Vec::new();
        let mut detected_languages = FastHashSet::default();
        let resource = resource_metrics.resource.unwrap_or_default();
        let source = self.attribute_translator.resource_to_metric_source(&resource);

        let resource_tag_mode = if self.config.resource_attributes_as_tags {
            ResourceAttributeTagMode::All
        } else {
            ResourceAttributeTagMode::Mapped
        };
        let resource_attribute_tags = self
            .attribute_translator
            .tags_from_attributes(&resource.attributes, resource_tag_mode)
            .into_shared();
        self.resolved_origin_tags = self
            .origin_tag_resolver
            .resolve_resource_tags(&resource.attributes, self.config.tag_cardinality);

        // Combine configured and resource-derived tags once per resource, then reuse them for every
        // instrumentation scope.
        let mut resource_tags = self.metric_tags.clone();
        resource_tags.extend_from_shared(&resource_attribute_tags);

        let host = match source {
            Some(Source {
                kind: SourceKind::HostnameKind,
                identifier,
            }) => Some(MetaString::from(identifier)),
            Some(Source {
                kind: SourceKind::AwsEcsFargateKind,
                ..
            }) => None,
            None => Some(self.default_hostname.clone()),
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
                    for (prefix, language) in RUNTIME_METRIC_PREFIX_LANGUAGE_MAP.iter() {
                        if metric.name.starts_with(prefix) {
                            detected_languages.insert(*language);
                        }
                    }

                    if RUNTIME_REMAPPING_ENABLED {
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
                                        map_histogram_runtime_metric_with_attributes(
                                            &metric,
                                            &mut new_metrics,
                                            mapping,
                                        );
                                    }
                                    _ => {}
                                }
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
                    self.map_to_dd_format(metric, &tags, host.clone(), &resource.attributes, metrics);
                events.append(&mut translated_events);
            }

            for metric in new_metrics {
                let mut translated_events =
                    self.map_to_dd_format(metric, &tags, host.clone(), &resource.attributes, metrics);
                events.append(&mut translated_events);
            }
        }

        metrics.metrics_received().increment(events.len() as u64);

        Ok((events.into_iter(), detected_languages))
    }

    /// Creates a new `OtlpMetricsTranslator` for tests.
    pub fn for_tests() -> OtlpMetricsTranslator {
        Self::for_tests_with_translator_metrics(OtlpMetricsTranslatorMetrics::for_tests())
    }

    /// Creates a new `OtlpMetricsTranslator` for tests with the given translator telemetry.
    pub fn for_tests_with_translator_metrics(
        translator_metrics: OtlpMetricsTranslatorMetrics,
    ) -> OtlpMetricsTranslator {
        let process_start_time_ns = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("System time is before the UNIX epoch, this should not happen.")
            .as_nanos() as u64;

        OtlpMetricsTranslator {
            config: Default::default(),
            default_hostname: MetaString::from_static("default-host"),
            context_resolver: ContextResolverBuilder::for_tests().build().0,
            origin_tag_resolver: OtlpOriginTagResolver::new(std::sync::Arc::new(
                saluki_env::workload::providers::NoopWorkloadProvider,
            )),
            resolved_origin_tags: SharedTagSet::default(),
            prev_pts: PointsCache::for_tests(),
            process_start_time_ns,
            attribute_translator: AttributeTranslator::new(),
            metric_type_override_warnings: FastHashSet::default(),
            metric_tags: SharedTagSet::default(),
            translator_metrics,
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
                        self.translator_metrics
                            .dropped_unsupported_temporality()
                            .increment(sum.data_points.len() as u64);
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
                            self.translator_metrics
                                .dropped_unsupported_temporality()
                                .increment(histogram.data_points.len() as u64);
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
                            debug!(
                                metric_name = base_dims.name,
                                temporality = exponential_histogram.aggregation_temporality,
                                "Unknown or unsupported aggregation temporality"
                            );
                            self.translator_metrics
                                .dropped_unsupported_temporality()
                                .increment(exponential_histogram.data_points.len() as u64);
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

        match self.context_resolver.resolve_with_optional_host_and_origin_tags(
            &dims.name,
            dims.host.as_deref(),
            &dims.tags,
            self.resolved_origin_tags.clone(),
        ) {
            Some(resolved_context) => {
                let timestamp_s = timestamp_ns / 1_000_000_000;
                let values = match data_type {
                    DataType::Gauge => MetricValues::gauge((timestamp_s, value)),
                    DataType::Count => MetricValues::counter((timestamp_s, value)),
                    DataType::Rate => MetricValues::rate((timestamp_s, value), Duration::ZERO),
                };

                let metric = Metric::from_parts(resolved_context, values, MetricMetadata::default());
                events.push(Event::Metric(metric));
            }
            None => {
                warn!("Failed to resolve context for metric: {}", dims.name);
            }
        }
    }

    fn record_sketch_event(
        &mut self, dims: &Dimensions, sketch: DDSketch, timestamp_ns: u64, events: &mut Vec<Event>,
        context: &TranslationContext, interval: i64,
    ) {
        context.metrics.metrics_received().increment(1);
        match self.context_resolver.resolve_with_optional_host_and_origin_tags(
            &dims.name,
            dims.host.as_deref(),
            &dims.tags,
            self.resolved_origin_tags.clone(),
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

#[cfg(test)]
mod tests {
    use agent_data_plane_config::domains::otlp::HistogramMode;
    use otlp_protos::opentelemetry::proto::metrics::v1::{
        number_data_point::Value as OtlpNumberDataPointValue, Gauge, HistogramDataPoint as OtlpHistogramDataPoint,
        NumberDataPoint as OtlpNumberDataPoint, ScopeMetrics, Sum,
    };
    use saluki_context::tags::Tag;

    use super::*;

    // Fixtures shared with the submodules' own test modules.
    pub(super) fn nanos_from_seconds(s: u64) -> u64 {
        s * 1_000_000_000
    }

    pub(super) fn string_attribute(key: &str, value: &str) -> OtlpKeyValue {
        OtlpKeyValue {
            key: key.to_string(),
            value: Some(otlp_protos::opentelemetry::proto::common::v1::AnyValue {
                value: Some(
                    otlp_protos::opentelemetry::proto::common::v1::any_value::Value::StringValue(value.to_string()),
                ),
            }),
        }
    }

    pub(super) fn distribution_sketch(metric: &Metric) -> &DDSketch {
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

    pub(super) fn resource_metrics_with_metric(metric: OtlpMetric) -> OtlpResourceMetrics {
        OtlpResourceMetrics {
            resource: None,
            scope_metrics: vec![ScopeMetrics {
                metrics: vec![metric],
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    pub(super) fn metric_by_name<'a>(events: &'a [Event], name: &str) -> &'a Metric {
        events
            .iter()
            .filter_map(|e| e.try_as_metric())
            .find(|m| m.context().name() == name)
            .unwrap_or_else(|| panic!("no metric named {name}"))
    }

    pub(super) fn gauge_metric_named(name: &str) -> OtlpMetric {
        OtlpMetric {
            name: name.to_string(),
            data: Some(OtlpMetricData::Gauge(Gauge {
                data_points: vec![OtlpNumberDataPoint {
                    value: Some(OtlpNumberDataPointValue::AsInt(1)),
                    time_unix_nano: nanos_from_seconds(1),
                    ..Default::default()
                }],
            })),
            ..Default::default()
        }
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

        let (events_iter, _) = translator
            .translate_metrics(single_gauge_resource_metrics(None), &metrics)
            .expect("translation should succeed");
        let events = events_iter.collect::<Vec<_>>();

        let metric = events[0].try_as_metric().expect("metric event");
        assert_eq!(metric.context().host(), Some("default-host"));
    }

    #[test]
    fn translate_metrics_preserves_resource_host() {
        let metrics = Metrics::for_tests();
        let mut translator = OtlpMetricsTranslator::for_tests();

        let (events_iter, _) = translator
            .translate_metrics(single_gauge_resource_metrics(Some("resource-host")), &metrics)
            .expect("translation should succeed");
        let events = events_iter.collect::<Vec<_>>();

        let metric = events[0].try_as_metric().expect("metric event");
        assert_eq!(metric.context().host(), Some("resource-host"));
    }

    #[test]
    fn translate_metrics_leaves_fargate_resource_host_unset() {
        let metrics = Metrics::for_tests();
        let mut translator = OtlpMetricsTranslator::for_tests();
        let mut resource_metrics = single_gauge_resource_metrics(None);
        resource_metrics.resource = Some(otlp_protos::opentelemetry::proto::resource::v1::Resource {
            attributes: [
                ("aws.ecs.launchtype", "fargate"),
                ("aws.ecs.task.arn", "arn:aws:ecs:region:account:task/task-id"),
            ]
            .into_iter()
            .map(|(key, value)| OtlpKeyValue {
                key: key.to_string(),
                value: Some(otlp_protos::opentelemetry::proto::common::v1::AnyValue {
                    value: Some(
                        otlp_protos::opentelemetry::proto::common::v1::any_value::Value::StringValue(value.to_string()),
                    ),
                }),
            })
            .collect(),
            ..Default::default()
        });

        let (events_iter, _) = translator
            .translate_metrics(resource_metrics, &metrics)
            .expect("translation should succeed");
        let events = events_iter.collect::<Vec<_>>();

        let metric = events[0].try_as_metric().expect("metric event");
        assert_eq!(metric.context().host(), None);
        assert_eq!(
            metric
                .context()
                .tags()
                .get_single_tag("task_arn")
                .map(|tag| tag.value()),
            Some(Some("arn:aws:ecs:region:account:task/task-id"))
        );
    }

    #[test]
    fn translate_metrics_preserves_fargate_task_arn_for_runtime_metrics() {
        let metrics = Metrics::for_tests();
        let mut translator = OtlpMetricsTranslator::for_tests();
        let mut resource_metrics = single_gauge_with_resource_attributes(vec![
            string_attribute("aws.ecs.launchtype", "fargate"),
            string_attribute("aws.ecs.task.arn", "arn:aws:ecs:region:account:task/resource-task"),
        ]);
        resource_metrics.scope_metrics[0].metrics[0].name = "process.runtime.dotnet.gc.heap.size".to_string();

        let (events_iter, _) = translator
            .translate_metrics(resource_metrics, &metrics)
            .expect("translation should succeed");
        let events = events_iter.collect::<Vec<_>>();

        assert!(events.iter().all(|event| {
            event
                .try_as_metric()
                .expect("metric event")
                .context()
                .tags()
                .get_single_tag("task_arn")
                .map(|tag| tag.value())
                == Some(Some("arn:aws:ecs:region:account:task/resource-task"))
        }));
    }

    #[test]
    fn translate_metrics_preserves_configured_and_resource_fargate_task_arns_for_runtime_metrics() {
        let metrics = Metrics::for_tests();
        let mut translator = OtlpMetricsTranslator::for_tests();
        let mut configured_tags = TagSet::default();
        configured_tags.insert_tag("task_arn:configured-task");
        translator.metric_tags = configured_tags.into_shared();

        let mut resource_metrics = single_gauge_with_resource_attributes(vec![
            string_attribute("aws.ecs.launchtype", "fargate"),
            string_attribute("aws.ecs.task.arn", "arn:aws:ecs:region:account:task/resource-task"),
        ]);
        resource_metrics.scope_metrics[0].metrics[0].name = "process.runtime.dotnet.gc.heap.size".to_string();

        let (events_iter, _) = translator
            .translate_metrics(resource_metrics, &metrics)
            .expect("translation should succeed");
        let events = events_iter.collect::<Vec<_>>();

        assert!(events.iter().all(|event| {
            let task_arns = event
                .try_as_metric()
                .expect("metric event")
                .context()
                .tags()
                .into_iter()
                .filter(|tag| tag.name() == "task_arn")
                .map(|tag| tag.value())
                .collect::<Vec<_>>();

            task_arns.contains(&Some("configured-task"))
                && task_arns.contains(&Some("arn:aws:ecs:region:account:task/resource-task"))
        }));
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

        let (events_iter, _) = translator
            .translate_metrics(resource_metrics, &metrics)
            .expect("translation should succeed");
        let events = events_iter.collect::<Vec<_>>();

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

        let (events_iter, _) = translator
            .translate_metrics(resource_metrics, &metrics)
            .expect("translation should succeed");
        let events = events_iter.collect::<Vec<_>>();

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

        let (events_iter, _) = translator
            .translate_metrics(single_gauge_with_resource_attributes(vec![]), &metrics)
            .expect("translation should succeed");
        let events = events_iter.collect::<Vec<_>>();

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

        let (events_iter, _) = translator
            .translate_metrics(resource_metrics, &metrics)
            .expect("translation should succeed");
        let events = events_iter.collect::<Vec<_>>();

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

        let (events_iter, _) = translator
            .translate_metrics(resource_metrics, &metrics)
            .expect("translation should succeed");
        let events = events_iter.collect::<Vec<_>>();

        let tags = events[0].try_as_metric().expect("metric event").context().tags();

        // The resource value wins; the data-point value is dropped.
        let values: Vec<&str> = tags
            .into_iter()
            .filter(|tag| tag.name() == "custom.key")
            .map(|tag| tag.value().unwrap_or(""))
            .collect();
        assert_eq!(values, vec!["resource"]);
    }

    // Self-telemetry: error, dropped-point, and latency metrics.

    use saluki_core::components::ComponentContext;
    use saluki_metrics::test::TestRecorder;

    fn test_translator_metrics(recorder: &TestRecorder) -> OtlpMetricsTranslatorMetrics {
        let _ = recorder; // recorder is already set as the default local recorder by the caller
        OtlpMetricsTranslatorMetrics::from_component_context(&ComponentContext::test_source("otlp_test"))
    }

    /// Default tags attached to every metric registered via `ComponentContext::test_source("otlp_test")`.
    const TELEMETRY_DEFAULT_TAGS: &[(&str, &str)] = &[("component_id", "otlp_test"), ("component_type", "source")];

    #[test]
    fn translate_metrics_records_processing_duration_on_success() {
        let recorder = TestRecorder::default();
        let _recorder_guard = metrics::set_default_local_recorder(&recorder);

        let translator_metrics = test_translator_metrics(&recorder);
        let metrics = Metrics::for_tests();
        let mut translator = OtlpMetricsTranslator::for_tests_with_translator_metrics(translator_metrics);

        let _ = translator.translate_metrics(single_gauge_resource_metrics(None), &metrics);

        let samples = recorder
            .histogram(("component_processing_duration_seconds", TELEMETRY_DEFAULT_TAGS))
            .expect("processing duration histogram should have a sample");
        assert_eq!(samples.len(), 1, "exactly one latency sample should be recorded");
        assert!(samples[0] >= 0.0, "duration should be non-negative");
    }

    #[test]
    fn translate_metrics_increments_dropped_points_for_unsupported_temporality() {
        let recorder = TestRecorder::default();
        let _recorder_guard = metrics::set_default_local_recorder(&recorder);

        let translator_metrics = test_translator_metrics(&recorder);
        let metrics = Metrics::for_tests();
        let mut translator = OtlpMetricsTranslator::for_tests_with_translator_metrics(translator_metrics);

        // A Sum with an invalid aggregation temporality (99) triggers the unsupported-temporality drop path.
        // Two data points are dropped, so the counter should increment by 2.
        let metric = OtlpMetric {
            name: "unsupported.temporality".to_string(),
            data: Some(OtlpMetricData::Sum(Sum {
                aggregation_temporality: 99,
                is_monotonic: false,
                data_points: vec![
                    OtlpNumberDataPoint {
                        value: Some(OtlpNumberDataPointValue::AsDouble(1.0)),
                        time_unix_nano: nanos_from_seconds(1),
                        ..Default::default()
                    },
                    OtlpNumberDataPoint {
                        value: Some(OtlpNumberDataPointValue::AsDouble(2.0)),
                        time_unix_nano: nanos_from_seconds(2),
                        ..Default::default()
                    },
                ],
            })),
            ..Default::default()
        };

        let _ = translator.translate_metrics(resource_metrics_with_metric(metric), &metrics);

        let tags: &[(&str, &str)] = &[
            ("component_id", "otlp_test"),
            ("component_type", "source"),
            ("reason", "unsupported_temporality"),
        ];
        assert_eq!(recorder.counter(("component_events_dropped_total", tags)), Some(2));
    }

    #[test]
    fn translate_metrics_increments_dropped_points_for_invalid_value() {
        let recorder = TestRecorder::default();
        let _recorder_guard = metrics::set_default_local_recorder(&recorder);

        let translator_metrics = test_translator_metrics(&recorder);
        let metrics = Metrics::for_tests();
        let mut translator = OtlpMetricsTranslator::for_tests_with_translator_metrics(translator_metrics);

        // A gauge with a NaN value triggers the invalid-value drop path.
        let metric = OtlpMetric {
            name: "nan.gauge".to_string(),
            data: Some(OtlpMetricData::Gauge(Gauge {
                data_points: vec![OtlpNumberDataPoint {
                    value: Some(OtlpNumberDataPointValue::AsDouble(f64::NAN)),
                    time_unix_nano: nanos_from_seconds(1),
                    ..Default::default()
                }],
            })),
            ..Default::default()
        };

        let _ = translator.translate_metrics(resource_metrics_with_metric(metric), &metrics);

        let tags: &[(&str, &str)] = &[
            ("component_id", "otlp_test"),
            ("component_type", "source"),
            ("reason", "invalid_value"),
        ];
        assert_eq!(recorder.counter(("component_events_dropped_total", tags)), Some(1));
    }

    #[test]
    fn translate_metrics_increments_dropped_points_for_histogram_conversion_failure() {
        let recorder = TestRecorder::default();
        let _recorder_guard = metrics::set_default_local_recorder(&recorder);

        let translator_metrics = test_translator_metrics(&recorder);
        let metrics = Metrics::for_tests();
        let mut translator = OtlpMetricsTranslator::for_tests_with_translator_metrics(translator_metrics);
        translator.config.hist_mode = HistogramMode::Distributions;

        // Mismatched bucket/bound counts trigger the histogram conversion failure path.
        let metric = OtlpMetric {
            name: "bad.histogram".to_string(),
            data: Some(OtlpMetricData::Histogram(
                otlp_protos::opentelemetry::proto::metrics::v1::Histogram {
                    aggregation_temporality: AggregationTemporality::Delta as i32,
                    data_points: vec![OtlpHistogramDataPoint {
                        count: 1,
                        sum: Some(0.5),
                        bucket_counts: vec![1],
                        explicit_bounds: vec![1.0, 2.0],
                        time_unix_nano: nanos_from_seconds(1),
                        ..Default::default()
                    }],
                },
            )),
            ..Default::default()
        };

        let _ = translator.translate_metrics(resource_metrics_with_metric(metric), &metrics);

        let tags: &[(&str, &str)] = &[
            ("component_id", "otlp_test"),
            ("component_type", "source"),
            ("reason", "histogram_conversion"),
        ];
        assert_eq!(recorder.counter(("component_events_dropped_total", tags)), Some(1));
    }

    #[test]
    fn translate_metrics_does_not_increment_drop_counters_on_success() {
        let recorder = TestRecorder::default();
        let _recorder_guard = metrics::set_default_local_recorder(&recorder);

        let translator_metrics = test_translator_metrics(&recorder);
        let metrics = Metrics::for_tests();
        let mut translator = OtlpMetricsTranslator::for_tests_with_translator_metrics(translator_metrics);

        // A valid gauge should not increment any drop counter.
        let _ = translator.translate_metrics(single_gauge_resource_metrics(None), &metrics);

        for reason in [
            "unsupported_temporality",
            "histogram_conversion",
            "invalid_value",
            "translate",
        ] {
            let tags: &[(&str, &str)] = &[
                ("component_id", "otlp_test"),
                ("component_type", "source"),
                ("reason", reason),
            ];
            let counter_name = if reason == "translate" {
                "component_errors_total"
            } else {
                "component_events_dropped_total"
            };
            assert_eq!(
                recorder.counter((counter_name, tags)),
                Some(0),
                "counter {counter_name} with reason={reason} should be zero after a successful translation"
            );
        }
    }
}
