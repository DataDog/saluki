//! Helper for processing [`AggregatedMetricsState`] into Prometheus text exposition format.
//!
//! [`TelemetryProcessor`] wraps a [`PrometheusRenderer`] and an optional set of [`RemapperRule`]s.
//! It has two modes:
//!
//! - **No rules**: every metric in the state is rendered under its raw name and tags.
//! - **With rules**: each metric is matched against the rules; only matched metrics are rendered,
//!   under their remapped name and tags. Counters, gauges, and histograms are all supported in
//!   both modes.

use std::collections::BTreeMap;

use prometheus_exposition::{MetricType, PrometheusRenderer};
use stringtheory::MetaString;

use super::aggregated::{AggregatedMetricValue, AggregatedMetricsState};
use super::remapper::RemapperRule;

/// Processes an [`AggregatedMetricsState`] into a Prometheus text exposition payload.
///
/// Owns the underlying [`PrometheusRenderer`] (whose internal buffers are reused across calls) and
/// an optional set of [`RemapperRule`]s. See the module-level docs for the two rendering modes.
pub struct TelemetryProcessor {
    renderer: PrometheusRenderer,
    rules: Vec<RemapperRule>,
}

impl TelemetryProcessor {
    /// Creates a new `TelemetryProcessor` with no remapper rules.
    ///
    /// In this mode, every metric in the state is rendered under its raw name and tags.
    pub fn new() -> Self {
        Self {
            renderer: PrometheusRenderer::new(),
            rules: Vec::new(),
        }
    }

    /// Configures the set of [`RemapperRule`]s applied during rendering.
    ///
    /// In this mode, only metrics that match at least one rule are rendered; rule order matters
    /// (see [`RemapperRule::with_continued_matching`] for how a single source metric can fan out
    /// to multiple remapped metrics).
    pub fn with_remapper_rules(mut self, rules: Vec<RemapperRule>) -> Self {
        self.rules = rules;
        self
    }

    /// Processes the given state and returns the resulting Prometheus exposition payload.
    pub fn process(&mut self, state: &AggregatedMetricsState) -> String {
        self.renderer.clear();

        if self.rules.is_empty() {
            self.process_all(state);
        } else {
            self.process_remapped(state);
        }

        self.renderer.output().to_string()
    }

    fn process_all(&mut self, state: &AggregatedMetricsState) {
        // Group metrics first by name, then by sorted tag set. Duplicate (name, tags) entries are
        // merged so counters sum, gauges take the latest, and histograms merge buckets/sum/count.
        let mut groups: BTreeMap<MetaString, BTreeMap<Vec<MetaString>, AggregatedMetricValue>> = BTreeMap::new();

        state.visit_metrics(|context, value| {
            let mut tags: Vec<MetaString> = context.tags().into_iter().map(|tag| tag.clone().into_inner()).collect();
            tags.sort();

            let series = groups.entry(context.name().clone()).or_default();
            series
                .entry(tags)
                .and_modify(|existing| existing.merge(value))
                .or_insert_with(|| value.clone());
        });

        for (name, series) in &groups {
            render_group(&mut self.renderer, name, None, series);
        }
    }

    fn process_remapped(&mut self, state: &AggregatedMetricsState) {
        // Collect, deduplicate, and group matched metrics by their remapped name and tags.
        //
        // Multiple source metrics can remap to the same (name, tags) identity. We merge those via
        // `AggregatedMetricValue::merge`, and tags are sorted to normalize their order so identical
        // tag sets in different orders deduplicate correctly.
        let mut groups: BTreeMap<&'static str, BTreeMap<Vec<MetaString>, AggregatedMetricValue>> = BTreeMap::new();
        let mut help_text: BTreeMap<&'static str, &'static str> = BTreeMap::new();

        state.visit_metrics(|context, value| {
            for rule in &self.rules {
                if let Some(mut remapped) = rule.try_match_no_context(context) {
                    remapped.tags.sort();

                    let continue_matching = rule.should_continue_matching();
                    if let Some(text) = rule.help_text() {
                        help_text.entry(remapped.name).or_insert(text);
                    }
                    let series = groups.entry(remapped.name).or_default();
                    series
                        .entry(remapped.tags)
                        .and_modify(|existing| existing.merge(value))
                        .or_insert_with(|| value.clone());

                    if !continue_matching {
                        return;
                    }
                }
            }
        });

        for (name, series) in &groups {
            render_group(&mut self.renderer, name, help_text.get(name).copied(), series);
        }
    }
}

impl Default for TelemetryProcessor {
    fn default() -> Self {
        Self::new()
    }
}

fn render_group(
    renderer: &mut PrometheusRenderer, name: &str, help_text: Option<&str>,
    series: &BTreeMap<Vec<MetaString>, AggregatedMetricValue>,
) {
    // Determine the metric type from the first series value.
    let metric_type = match series.values().next() {
        Some(AggregatedMetricValue::Counter(_)) => MetricType::Counter,
        Some(AggregatedMetricValue::Gauge(_)) => MetricType::Gauge,
        Some(AggregatedMetricValue::Histogram(_)) => MetricType::Histogram,
        None => return,
    };

    match metric_type {
        MetricType::Counter | MetricType::Gauge => {
            let rendered = series.iter().map(|(tags, value)| (split_tags(tags), value.value()));
            renderer.render_scalar_group(name, metric_type, help_text, rendered);
        }
        MetricType::Histogram => {
            renderer.begin_group(name, metric_type, help_text);
            for (tags, value) in series {
                if let AggregatedMetricValue::Histogram(histogram) = value {
                    renderer.write_histogram_series(
                        split_tags(tags),
                        histogram.buckets(),
                        histogram.sum(),
                        histogram.count(),
                    );
                }
            }
            renderer.finish_group();
        }
        MetricType::Summary => {}
    }
}

/// Splits a sorted `key:value` tag list into Prometheus label pairs, dropping bare tags.
fn split_tags(tags: &[MetaString]) -> impl Iterator<Item = (&str, &str)> {
    tags.iter().filter_map(|tag| tag.as_ref().split_once(':'))
}

#[cfg(test)]
mod tests {
    use saluki_context::Context;

    use super::super::aggregated::AggregatedMetricsProcessor;
    use super::super::reflector::Processor as _;
    use super::*;
    use crate::data_model::event::{metric::Metric, Event};

    fn process_all(metrics: Vec<Event>) -> AggregatedMetricsState {
        let processor = AggregatedMetricsProcessor;
        let state = processor.build_initial_state();
        for metric in metrics {
            processor.process(metric, &state);
        }
        state
    }

    #[test]
    fn renders_counter_and_gauge_groups_without_rules() {
        let state = process_all(vec![
            Event::Metric(Metric::counter(
                Context::from_static_parts("adp.requests_total", &["method:get"]),
                10.0,
            )),
            Event::Metric(Metric::counter(
                Context::from_static_parts("adp.requests_total", &["method:post"]),
                3.0,
            )),
            Event::Metric(Metric::gauge(
                Context::from_static_parts("adp.queue_depth", &["queue:work"]),
                5.0,
            )),
        ]);

        let output = TelemetryProcessor::new().process(&state);

        assert!(output.contains("# TYPE adp__requests_total counter"));
        assert!(output.contains("adp__requests_total{method=\"get\"} 10"));
        assert!(output.contains("adp__requests_total{method=\"post\"} 3"));
        assert!(output.contains("# TYPE adp__queue_depth gauge"));
        assert!(output.contains("adp__queue_depth{queue=\"work\"} 5"));
    }

    #[test]
    fn renders_histogram_groups_without_rules() {
        let state = process_all(vec![
            Event::Metric(Metric::histogram(
                Context::from_static_parts("adp.latency_seconds", &["op:read"]),
                [0.001, 0.002, 0.5],
            )),
            Event::Metric(Metric::histogram(
                Context::from_static_parts("adp.latency_seconds", &["op:write"]),
                [0.01, 0.02],
            )),
        ]);

        let output = TelemetryProcessor::new().process(&state);

        assert!(output.contains("# TYPE adp__latency_seconds histogram"));
        assert!(output.contains("adp__latency_seconds_bucket{op=\"read\","));
        assert!(output.contains("adp__latency_seconds_bucket{op=\"write\","));
        assert!(output.contains("le=\"+Inf\""));
        assert!(output.contains("adp__latency_seconds_sum{op=\"read\"}"));
        assert!(output.contains("adp__latency_seconds_count{op=\"read\"} 3"));
        assert!(output.contains("adp__latency_seconds_count{op=\"write\"} 2"));
    }

    #[test]
    fn renders_only_matched_metrics_with_rules() {
        let state = process_all(vec![
            Event::Metric(Metric::counter(
                Context::from_static_parts("src.matched", &["component_id:x"]),
                42.0,
            )),
            Event::Metric(Metric::counter(
                Context::from_static_parts("src.unmatched", &["component_id:x"]),
                100.0,
            )),
        ]);

        let rules =
            vec![RemapperRule::by_name("src.matched", "dst.renamed").with_help_text("Renamed counter help text")];

        let output = TelemetryProcessor::new().with_remapper_rules(rules).process(&state);

        assert!(output.contains("# HELP dst__renamed Renamed counter help text"));
        assert!(output.contains("# TYPE dst__renamed counter"));
        assert!(output.contains("dst__renamed 42"));
        // The unmatched counter must not appear.
        assert!(!output.contains("unmatched"));
    }

    #[test]
    fn renders_histograms_through_rules() {
        let state = process_all(vec![Event::Metric(Metric::histogram(
            Context::from_static_parts("src.latency_seconds", &["op:read"]),
            [0.001, 0.5],
        ))]);

        let rules = vec![RemapperRule::by_name("src.latency_seconds", "dst.latency_seconds")
            .with_original_tags(["op"])
            .with_help_text("Remapped latency")];

        let output = TelemetryProcessor::new().with_remapper_rules(rules).process(&state);

        assert!(output.contains("# HELP dst__latency_seconds Remapped latency"));
        assert!(output.contains("# TYPE dst__latency_seconds histogram"));
        assert!(output.contains("dst__latency_seconds_bucket{op=\"read\","));
        assert!(output.contains("dst__latency_seconds_count{op=\"read\"} 2"));
    }
}
