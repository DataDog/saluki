//! A frozen copy of the metric tag filterlist as it was before tag projection moved into this crate.
//!
//! The functions below are the original `agent-data-plane` transform (`tag_filterlist`), with the configuration entry
//! types and the value rule validation it used from `agent-data-plane-config`, copied verbatim with only their imports
//! and visibility adjusted, and unused derives and documentation left out. The live rule view, the component builder,
//! and the memory bounds are left out, since they carry no per-metric semantics; the run loop's processing of one
//! buffer is kept as [`TagFilterlist::process_buffer`]. The property tests compare
//! [`TagProjection`][crate::tag_projection::TagProjection] with this copy, so that the kernel keeps the native behavior
//! while the legacy component calls the kernel. Do not change it.

use std::{collections::hash_map::Entry, fmt, num::NonZeroUsize, time::Duration};

use metrics::{Counter, Gauge};
use saluki_common::collections::{FastHashMap, FastHashSet};
use saluki_core::{
    cache::{Cache, CacheBuilder},
    data_model::{
        event::metric::{
            context::{Context, TagSetMutViewState},
            Metric, MetricValues,
        },
        tags::Tag,
    },
    topology::EventsBuffer,
};
use saluki_error::{generic_error, GenericError};
use saluki_metrics::MetricsBuilder;

// From `agent-data-plane-config`'s DogStatsD domain.

#[derive(Debug)]
pub(super) struct MetricTagFilterEntry {
    pub(super) metric_name: String,
    pub(super) action: FilterAction,
    pub(super) tags: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct MetricTagValueAllowlistEntry {
    pub(super) metric_prefix: String,
    pub(super) tag_name: String,
    pub(super) values: Vec<String>,
    pub(super) on_miss: TagValueMismatchAction,
    pub(super) replacement: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct InvalidMetricTagValueAllowlist(String);

impl fmt::Display for InvalidMetricTagValueAllowlist {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

pub(super) fn validate_metric_tag_value_allowlists(
    entries: &[MetricTagValueAllowlistEntry],
) -> Result<(), InvalidMetricTagValueAllowlist> {
    for (index, entry) in entries.iter().enumerate() {
        let rule = index + 1;
        if entry.metric_prefix.is_empty() {
            return Err(InvalidMetricTagValueAllowlist(format!(
                "metric tag value allow-list rule {rule} has an empty `metric_prefix`; configure a non-empty metric-name prefix"
            )));
        }
        if entry.tag_name.is_empty() {
            return Err(InvalidMetricTagValueAllowlist(format!(
                "metric tag value allow-list rule {rule} for prefix '{}' has an empty `tag_name`; configure a non-empty tag name",
                entry.metric_prefix
            )));
        }
        if entry.tag_name.contains(':') {
            return Err(InvalidMetricTagValueAllowlist(format!(
                "metric tag value allow-list tag name '{}' contains ':'; configure only the tag key, without a colon or value",
                entry.tag_name
            )));
        }
    }

    let mut sorted_entries = entries.iter().collect::<Vec<_>>();
    sorted_entries.sort_unstable_by(|left, right| {
        left.metric_prefix
            .cmp(&right.metric_prefix)
            .then_with(|| left.tag_name.cmp(&right.tag_name))
    });

    // After sorting by prefix and then tag, duplicate prefix/tag pairs are adjacent. Any distinct prefix that extends
    // another prefix follows the complete group for the shorter prefix, so one adjacent pair also exposes that overlap.
    for pair in sorted_entries.windows(2) {
        let [left, right] = pair else {
            unreachable!("a two-entry window must contain two entries");
        };
        if left.metric_prefix == right.metric_prefix {
            if left.tag_name == right.tag_name {
                return Err(InvalidMetricTagValueAllowlist(format!(
                    "metric prefix '{}' is configured more than once for tag '{}'; configure each prefix and tag pair only once",
                    left.metric_prefix, left.tag_name
                )));
            }
        } else if right.metric_prefix.starts_with(&left.metric_prefix) {
            return Err(InvalidMetricTagValueAllowlist(format!(
                "overlapping metric prefixes '{}' and '{}' are configured; configure distinct prefixes that do not overlap",
                left.metric_prefix, right.metric_prefix
            )));
        }
    }

    Ok(())
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum TagValueMismatchAction {
    #[default]
    Remove,
    Replace,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum FilterAction {
    Include,
    #[default]
    Exclude,
}

// From `agent-data-plane`'s tag filterlist telemetry.

#[derive(Clone)]
pub(super) struct Telemetry {
    rule_hits: Counter,
    rule_misses: Counter,
    noop_hits: Counter,
    metrics_modified: Counter,
    tags_filtered: Counter,
    size: Gauge,
}

impl Telemetry {
    pub(super) fn new(builder: &MetricsBuilder) -> Self {
        Self {
            rule_hits: builder.register_counter("tag_filterlist_rule_hits_total"),
            rule_misses: builder.register_counter("tag_filterlist_rule_misses_total"),
            noop_hits: builder.register_counter("tag_filterlist_noop_hits_total"),
            metrics_modified: builder.register_counter("tag_filterlist_metrics_modified_total"),
            tags_filtered: builder.register_counter("tag_filterlist_tags_filtered_total"),
            size: builder.register_gauge("tag_filterlist_size"),
        }
    }

    pub(super) fn record(&self, outcome: FilterMetricTagsOutcome) {
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

    pub(super) fn set_size(&self, count: usize) {
        self.size.set(count as f64);
    }
}

// From `agent-data-plane`'s tag filterlist transform.

const CONTEXT_CACHE_TTI: Duration = Duration::from_secs(30);
const CONTEXT_CACHE_EXPIRATION_INTERVAL: Duration = Duration::from_secs(1);

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

struct CompiledKeyFilter {
    is_exclude: bool,
    tag_names: FastHashSet<String>,
}

#[derive(Clone)]
struct CompiledValuePrefixFilter {
    metric_prefix: String,
    value_allowlists: FastHashMap<String, CompiledTagValueAllowlist>,
}

#[derive(Clone, Default)]
pub(super) struct CompiledValuePrefixFilters(Vec<CompiledValuePrefixFilter>);

impl CompiledValuePrefixFilters {
    fn sort(&mut self) {
        self.0
            .sort_unstable_by(|left, right| left.metric_prefix.cmp(&right.metric_prefix));
    }

    fn get(&self, metric_name: &str) -> Option<&CompiledValuePrefixFilter> {
        find_matching_prefix(&self.0, metric_name, |filter| filter.metric_prefix.as_str())
    }

    fn rule_count(&self) -> usize {
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

#[derive(Default)]
pub(super) struct CompiledFilters {
    key_filters: FastHashMap<String, CompiledKeyFilter>,
    pub(super) value_prefix_filters: CompiledValuePrefixFilters,
}

impl CompiledFilters {
    pub(super) fn rule_count(&self) -> usize {
        self.key_filters.len() + self.value_prefix_filters.rule_count()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum FilterMetricTagsOutcome {
    RuleMiss,
    NoChange,
    Modified { removed_tags: usize },
}

pub(super) fn compile_filters(entries: &[MetricTagFilterEntry]) -> CompiledFilters {
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

pub(super) fn add_value_allowlists(
    filters: &mut CompiledFilters, entries: &[MetricTagValueAllowlistEntry],
) -> Result<(), GenericError> {
    validate_metric_tag_value_allowlists(entries).map_err(|error| generic_error!(error.to_string()))?;

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
    Ok(())
}

pub(super) fn compile_all_filters(
    entries: &[MetricTagFilterEntry], value_allowlists: &[MetricTagValueAllowlistEntry],
) -> Result<CompiledFilters, GenericError> {
    let mut filters = compile_filters(entries);
    add_value_allowlists(&mut filters, value_allowlists)?;
    Ok(filters)
}

pub(super) fn compile_filters_with_values(
    entries: &[MetricTagFilterEntry], value_prefix_filters: CompiledValuePrefixFilters,
) -> CompiledFilters {
    let mut filters = compile_filters(entries);
    filters.value_prefix_filters = value_prefix_filters;
    filters
}

pub(super) struct TagFilterlist {
    pub(super) filters: CompiledFilters,
    pub(super) telemetry: Telemetry,
    pub(super) context_cache: Cache<Context, Option<(Context, usize)>>,
}

pub(super) fn build_context_cache(capacity: usize) -> Cache<Context, Option<(Context, usize)>> {
    let capacity = NonZeroUsize::new(capacity).unwrap_or(NonZeroUsize::MIN);
    CacheBuilder::from_identifier("tag_filterlist/context_cache")
        .expect("identifier cannot be empty")
        .with_capacity(capacity)
        .with_time_to_idle(Some(CONTEXT_CACHE_TTI))
        .with_expiration_interval(CONTEXT_CACHE_EXPIRATION_INTERVAL)
        .build()
}

impl TagFilterlist {
    /// The run loop's processing of one buffer, before it is dispatched, with the view state the loop keeps across
    /// buffers.
    pub(super) fn process_buffer(&mut self, events: &mut EventsBuffer, view_state: &mut TagSetMutViewState) {
        for event in events {
            if let Some(metric) = event.try_as_metric_mut() {
                if metric.values().is_sketch() || matches!(metric.values(), MetricValues::Counter(_)) {
                    let original_context = metric.context().clone();

                    if let Some(cached) = self.context_cache.get(&original_context) {
                        match cached {
                            None => self.telemetry.record(FilterMetricTagsOutcome::NoChange),
                            Some((filtered_ctx, removed_tags)) => {
                                *metric.context_mut() = filtered_ctx;
                                self.telemetry
                                    .record(FilterMetricTagsOutcome::Modified { removed_tags });
                            }
                        }
                    } else {
                        let outcome = filter_metric_tags(metric, view_state, &self.filters);
                        self.telemetry.record(outcome);

                        match outcome {
                            FilterMetricTagsOutcome::RuleMiss => {}
                            FilterMetricTagsOutcome::NoChange => {
                                self.context_cache.insert(original_context, None);
                            }
                            FilterMetricTagsOutcome::Modified { removed_tags } => {
                                self.context_cache
                                    .insert(original_context, Some((metric.context().clone(), removed_tags)));
                            }
                        }
                    }
                }
            }
        }
    }
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

#[inline]
pub(super) fn filter_metric_tags(
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
