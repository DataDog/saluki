//! Metric Tag Filterlist synchronous transform.
//!
//! Removes or retains specific tags from distribution and count metrics based on per-metric
//! configuration. Supports whole-tag filtering and value allow-listing.
//!
//! Whole-tag configuration is read from `metric_tag_filterlist` and can be updated at runtime via
//! Remote Config. Value allow-list configuration is read independently from the static
//! `metric_tag_value_allowlist` key.
//!
//! Both forms of filtering exist to merge samples that differ only in a filtered tag into a single aggregated
//! context, so they only apply to metrics that are aggregated. The `dogstatsd_no_agg_split` transform routes
//! timestamped metrics around this transform, leaving their tags untouched.
//!
//! # Normalization
//!
//! The intake normalizes metric names and tags on ingest, and users write rules against the normalized form they see
//! in Datadog. Rules therefore match normalized names: a whole-tag rule's metric name is compared after metric-name
//! normalization, and the tag keys of both kinds of rule are compared after tag-key normalization (see
//! `SPEC.md` in the `datadog-agent-tag-normalization` crate). Configured strings are normalized once, when the rules are compiled, and
//! incoming names are normalized only for comparison. Tags that are kept are still sent verbatim, and tag values are
//! still matched exactly.

mod telemetry;

use std::{collections::hash_map::Entry, num::NonZeroUsize, time::Duration};

use async_trait::async_trait;
use datadog_agent_tag_normalization::{can_have_value, rule_key, tag_key, TagBuf};
use saluki_common::collections::{FastHashMap, FastHashSet};
use saluki_common::sync::live::Live;
use saluki_core::{
    accounting::{MemoryBounds, MemoryBoundsBuilder},
    cache::{Cache, CacheBuilder},
    components::{
        transforms::{Transform, TransformBuilder, TransformContext},
        BuildContext,
    },
    data_model::{
        event::{
            metric::{
                context::{Context, TagSetMutViewState},
                Metric, MetricValues,
            },
            EventType,
        },
        tags::{Tag, TagStr},
    },
    observability::ComponentMetricsExt,
    topology::OutputDefinition,
};
use saluki_error::GenericError;
use saluki_metrics::MetricsBuilder;
use tokio::select;
use tracing::{debug, error, warn};

use super::metric_name::{self, NameBuf};

/// One whole-tag filter entry.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MetricTagFilterEntry {
    /// Metric name the entry applies to, compared after metric-name normalization. An entry with an empty name is
    /// ignored.
    pub metric_name: String,

    /// Whether the listed tags are kept or removed.
    pub action: FilterAction,

    /// Tag keys the action applies to, compared after tag-key normalization.
    pub tags: Vec<String>,
}

/// Whether a whole-tag filter entry keeps or removes its tags.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FilterAction {
    /// Keep only the listed tags.
    Include,

    /// Remove the listed tags.
    Exclude,
}

/// One tag value allow-list entry.
///
/// The tag name is compared after tag-key normalization. Value matching is exact and case-sensitive, including any
/// whitespace.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MetricTagValueAllowlistEntry {
    /// Metric-name prefix the entry applies to.
    pub metric_prefix: String,

    /// Tag key whose values are constrained, compared after tag-key normalization. Bare tags have no value and are not
    /// changed.
    pub tag_name: String,

    /// Tag values retained unchanged. An empty list treats every key/value tag as a mismatch.
    pub values: Vec<String>,

    /// Action applied when a tag value is absent from `values`.
    pub on_miss: TagValueMismatchAction,

    /// Replacement value used when `on_miss` is [`Replace`][TagValueMismatchAction::Replace], emitted as configured
    /// after the normalized tag name.
    pub replacement: String,
}

/// Action applied when a tag value is absent from its allow-list.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TagValueMismatchAction {
    /// Removes the tag.
    Remove,

    /// Replaces the tag value with the entry's replacement.
    Replace,
}

const CONTEXT_CACHE_TTI: Duration = Duration::from_secs(30);
const CONTEXT_CACHE_EXPIRATION_INTERVAL: Duration = Duration::from_secs(1);

use self::telemetry::Telemetry;

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
struct CompiledValuePrefixFilters(Vec<CompiledValuePrefixFilter>);

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

/// Compiled exact-name whole-tag filters and metric-prefix value filters.
#[derive(Default)]
pub struct CompiledFilters {
    key_filters: FastHashMap<String, CompiledKeyFilter>,
    value_prefix_filters: CompiledValuePrefixFilters,
}

impl CompiledFilters {
    fn rule_count(&self) -> usize {
        self.key_filters.len() + self.value_prefix_filters.rule_count()
    }

    /// Returns the whole-tag rule for `metric_name`, compared in the form Datadog stores metric names in.
    fn key_filter(&self, metric_name: &str) -> Option<&CompiledKeyFilter> {
        if self.key_filters.is_empty() {
            return None;
        }
        if metric_name::is_normalized(metric_name) {
            return self.key_filters.get(metric_name);
        }

        let mut buf = NameBuf::new();
        self.key_filters.get(normalize_metric_name(&mut buf, metric_name)?)
    }
}

/// Normalizes `name` into `buf`, returning `None` if the intake never stores it. See [`metric_name::normalize_into`].
fn normalize_metric_name<'buf>(buf: &'buf mut NameBuf, name: &str) -> Option<&'buf str> {
    let normalized = metric_name::normalize_into(buf, name)?;
    Some(simdutf8::basic::from_utf8(normalized).expect("normalized metric names are ASCII"))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
/// Outcome of attempting to apply tag filter rules to a metric.
pub enum FilterMetricTagsOutcome {
    /// No rule existed for the metric name.
    RuleMiss,
    /// A rule existed, but applying it didn't change any tags.
    NoChange,
    /// A rule existed and removed one or more tags.
    Modified {
        /// Total number of instrumented and origin tags removed.
        removed_tags: usize,
    },
}

/// Compile a slice of filter entries into an O(1)-lookup table.
///
/// Metric names and tag keys are normalized first. An entry whose metric name the intake never stores, and a tag key
/// no stored tag can have, can't match anything, so each is dropped with a warning.
///
/// Merge rules, applied to entries whose normalized metric names are equal:
/// - Same metric name + same action → union of tag key sets.
/// - Same metric name + conflicting actions → `exclude` wins, with a warning.
pub fn compile_filters(entries: &[MetricTagFilterEntry]) -> CompiledFilters {
    let mut filters = CompiledFilters::default();
    let mut name_buf = NameBuf::new();
    // For each normalized metric name, the first configured name and whether any entry listed tags. Only used for
    // warnings.
    let mut sources: FastHashMap<String, (String, bool)> = FastHashMap::default();

    for entry in entries {
        if entry.metric_name.is_empty() {
            continue;
        }

        let Some(metric_name) = normalize_metric_name(&mut name_buf, &entry.metric_name) else {
            warn!(
                setting = "metric_tag_filterlist",
                metric_name = %entry.metric_name,
                "Dropping entry because Datadog never stores its metric name."
            );
            continue;
        };
        let metric_name = metric_name.to_string();

        let is_exclude = entry.action == FilterAction::Exclude;
        let mut tag_set = FastHashSet::default();
        tag_set.reserve(entry.tags.len());
        for tag in &entry.tags {
            match rule_key(tag) {
                Some(key) => {
                    tag_set.insert(key);
                }
                None => warn!(
                    setting = "metric_tag_filterlist",
                    metric_name = %entry.metric_name,
                    tag = %tag,
                    "Ignoring tag key containing ':'. List the tag key only, without a value."
                ),
            }
        }

        let source = sources
            .entry(metric_name.clone())
            .or_insert_with(|| (entry.metric_name.clone(), false));
        source.1 |= !entry.tags.is_empty();

        match filters.key_filters.entry(metric_name) {
            Entry::Vacant(vacant) => {
                vacant.insert(CompiledKeyFilter {
                    is_exclude,
                    tag_names: tag_set,
                });
            }
            Entry::Occupied(mut occupied) => {
                if occupied.get().is_exclude == is_exclude {
                    occupied.get_mut().tag_names.extend(tag_set);
                    continue;
                }
                warn!(
                    setting = "metric_tag_filterlist",
                    metric_name = %occupied.key(),
                    first_entry = %source.0,
                    conflicting_entry = %entry.metric_name,
                    "Entries for the same metric configure both include and exclude. Keeping only the exclude entries."
                );
                if is_exclude {
                    *occupied.get_mut() = CompiledKeyFilter {
                        is_exclude,
                        tag_names: tag_set,
                    };
                }
            }
        }
    }

    for (metric_name, filter) in &filters.key_filters {
        if !filter.is_exclude && filter.tag_names.is_empty() && sources[metric_name].1 {
            warn!(
                setting = "metric_tag_filterlist",
                metric_name = %sources[metric_name].0,
                "Include rule has no usable tag keys left, so it removes every tag."
            );
        }
    }

    filters
}

/// Adds value allow-list entries to a compiled filter table.
///
/// Rules are keyed by the normalized form of their tag names. The configuration boundary validates the configured
/// strings, so each metric matches at most one prefix. A rule whose tag name no stored tag with a value can have, or
/// whose tag name normalizes to the same key as an earlier rule for the same prefix, is dropped with a warning, as
/// `metric_tag_filterlist` does with entries that can't match.
///
/// Unlike whole-tag rule names, metric prefixes are not normalized: they are compared exactly with the metric name as
/// ADP sees it. This keeps prefix matching and its overlap validation simple for this experimental setting.
pub fn add_value_allowlists(filters: &mut CompiledFilters, entries: &[MetricTagValueAllowlistEntry]) {
    for entry in entries {
        let tag_key = rule_key(&entry.tag_name)
            .expect("the configuration boundary rejects value allow-list tag names containing ':'");
        if !can_have_value(&tag_key) {
            warn!(
                setting = "metric_tag_value_allowlist",
                metric_prefix = %entry.metric_prefix,
                tag_name = %entry.tag_name,
                "Ignoring rule because its tag name is too long for Datadog to store a tag with that name and a value."
            );
            continue;
        }

        let on_miss = match entry.on_miss {
            TagValueMismatchAction::Remove => CompiledMismatchAction::Remove,
            TagValueMismatchAction::Replace => {
                CompiledMismatchAction::Replace(Tag::from(format!("{}:{}", tag_key, entry.replacement)))
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
            match filter.value_allowlists.entry(tag_key) {
                Entry::Vacant(vacant) => {
                    vacant.insert(allowlist);
                }
                Entry::Occupied(occupied) => warn!(
                    setting = "metric_tag_value_allowlist",
                    metric_prefix = %entry.metric_prefix,
                    tag_name = %entry.tag_name,
                    normalized_tag_name = %occupied.key(),
                    "Ignoring rule because its tag name normalizes to the same tag key as an earlier rule for this \
                     prefix."
                ),
            }
        } else {
            let mut value_allowlists = FastHashMap::default();
            value_allowlists.insert(tag_key, allowlist);
            filters.value_prefix_filters.0.push(CompiledValuePrefixFilter {
                metric_prefix: entry.metric_prefix.clone(),
                value_allowlists,
            });
        }
    }

    filters.value_prefix_filters.sort();
}

fn compile_all_filters(
    entries: &[MetricTagFilterEntry], value_allowlists: &[MetricTagValueAllowlistEntry],
) -> CompiledFilters {
    let mut filters = compile_filters(entries);
    add_value_allowlists(&mut filters, value_allowlists);
    filters
}

fn compile_filters_with_values(
    entries: &[MetricTagFilterEntry], value_prefix_filters: CompiledValuePrefixFilters,
) -> CompiledFilters {
    let mut filters = compile_filters(entries);
    filters.value_prefix_filters = value_prefix_filters;
    filters
}

/// Metric Tag Filterlist transform.
///
/// Removes, retains, or replaces tags on counter and sketch-backed metrics based on per-metric configuration. Gauges
/// and other metric types pass through unchanged. Whole-tag rules are read from `metric_tag_filterlist` and support
/// runtime updates via Remote Config. Value rules are read from the static `metric_tag_value_allowlist` key and match
/// metric names after DogStatsD mapper rewrites and metric namespace prefixing.
pub struct TagFilterlistConfiguration {
    entries: Live<Vec<MetricTagFilterEntry>>,

    /// Compiled per-metric-prefix tag value allow-list rules.
    ///
    /// Configured independently from `metric_tag_filterlist` so a remotely configured whole-tag filter does not
    /// replace locally configured value rules. This key is static: changing it requires restarting ADP. Construction
    /// compiles the rules once.
    value_prefix_filters: CompiledValuePrefixFilters,

    /// Maximum number of entries in the per-context deduplication cache used by the tag filter.
    ///
    /// Configured via `data_plane.dogstatsd.aggregator_tag_filter_cache_capacity` in the agent config
    /// stream. High-throughput deployments with many unique metric contexts may benefit from
    /// increasing this value to reduce cache churn.
    context_cache_capacity: usize,
}

impl TagFilterlistConfiguration {
    /// Creates a new `TagFilterlistConfiguration`.
    ///
    /// `value_allowlists` must already be valid: distinct metric prefixes don't overlap, and each prefix and tag pair
    /// appears once. The configuration boundary rejects invalid rules, at startup and on update.
    pub fn new(
        entries: Live<Vec<MetricTagFilterEntry>>, value_allowlists: &[MetricTagValueAllowlistEntry],
        context_cache_capacity: usize,
    ) -> Self {
        let value_prefix_filters = compile_all_filters(&[], value_allowlists).value_prefix_filters;
        Self {
            entries,
            value_prefix_filters,
            context_cache_capacity,
        }
    }
}

#[async_trait]
impl TransformBuilder for TagFilterlistConfiguration {
    fn input_event_type(&self) -> EventType {
        EventType::Metric
    }

    fn outputs(&self) -> &[OutputDefinition<EventType>] {
        static OUTPUTS: &[OutputDefinition<EventType>] = &[OutputDefinition::default_output(EventType::Metric)];
        OUTPUTS
    }

    async fn build(&self, context: BuildContext) -> Result<Box<dyn Transform + Send>, GenericError> {
        let metrics_builder = MetricsBuilder::from_component_context(context.component_context());
        let telemetry = Telemetry::new(&metrics_builder);
        let filters = compile_filters_with_values(&self.entries, self.value_prefix_filters.clone());
        telemetry.set_size(filters.rule_count());

        Ok(Box::new(TagFilterlist {
            filters,
            entries: self.entries.clone(),
            value_prefix_filters: self.value_prefix_filters.clone(),
            telemetry,
            context_cache: build_context_cache(self.context_cache_capacity),
            context_cache_capacity: self.context_cache_capacity,
        }))
    }
}

impl MemoryBounds for TagFilterlistConfiguration {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        builder.minimum().with_single_value::<TagFilterlist>("component struct");

        builder
            .firm()
            .with_fixed_amount("context cache", self.context_cache_capacity * 64);
    }
}

struct TagFilterlist {
    filters: CompiledFilters,
    entries: Live<Vec<MetricTagFilterEntry>>,
    value_prefix_filters: CompiledValuePrefixFilters,
    telemetry: Telemetry,
    context_cache: Cache<Context, Option<(Context, usize)>>,
    context_cache_capacity: usize,
}

fn build_context_cache(capacity: usize) -> Cache<Context, Option<(Context, usize)>> {
    let capacity = NonZeroUsize::new(capacity).unwrap_or(NonZeroUsize::MIN);
    CacheBuilder::from_identifier("tag_filterlist/context_cache")
        .expect("identifier cannot be empty")
        .with_capacity(capacity)
        .with_time_to_idle(Some(CONTEXT_CACHE_TTI))
        .with_expiration_interval(CONTEXT_CACHE_EXPIRATION_INTERVAL)
        .build()
}

impl TagFilterlist {
    fn filter_cached(&self, metric: &mut Metric, state: &mut TagSetMutViewState) -> FilterMetricTagsOutcome {
        if let Some(cached) = self.context_cache.get(metric.context()) {
            return match cached {
                None => FilterMetricTagsOutcome::NoChange,
                Some((filtered_context, removed_tags)) => {
                    *metric.context_mut() = filtered_context;
                    FilterMetricTagsOutcome::Modified { removed_tags }
                }
            };
        }

        let metric_name = metric.context().name();
        let key_filter = self.filters.key_filter(metric_name.as_ref());
        let value_filter = self.filters.value_prefix_filters.get(metric_name.as_ref());
        if key_filter.is_none() && value_filter.is_none() {
            return FilterMetricTagsOutcome::RuleMiss;
        }

        let original_context = metric.context().clone();
        let removed_tags = apply_metric_tag_filters(metric, state, key_filter, value_filter);
        if removed_tags == 0 {
            self.context_cache.insert(original_context, None);
            FilterMetricTagsOutcome::NoChange
        } else {
            self.context_cache
                .insert(original_context, Some((metric.context().clone(), removed_tags)));
            FilterMetricTagsOutcome::Modified { removed_tags }
        }
    }
}

#[async_trait]
impl Transform for TagFilterlist {
    async fn run(mut self: Box<Self>, mut context: TransformContext) -> Result<(), GenericError> {
        let mut health = context.take_health_handle();
        health.mark_ready();

        let mut view_state = TagSetMutViewState::default();

        debug!("Metric Tag Filterlist transform started.");

        loop {
            select! {
                _ = health.live() => continue,
                maybe_events = context.events().next() => match maybe_events {
                    Some(mut events) => {
                        for metric in events.events_mut::<Metric>() {
                            if metric.values().is_sketch()
                                || matches!(metric.values(), MetricValues::Counter(_))
                            {
                                self.telemetry.record(self.filter_cached(metric, &mut view_state));
                            }
                        }
                        if let Err(e) = context.dispatcher().dispatch(events).await {
                            error!(error = %e, "Failed to dispatch events.");
                        }
                    }
                    None => break,
                },
                new_entries = self.entries.changed() => {
                    self.filters = compile_filters_with_values(&new_entries, self.value_prefix_filters.clone());
                    self.context_cache = build_context_cache(self.context_cache_capacity);
                    let rule_count = self.filters.rule_count();
                    self.telemetry.set_size(rule_count);
                    self.telemetry.increment_updates();
                    debug!(rules_loaded = rule_count, "Updated metric tag filterlist.");
                },
            }
        }

        debug!("Metric Tag Filterlist transform stopped.");

        Ok(())
    }
}

#[inline]
fn filter_tag(
    tag: &TagStr, key_filter: Option<&CompiledKeyFilter>, value_filter: Option<&CompiledValuePrefixFilter>,
    key_buf: &mut TagBuf, replacements: &mut Vec<Tag>,
) -> bool {
    let key = tag_key(tag.as_str(), key_buf);

    if let Some(key_filter) = key_filter {
        if key_filter.is_exclude == key_filter.tag_names.contains(key.key) {
            return false;
        }
    }

    // A raw tag whose `:` falls past the intake's length limit is stored as a bare tag, so it has no value here either.
    if !key.has_value {
        return true;
    }
    let Some(allowlist) = value_filter.and_then(|filter| filter.value_allowlists.get(key.key)) else {
        return true;
    };

    let Some(value) = tag.value() else {
        return true;
    };
    if allowlist.allowed_values.contains(value) {
        return true;
    }

    match &allowlist.on_miss {
        CompiledMismatchAction::Remove => false,
        CompiledMismatchAction::Replace(replacement) if replacement.as_str() == tag.as_str() => true,
        CompiledMismatchAction::Replace(replacement) => {
            replacements.push(replacement.clone());
            false
        }
    }
}

/// Filters the tags of a metric according to the compiled filter table.
///
/// Both instrumented tags and origin tags are filtered using the same key and value rules, matching normalized metric
/// names and tag keys. Whole-tag filtering runs first, so an `include` rule must include a value-filtered tag key for
/// its allow-list to apply. A metric and tag match at most one value rule: configuration validation rejects overlapping
/// prefixes and duplicate rules, and compilation drops rules whose tag names normalize to the same key. Value rules
/// leave bare tags unchanged; empty-string values are processed and can be retained by including `""` in the configured
/// values.
///
/// The caller applies this function only to counters and sketch-backed metrics with a matching rule. If no tags
/// change, the context is left untouched.
fn apply_metric_tag_filters(
    metric: &mut Metric, state: &mut TagSetMutViewState, key_filter: Option<&CompiledKeyFilter>,
    value_filter: Option<&CompiledValuePrefixFilter>,
) -> usize {
    let mut key_buf = TagBuf::new();
    let mut tag_replacements = Vec::new();
    let mut origin_tag_replacements = Vec::new();
    let mut tag_set_view = metric.context_mut().tags_mut_view(state);
    tag_set_view.retain_tags(|tag| filter_tag(tag, key_filter, value_filter, &mut key_buf, &mut tag_replacements));
    tag_set_view.retain_origin_tags(|tag| {
        filter_tag(
            tag,
            key_filter,
            value_filter,
            &mut key_buf,
            &mut origin_tag_replacements,
        )
    });
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

    total_removed
}

#[cfg(test)]
fn filter_metric_tags(
    metric: &mut Metric, state: &mut TagSetMutViewState, filters: &CompiledFilters,
) -> FilterMetricTagsOutcome {
    let metric_name = metric.context().name();
    let key_filter = filters.key_filter(metric_name.as_ref());
    let value_filter = filters.value_prefix_filters.get(metric_name.as_ref());
    if key_filter.is_none() && value_filter.is_none() {
        return FilterMetricTagsOutcome::RuleMiss;
    }
    match apply_metric_tag_filters(metric, state, key_filter, value_filter) {
        0 => FilterMetricTagsOutcome::NoChange,
        removed_tags => FilterMetricTagsOutcome::Modified { removed_tags },
    }
}

#[cfg(test)]
mod tests {
    use saluki_core::{
        components::test_util::{ComponentControl, Output, TestComponentDriver},
        data_model::{
            event::{
                metric::{
                    context::{Context, TagSetMutViewState},
                    Metric, MetricValues,
                },
                Event,
            },
            tags::{Tag, TagSet},
        },
        topology::EventsBuffer,
    };
    use saluki_metrics::{test::TestRecorder, MetricsBuilder};

    use super::*;

    fn distribution_metric(name: &'static str, tags: &[&'static str]) -> Metric {
        let context = Context::from_static_parts(name, tags);
        Metric::distribution(context, 1.0)
    }

    fn distribution_metric_with_origin_tags(
        name: &'static str, tags: &[&'static str], origin_tags: &[&'static str],
    ) -> Metric {
        let origin_tag_set: TagSet = origin_tags.iter().map(|s| Tag::from(*s)).collect();
        let context = Context::from_static_parts(name, tags).with_origin_tags(origin_tag_set.into_shared());
        Metric::distribution(context, 1.0)
    }

    fn counter_metric(name: &'static str, tags: &[&'static str]) -> Metric {
        let context = Context::from_static_parts(name, tags);
        Metric::counter(context, 1.0)
    }

    fn tag_names(metric: &Metric) -> Vec<String> {
        let mut names: Vec<_> = metric
            .context()
            .tags()
            .into_iter()
            .map(|t| t.as_str().to_owned())
            .collect();
        names.sort();
        names
    }

    fn value_allowlist(
        metric_prefix: &str, tag_name: &str, values: &[&str], on_miss: TagValueMismatchAction, replacement: &str,
    ) -> MetricTagValueAllowlistEntry {
        MetricTagValueAllowlistEntry {
            metric_prefix: metric_prefix.to_string(),
            tag_name: tag_name.to_string(),
            values: values.iter().map(|value| (*value).to_string()).collect(),
            on_miss,
            replacement: replacement.to_string(),
        }
    }

    #[test]
    fn filter_metric_tags_treats_distribution_and_count_metrics_identically() {
        // `filter_metric_tags` applies the same rule logic to distribution (sketch) and count metrics. The
        // run-loop type guard (exercised by `run_loop_enforces_type_guard_and_exercises_context_cache`) is what
        // restricts which metric types ever reach this function; the function itself never branches on the type.
        // Each case is asserted against both a distribution and a counter, and against both the resulting tags
        // and the returned outcome, so the two paths can't silently drift apart.
        struct Case {
            name: &'static str,
            action: FilterAction,
            filter_metric_name: &'static str,
            filter_tags: &'static [&'static str],
            input_tags: &'static [&'static str],
            expected_tags: &'static [&'static str],
            expected_outcome: FilterMetricTagsOutcome,
        }

        let cases = [
            Case {
                name: "exclude removes the listed tags",
                action: FilterAction::Exclude,
                filter_metric_name: "my.metric",
                filter_tags: &["env", "host"],
                input_tags: &["env:prod", "service:web", "host:h1"],
                expected_tags: &["service:web"],
                expected_outcome: FilterMetricTagsOutcome::Modified { removed_tags: 2 },
            },
            Case {
                name: "include keeps only the listed tags",
                action: FilterAction::Include,
                filter_metric_name: "my.metric",
                filter_tags: &["env"],
                input_tags: &["env:prod", "service:web", "host:h1"],
                expected_tags: &["env:prod"],
                expected_outcome: FilterMetricTagsOutcome::Modified { removed_tags: 2 },
            },
            Case {
                name: "a rule for a different metric name is a miss",
                action: FilterAction::Exclude,
                filter_metric_name: "other.metric",
                filter_tags: &["env"],
                input_tags: &["env:prod", "service:web"],
                expected_tags: &["env:prod", "service:web"],
                expected_outcome: FilterMetricTagsOutcome::RuleMiss,
            },
            Case {
                name: "a matching rule that removes no tags is a no-op",
                action: FilterAction::Exclude,
                filter_metric_name: "my.metric",
                filter_tags: &["region"],
                input_tags: &["env:prod", "service:web"],
                expected_tags: &["env:prod", "service:web"],
                expected_outcome: FilterMetricTagsOutcome::NoChange,
            },
        ];

        type MetricBuilder = fn(&'static str, &[&'static str]) -> Metric;
        let builders: [(&str, MetricBuilder); 2] = [("distribution", distribution_metric), ("counter", counter_metric)];

        for case in &cases {
            let entries = vec![MetricTagFilterEntry {
                metric_name: case.filter_metric_name.to_string(),
                action: case.action,
                tags: case.filter_tags.iter().map(|s| s.to_string()).collect(),
            }];
            let filters = compile_filters(&entries);

            for (kind, build) in builders {
                let mut metric = build("my.metric", case.input_tags);
                let mut state = TagSetMutViewState::default();
                let outcome = filter_metric_tags(&mut metric, &mut state, &filters);

                let expected_tags: Vec<String> = case.expected_tags.iter().map(|s| s.to_string()).collect();
                assert_eq!(tag_names(&metric), expected_tags, "{kind}: {}", case.name);
                assert_eq!(outcome, case.expected_outcome, "{kind}: {}", case.name);
            }
        }
    }

    #[test]
    fn value_allowlist_matches_metric_prefix_and_filters_values() {
        let filters = compile_all_filters(
            &[],
            &[value_allowlist(
                "my.",
                "customer_id",
                &["top-1", "top-2"],
                TagValueMismatchAction::Remove,
                "other",
            )],
        );
        let mut state = TagSetMutViewState::default();

        let mut allowed = counter_metric("my.metric", &["customer_id:top-1", "service:web"]);
        assert_eq!(
            filter_metric_tags(&mut allowed, &mut state, &filters),
            FilterMetricTagsOutcome::NoChange
        );
        assert_eq!(tag_names(&allowed), vec!["customer_id:top-1", "service:web"]);

        let mut unlisted = counter_metric("my.metric", &["customer_id:long-tail", "service:web"]);
        assert_eq!(
            filter_metric_tags(&mut unlisted, &mut state, &filters),
            FilterMetricTagsOutcome::Modified { removed_tags: 1 }
        );
        assert_eq!(tag_names(&unlisted), vec!["service:web"]);

        let mut unrelated = counter_metric("other.metric", &["customer_id:long-tail", "service:web"]);
        assert_eq!(
            filter_metric_tags(&mut unrelated, &mut state, &filters),
            FilterMetricTagsOutcome::RuleMiss
        );
        assert_eq!(tag_names(&unrelated), vec!["customer_id:long-tail", "service:web"]);
    }

    #[test]
    fn value_allowlist_replaces_and_deduplicates_unlisted_values() {
        let filters = compile_all_filters(
            &[],
            &[value_allowlist(
                "my.metric",
                "customer_id",
                &[],
                TagValueMismatchAction::Replace,
                "other",
            )],
        );
        let mut metric = distribution_metric(
            "my.metric",
            &[
                "customer_id:first",
                "customer_id:second",
                "customer_id:other",
                "service:web",
            ],
        );
        let mut state = TagSetMutViewState::default();

        assert_eq!(
            filter_metric_tags(&mut metric, &mut state, &filters),
            FilterMetricTagsOutcome::Modified { removed_tags: 2 }
        );
        assert_eq!(tag_names(&metric), vec!["customer_id:other", "service:web"]);
    }

    #[test]
    fn value_allowlist_ignores_bare_tags_and_treats_empty_strings_as_values() {
        let mut state = TagSetMutViewState::default();
        let filters = compile_all_filters(
            &[],
            &[value_allowlist(
                "my.metric",
                "customer_id",
                &["top-1"],
                TagValueMismatchAction::Remove,
                "other",
            )],
        );
        let mut unlisted_empty = counter_metric("my.metric", &["customer_id", "customer_id:", "service:web"]);

        assert_eq!(
            filter_metric_tags(&mut unlisted_empty, &mut state, &filters),
            FilterMetricTagsOutcome::Modified { removed_tags: 1 }
        );
        assert_eq!(tag_names(&unlisted_empty), vec!["customer_id", "service:web"]);

        let filters = compile_all_filters(
            &[],
            &[value_allowlist(
                "my.metric",
                "customer_id",
                &[""],
                TagValueMismatchAction::Remove,
                "other",
            )],
        );
        let mut listed_empty = counter_metric("my.metric", &["customer_id", "customer_id:", "service:web"]);

        assert_eq!(
            filter_metric_tags(&mut listed_empty, &mut state, &filters),
            FilterMetricTagsOutcome::NoChange
        );
        assert_eq!(
            tag_names(&listed_empty),
            vec!["customer_id", "customer_id:", "service:web"]
        );
    }

    #[test]
    fn value_allowlist_applies_to_origin_tags() {
        let filters = compile_all_filters(
            &[],
            &[value_allowlist(
                "my.metric",
                "customer_id",
                &["top-1"],
                TagValueMismatchAction::Replace,
                "other",
            )],
        );
        let mut metric = distribution_metric_with_origin_tags(
            "my.metric",
            &[],
            &["customer_id:top-1", "customer_id:long-tail", "service:web"],
        );
        let mut state = TagSetMutViewState::default();

        assert_eq!(
            filter_metric_tags(&mut metric, &mut state, &filters),
            FilterMetricTagsOutcome::Modified { removed_tags: 1 }
        );
        assert_eq!(
            origin_tag_names(&metric),
            vec!["customer_id:other", "customer_id:top-1", "service:web"]
        );
    }

    #[test]
    fn same_value_allowlist_prefix_for_different_tags_applies_both_rules() {
        let filters = compile_all_filters(
            &[],
            &[
                value_allowlist(
                    "my.",
                    "customer_id",
                    &["top-1"],
                    TagValueMismatchAction::Remove,
                    "other",
                ),
                value_allowlist("my.", "region", &["us-east-1"], TagValueMismatchAction::Remove, "other"),
            ],
        );
        let mut metric = distribution_metric("my.metric.requests", &["customer_id:long-tail", "region:us-west-2"]);
        let mut state = TagSetMutViewState::default();

        assert_eq!(
            filter_metric_tags(&mut metric, &mut state, &filters),
            FilterMetricTagsOutcome::Modified { removed_tags: 2 }
        );
        assert!(tag_names(&metric).is_empty());
    }

    #[test]
    fn value_allowlist_preserves_whitespace_in_values_and_replacements() {
        // The tag name is normalized like any tag key, so its trailing space becomes the `_` of the stored key
        // `customer_id_`. Values and the replacement are matched and emitted exactly as configured.
        let filters = compile_all_filters(
            &[],
            &[value_allowlist(
                "my. ",
                "customer_id ",
                &[" top-1 "],
                TagValueMismatchAction::Replace,
                " other ",
            )],
        );
        let mut state = TagSetMutViewState::default();

        let mut allowed = distribution_metric("my. requests", &["customer_id : top-1 "]);
        assert_eq!(
            filter_metric_tags(&mut allowed, &mut state, &filters),
            FilterMetricTagsOutcome::NoChange
        );
        assert_eq!(tag_names(&allowed), vec!["customer_id : top-1 "]);

        let mut replaced = distribution_metric("my. requests", &["customer_id :long-tail"]);
        assert_eq!(
            filter_metric_tags(&mut replaced, &mut state, &filters),
            FilterMetricTagsOutcome::Modified { removed_tags: 1 }
        );
        assert_eq!(tag_names(&replaced), vec!["customer_id_: other "]);
    }

    #[test]
    fn compiled_rule_count_counts_exact_rules_and_each_prefix_tag_rule() {
        let key_entries = [MetricTagFilterEntry {
            metric_name: "my.metric".to_string(),
            action: FilterAction::Exclude,
            tags: vec!["host".to_string()],
        }];
        let value_entries = [
            value_allowlist("my.", "customer_id", &[], TagValueMismatchAction::Remove, "other"),
            value_allowlist("my.", "region", &[], TagValueMismatchAction::Remove, "other"),
        ];

        let filters = compile_all_filters(&key_entries, &value_entries);
        assert_eq!(filters.rule_count(), 3);
    }

    #[test]
    fn non_distribution_metric_unchanged() {
        let metric = counter_metric("my.counter", &["env:prod", "service:web"]);
        assert!(!metric.values().is_sketch(), "counter should not be a sketch");
        assert_eq!(tag_names(&metric), vec!["env:prod", "service:web"]);
    }

    #[test]
    fn bare_tag_excluded_by_name() {
        let entries = vec![MetricTagFilterEntry {
            metric_name: "my.dist".to_string(),
            action: FilterAction::Exclude,
            tags: vec!["production".to_string()],
        }];
        let filters = compile_filters(&entries);

        let mut metric = distribution_metric("my.dist", &["production", "service:web"]);
        let mut state = TagSetMutViewState::default();
        filter_metric_tags(&mut metric, &mut state, &filters);

        assert_eq!(tag_names(&metric), vec!["service:web"]);
    }

    #[test]
    fn empty_tag_list_include_removes_all() {
        let entries = vec![MetricTagFilterEntry {
            metric_name: "my.dist".to_string(),
            action: FilterAction::Include,
            tags: vec![],
        }];
        let filters = compile_filters(&entries);

        let mut metric = distribution_metric("my.dist", &["env:prod", "service:web"]);
        let mut state = TagSetMutViewState::default();
        filter_metric_tags(&mut metric, &mut state, &filters);

        assert!(metric.context().tags().is_empty());
    }

    #[test]
    fn empty_tag_list_exclude_keeps_all() {
        let entries = vec![MetricTagFilterEntry {
            metric_name: "my.dist".to_string(),
            action: FilterAction::Exclude,
            tags: vec![],
        }];
        let filters = compile_filters(&entries);

        let mut metric = distribution_metric("my.dist", &["env:prod", "service:web"]);
        let mut state = TagSetMutViewState::default();
        filter_metric_tags(&mut metric, &mut state, &filters);

        assert_eq!(tag_names(&metric), vec!["env:prod", "service:web"]);
    }

    #[test]
    fn merge_same_action_unions_tags() {
        let entries = vec![
            MetricTagFilterEntry {
                metric_name: "my.dist".to_string(),
                action: FilterAction::Exclude,
                tags: vec!["env".to_string()],
            },
            MetricTagFilterEntry {
                metric_name: "my.dist".to_string(),
                action: FilterAction::Exclude,
                tags: vec!["host".to_string()],
            },
        ];
        let filters = compile_filters(&entries);

        let mut metric = distribution_metric("my.dist", &["env:prod", "host:h1", "service:web"]);
        let mut state = TagSetMutViewState::default();
        filter_metric_tags(&mut metric, &mut state, &filters);

        assert_eq!(tag_names(&metric), vec!["service:web"]);
    }

    #[test]
    fn merge_conflicting_actions_exclude_wins() {
        let entries = vec![
            MetricTagFilterEntry {
                metric_name: "my.dist".to_string(),
                action: FilterAction::Include,
                tags: vec!["env".to_string()],
            },
            MetricTagFilterEntry {
                metric_name: "my.dist".to_string(),
                action: FilterAction::Exclude,
                tags: vec!["host".to_string()],
            },
        ];
        let filters = compile_filters(&entries);

        let mut metric = distribution_metric("my.dist", &["env:prod", "host:h1", "service:web"]);
        let mut state = TagSetMutViewState::default();
        filter_metric_tags(&mut metric, &mut state, &filters);

        assert_eq!(tag_names(&metric), vec!["env:prod", "service:web"]);
    }

    #[test]
    fn merge_conflicting_actions_exclude_first_wins() {
        let entries = vec![
            MetricTagFilterEntry {
                metric_name: "my.dist".to_string(),
                action: FilterAction::Exclude,
                tags: vec!["host".to_string()],
            },
            MetricTagFilterEntry {
                metric_name: "my.dist".to_string(),
                action: FilterAction::Include,
                tags: vec!["env".to_string()],
            },
        ];
        let filters = compile_filters(&entries);

        let mut metric = distribution_metric("my.dist", &["env:prod", "host:h1", "service:web"]);
        let mut state = TagSetMutViewState::default();
        filter_metric_tags(&mut metric, &mut state, &filters);

        assert_eq!(tag_names(&metric), vec!["env:prod", "service:web"]);
    }

    #[test]
    fn no_config_is_noop() {
        let filters = compile_filters(&[]);
        let mut metric = distribution_metric("my.dist", &["env:prod", "service:web"]);
        let mut state = TagSetMutViewState::default();
        filter_metric_tags(&mut metric, &mut state, &filters);
        assert_eq!(tag_names(&metric), vec!["env:prod", "service:web"]);
    }

    #[test]
    fn empty_metric_name_is_ignored() {
        let entries = vec![
            MetricTagFilterEntry {
                metric_name: String::new(),
                action: FilterAction::Exclude,
                tags: vec!["env".to_string()],
            },
            MetricTagFilterEntry {
                metric_name: "my.dist".to_string(),
                action: FilterAction::Exclude,
                tags: vec!["host".to_string()],
            },
        ];
        let filters = compile_filters(&entries);

        assert!(!filters.key_filters.contains_key(""));
        assert!(filters.key_filters.contains_key("my.dist"));
    }

    fn key_entry(metric_name: &str, action: FilterAction, tags: &[&str]) -> MetricTagFilterEntry {
        MetricTagFilterEntry {
            metric_name: metric_name.to_string(),
            action,
            tags: tags.iter().map(|tag| tag.to_string()).collect(),
        }
    }

    #[test]
    fn whole_tag_rules_match_normalized_tag_keys() {
        // Each case is a rule key and an input tag, and whether the tag's stored key matches the rule.
        let cases: &[(&str, &str, bool)] = &[
            ("env", "Env:Prod", true),
            ("Env", "env:prod", true),
            ("my tag", "My Tag:x", true),
            ("my tag", "my_tag", true),
            ("env", "1env:prod", true),
            ("caf\u{e9}", "CAF\u{c9}:x", true),
            // A trailing `_` is part of a stored key only when a value follows it.
            ("foo_", "foo_:x", true),
            ("foo_", "foo__:x", true),
            ("foo_", "foo_", false),
            ("foo", "foo_", true),
            ("foo", "foo_:x", false),
            // Values are not part of the key.
            ("env", "envx:prod", false),
        ];

        for &(rule, tag, matches) in cases {
            let filters = compile_filters(&[key_entry("my.dist", FilterAction::Exclude, &[rule])]);
            let mut metric = distribution_metric_with_origin_tags("my.dist", &[tag, "service:web"], &[tag]);
            filter_metric_tags(&mut metric, &mut TagSetMutViewState::default(), &filters);

            let expected: Vec<String> = if matches {
                vec!["service:web".to_string()]
            } else {
                let mut expected = vec![tag.to_string(), "service:web".to_string()];
                expected.sort();
                expected
            };
            assert_eq!(tag_names(&metric), expected, "rule {rule:?}, tag {tag:?}");
            assert_eq!(
                origin_tag_names(&metric).is_empty(),
                matches,
                "rule {rule:?}, origin tag {tag:?}"
            );
        }
    }

    #[test]
    fn include_rules_keep_tags_by_normalized_key_and_send_them_verbatim() {
        let filters = compile_filters(&[key_entry("my.dist", FilterAction::Include, &["Env", "Kube Namespace"])]);
        let mut metric = distribution_metric("my.dist", &["ENV:Prod", "kube_namespace:Default", "service:web"]);
        filter_metric_tags(&mut metric, &mut TagSetMutViewState::default(), &filters);

        assert_eq!(tag_names(&metric), vec!["ENV:Prod", "kube_namespace:Default"]);
    }

    #[test]
    fn rule_keys_with_a_colon_are_dropped_and_the_empty_key_is_kept() {
        let filters = compile_filters(&[key_entry(
            "my.dist",
            FilterAction::Include,
            &["123", "a:b", "", "_", "env"],
        )]);

        // `123`, `` and `_` all normalize to the empty key, which the intake stores for `:x` and `123:x`.
        let key_filter = filters.key_filters.get("my.dist").expect("rule should compile");
        assert_eq!(
            key_filter.tag_names,
            FastHashSet::from_iter(["env".to_string(), String::new()])
        );

        let mut metric = distribution_metric("my.dist", &["env:prod", "123:x", ":x", "a:b"]);
        filter_metric_tags(&mut metric, &mut TagSetMutViewState::default(), &filters);
        assert_eq!(tag_names(&metric), vec!["123:x", ":x", "env:prod"]);
    }

    #[test]
    fn whole_tag_rules_match_normalized_metric_names() {
        let filters = compile_filters(&[
            key_entry("my-service.requests", FilterAction::Exclude, &["env"]),
            key_entry("123", FilterAction::Exclude, &["env"]),
        ]);
        assert_eq!(
            filters.key_filters.len(),
            1,
            "a name the intake never stores is dropped"
        );

        for (name, matches) in [
            ("my_service.requests", true),
            ("my-service.requests", true),
            ("my service.requests", true),
            ("1my_service.requests", true),
            ("My_service.requests", false),
            ("my_service.requests.count", false),
        ] {
            let mut metric = counter_metric(name, &["env:prod", "service:web"]);
            let outcome = filter_metric_tags(&mut metric, &mut TagSetMutViewState::default(), &filters);
            if matches {
                assert_eq!(
                    outcome,
                    FilterMetricTagsOutcome::Modified { removed_tags: 1 },
                    "name: {name:?}"
                );
            } else {
                assert_eq!(outcome, FilterMetricTagsOutcome::RuleMiss, "name: {name:?}");
            }
        }
    }

    #[test]
    fn rules_merge_after_normalization() {
        let filters = compile_filters(&[
            key_entry("my.metric", FilterAction::Exclude, &["Env"]),
            key_entry("1my.metric", FilterAction::Exclude, &["host"]),
            key_entry("other.metric", FilterAction::Include, &["host"]),
            key_entry("_other.metric", FilterAction::Exclude, &["Env"]),
        ]);
        assert_eq!(filters.key_filters.len(), 2);

        let union = filters.key_filters.get("my.metric").expect("merged rule");
        assert!(union.is_exclude);
        assert_eq!(
            union.tag_names,
            FastHashSet::from_iter(["env".to_string(), "host".to_string()])
        );

        let exclude_wins = filters.key_filters.get("other.metric").expect("merged rule");
        assert!(exclude_wins.is_exclude);
        assert_eq!(exclude_wins.tag_names, FastHashSet::from_iter(["env".to_string()]));
    }

    #[test]
    fn value_allowlist_matches_normalized_tag_names_and_exact_values() {
        let filters = compile_all_filters(
            &[],
            &[value_allowlist(
                "my.",
                "Customer ID",
                &["top-1"],
                TagValueMismatchAction::Replace,
                "other",
            )],
        );
        let mut state = TagSetMutViewState::default();

        let mut allowed = counter_metric("my.metric", &["Customer_ID:top-1"]);
        assert_eq!(
            filter_metric_tags(&mut allowed, &mut state, &filters),
            FilterMetricTagsOutcome::NoChange
        );
        assert_eq!(tag_names(&allowed), vec!["Customer_ID:top-1"]);

        // Values are still compared exactly, and the replacement uses the normalized tag name.
        for tag in ["customer_id:TOP-1", "CUSTOMER ID:long-tail"] {
            let mut replaced = counter_metric("my.metric", &[tag]);
            assert_eq!(
                filter_metric_tags(&mut replaced, &mut state, &filters),
                FilterMetricTagsOutcome::Modified { removed_tags: 1 },
                "tag: {tag:?}"
            );
            assert_eq!(tag_names(&replaced), vec!["customer_id:other"], "tag: {tag:?}");
        }

        let mut already_replaced = counter_metric("my.metric", &["customer_id:other"]);
        assert_eq!(
            filter_metric_tags(&mut already_replaced, &mut state, &filters),
            FilterMetricTagsOutcome::NoChange
        );
    }

    #[test]
    fn value_allowlist_ignores_tags_stored_without_a_value() {
        // The intake stops reading a tag at input offset 400, so a `:` past that is never stored and the tag is bare:
        // this one is stored as just `a`.
        let filters = compile_all_filters(
            &[],
            &[value_allowlist("my.", "a", &[], TagValueMismatchAction::Remove, "")],
        );
        let tag: &'static str = format!("a{}:value", "!".repeat(500)).leak();

        let mut metric = counter_metric("my.metric", &[tag]);
        assert_eq!(
            filter_metric_tags(&mut metric, &mut TagSetMutViewState::default(), &filters),
            FilterMetricTagsOutcome::NoChange
        );
    }

    #[test]
    fn value_allowlist_drops_rules_that_cannot_have_a_value_or_collide() {
        let filters = compile_all_filters(
            &[],
            &[
                // A key of 200 bytes or more is never stored with a value.
                value_allowlist("my.", &"a".repeat(200), &[], TagValueMismatchAction::Remove, ""),
                value_allowlist("my.", "customer_id", &["top-1"], TagValueMismatchAction::Remove, ""),
                // Normalizes to the same key as the rule above, so it is dropped and the first rule is kept.
                value_allowlist("my.", "Customer ID", &[], TagValueMismatchAction::Remove, ""),
                value_allowlist("other.", "customer_id", &[], TagValueMismatchAction::Remove, ""),
            ],
        );
        assert_eq!(filters.rule_count(), 2);

        let mut metric = counter_metric("my.metric", &["customer_id:top-1", "env:x"]);
        assert_eq!(
            filter_metric_tags(&mut metric, &mut TagSetMutViewState::default(), &filters),
            FilterMetricTagsOutcome::NoChange
        );
    }

    #[test]
    fn include_rule_and_value_rule_match_the_same_normalized_key() {
        let filters = compile_all_filters(
            &[key_entry("my.metric", FilterAction::Include, &["Customer ID"])],
            &[value_allowlist(
                "my.",
                "customer_id",
                &["top-1"],
                TagValueMismatchAction::Replace,
                "other",
            )],
        );
        let mut metric = counter_metric("my.metric", &["CUSTOMER_ID:top-1", "service:web"]);
        filter_metric_tags(&mut metric, &mut TagSetMutViewState::default(), &filters);
        assert_eq!(tag_names(&metric), vec!["CUSTOMER_ID:top-1"]);

        let mut metric = counter_metric("my.metric", &["Customer_Id:long-tail", "service:web"]);
        filter_metric_tags(&mut metric, &mut TagSetMutViewState::default(), &filters);
        assert_eq!(tag_names(&metric), vec!["customer_id:other"]);
    }

    #[test]
    fn replacements_for_different_spellings_of_a_key_collapse_into_one_tag() {
        let filters = compile_all_filters(
            &[],
            &[value_allowlist(
                "my.",
                "env",
                &[],
                TagValueMismatchAction::Replace,
                "other",
            )],
        );
        let mut metric = counter_metric("my.metric", &["Env:a", "ENV:b", "env:c"]);
        assert_eq!(
            filter_metric_tags(&mut metric, &mut TagSetMutViewState::default(), &filters),
            FilterMetricTagsOutcome::Modified { removed_tags: 3 }
        );
        assert_eq!(tag_names(&metric), vec!["env:other"]);
    }

    #[test]
    fn origin_tags_preserved_after_filtering() {
        let context = Context::from_static_parts("my.dist", &["env:prod", "host:h1"]);
        let tag_set: TagSet = [Tag::from("service:web")].into_iter().collect();
        let new_context = context.with_tags(tag_set.into_shared());
        assert_eq!(new_context.name().as_ref(), "my.dist");
        assert!(new_context.origin_tags().is_empty());
        let names: Vec<_> = new_context.tags().into_iter().map(|t| t.as_str().to_owned()).collect();
        assert_eq!(names, vec!["service:web"]);
    }

    fn origin_tag_names(metric: &Metric) -> Vec<String> {
        let mut names: Vec<_> = metric
            .context()
            .origin_tags()
            .into_iter()
            .map(|t| t.as_str().to_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn exclude_removes_listed_origin_tags() {
        let entries = vec![MetricTagFilterEntry {
            metric_name: "my.dist".to_string(),
            action: FilterAction::Exclude,
            tags: vec!["env".to_string(), "host".to_string()],
        }];
        let filters = compile_filters(&entries);

        let mut metric =
            distribution_metric_with_origin_tags("my.dist", &["env:prod"], &["env:prod", "host:h1", "service:web"]);
        let mut state = TagSetMutViewState::default();
        filter_metric_tags(&mut metric, &mut state, &filters);

        assert_eq!(origin_tag_names(&metric), vec!["service:web"]);
    }

    #[test]
    fn include_keeps_only_listed_origin_tags() {
        let entries = vec![MetricTagFilterEntry {
            metric_name: "my.dist".to_string(),
            action: FilterAction::Include,
            tags: vec!["env".to_string()],
        }];
        let filters = compile_filters(&entries);

        let mut metric =
            distribution_metric_with_origin_tags("my.dist", &["env:prod"], &["env:prod", "host:h1", "service:web"]);
        let mut state = TagSetMutViewState::default();
        filter_metric_tags(&mut metric, &mut state, &filters);

        assert_eq!(origin_tag_names(&metric), vec!["env:prod"]);
    }

    #[test]
    fn origin_tags_empty_unchanged() {
        let entries = vec![MetricTagFilterEntry {
            metric_name: "my.dist".to_string(),
            action: FilterAction::Exclude,
            tags: vec!["env".to_string()],
        }];
        let filters = compile_filters(&entries);

        let mut metric = distribution_metric("my.dist", &["env:prod", "service:web"]);
        let mut state = TagSetMutViewState::default();
        filter_metric_tags(&mut metric, &mut state, &filters);

        assert_eq!(tag_names(&metric), vec!["service:web"]);
        assert!(metric.context().origin_tags().is_empty());
    }

    #[test]
    fn filtering_origin_tags_does_not_affect_shared_origin() {
        let origin_tag_set: TagSet = ["env:prod", "host:h1", "service:web"]
            .iter()
            .map(|s| Tag::from(*s))
            .collect();
        let shared_origin = origin_tag_set.into_shared();

        let ctx1 = Context::from_static_parts("my.dist", &[]).with_origin_tags(shared_origin.clone());
        let ctx2 = Context::from_static_parts("my.dist", &[]).with_origin_tags(shared_origin.clone());

        let mut metric1 = Metric::distribution(ctx1, 1.0);
        let metric2 = Metric::distribution(ctx2, 1.0);

        let entries = vec![MetricTagFilterEntry {
            metric_name: "my.dist".to_string(),
            action: FilterAction::Exclude,
            tags: vec!["env".to_string(), "host".to_string()],
        }];
        let filters = compile_filters(&entries);

        let mut state = TagSetMutViewState::default();
        filter_metric_tags(&mut metric1, &mut state, &filters);

        assert_eq!(origin_tag_names(&metric1), vec!["service:web"]);
        let metric2_origin: Vec<_> = metric2
            .context()
            .origin_tags()
            .into_iter()
            .map(|t| t.as_str().to_owned())
            .collect();
        assert!(
            metric2_origin.contains(&"env:prod".to_owned()),
            "shared origin_tags should not be mutated"
        );
        assert!(metric2_origin.contains(&"host:h1".to_owned()));
    }

    #[test]
    fn combined_tags_and_origin_tags_filtering() {
        let entries = vec![MetricTagFilterEntry {
            metric_name: "my.dist".to_string(),
            action: FilterAction::Exclude,
            tags: vec!["env".to_string(), "host".to_string()],
        }];
        let filters = compile_filters(&entries);

        let mut metric = distribution_metric_with_origin_tags(
            "my.dist",
            &["env:prod", "service:web", "host:h1"],
            &["env:prod", "host:h1", "region:us-east-1"],
        );
        let mut state = TagSetMutViewState::default();
        filter_metric_tags(&mut metric, &mut state, &filters);

        assert_eq!(tag_names(&metric), vec!["service:web"]);
        assert_eq!(origin_tag_names(&metric), vec!["region:us-east-1"]);
    }

    #[test]
    fn telemetry_records_hits_misses_and_filtered_tags() {
        let recorder = TestRecorder::default();
        let _local = metrics::set_default_local_recorder(&recorder);

        let builder = MetricsBuilder::default();
        let telemetry = Telemetry::new(&builder);

        assert_eq!(recorder.counter("tag_filterlist_rule_hits_total"), Some(0));
        assert_eq!(recorder.counter("tag_filterlist_rule_misses_total"), Some(0));
        assert_eq!(recorder.counter("tag_filterlist_noop_hits_total"), Some(0));
        assert_eq!(recorder.counter("tag_filterlist_metrics_modified_total"), Some(0));
        assert_eq!(recorder.counter("tag_filterlist_tags_filtered_total"), Some(0));

        telemetry.record(FilterMetricTagsOutcome::RuleMiss);
        telemetry.record(FilterMetricTagsOutcome::NoChange);
        telemetry.record(FilterMetricTagsOutcome::Modified { removed_tags: 3 });

        assert_eq!(recorder.counter("tag_filterlist_rule_hits_total"), Some(2));
        assert_eq!(recorder.counter("tag_filterlist_rule_misses_total"), Some(1));
        assert_eq!(recorder.counter("tag_filterlist_noop_hits_total"), Some(1));
        assert_eq!(recorder.counter("tag_filterlist_metrics_modified_total"), Some(1));
        assert_eq!(recorder.counter("tag_filterlist_tags_filtered_total"), Some(3));
    }

    #[test]
    fn telemetry_records_size() {
        let recorder = TestRecorder::default();
        let _local = metrics::set_default_local_recorder(&recorder);

        let builder = MetricsBuilder::default();
        let telemetry = Telemetry::new(&builder);

        telemetry.set_size(5);
        assert_eq!(recorder.gauge("tag_filterlist_size"), Some(5.0));

        telemetry.set_size(3);
        assert_eq!(recorder.gauge("tag_filterlist_size"), Some(3.0));

        telemetry.set_size(0);
        assert_eq!(recorder.gauge("tag_filterlist_size"), Some(0.0));
    }

    #[test]
    fn telemetry_records_updates() {
        let recorder = TestRecorder::default();
        let _local = metrics::set_default_local_recorder(&recorder);

        let builder = MetricsBuilder::default();
        let telemetry = Telemetry::new(&builder);

        assert_eq!(recorder.counter("tag_filterlist_updates_total"), Some(0));

        telemetry.increment_updates();
        assert_eq!(recorder.counter("tag_filterlist_updates_total"), Some(1));

        telemetry.increment_updates();
        assert_eq!(recorder.counter("tag_filterlist_updates_total"), Some(2));
    }

    #[tokio::test]
    async fn typed_live_updates_replace_whole_tag_rules_without_replacing_static_value_rules() {
        /// Sends the probe metric through the transform, and returns its tags as dispatched.
        async fn filter_probe(
            control: &ComponentControl<EventsBuffer>, output: &mut Output<EventsBuffer>,
        ) -> Vec<String> {
            let probe = distribution_metric("my.metric", &["customer_id:long-tail", "host:h1", "service:web"]);
            control.send_event(Event::Metric(probe)).await;
            let metric = output
                .next()
                .await
                .and_then(|buffer| buffer.into_iter().next())
                .and_then(Event::try_into_metric)
                .expect("the probe should be dispatched");
            tag_names(&metric)
        }

        let recorder = TestRecorder::default();
        let _local = metrics::set_default_local_recorder(&recorder);
        let updates = || {
            recorder.counter((
                "tag_filterlist_updates_total",
                &[("component_id", "tag_filterlist"), ("component_type", "transform")],
            ))
        };

        let (mut publisher, source) = saluki_common::sync::live::channel(Vec::<MetricTagFilterEntry>::new());
        let entries = source.live(|entries| entries);
        let value_allowlists = [value_allowlist(
            "my.",
            "customer_id",
            &["top-1"],
            TagValueMismatchAction::Remove,
            "other",
        )];
        let builder = TagFilterlistConfiguration::new(entries, &value_allowlists, 100_000);
        let (control, mut outputs) = TestComponentDriver::options()
            .with_component_id("tag_filterlist")
            .transform(builder)
            .await
            .expect("tag filterlist should build");
        let output = outputs.default_output();

        // This caches the probe's context, so later probes only change if an update rebuilds the cache.
        assert_eq!(filter_probe(&control, output).await, ["host:h1", "service:web"]);

        publisher.publish(vec![MetricTagFilterEntry {
            metric_name: "my.metric".to_string(),
            action: FilterAction::Exclude,
            tags: vec!["host".to_string()],
        }]);
        control
            .wait_until(
                || updates() == Some(1),
                || "the typed tag filter update is applied".to_string(),
            )
            .await;
        assert_eq!(filter_probe(&control, output).await, ["service:web"]);

        publisher.publish(Vec::new());
        control
            .wait_until(
                || updates() == Some(2),
                || "the cleared tag filter update is applied".to_string(),
            )
            .await;
        assert_eq!(filter_probe(&control, output).await, ["host:h1", "service:web"]);

        control.shutdown().await.expect("tag filterlist run should succeed");
    }

    #[test]
    fn modified_filter_marks_tagsets_as_modified() {
        let entries = vec![MetricTagFilterEntry {
            metric_name: "my.dist".to_string(),
            action: FilterAction::Exclude,
            tags: vec!["host".to_string()],
        }];
        let filters = compile_filters(&entries);

        let mut metric = distribution_metric("my.dist", &["env:prod", "host:h1"]);

        let mut state = TagSetMutViewState::default();
        let outcome = filter_metric_tags(&mut metric, &mut state, &filters);

        assert_eq!(outcome, FilterMetricTagsOutcome::Modified { removed_tags: 1 });
        assert_eq!(tag_names(&metric), vec!["env:prod"]);
        assert!(metric.context().tags().is_modified());
    }

    #[test]
    fn no_change_does_not_mark_tagsets_as_modified() {
        let entries = vec![MetricTagFilterEntry {
            metric_name: "my.dist".to_string(),
            action: FilterAction::Exclude,
            tags: vec!["region".to_string()],
        }];
        let filters = compile_filters(&entries);
        let shared_tags = ["env:prod", "host:h1"]
            .into_iter()
            .map(Tag::from)
            .collect::<TagSet>()
            .into_shared();
        let context = Context::from_parts("my.dist", shared_tags);
        let mut metric = Metric::distribution(context, 1.0);

        assert!(!metric.context().tags().is_modified());
        assert!(!metric.context().origin_tags().is_modified());

        let mut state = TagSetMutViewState::default();
        let outcome = filter_metric_tags(&mut metric, &mut state, &filters);

        assert_eq!(outcome, FilterMetricTagsOutcome::NoChange);
        assert_eq!(tag_names(&metric), vec!["env:prod", "host:h1"]);
        assert!(!metric.context().tags().is_modified());
        assert!(!metric.context().origin_tags().is_modified());
    }

    #[tokio::test]
    async fn run_loop_enforces_type_guard_and_exercises_context_cache() {
        // The other tests call `filter_metric_tags` directly; this one drives the real `Transform::run()` loop to
        // cover two behaviors those can't reach:
        //   1. the run-loop type guard filters only distribution (sketch) and count metrics, leaving other metric
        //      types (a gauge here) completely untouched even when a rule matches their name; and
        //   2. the per-context dedup cache is exercised end-to-end -- the cache is keyed by `Context`, so metrics
        //      that share a (name, tags) context resolve to a single cache entry, and every metric sharing that
        //      context is filtered identically (the second and later occurrences take the cache-hit branch).

        let entries = vec![MetricTagFilterEntry {
            metric_name: "svc.latency".to_string(),
            action: FilterAction::Exclude,
            tags: vec!["host".to_string()],
        }];
        let value_allowlists = vec![value_allowlist(
            "svc.",
            "customer_id",
            &[],
            TagValueMismatchAction::Replace,
            "other",
        )];
        let builder = TagFilterlistConfiguration::new(Live::new_fixed(entries), &value_allowlists, 100_000);
        let (control, mut outputs) = TestComponentDriver::transform(builder)
            .await
            .expect("tag filterlist should build");

        // A distribution, a counter, and a gauge that all share the same (name, tags) context, followed by a repeat
        // of the distribution. The counter and the repeated distribution hit the cache entry created by the first
        // distribution.
        let tags = &["host:h1", "env:prod", "customer_id:long-tail"];
        let unmatched = Context::from_static_parts("other.latency", tags);
        control
            .send_events([
                Event::Metric(Metric::distribution(
                    Context::from_static_parts("svc.latency", tags),
                    1.0,
                )),
                Event::Metric(Metric::counter(Context::from_static_parts("svc.latency", tags), 1.0)),
                Event::Metric(Metric::gauge(Context::from_static_parts("svc.latency", tags), 1.0)),
                Event::Metric(Metric::distribution(
                    Context::from_static_parts("svc.latency", tags),
                    1.0,
                )),
                Event::Metric(Metric::counter(unmatched.clone(), 1.0)),
            ])
            .await;

        // Shutting down closes the input, which is what ends the run loop.
        control.shutdown().await.expect("tag filterlist run should succeed");

        let dispatched: Vec<Metric> = outputs
            .default_output()
            .collect_events()
            .await
            .into_iter()
            .map(|event| event.try_into_metric().expect("only metrics should be dispatched"))
            .collect();

        // Nothing is dropped by the transform; order is preserved.
        assert_eq!(dispatched.len(), 5);

        let sorted_tags = |metric: &Metric| {
            let mut names: Vec<String> = metric
                .context()
                .tags()
                .into_iter()
                .map(|t| t.as_str().to_owned())
                .collect();
            names.sort();
            names
        };

        // Distribution (sketch) -> filtered on the cache-miss path.
        assert!(dispatched[0].values().is_sketch());
        assert_eq!(sorted_tags(&dispatched[0]), vec!["customer_id:other", "env:prod"]);
        // Counter (count metric) -> filtered via the cache-hit branch (shares the distribution's context entry).
        assert!(matches!(dispatched[1].values(), MetricValues::Counter(_)));
        assert_eq!(sorted_tags(&dispatched[1]), vec!["customer_id:other", "env:prod"]);
        // Gauge -> NOT a sketch and NOT a counter, so the type guard skips it and it passes through untouched.
        assert!(!dispatched[2].values().is_sketch());
        assert!(!matches!(dispatched[2].values(), MetricValues::Counter(_)));
        assert_eq!(
            sorted_tags(&dispatched[2]),
            vec!["customer_id:long-tail", "env:prod", "host:h1"],
            "gauge metrics must not be filtered by the type guard"
        );
        // Repeated distribution -> filtered via the cache-hit branch, identical to the first distribution.
        assert!(dispatched[3].values().is_sketch());
        assert_eq!(sorted_tags(&dispatched[3]), vec!["customer_id:other", "env:prod"]);
        let cached_tag = dispatched[0].context().tags().get_single_tag("customer_id").unwrap();
        for index in [1, 3] {
            let hit_tag = dispatched[index]
                .context()
                .tags()
                .get_single_tag("customer_id")
                .unwrap();
            assert!(std::ptr::eq(cached_tag.as_str(), hit_tag.as_str()));
        }
        let expected_name = unmatched.name();
        let unmatched_name = dispatched[4].context().name();
        assert!(std::ptr::eq::<str>(expected_name.as_ref(), unmatched_name.as_ref()));
    }

    mod pipeline_integration {
        use std::{
            num::{NonZeroU64, NonZeroUsize},
            sync::Mutex,
            time::Duration,
        };

        use async_trait::async_trait;
        use saluki_common::sync::live::Live;
        use saluki_components::transforms::{
            aggregate_context_snapshot_channel, AggregateConfiguration, AggregateMetricType, ChainedConfiguration,
            DogStatsDMapperConfiguration, DogStatsDMapperProfile, DogStatsDMetricMapping, HistogramConfiguration,
        };
        use saluki_core::{
            accounting::{ComponentRegistry, MemoryBounds, MemoryBoundsBuilder, MemoryLimiter},
            components::{
                destinations::{Destination, DestinationBuilder, DestinationContext},
                sources::{Source, SourceBuilder, SourceContext},
                BuildContext,
            },
            data_model::event::{metric::Metric, EventType},
            health::HealthRegistry,
            runtime::{state::ResourceRegistry, Supervisor},
            topology::{OutputDefinition, TopologyBlueprint},
        };
        use saluki_error::{generic_error, GenericError};
        use tokio::sync::{mpsc, oneshot};

        use super::*;
        use crate::components::dogstatsd_filterlist::MetricNameFilter;
        use crate::components::{
            dogstatsd_no_agg_split::DogStatsDNoAggSplitConfiguration,
            dogstatsd_prefix_filter::DogStatsDPrefixFilterConfiguration,
        };

        #[tokio::test]
        async fn retained_context_identity_follows_dogstatsd_post_processing() {
            tokio::time::timeout(Duration::from_secs(5), async {
                let mapper = DogStatsDMapperConfiguration::new(
                    NonZeroUsize::new(64 * 1024).expect("not zero"),
                    1_000,
                    vec![DogStatsDMapperProfile {
                        name: "retained-context-test".to_string(),
                        prefix: "raw.requests.".to_string(),
                        mappings: vec![DogStatsDMetricMapping {
                            metric_match: "raw.requests.*".to_string(),
                            match_type: String::new(),
                            name: "mapped.requests".to_string(),
                            tags: [("route".to_string(), "$1".to_string())].into(),
                        }],
                    }],
                );
                let mapper_chain = ChainedConfiguration::default().with_transform_builder("dogstatsd_mapper", mapper);
                let prefix_filter = DogStatsDPrefixFilterConfiguration::new(
                    "tenant".to_string(),
                    Vec::new(),
                    Live::new_fixed(MetricNameFilter {
                        values: vec!["tenant.raw.blocked".to_string()],
                        match_prefix: false,
                        prefix_rules: Vec::new(),
                    }),
                );
                let tag_filter = TagFilterlistConfiguration::new(
                    Live::new_fixed(vec![MetricTagFilterEntry {
                        metric_name: "tenant.mapped.requests".to_string(),
                        action: FilterAction::Exclude,
                        tags: vec!["remove".to_string()],
                    }]),
                    &[MetricTagValueAllowlistEntry {
                        metric_prefix: "tenant.mapped.".to_string(),
                        tag_name: "customer_id".to_string(),
                        values: vec!["top-1".to_string()],
                        on_miss: TagValueMismatchAction::Remove,
                        replacement: "other".to_string(),
                    }],
                    0,
                );
                let hist_config = HistogramConfiguration::new(
                    &[
                        "max".to_string(),
                        "median".to_string(),
                        "avg".to_string(),
                        "count".to_string(),
                    ],
                    &["0.95".to_string()],
                    false,
                    String::new(),
                );
                let (snapshot_handle, context_snapshot_receiver) = aggregate_context_snapshot_channel();
                let aggregate = AggregateConfiguration {
                    window_duration_seconds: NonZeroU64::new(10).expect("not zero"),
                    primary_flush_interval: Duration::from_secs(60),
                    context_limit: 1_000_000,
                    flush_open_windows: false,
                    counter_expiry_seconds: Some(300),
                    hist_config,
                    origin_telemetry_enabled: false,
                    context_snapshot_receiver,
                };

                let (events_tx, events_rx) = mpsc::channel(2);
                let source = ControlledMetricSourceBuilder {
                    events: Mutex::new(Some(events_rx)),
                    outputs: vec![OutputDefinition::default_output(EventType::Metric)],
                };
                let component_registry = ComponentRegistry::default();
                let mut blueprint = TopologyBlueprint::new("dogstatsd_retained_identity", &component_registry);
                blueprint
                    .add_source("source", source)
                    .expect("controlled source should be accepted")
                    .add_transform("mapper", mapper_chain)
                    .expect("mapper should be accepted")
                    .add_transform("prefix_filter", prefix_filter)
                    .expect("prefix filter should be accepted")
                    .add_transform("tag_filter", tag_filter)
                    .expect("tag filter should be accepted")
                    .add_transform("aggregate", aggregate)
                    .expect("aggregate should be accepted")
                    .add_destination("destination", DrainingMetricDestinationBuilder)
                    .expect("draining destination should be accepted");
                blueprint
                    .connect_components_in_order([
                        "source",
                        "mapper",
                        "prefix_filter",
                        "tag_filter",
                        "aggregate",
                        "destination",
                    ])
                    .expect("DogStatsD post-processing topology should connect");
                blueprint
                    .with_health_registry(HealthRegistry::new())
                    .with_memory_limiter(MemoryLimiter::noop())
                    .with_resource_registry(ResourceRegistry::new())
                    .with_ambient_worker_pool();

                let mut supervisor =
                    Supervisor::new("dogstatsd-retained-identity").expect("test supervisor should be created");
                supervisor.add_worker(blueprint);
                let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
                let topology_task = tokio::spawn(async move { supervisor.run_with_shutdown(shutdown_rx).await });

                events_tx
                    .send(Event::Metric(Metric::counter("raw.blocked", 1.0)))
                    .await
                    .expect("controlled source should accept the blocked metric");
                let input_context = Context::from_static_parts(
                    "raw.requests.checkout",
                    &[
                        "customer_id:top-1",
                        "customer_id:long-tail",
                        "keep:client",
                        "remove:secret",
                    ],
                );
                events_tx
                    .send(Event::Metric(Metric::counter(input_context.clone(), 1.0)))
                    .await
                    .expect("controlled source should accept the retained metric");

                let expected_context = Context::from_static_parts(
                    "tenant.mapped.requests",
                    &["customer_id:top-1", "keep:client", "route:checkout"],
                );
                let snapshot = loop {
                    let snapshot = snapshot_handle
                        .snapshot()
                        .await
                        .expect("running aggregate should fulfill snapshots");
                    if snapshot.iter().any(|entry| entry.context() == &expected_context) {
                        break snapshot;
                    }
                    tokio::task::yield_now().await;
                };

                assert_eq!(snapshot.len(), 1);
                assert_eq!(snapshot[0].context(), &expected_context);
                assert_eq!(snapshot[0].metric_type(), AggregateMetricType::Counter);
                assert_eq!(snapshot[0].context().tags().len(), 3);
                assert_eq!(
                    snapshot[0]
                        .context()
                        .tags()
                        .get_single_tag("customer_id")
                        .and_then(|tag| tag.value()),
                    Some("top-1")
                );
                assert_eq!(
                    snapshot[0]
                        .context()
                        .tags()
                        .get_single_tag("keep")
                        .and_then(|tag| tag.value()),
                    Some("client")
                );
                assert_eq!(
                    snapshot[0]
                        .context()
                        .tags()
                        .get_single_tag("route")
                        .and_then(|tag| tag.value()),
                    Some("checkout")
                );
                assert!(snapshot[0].context().tags().get_single_tag("remove").is_none());
                assert!(snapshot.iter().all(|entry| entry.context() != &input_context));
                assert!(snapshot
                    .iter()
                    .all(|entry| !matches!(entry.context().name().as_ref(), "raw.blocked" | "tenant.raw.blocked")));

                drop(events_tx);
                drop(snapshot_handle);
                shutdown_tx.send(()).expect("test topology should still be running");
                let topology_result = topology_task.await.expect("topology task should not panic");
                assert!(
                    topology_result.is_ok(),
                    "topology should stop cleanly: {topology_result:?}"
                );
            })
            .await
            .expect("DogStatsD retained identity test should complete without hanging");
        }

        #[tokio::test]
        async fn no_agg_split_excludes_timestamped_metrics_from_tag_filterlist() {
            tokio::time::timeout(Duration::from_secs(5), async {
                let no_agg_split = DogStatsDNoAggSplitConfiguration::new(Duration::from_millis(50));
                let tag_filter = TagFilterlistConfiguration::new(
                    Live::new_fixed(vec![MetricTagFilterEntry {
                        metric_name: "app.requests".to_string(),
                        action: FilterAction::Exclude,
                        tags: vec!["remove".to_string()],
                    }]),
                    &[],
                    0,
                );
                let hist_config = HistogramConfiguration::new(&[], &[], false, String::new());
                let (snapshot_handle, context_snapshot_receiver) = aggregate_context_snapshot_channel();
                let aggregate = AggregateConfiguration {
                    window_duration_seconds: NonZeroU64::new(10).expect("not zero"),
                    primary_flush_interval: Duration::from_secs(60),
                    context_limit: 1_000_000,
                    flush_open_windows: false,
                    counter_expiry_seconds: Some(300),
                    hist_config,
                    origin_telemetry_enabled: false,
                    context_snapshot_receiver,
                };

                let (events_tx, events_rx) = mpsc::channel(2);
                let source = ControlledMetricSourceBuilder {
                    events: Mutex::new(Some(events_rx)),
                    outputs: vec![OutputDefinition::default_output(EventType::Metric)],
                };
                let (passthrough_tx, mut passthrough_rx) = mpsc::unbounded_channel();
                let passthrough_destination = CollectingMetricDestinationBuilder { sender: passthrough_tx };

                let component_registry = ComponentRegistry::default();
                let mut blueprint = TopologyBlueprint::new("dogstatsd_no_agg_split_filterlist", &component_registry);
                blueprint
                    .add_source("source", source)
                    .expect("controlled source should be accepted")
                    .add_transform("no_agg_split", no_agg_split)
                    .expect("no-agg split should be accepted")
                    .add_transform("tag_filter", tag_filter)
                    .expect("tag filter should be accepted")
                    .add_transform("aggregate", aggregate)
                    .expect("aggregate should be accepted")
                    .add_destination("aggregate_out", DrainingMetricDestinationBuilder)
                    .expect("draining destination should be accepted")
                    .add_destination("passthrough_out", passthrough_destination)
                    .expect("collecting destination should be accepted");
                blueprint
                    .connect_components_in_order(["source", "no_agg_split", "tag_filter", "aggregate", "aggregate_out"])
                    .expect("no-agg split topology should connect")
                    .connect_components("no_agg_split.passthrough", "passthrough_out")
                    .expect("passthrough output should connect");
                blueprint
                    .with_health_registry(HealthRegistry::new())
                    .with_memory_limiter(MemoryLimiter::noop())
                    .with_resource_registry(ResourceRegistry::new())
                    .with_ambient_worker_pool();

                let mut supervisor =
                    Supervisor::new("dogstatsd-no-agg-split-filterlist").expect("test supervisor should be created");
                supervisor.add_worker(blueprint);
                let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
                let topology_task = tokio::spawn(async move { supervisor.run_with_shutdown(shutdown_rx).await });

                let timestamped_context =
                    Context::from_static_parts("app.requests", &["keep:client", "remove:secret", "which:timestamped"]);
                events_tx
                    .send(Event::Metric(Metric::counter(
                        timestamped_context.clone(),
                        (1_700_000_000, 1.0),
                    )))
                    .await
                    .expect("controlled source should accept the timestamped metric");

                let non_timestamped_context = Context::from_static_parts(
                    "app.requests",
                    &["keep:client", "remove:secret", "which:non_timestamped"],
                );
                events_tx
                    .send(Event::Metric(Metric::counter(non_timestamped_context.clone(), 1.0)))
                    .await
                    .expect("controlled source should accept the non-timestamped metric");

                let passthrough_metric = passthrough_rx
                    .recv()
                    .await
                    .expect("passthrough destination should receive the timestamped metric");
                assert_eq!(
                    passthrough_metric
                        .context()
                        .tags()
                        .get_single_tag("remove")
                        .and_then(|tag| tag.value()),
                    Some("secret"),
                    "timestamped metrics must not be filtered by the tag filterlist"
                );

                let expected_context =
                    Context::from_static_parts("app.requests", &["keep:client", "which:non_timestamped"]);
                let snapshot = loop {
                    let snapshot = snapshot_handle
                        .snapshot()
                        .await
                        .expect("running aggregate should fulfill snapshots");
                    if snapshot.iter().any(|entry| entry.context() == &expected_context) {
                        break snapshot;
                    }
                    tokio::task::yield_now().await;
                };
                assert!(snapshot.iter().all(|entry| entry.context() != &non_timestamped_context));
                assert_eq!(snapshot.len(), 1);
                assert!(snapshot[0].context().tags().get_single_tag("remove").is_none());

                drop(events_tx);
                drop(snapshot_handle);
                shutdown_tx.send(()).expect("test topology should still be running");
                let topology_result = topology_task.await.expect("topology task should not panic");
                assert!(
                    topology_result.is_ok(),
                    "topology should stop cleanly: {topology_result:?}"
                );
            })
            .await
            .expect("no-agg split filterlist exclusion test should complete without hanging");
        }

        struct ControlledMetricSource {
            events: mpsc::Receiver<Event>,
        }

        #[async_trait]
        impl Source for ControlledMetricSource {
            async fn run(mut self: Box<Self>, mut context: SourceContext) -> Result<(), GenericError> {
                let shutdown = context.take_shutdown_handle();
                tokio::pin!(shutdown);
                let mut events_open = true;

                loop {
                    tokio::select! {
                        _ = &mut shutdown => break,
                        maybe_event = self.events.recv(), if events_open => match maybe_event {
                            Some(event) => context.dispatcher().dispatch_one(event).await?,
                            None => events_open = false,
                        }
                    }
                }
                Ok(())
            }
        }

        struct ControlledMetricSourceBuilder {
            events: Mutex<Option<mpsc::Receiver<Event>>>,
            outputs: Vec<OutputDefinition<EventType>>,
        }

        #[async_trait]
        impl SourceBuilder for ControlledMetricSourceBuilder {
            fn outputs(&self) -> &[OutputDefinition<EventType>] {
                &self.outputs
            }

            async fn build(&self, _context: BuildContext) -> Result<Box<dyn Source + Send>, GenericError> {
                let events = self
                    .events
                    .lock()
                    .map_err(|_| generic_error!("controlled metric source receiver lock is poisoned"))?
                    .take()
                    .ok_or_else(|| generic_error!("controlled metric source receiver has already been taken"))?;
                Ok(Box::new(ControlledMetricSource { events }))
            }
        }

        impl MemoryBounds for ControlledMetricSourceBuilder {
            fn specify_bounds(&self, _builder: &mut MemoryBoundsBuilder) {}
        }

        struct DrainingMetricDestination;

        #[async_trait]
        impl Destination for DrainingMetricDestination {
            async fn run(self: Box<Self>, mut context: DestinationContext) -> Result<(), GenericError> {
                while context.events().next().await.is_some() {}
                Ok(())
            }
        }

        struct DrainingMetricDestinationBuilder;

        #[async_trait]
        impl DestinationBuilder for DrainingMetricDestinationBuilder {
            fn input_event_type(&self) -> EventType {
                EventType::Metric
            }

            async fn build(&self, _context: BuildContext) -> Result<Box<dyn Destination + Send>, GenericError> {
                Ok(Box::new(DrainingMetricDestination))
            }
        }

        impl MemoryBounds for DrainingMetricDestinationBuilder {
            fn specify_bounds(&self, _builder: &mut MemoryBoundsBuilder) {}
        }

        struct CollectingMetricDestination {
            sender: mpsc::UnboundedSender<Metric>,
        }

        #[async_trait]
        impl Destination for CollectingMetricDestination {
            async fn run(self: Box<Self>, mut context: DestinationContext) -> Result<(), GenericError> {
                while let Some(events) = context.events().next().await {
                    for event in events {
                        if let Some(metric) = event.try_into_metric() {
                            let _ = self.sender.send(metric);
                        }
                    }
                }
                Ok(())
            }
        }

        struct CollectingMetricDestinationBuilder {
            sender: mpsc::UnboundedSender<Metric>,
        }

        #[async_trait]
        impl DestinationBuilder for CollectingMetricDestinationBuilder {
            fn input_event_type(&self) -> EventType {
                EventType::Metric
            }

            async fn build(&self, _context: BuildContext) -> Result<Box<dyn Destination + Send>, GenericError> {
                Ok(Box::new(CollectingMetricDestination {
                    sender: self.sender.clone(),
                }))
            }
        }

        impl MemoryBounds for CollectingMetricDestinationBuilder {
            fn specify_bounds(&self, _builder: &mut MemoryBoundsBuilder) {}
        }
    }
}
