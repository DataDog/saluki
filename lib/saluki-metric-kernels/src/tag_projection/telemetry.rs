//! Telemetry for tag projection.

use metrics::{Counter, Gauge};
use saluki_core::{components::ComponentContext, observability::ComponentMetricsExt as _};
use saluki_metrics::MetricsBuilder;

use super::FilterMetricTagsOutcome;

/// The native telemetry of tag projection, under the names the native tag filterlist transform reports.
///
/// Rule hits and misses count metrics of the kinds tag projection rewrites; metrics of other kinds record nothing. A
/// result served from the context memo records the outcome it caches.
#[derive(Clone)]
pub struct TagProjectionTelemetry {
    rule_hits: Counter,
    rule_misses: Counter,
    noop_hits: Counter,
    metrics_modified: Counter,
    tags_filtered: Counter,
    size: Gauge,
    updates: Counter,
}

impl TagProjectionTelemetry {
    /// Registers the telemetry with `builder`.
    pub fn new(builder: &MetricsBuilder) -> Self {
        Self {
            rule_hits: builder.register_counter("tag_filterlist_rule_hits_total"),
            rule_misses: builder.register_counter("tag_filterlist_rule_misses_total"),
            noop_hits: builder.register_counter("tag_filterlist_noop_hits_total"),
            metrics_modified: builder.register_counter("tag_filterlist_metrics_modified_total"),
            tags_filtered: builder.register_counter("tag_filterlist_tags_filtered_total"),
            size: builder.register_gauge("tag_filterlist_size"),
            updates: builder.register_counter("tag_filterlist_updates_total"),
        }
    }

    /// Registers only the telemetry a [`TagProjection`][super::TagProjection] kernel records over its life, for the
    /// component `component`: everything but the update count.
    ///
    /// A kernel's rules never change: a change of rules builds a new kernel, so whatever installs it counts the update
    /// instead. [`increment_updates`][Self::increment_updates] records nothing on this telemetry.
    pub fn for_kernel(component: &ComponentContext) -> Self {
        let builder = MetricsBuilder::from_component_context(component);
        Self {
            rule_hits: builder.register_counter("tag_filterlist_rule_hits_total"),
            rule_misses: builder.register_counter("tag_filterlist_rule_misses_total"),
            noop_hits: builder.register_counter("tag_filterlist_noop_hits_total"),
            metrics_modified: builder.register_counter("tag_filterlist_metrics_modified_total"),
            tags_filtered: builder.register_counter("tag_filterlist_tags_filtered_total"),
            size: builder.register_gauge("tag_filterlist_size"),
            updates: Counter::noop(),
        }
    }

    /// Returns telemetry that records nothing.
    pub fn noop() -> Self {
        Self {
            rule_hits: Counter::noop(),
            rule_misses: Counter::noop(),
            noop_hits: Counter::noop(),
            metrics_modified: Counter::noop(),
            tags_filtered: Counter::noop(),
            size: Gauge::noop(),
            updates: Counter::noop(),
        }
    }

    /// Records the outcome of projecting one metric's tags.
    pub fn record(&self, outcome: FilterMetricTagsOutcome) {
        match outcome {
            FilterMetricTagsOutcome::RuleMiss => {
                self.rule_misses.increment(1);
            }
            FilterMetricTagsOutcome::NoChange => {
                self.rule_hits.increment(1);
                self.noop_hits.increment(1);
            }
            FilterMetricTagsOutcome::Modified { removed_tags } => {
                self.rule_hits.increment(1);
                self.metrics_modified.increment(1);
                self.tags_filtered.increment(removed_tags as u64);
            }
        }
    }

    /// Records the number of rules in effect: one per metric name with tag rules, plus one per metric prefix and tag
    /// with a value rule.
    pub fn set_size(&self, count: usize) {
        self.size.set(count as f64);
    }

    /// Records one replacement of the rules in effect.
    pub fn increment_updates(&self) {
        self.updates.increment(1);
    }
}
