//! Usage beacon metrics emitted once per OTLP metrics request.

use std::time::{SystemTime, UNIX_EPOCH};

use saluki_common::collections::FastHashSet;
use saluki_context::tags::SharedTagSet;
use saluki_core::data_model::event::metric::{Metric, MetricMetadata, MetricValues};
use saluki_core::data_model::event::Event;

use super::OtlpMetricsTranslator;

impl OtlpMetricsTranslator {
    /// Emits usage beacon metrics for a completed OTLP metrics request.
    ///
    /// Emits `datadog.agent.otlp.metrics` as a gauge with value `1` and no tags, plus one
    /// `datadog.agent.otlp.runtime_metrics` gauge (value `1`, tag `language:<language>`) for each
    /// distinct runtime language detected during translation. Both beacons use the configured
    /// default hostname, independent of per-datapoint resource-host resolution.
    ///
    /// The `languages` set is provided by the caller, who accumulated it per-request to avoid
    /// sharing state across concurrent requests on the same translator.
    pub fn emit_usage_beacons(&mut self, languages: FastHashSet<&'static str>) -> Vec<Event> {
        let mut events = Vec::new();
        let timestamp_s = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        // Emit `datadog.agent.otlp.metrics` — one per request, value 1, no tags.
        if let Some(context) = self.context_resolver.resolve_with_optional_host_and_origin_tags(
            "datadog.agent.otlp.metrics",
            Some(&self.default_hostname),
            std::iter::empty::<&str>(),
            SharedTagSet::default(),
        ) {
            let metric = Metric::from_parts(
                context,
                MetricValues::gauge((timestamp_s, 1.0)),
                MetricMetadata::default().with_source_type(Some(std::sync::Arc::from("System"))),
            );
            events.push(Event::Metric(metric));
        }

        // Emit `datadog.agent.otlp.runtime_metrics` — one per detected language, value 1,
        // tag `language:<language>`.
        for language in languages {
            let tag = format!("language:{}", language);
            let tags = [tag];

            if let Some(context) = self.context_resolver.resolve_with_optional_host_and_origin_tags(
                "datadog.agent.otlp.runtime_metrics",
                Some(&self.default_hostname),
                tags.iter().map(|t| t.as_str()),
                SharedTagSet::default(),
            ) {
                let metric = Metric::from_parts(
                    context,
                    MetricValues::gauge((timestamp_s, 1.0)),
                    MetricMetadata::default().with_source_type(Some(std::sync::Arc::from("System"))),
                );
                events.push(Event::Metric(metric));
            }
        }

        events
    }
}

#[cfg(test)]
mod tests {
    use otlp_protos::opentelemetry::proto::common::v1::KeyValue as OtlpKeyValue;
    use saluki_core::data_model::event::metric::MetricValues;

    use super::super::tests::{gauge_metric_named, metric_by_name, resource_metrics_with_metric};
    use super::*;
    use crate::sources::otlp::Metrics;

    #[test]
    fn emit_usage_beacons_emits_metrics_beacon_with_value_1_and_default_host() {
        let mut translator = OtlpMetricsTranslator::for_tests();

        let events = translator.emit_usage_beacons(FastHashSet::default());

        let beacon = metric_by_name(&events, "datadog.agent.otlp.metrics");
        assert_eq!(
            beacon.values(),
            &MetricValues::gauge((SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs(), 1.0,))
        );
        assert_eq!(beacon.context().host(), Some("default-host"));
        // No tags on the metrics beacon.
        let tags: Vec<String> = beacon.context().tags().into_iter().map(|t| t.to_string()).collect();
        assert!(tags.is_empty(), "metrics beacon should have no tags");
    }

    #[test]
    fn emit_usage_beacons_emits_no_runtime_beacons_when_no_runtime_metrics() {
        let mut translator = OtlpMetricsTranslator::for_tests();

        let events = translator.emit_usage_beacons(FastHashSet::default());

        let runtime_beacons: Vec<_> = events
            .iter()
            .filter_map(|e| e.try_as_metric())
            .filter(|m| m.context().name() == "datadog.agent.otlp.runtime_metrics")
            .collect();
        assert!(
            runtime_beacons.is_empty(),
            "no runtime_metrics beacons should be emitted when no runtime metrics were translated"
        );
    }

    #[test]
    fn emit_usage_beacons_emits_runtime_beacon_for_go_language() {
        let metrics = Metrics::for_tests();
        let mut translator = OtlpMetricsTranslator::for_tests();

        let resource_metrics = resource_metrics_with_metric(gauge_metric_named("process.runtime.go.goroutines"));
        let (_, languages) = translator.translate_metrics(resource_metrics, &metrics).unwrap();

        let events = translator.emit_usage_beacons(languages);

        let runtime_beacon = metric_by_name(&events, "datadog.agent.otlp.runtime_metrics");
        assert_eq!(
            runtime_beacon.values(),
            &MetricValues::gauge((SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs(), 1.0,))
        );
        assert_eq!(runtime_beacon.context().host(), Some("default-host"));
        let tags: Vec<String> = runtime_beacon
            .context()
            .tags()
            .into_iter()
            .map(|t| t.to_string())
            .collect();
        assert_eq!(tags, vec!["language:go"]);
    }

    #[test]
    fn emit_usage_beacons_deduplicates_languages_within_a_request() {
        let metrics = Metrics::for_tests();
        let mut translator = OtlpMetricsTranslator::for_tests();

        let resource_metrics = resource_metrics_with_metric(gauge_metric_named("process.runtime.go.goroutines"));
        let (_, mut languages) = translator.translate_metrics(resource_metrics, &metrics).unwrap();
        let resource_metrics = resource_metrics_with_metric(gauge_metric_named("process.runtime.go.gc.pause"));
        let (_, langs2) = translator.translate_metrics(resource_metrics, &metrics).unwrap();
        languages.extend(langs2);

        let events = translator.emit_usage_beacons(languages);

        let runtime_beacons: Vec<_> = events
            .iter()
            .filter_map(|e| e.try_as_metric())
            .filter(|m| m.context().name() == "datadog.agent.otlp.runtime_metrics")
            .collect();
        assert_eq!(
            runtime_beacons.len(),
            1,
            "duplicate Go metrics should produce one language beacon"
        );
    }

    #[test]
    fn emit_usage_beacons_emits_one_beacon_per_distinct_language() {
        let metrics = Metrics::for_tests();
        let mut translator = OtlpMetricsTranslator::for_tests();

        let resource_metrics = resource_metrics_with_metric(gauge_metric_named("process.runtime.go.goroutines"));
        let (_, mut languages) = translator.translate_metrics(resource_metrics, &metrics).unwrap();
        let resource_metrics = resource_metrics_with_metric(gauge_metric_named("process.runtime.jvm.memory.usage"));
        let (_, langs2) = translator.translate_metrics(resource_metrics, &metrics).unwrap();
        languages.extend(langs2);

        let events = translator.emit_usage_beacons(languages);

        let runtime_beacons: Vec<_> = events
            .iter()
            .filter_map(|e| e.try_as_metric())
            .filter(|m| m.context().name() == "datadog.agent.otlp.runtime_metrics")
            .collect();
        assert_eq!(
            runtime_beacons.len(),
            2,
            "two distinct languages should produce two beacons"
        );

        let languages: Vec<String> = runtime_beacons
            .iter()
            .flat_map(|m| m.context().tags().into_iter().map(|t| t.to_string()))
            .collect();
        assert!(languages.contains(&"language:go".to_string()));
        assert!(languages.contains(&"language:jvm".to_string()));
    }

    #[test]
    fn emit_usage_beacons_beacon_host_is_default_not_resource_host() {
        let metrics = Metrics::for_tests();
        let mut translator = OtlpMetricsTranslator::for_tests();

        let mut resource_metrics = resource_metrics_with_metric(gauge_metric_named("process.runtime.go.goroutines"));
        resource_metrics.resource = Some(otlp_protos::opentelemetry::proto::resource::v1::Resource {
            attributes: vec![OtlpKeyValue {
                key: "host.name".to_string(),
                value: Some(otlp_protos::opentelemetry::proto::common::v1::AnyValue {
                    value: Some(
                        otlp_protos::opentelemetry::proto::common::v1::any_value::Value::StringValue(
                            "resource-host".to_string(),
                        ),
                    ),
                }),
            }],
            ..Default::default()
        });
        let (_, languages) = translator.translate_metrics(resource_metrics, &metrics).unwrap();

        let events = translator.emit_usage_beacons(languages);

        // The beacon should use the default hostname, not the resource-derived host.
        let beacon = metric_by_name(&events, "datadog.agent.otlp.metrics");
        assert_eq!(beacon.context().host(), Some("default-host"));

        let runtime_beacon = metric_by_name(&events, "datadog.agent.otlp.runtime_metrics");
        assert_eq!(runtime_beacon.context().host(), Some("default-host"));
    }
}
