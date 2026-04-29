use metrics::{Counter, Gauge};
use saluki_metrics::MetricsBuilder;

use super::FilterMetricTagsOutcome;

#[derive(Clone)]
pub struct Telemetry {
    rule_hits: Counter,
    rule_misses: Counter,
    noop_hits: Counter,
    metrics_modified: Counter,
    tags_filtered: Counter,
    rules_loaded: Gauge,
}

impl Telemetry {
    pub fn new(builder: &MetricsBuilder) -> Self {
        let me = Self {
            rule_hits: builder.register_debug_counter("tag_filterlist_rule_hits_total"),
            rule_misses: builder.register_debug_counter("tag_filterlist_rule_misses_total"),
            noop_hits: builder.register_debug_counter("tag_filterlist_noop_hits_total"),
            metrics_modified: builder.register_debug_counter("tag_filterlist_metrics_modified_total"),
            tags_filtered: builder.register_debug_counter("tag_filterlist_tags_filtered_total"),
            rules_loaded: builder.register_debug_gauge("tag_filterlist_rules_loaded"),
        };
        me.rules_loaded.set(0.0);
        me
    }

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

    /// Records the number of tag filter rules currently loaded by the
    /// transform. Called whenever the configuration is refreshed (initial
    /// build or `metric_tag_filterlist` Remote Config update).
    pub fn set_rules_loaded(&self, count: usize) {
        self.rules_loaded.set(count as f64);
    }
}
