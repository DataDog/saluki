use metrics::Counter;
use saluki_metrics::MetricsBuilder;

use super::FilterMetricTagsOutcome;

#[derive(Clone)]
pub struct Telemetry {
    rule_hits: Counter,
    rule_misses: Counter,
    noop_hits: Counter,
    metrics_modified: Counter,
    tags_filtered: Counter,
}

impl Telemetry {
    pub fn new(builder: &MetricsBuilder) -> Self {
        Self {
            rule_hits: builder.register_debug_counter("tag_filterlist_rule_hits_total"),
            rule_misses: builder.register_debug_counter("tag_filterlist_rule_misses_total"),
            noop_hits: builder.register_debug_counter("tag_filterlist_noop_hits_total"),
            metrics_modified: builder.register_debug_counter("tag_filterlist_metrics_modified_total"),
            tags_filtered: builder.register_debug_counter("tag_filterlist_tags_filtered_total"),
        }
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
}
