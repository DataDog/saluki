//! What each operator reports: the series a node running it records, beside the component series every node has.
//!
//! An operator's kernel registers its series through the telemetry its build context gives it, so the description of
//! what it reports belongs with the kernel's factory, here, where a host and the editor, which has only this crate, can
//! both read it. Each series says who records it ([`Recorder`]): the kernel, the region host running the node, or the
//! installer, from the plan of the generation that commits.
//!
//! A host can add series a node reports because of its role, such as the native filter a `split` stands for: those
//! depend on the node's name, not its operator, and are the host's to describe.

/// The kind of a reported series.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum SeriesKind {
    /// A counter.
    Counter,

    /// A gauge.
    Gauge,
}

/// A value the installer reads from the plan of the generation that commits, never from one still being prepared.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum PlanValue {
    /// The number of distinct values a `split`'s predicate matches the intake name against: the length of its list, as
    /// the plan writes it, each value once.
    ListLength,

    /// The number of entries a name matcher keeps for the values a `split`'s predicate matches the intake name against,
    /// exactly or by prefix: its distinct names, or the prefixes that no other prefix covers.
    MatcherEntries {
        /// Whether to count the values matched by prefix, rather than exactly.
        prefixes: bool,
    },

    /// The number of rules a `tag_projection` node's `rules` and `value_rules` put in effect: one per metric name with
    /// tag rules, plus one per metric prefix and tag with a value rule.
    TagProjectionRules,

    /// The number of configuration changes, after the first installation, that changed the node's parameters.
    ParamChanges,
}

/// Who records a reported series, and so where its value comes from.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum Recorder {
    /// The node's kernel, through the telemetry its build context registers, labeled with the node's component.
    Kernel,

    /// The node's kernel's context cache, labeled with the cache's identifier (`cache_id`) rather than a component.
    Cache,

    /// The region host running the node, counting the events that leave the named output port: a `split` has no
    /// kernel.
    Port(&'static str),

    /// The node's kernel, counting the events that enter its step, labeled with the node's component; or, where the
    /// region running it is named after it and its input counts them under that component, the region's input.
    StepInput,

    /// The installer, from the plan of the generation that commits.
    Plan(PlanValue),
}

/// One series a node reports, with its node's labels.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ReportedSeries {
    /// The series' name.
    pub name: &'static str,

    /// The series' kind.
    pub kind: SeriesKind,

    /// Tags the series carries beside its node's labels, as `key:value`.
    pub tags: &'static [&'static str],

    /// Who records it.
    pub recorder: Recorder,
}

impl ReportedSeries {
    /// Returns a counter without tags.
    pub const fn counter(name: &'static str, recorder: Recorder) -> Self {
        Self {
            name,
            kind: SeriesKind::Counter,
            tags: &[],
            recorder,
        }
    }

    /// Returns a gauge without tags.
    pub const fn gauge(name: &'static str, recorder: Recorder) -> Self {
        Self {
            name,
            kind: SeriesKind::Gauge,
            tags: &[],
            recorder,
        }
    }

    /// Returns this series with `tags`.
    pub const fn with_tags(self, tags: &'static [&'static str]) -> Self {
        Self { tags, ..self }
    }

    /// Returns `true` if the series carries every one of `tags`.
    pub fn has_tags<'a>(&self, mut tags: impl Iterator<Item = &'a str>) -> bool {
        tags.all(|tag| self.tags.contains(&tag))
    }
}

/// One of aggregation's per-type context gauges, for one metric type.
const fn by_type(name: &'static str, tags: &'static [&'static str]) -> ReportedSeries {
    ReportedSeries::gauge(name, Recorder::Kernel).with_tags(tags)
}

/// What an `aggregate` node reports: the native aggregation component's telemetry, which the Datadog Agent's
/// `aggregator.*` metrics are remapped from. Every lineage of the node reports under the same series: its context
/// gauges add up, and each lineage counts its own flushes.
pub(super) const AGGREGATE: &[ReportedSeries] = {
    use Recorder::*;
    const COUNTER: &[&str] = &["metric_type:counter"];
    const GAUGE: &[&str] = &["metric_type:gauge"];
    const RATE: &[&str] = &["metric_type:rate"];
    const SET: &[&str] = &["metric_type:set"];
    const HISTOGRAM: &[&str] = &["metric_type:histogram"];
    const DISTRIBUTION: &[&str] = &["metric_type:distribution"];
    &[
        ReportedSeries::gauge("aggregate_active_contexts", Kernel),
        by_type("aggregate_active_contexts_by_type", COUNTER),
        by_type("aggregate_active_contexts_by_type", GAUGE),
        by_type("aggregate_active_contexts_by_type", RATE),
        by_type("aggregate_active_contexts_by_type", SET),
        by_type("aggregate_active_contexts_by_type", HISTOGRAM),
        by_type("aggregate_active_contexts_by_type", DISTRIBUTION),
        by_type("aggregate_active_contexts_bytes_by_type", COUNTER),
        by_type("aggregate_active_contexts_bytes_by_type", GAUGE),
        by_type("aggregate_active_contexts_bytes_by_type", RATE),
        by_type("aggregate_active_contexts_bytes_by_type", SET),
        by_type("aggregate_active_contexts_bytes_by_type", HISTOGRAM),
        by_type("aggregate_active_contexts_bytes_by_type", DISTRIBUTION),
        // The metrics the context limit keeps out.
        ReportedSeries::counter("component_events_dropped_total", Kernel).with_tags(&["intentional:true"]),
        // Remapped `aggregator.number_of_flush`.
        ReportedSeries::counter("aggregate_flushes_total", Kernel),
        ReportedSeries::counter("aggregate_flushed_total", Kernel).with_tags(&["data_type:series"]),
        ReportedSeries::counter("aggregate_flushed_total", Kernel).with_tags(&["data_type:sketches"]),
        // Remapped `aggregator.processed`: what enters aggregation, wherever it runs.
        ReportedSeries::counter("component_events_received_total", StepInput),
    ]
};

/// What a `timestamp_split` node reports: the native no-aggregation pipeline's counters. The native batch duration
/// histogram and buffer-full drop counter belong to the native passthrough batcher, which the plan does not have.
pub(super) const TIMESTAMP_SPLIT: &[ReportedSeries] = &[
    ReportedSeries::counter("aggregate_passthrough_metrics_total", Recorder::Kernel),
    ReportedSeries::counter("aggregate_passthrough_flushes_total", Recorder::Kernel),
];

/// What a `tag_projection` node reports: its kernel's rule counters, and its context memo's cache counters under the
/// memo's identifier (and its size gauges, where the host's gauge mode has them, which are not listed). How many rules
/// are in effect, and how often they change, are the native tag filterlist's, which a host gives the node in that role.
pub(super) const TAG_PROJECTION: &[ReportedSeries] = {
    use Recorder::*;
    &[
        ReportedSeries::counter("tag_filterlist_rule_hits_total", Kernel),
        ReportedSeries::counter("tag_filterlist_rule_misses_total", Kernel),
        ReportedSeries::counter("tag_filterlist_noop_hits_total", Kernel),
        ReportedSeries::counter("tag_filterlist_metrics_modified_total", Kernel),
        ReportedSeries::counter("tag_filterlist_tags_filtered_total", Kernel),
        ReportedSeries::counter("cache_hits_total", Cache),
        ReportedSeries::counter("cache_misses_total", Cache),
        ReportedSeries::counter("cache_items_inserted_total", Cache),
        ReportedSeries::counter("cache_items_removed_total", Cache),
        ReportedSeries::counter("cache_items_evicted_total", Cache),
        ReportedSeries::counter("cache_items_expired_total", Cache),
    ]
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::operators::{names, OperatorRegistry};

    #[test]
    fn only_operators_whose_kernels_register_telemetry_report_series() {
        let reporting = OperatorRegistry::native()
            .iter()
            .filter(|def| !def.reports().is_empty())
            .map(|def| def.name())
            .collect::<Vec<_>>();
        assert_eq!(
            reporting,
            [names::TIMESTAMP_SPLIT, names::TAG_PROJECTION, names::AGGREGATE]
        );
        // A cache's series are labeled with the cache's identifier.
        assert!(AGGREGATE
            .iter()
            .all(|series| matches!(series.recorder, Recorder::Kernel | Recorder::StepInput)));
        assert!(TAG_PROJECTION
            .iter()
            .all(|series| matches!(series.recorder, Recorder::Kernel | Recorder::Cache)));
    }
}
