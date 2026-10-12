//! Telemetry for tag projection.

use metrics::{Counter, Gauge};
use saluki_core::{components::ComponentContext, observability::ComponentMetricsExt as _};
use saluki_metrics::MetricsBuilder;

use super::FilterMetricTagsOutcome;

/// What an accumulating telemetry counted since it last published.
#[derive(Clone, Copy, Default)]
struct Pending {
    rule_hits: u64,
    rule_misses: u64,
    noop_hits: u64,
    metrics_modified: u64,
    tags_filtered: u64,
}

/// The native telemetry of tag projection, under the names the native tag filterlist transform reports.
///
/// Rule hits and misses count metrics of the kinds tag projection rewrites; metrics of other kinds record nothing. A
/// result served from the context memo records the outcome it caches.
///
/// Telemetry created with [`new`][Self::new] writes its counters on every metric it records. Telemetry created with
/// [`for_kernel`][Self::for_kernel] counts the outcomes in integers of its own, and adds them to its counters when the
/// kernel's host [publishes][Self::publish] it, and when it is dropped: kernels of one component that run in parallel
/// then never write a shared counter per metric. A clone shares the counters, and accumulates on its own from nothing.
pub struct TagProjectionTelemetry {
    rule_hits: Counter,
    rule_misses: Counter,
    noop_hits: Counter,
    metrics_modified: Counter,
    tags_filtered: Counter,
    size: Gauge,
    updates: Counter,
    /// What the telemetry counted and has not published, if it accumulates.
    pending: Option<Pending>,
}

impl TagProjectionTelemetry {
    /// Registers the telemetry with `builder`, writing its counters as it records outcomes.
    pub fn new(builder: &MetricsBuilder) -> Self {
        Self {
            rule_hits: builder.register_counter("tag_filterlist_rule_hits_total"),
            rule_misses: builder.register_counter("tag_filterlist_rule_misses_total"),
            noop_hits: builder.register_counter("tag_filterlist_noop_hits_total"),
            metrics_modified: builder.register_counter("tag_filterlist_metrics_modified_total"),
            tags_filtered: builder.register_counter("tag_filterlist_tags_filtered_total"),
            size: builder.register_gauge("tag_filterlist_size"),
            updates: builder.register_counter("tag_filterlist_updates_total"),
            pending: None,
        }
    }

    /// Registers only the telemetry a [`TagProjection`][super::TagProjection] kernel records over its life, for the
    /// component `component`: everything but the rule count and the update count. The outcomes it records accumulate
    /// until the kernel's host publishes them.
    ///
    /// A kernel's rules never change: a change of rules builds a new kernel, so whatever installs it records the rule
    /// count and counts the update instead, once the kernel's rules are in effect rather than when it is built.
    /// [`set_size`][Self::set_size] and [`increment_updates`][Self::increment_updates] record nothing on this
    /// telemetry.
    pub fn for_kernel(component: &ComponentContext) -> Self {
        let builder = MetricsBuilder::from_component_context(component);
        Self {
            rule_hits: builder.register_counter("tag_filterlist_rule_hits_total"),
            rule_misses: builder.register_counter("tag_filterlist_rule_misses_total"),
            noop_hits: builder.register_counter("tag_filterlist_noop_hits_total"),
            metrics_modified: builder.register_counter("tag_filterlist_metrics_modified_total"),
            tags_filtered: builder.register_counter("tag_filterlist_tags_filtered_total"),
            size: Gauge::noop(),
            updates: Counter::noop(),
            pending: Some(Pending::default()),
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
            pending: None,
        }
    }

    /// Records the outcome of projecting one metric's tags.
    pub fn record(&mut self, outcome: FilterMetricTagsOutcome) {
        let mut counts = Pending::default();
        match outcome {
            FilterMetricTagsOutcome::RuleMiss => counts.rule_misses = 1,
            FilterMetricTagsOutcome::NoChange => {
                counts.rule_hits = 1;
                counts.noop_hits = 1;
            }
            FilterMetricTagsOutcome::Modified { removed_tags } => {
                counts.rule_hits = 1;
                counts.metrics_modified = 1;
                counts.tags_filtered = removed_tags as u64;
            }
        }
        match self.pending.as_mut() {
            Some(pending) => {
                pending.rule_hits += counts.rule_hits;
                pending.rule_misses += counts.rule_misses;
                pending.noop_hits += counts.noop_hits;
                pending.metrics_modified += counts.metrics_modified;
                pending.tags_filtered += counts.tags_filtered;
            }
            None => self.write(counts),
        }
    }

    /// Adds what the telemetry counted since it last published to its counters, if it accumulates; otherwise, does
    /// nothing, since it wrote everything as it recorded it.
    pub fn publish(&mut self) {
        if let Some(pending) = self.pending.as_mut().map(std::mem::take) {
            self.write(pending);
        }
    }

    /// Adds `counts` to the counters, one write per counter that changed.
    fn write(&self, counts: Pending) {
        for (counter, count) in [
            (&self.rule_hits, counts.rule_hits),
            (&self.rule_misses, counts.rule_misses),
            (&self.noop_hits, counts.noop_hits),
            (&self.metrics_modified, counts.metrics_modified),
            (&self.tags_filtered, counts.tags_filtered),
        ] {
            if count > 0 {
                counter.increment(count);
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

impl Clone for TagProjectionTelemetry {
    fn clone(&self) -> Self {
        Self {
            rule_hits: self.rule_hits.clone(),
            rule_misses: self.rule_misses.clone(),
            noop_hits: self.noop_hits.clone(),
            metrics_modified: self.metrics_modified.clone(),
            tags_filtered: self.tags_filtered.clone(),
            size: self.size.clone(),
            updates: self.updates.clone(),
            pending: self.pending.map(|_| Pending::default()),
        }
    }
}

impl Drop for TagProjectionTelemetry {
    fn drop(&mut self) {
        // What a kernel counted since its host last published reaches the counters when the kernel is dropped.
        self.publish();
    }
}
