//! Pure metric tag filterlist and tag value allowlist kernel.
//!
//! Removes, retains, or replaces tags on counter and sketch-backed metrics based on per-metric whole-tag rules and
//! per-prefix tag value allowlists.

use std::collections::hash_map::Entry;

use saluki_common::collections::{FastHashMap, FastHashSet};
use saluki_core::data_model::{
    event::metric::{context::TagSetMutViewState, Metric, MetricValues},
    tags::Tag,
};
use serde::{Deserialize, Serialize};

/// One whole-tag filter entry.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct MetricTagFilterEntry {
    /// Metric name the entry applies to. An entry with an empty name is ignored.
    pub metric_name: String,

    /// Whether the listed tags are kept or removed.
    pub action: FilterAction,

    /// Tag keys the action applies to.
    pub tags: Vec<String>,
}

/// Whether a whole-tag filter entry keeps or removes its tags.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum FilterAction {
    /// Keep only the listed tags.
    Include,

    /// Remove the listed tags.
    Exclude,
}

/// One tag value allowlist entry.
///
/// Matching is exact and case-sensitive, including whitespace.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct MetricTagValueAllowlistEntry {
    /// Metric-name prefix the entry applies to.
    pub metric_prefix: String,

    /// Tag key whose values are constrained. Bare tags have no value and are unchanged.
    pub tag_name: String,

    /// Tag values retained unchanged. An empty list treats every key/value tag as a mismatch.
    pub values: Vec<String>,

    /// Action applied when a tag value is absent from `values`.
    pub on_miss: TagValueMismatchAction,

    /// Replacement value used when `on_miss` is [`TagValueMismatchAction::Replace`].
    pub replacement: String,
}

/// Action applied when a tag value is absent from its allowlist.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum TagValueMismatchAction {
    /// Removes the tag.
    Remove,

    /// Replaces the tag value with the entry's replacement.
    Replace,
}

#[derive(Clone, Eq, PartialEq)]
enum CompiledMismatchAction {
    Remove,
    Replace(Tag),
}

#[derive(Clone)]
struct CompiledTagValueAllowlist {
    allowed_values: FastHashSet<String>,
    on_miss: CompiledMismatchAction,
}

#[derive(Clone)]
struct CompiledKeyFilter {
    is_exclude: bool,
    tag_names: FastHashSet<String>,
}

#[derive(Clone)]
struct CompiledValuePrefixFilter {
    metric_prefix: String,
    value_allowlists: FastHashMap<String, CompiledTagValueAllowlist>,
}

/// Precompiled per-prefix tag value allowlist table, reusable across whole-tag filter reloads.
#[derive(Clone, Default)]
pub struct CompiledValuePrefixFilters(Vec<CompiledValuePrefixFilter>);

impl CompiledValuePrefixFilters {
    /// Compiles value allowlist entries into a sorted prefix lookup table.
    pub fn compile(entries: &[MetricTagValueAllowlistEntry]) -> Self {
        let mut filters = CompiledFilters::default();
        add_value_allowlists(&mut filters, entries);
        filters.value_prefix_filters
    }

    fn sort(&mut self) {
        self.0
            .sort_unstable_by(|left, right| left.metric_prefix.cmp(&right.metric_prefix));
    }

    fn get(&self, metric_name: &str) -> Option<&CompiledValuePrefixFilter> {
        find_matching_prefix(&self.0, metric_name, |filter| filter.metric_prefix.as_str())
    }

    /// Returns the total number of compiled value-allowlist rules.
    pub fn rule_count(&self) -> usize {
        self.0.iter().map(|filter| filter.value_allowlists.len()).sum()
    }
}

fn find_matching_prefix<'a, T>(entries: &'a [T], value: &str, prefix: impl Fn(&T) -> &str) -> Option<&'a T> {
    match entries.binary_search_by(|candidate| prefix(candidate).cmp(value)) {
        Ok(index) => Some(&entries[index]),
        Err(index) if index > 0 && value.starts_with(prefix(&entries[index - 1])) => Some(&entries[index - 1]),
        _ => None,
    }
}

/// Compiled exact-name whole-tag filters and metric-prefix value filters.
#[derive(Clone, Default)]
pub struct CompiledFilters {
    key_filters: FastHashMap<String, CompiledKeyFilter>,
    value_prefix_filters: CompiledValuePrefixFilters,
}

impl CompiledFilters {
    /// Returns the total number of compiled whole-tag and value-allowlist rules.
    pub fn rule_count(&self) -> usize {
        self.key_filters.len() + self.value_prefix_filters.rule_count()
    }

    /// Returns `true` when no rules are configured.
    pub fn is_empty(&self) -> bool {
        self.rule_count() == 0
    }

    /// Returns `true` when a whole-tag filter rule is registered for `metric_name`.
    pub fn has_key_filter(&self, metric_name: &str) -> bool {
        self.key_filters.contains_key(metric_name)
    }

    /// Returns the compiled value-prefix filters for reuse across whole-tag updates.
    pub fn value_prefix_filters(&self) -> &CompiledValuePrefixFilters {
        &self.value_prefix_filters
    }
}

/// Outcome of attempting to apply tag filter rules to a metric.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FilterMetricTagsOutcome {
    /// No rule existed for the metric name.
    RuleMiss,
    /// A rule existed, but applying it did not change any tags.
    NoChange,
    /// A rule existed and modified one or more tags.
    Modified {
        /// Total number of instrumented and origin tags removed or replaced.
        removed_tags: usize,
    },
}

/// Returns whether `metric` is eligible for tag filtering (counters and sketch-backed metrics).
#[inline]
pub fn is_tag_filter_eligible(metric: &Metric) -> bool {
    metric.values().is_sketch() || matches!(metric.values(), MetricValues::Counter(_))
}

/// Compiles whole-tag filter entries into an O(1) lookup table.
///
/// Merge rules:
/// - Same metric name + same action -> union of tag key sets.
/// - Same metric name + conflicting actions -> `Exclude` wins.
pub fn compile_filters(entries: &[MetricTagFilterEntry]) -> CompiledFilters {
    let mut filters = CompiledFilters::default();

    for entry in entries {
        if entry.metric_name.is_empty() {
            continue;
        }

        let is_exclude = entry.action == FilterAction::Exclude;
        let mut tag_set = FastHashSet::default();
        tag_set.reserve(entry.tags.len());
        tag_set.extend(entry.tags.iter().cloned());

        match filters.key_filters.entry(entry.metric_name.clone()) {
            Entry::Vacant(vacant) => {
                vacant.insert(CompiledKeyFilter {
                    is_exclude,
                    tag_names: tag_set,
                });
            }
            Entry::Occupied(mut occupied) => {
                let existing = occupied.get_mut();
                if existing.is_exclude == is_exclude {
                    existing.tag_names.extend(tag_set);
                } else if is_exclude {
                    existing.is_exclude = true;
                    existing.tag_names = tag_set;
                }
            }
        }
    }

    filters
}

/// Adds tag value allowlist entries to `filters`.
pub fn add_value_allowlists(filters: &mut CompiledFilters, entries: &[MetricTagValueAllowlistEntry]) {
    for entry in entries {
        let on_miss = match entry.on_miss {
            TagValueMismatchAction::Remove => CompiledMismatchAction::Remove,
            TagValueMismatchAction::Replace => {
                CompiledMismatchAction::Replace(Tag::from(format!("{}:{}", entry.tag_name, entry.replacement)))
            }
        };

        let mut allowed_values = FastHashSet::default();
        allowed_values.reserve(entry.values.len());
        allowed_values.extend(entry.values.iter().cloned());
        let allowlist = CompiledTagValueAllowlist {
            allowed_values,
            on_miss,
        };

        if let Some(filter) = filters
            .value_prefix_filters
            .0
            .iter_mut()
            .find(|filter| filter.metric_prefix == entry.metric_prefix)
        {
            filter.value_allowlists.insert(entry.tag_name.clone(), allowlist);
        } else {
            let mut value_allowlists = FastHashMap::default();
            value_allowlists.insert(entry.tag_name.clone(), allowlist);
            filters.value_prefix_filters.0.push(CompiledValuePrefixFilter {
                metric_prefix: entry.metric_prefix.clone(),
                value_allowlists,
            });
        }
    }

    filters.value_prefix_filters.sort();
}

/// Compiles whole-tag entries together with precompiled value-prefix filters.
pub fn compile_filters_with_values(
    entries: &[MetricTagFilterEntry], value_prefix_filters: CompiledValuePrefixFilters,
) -> CompiledFilters {
    let mut filters = compile_filters(entries);
    filters.value_prefix_filters = value_prefix_filters;
    filters
}

/// Compiles both whole-tag entries and tag value allowlists into one [`CompiledFilters`] table.
pub fn compile_all_filters(
    entries: &[MetricTagFilterEntry], value_allowlists: &[MetricTagValueAllowlistEntry],
) -> CompiledFilters {
    let mut filters = compile_filters(entries);
    add_value_allowlists(&mut filters, value_allowlists);
    filters
}

#[inline]
fn should_keep_tag(tag: &Tag, is_exclude: bool, names: &FastHashSet<String>) -> bool {
    is_exclude != names.contains(tag.as_borrowed().name())
}

#[inline]
fn filter_tag(
    tag: &Tag, key_filter: Option<&CompiledKeyFilter>, value_filter: Option<&CompiledValuePrefixFilter>,
    replacements: &mut Vec<Tag>,
) -> bool {
    if let Some(key_filter) = key_filter {
        if !should_keep_tag(tag, key_filter.is_exclude, &key_filter.tag_names) {
            return false;
        }
    }

    let tag = tag.as_borrowed();

    let Some(value) = tag.value() else {
        return true;
    };
    let Some(allowlist) = value_filter.and_then(|filter| filter.value_allowlists.get(tag.name())) else {
        return true;
    };
    if allowlist.allowed_values.contains(value) {
        return true;
    }

    match &allowlist.on_miss {
        CompiledMismatchAction::Remove => false,
        CompiledMismatchAction::Replace(replacement) if replacement.as_str() == tag.as_ref() => true,
        CompiledMismatchAction::Replace(replacement) => {
            replacements.push(replacement.clone());
            false
        }
    }
}

/// Filters the tags of `metric` according to `filters`.
///
/// Callers apply this function only to counters and sketch-backed metrics (see [`is_tag_filter_eligible`]).
/// If no rule applies or no tags change, the metric context is left untouched without allocating.
#[inline]
pub fn filter_metric_tags(
    metric: &mut Metric, state: &mut TagSetMutViewState, filters: &CompiledFilters,
) -> FilterMetricTagsOutcome {
    let metric_name = metric.context().name().clone();
    let key_filter = filters.key_filters.get(metric_name.as_ref());
    let value_filter = filters.value_prefix_filters.get(metric_name.as_ref());
    if key_filter.is_none() && value_filter.is_none() {
        return FilterMetricTagsOutcome::RuleMiss;
    }

    let mut tag_replacements = Vec::new();
    let mut origin_tag_replacements = Vec::new();
    let mut tag_set_view = metric.context_mut().tags_mut_view(state);
    tag_set_view.retain_tags(|tag| filter_tag(tag, key_filter, value_filter, &mut tag_replacements));
    tag_set_view.retain_origin_tags(|tag| filter_tag(tag, key_filter, value_filter, &mut origin_tag_replacements));
    let total_removed = tag_set_view.finish();

    if !tag_replacements.is_empty() || !origin_tag_replacements.is_empty() {
        metric.context_mut().with_tag_sets_mut(|tags, origin_tags| {
            for replacement in tag_replacements {
                tags.insert_tag(replacement);
            }
            for replacement in origin_tag_replacements {
                origin_tags.insert_tag(replacement);
            }
        });
    }

    if total_removed == 0 {
        FilterMetricTagsOutcome::NoChange
    } else {
        FilterMetricTagsOutcome::Modified {
            removed_tags: total_removed,
        }
    }
}

/// Pure synchronous kernel holding compiled tag filter rules and scratch view state.
#[derive(Default)]
pub struct TagFilterKernel {
    filters: CompiledFilters,
    view_state: TagSetMutViewState,
}

impl TagFilterKernel {
    /// Creates a new `TagFilterKernel` from whole-tag entries and value allowlist entries.
    pub fn new(entries: &[MetricTagFilterEntry], value_allowlists: &[MetricTagValueAllowlistEntry]) -> Self {
        Self {
            filters: compile_all_filters(entries, value_allowlists),
            view_state: TagSetMutViewState::default(),
        }
    }

    /// Returns `true` when no rules are configured.
    pub fn is_noop(&self) -> bool {
        self.filters.is_empty()
    }

    /// Applies the compiled tag filter rules to `metric` if its type is eligible.
    pub fn apply(&mut self, metric: &mut Metric) -> FilterMetricTagsOutcome {
        if !is_tag_filter_eligible(metric) {
            return FilterMetricTagsOutcome::RuleMiss;
        }

        filter_metric_tags(metric, &mut self.view_state, &self.filters)
    }
}

#[cfg(test)]
mod tests {
    use saluki_core::data_model::event::metric::context::Context;

    use super::*;

    #[test]
    fn excludes_and_replaces_tags_on_counters_and_sketches_only() {
        let entries = vec![MetricTagFilterEntry {
            metric_name: "req.count".to_string(),
            action: FilterAction::Exclude,
            tags: vec!["drop_me".to_string()],
        }];
        let value_allowlists = vec![MetricTagValueAllowlistEntry {
            metric_prefix: "req.".to_string(),
            tag_name: "status".to_string(),
            values: vec!["200".to_string()],
            on_miss: TagValueMismatchAction::Replace,
            replacement: "other".to_string(),
        }];

        let mut kernel = TagFilterKernel::new(&entries, &value_allowlists);

        let mut counter = Metric::counter(
            Context::from_static_parts("req.count", &["drop_me:1", "status:500", "env:prod"]),
            1.0,
        );
        assert_eq!(
            kernel.apply(&mut counter),
            FilterMetricTagsOutcome::Modified { removed_tags: 2 }
        );
        assert!(!counter.context().tags().has_tag(Tag::from("drop_me:1")));
        assert!(counter.context().tags().has_tag(Tag::from("status:other")));
        assert!(counter.context().tags().has_tag(Tag::from("env:prod")));

        // Gauges are not eligible and remain untouched.
        let mut gauge = Metric::gauge(
            Context::from_static_parts("req.count", &["drop_me:1", "status:500"]),
            1.0,
        );
        assert_eq!(kernel.apply(&mut gauge), FilterMetricTagsOutcome::RuleMiss);
        assert!(gauge.context().tags().has_tag(Tag::from("drop_me:1")));
    }
}
