//! Metric Tag Filterlist synchronous transform.
//!
//! Removes or retains specific tags from distribution and count metrics based on per-metric
//! configuration. Supports both "exclude" (denylist) and "include" (allowlist) modes.
//!
//! Configuration is read from the `metric_tag_filterlist` key and can be updated at runtime via
//! Remote Config.

mod telemetry;

use std::{collections::HashMap as StdHashMap, num::NonZeroUsize, time::Duration};

use async_trait::async_trait;
use foldhash::fast::RandomState as FoldHashState;
use hashbrown::{HashMap, HashSet};
use saluki_common::cache::{Cache, CacheBuilder};
use saluki_config::GenericConfiguration;
use saluki_context::{tags::Tag, Context, TagSetMutViewState};
use saluki_core::accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_core::{
    components::{
        transforms::{Transform, TransformBuilder, TransformContext},
        ComponentContext,
    },
    data_model::event::{
        metric::{Metric, MetricValues},
        EventType,
    },
    observability::ComponentMetricsExt,
    topology::OutputDefinition,
};
use saluki_error::{generic_error, GenericError};
use saluki_metrics::MetricsBuilder;
use serde::{de::Deserializer, Deserialize};
use tokio::select;
use tracing::{debug, error, warn};

use crate::components::dogstatsd_filterlist::METRIC_TAG_FILTERLIST_CONFIG_KEY;

const CONTEXT_CACHE_TTI: Duration = Duration::from_secs(30);
const CONTEXT_CACHE_EXPIRATION_INTERVAL: Duration = Duration::from_secs(1);

fn default_context_cache_capacity() -> usize {
    100_000
}

use self::telemetry::Telemetry;

/// Action applied to the configured tag list: keep only listed tags, or remove listed tags.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum FilterAction {
    /// Keep only the tags whose key appears in the configured list.
    Include,
    /// Remove the tags whose key appears in the configured list.
    #[default]
    Exclude,
}

impl<'de> Deserialize<'de> for FilterAction {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = Option::<String>::deserialize(deserializer)?;

        match raw.as_deref() {
            Some("include") => Ok(Self::Include),
            Some("exclude") | None | Some("") => Ok(Self::Exclude),
            Some(other) => {
                warn!(
                    action = %other,
                    "`metric_tag_filterlist.*.action` should be either `include` or `exclude`; defaulting to `exclude`."
                );
                Ok(Self::Exclude)
            }
        }
    }
}

fn default_tag_value_replacement() -> String {
    "other".to_string()
}

/// Action applied when a tag value is absent from its allowlist.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum TagValueMismatchAction {
    /// Removes the tag.
    #[default]
    Remove,
    /// Replaces the tag value with the configured sentinel.
    Replace,
}

/// An allowlist and mismatch behavior for one tag key.
#[derive(Clone, Debug, Deserialize)]
pub struct TagValueAllowlist {
    /// Tag values retained unchanged.
    ///
    /// The list is empty by default, so every value is treated as a mismatch.
    #[serde(default)]
    pub values: Vec<String>,
    /// Action applied to values absent from `values`.
    ///
    /// The default is [`Remove`][TagValueMismatchAction::Remove].
    #[serde(default)]
    pub on_miss: TagValueMismatchAction,
    /// Value used when `on_miss` is [`Replace`][TagValueMismatchAction::Replace].
    ///
    /// The default is `other`. Configure a value that cannot collide with a real tag value.
    #[serde(default = "default_tag_value_replacement")]
    pub replacement: String,
}

impl Default for TagValueAllowlist {
    fn default() -> Self {
        Self {
            values: Vec::new(),
            on_miss: TagValueMismatchAction::Remove,
            replacement: default_tag_value_replacement(),
        }
    }
}

/// A single metric tag filter entry.
#[derive(Clone, Debug, Deserialize)]
pub struct MetricTagFilterEntry {
    /// The exact metric name this entry applies to.
    pub metric_name: String,
    /// Whether to include or exclude the listed tags.
    #[serde(default)]
    pub action: FilterAction,
    /// Tag key names to include or exclude.
    pub tags: Vec<String>,
    /// Value allowlists for specific tag keys.
    ///
    /// After applying `action` and `tags`, a tag whose key is present in this map is retained only
    /// when its value is listed. The configured mismatch action removes the tag or replaces its
    /// value. The map is empty by default, which disables value filtering. Use this for tag keys
    /// whose unbounded values would create excessive metric cardinality.
    #[serde(default)]
    pub tag_value_allowlists: StdHashMap<String, TagValueAllowlist>,
}

#[derive(Eq, PartialEq)]
enum CompiledMismatchAction {
    Remove,
    Replace(Tag),
}

struct CompiledTagValueAllowlist {
    allowed_values: HashSet<String, FoldHashState>,
    on_miss: CompiledMismatchAction,
}

/// A compiled filter rule for one metric name.
pub struct CompiledFilter {
    is_exclude: bool,
    tag_names: HashSet<String, FoldHashState>,
    tag_value_allowlists: HashMap<String, CompiledTagValueAllowlist, FoldHashState>,
}

/// Compiled filter table keyed by metric name.
pub type CompiledFilters = HashMap<String, CompiledFilter, FoldHashState>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
/// Outcome of attempting to apply `metric_tag_filterlist` rules to a metric.
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
/// Merge rules:
/// - Same metric name + same action → union of tag key sets.
/// - Same metric name + conflicting actions → `exclude` wins.
/// - Value allowlists for the same metric and tag key → union of allowed values.
///
/// # Errors
///
/// Returns an error when duplicate rules for the same metric and tag key configure different
/// mismatch actions or replacement values.
pub fn compile_filters(entries: &[MetricTagFilterEntry]) -> Result<CompiledFilters, GenericError> {
    let mut filters: CompiledFilters = HashMap::with_hasher(FoldHashState::default());

    for entry in entries {
        if entry.metric_name.is_empty() {
            continue;
        }

        let is_exclude = entry.action == FilterAction::Exclude;
        let mut tag_names = HashSet::with_capacity_and_hasher(entry.tags.len(), FoldHashState::default());
        tag_names.extend(entry.tags.iter().cloned());

        let mut tag_value_allowlists =
            HashMap::with_capacity_and_hasher(entry.tag_value_allowlists.len(), FoldHashState::default());
        for (tag_name, allowlist) in &entry.tag_value_allowlists {
            let mut allowed_values =
                HashSet::with_capacity_and_hasher(allowlist.values.len(), FoldHashState::default());
            allowed_values.extend(allowlist.values.iter().cloned());
            let on_miss = match allowlist.on_miss {
                TagValueMismatchAction::Remove => CompiledMismatchAction::Remove,
                TagValueMismatchAction::Replace => {
                    CompiledMismatchAction::Replace(Tag::from(format!("{}:{}", tag_name, allowlist.replacement)))
                }
            };
            tag_value_allowlists.insert(
                tag_name.clone(),
                CompiledTagValueAllowlist {
                    allowed_values,
                    on_miss,
                },
            );
        }

        match filters.entry(entry.metric_name.clone()) {
            hashbrown::hash_map::Entry::Vacant(e) => {
                e.insert(CompiledFilter {
                    is_exclude,
                    tag_names,
                    tag_value_allowlists,
                });
            }
            hashbrown::hash_map::Entry::Occupied(mut e) => {
                let existing = e.get_mut();
                if existing.is_exclude == is_exclude {
                    existing.tag_names.extend(tag_names);
                } else if is_exclude {
                    existing.is_exclude = true;
                    existing.tag_names = tag_names;
                }

                for (tag_name, allowlist) in tag_value_allowlists {
                    match existing.tag_value_allowlists.entry(tag_name) {
                        hashbrown::hash_map::Entry::Vacant(e) => {
                            e.insert(allowlist);
                        }
                        hashbrown::hash_map::Entry::Occupied(mut e) => {
                            if e.get().on_miss != allowlist.on_miss {
                                return Err(generic_error!(
                                    "Conflicting tag value mismatch behavior for metric '{}' and tag '{}'. \
                                     Combine the duplicate rules or configure the same `on_miss` and `replacement` values.",
                                    entry.metric_name,
                                    e.key()
                                ));
                            }
                            e.get_mut().allowed_values.extend(allowlist.allowed_values);
                        }
                    }
                }
            }
        }
    }

    Ok(filters)
}

/// Metric Tag Filterlist transform.
///
/// Removes or retains specific tags from counter and sketch-backed metrics based on per-metric configuration.
/// Configuration is read from `metric_tag_filterlist` and supports runtime updates via Remote Config.
#[derive(Deserialize)]
pub struct TagFilterlistConfiguration {
    #[serde(default, rename = "metric_tag_filterlist")]
    entries: Vec<MetricTagFilterEntry>,

    /// Maximum number of entries in the per-context deduplication cache used by the tag filter.
    ///
    /// Configured via `data_plane.dogstatsd.aggregator_tag_filter_cache_capacity` in the agent config
    /// stream. High-throughput deployments with many unique metric contexts may benefit from
    /// increasing this value to reduce cache churn.
    ///
    /// Defaults to 100,000.
    #[serde(skip)]
    context_cache_capacity: usize,

    #[serde(skip)]
    configuration: Option<GenericConfiguration>,
}

impl TagFilterlistConfiguration {
    /// Creates a new `TagFilterlistConfiguration` from the given configuration.
    pub fn from_configuration(config: &GenericConfiguration) -> Result<Self, GenericError> {
        let mut typed: Self = config.as_typed()?;
        typed.context_cache_capacity = config
            .try_get_typed("data_plane.dogstatsd.aggregator_tag_filter_cache_capacity")?
            .unwrap_or_else(default_context_cache_capacity);
        typed.configuration = Some(config.clone());
        Ok(typed)
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

    async fn build(&self, context: ComponentContext) -> Result<Box<dyn Transform + Send>, GenericError> {
        let metrics_builder = MetricsBuilder::from_component_context(&context);
        let telemetry = Telemetry::new(&metrics_builder);
        let filters = compile_filters(&self.entries)?;
        telemetry.set_size(filters.len());

        Ok(Box::new(TagFilterlist {
            filters,
            configuration: self
                .configuration
                .clone()
                .expect("configuration must be set via from_configuration"),
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
    configuration: GenericConfiguration,
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

#[async_trait]
impl Transform for TagFilterlist {
    async fn run(mut self: Box<Self>, mut context: TransformContext) -> Result<(), GenericError> {
        let mut health = context.take_health_handle();
        health.mark_ready();

        let mut watcher = self.configuration.watch_for_updates(METRIC_TAG_FILTERLIST_CONFIG_KEY);

        let mut view_state = TagSetMutViewState::default();

        debug!("Metric Tag Filterlist transform started.");

        loop {
            select! {
                _ = health.live() => continue,
                maybe_events = context.events().next() => match maybe_events {
                    Some(mut events) => {
                        for event in &mut events {
                            if let Some(metric) = event.try_as_metric_mut() {
                                if metric.values().is_sketch()
                                    || matches!(metric.values(), MetricValues::Counter(_))
                                {
                                    let original_context = metric.context().clone();

                                    if let Some(cached) = self.context_cache.get(&original_context) {
                                        match cached {
                                            None => self.telemetry.record(FilterMetricTagsOutcome::NoChange),
                                            Some((filtered_ctx, removed_tags)) => {
                                                *metric.context_mut() = filtered_ctx;
                                                self.telemetry.record(FilterMetricTagsOutcome::Modified { removed_tags });
                                            }
                                        }
                                    } else {
                                        let outcome = filter_metric_tags(metric, &mut view_state, &self.filters);
                                        self.telemetry.record(outcome);

                                        match outcome {
                                            FilterMetricTagsOutcome::RuleMiss => {}
                                            FilterMetricTagsOutcome::NoChange => {
                                                self.context_cache.insert(original_context, None);
                                            }
                                            FilterMetricTagsOutcome::Modified { removed_tags } => {
                                                self.context_cache.insert(
                                                    original_context,
                                                    Some((metric.context().clone(), removed_tags)),
                                                );
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        if let Err(e) = context.dispatcher().dispatch(events).await {
                            error!(error = %e, "Failed to dispatch events.");
                        }
                    }
                    None => break,
                },
                (_, new_entries) = watcher.changed::<Vec<MetricTagFilterEntry>>() => {
                    match compile_filters(new_entries.as_deref().unwrap_or(&[])) {
                        Ok(filters) => {
                            self.filters = filters;
                            self.context_cache = build_context_cache(self.context_cache_capacity);
                            self.telemetry.set_size(self.filters.len());
                            self.telemetry.increment_updates();
                            debug!(rules_loaded = self.filters.len(), "Updated metric tag filterlist.");
                        }
                        Err(error) => {
                            error!(%error, "Rejected invalid metric tag filterlist update; retaining the previous rules.");
                        }
                    }
                },
            }
        }

        debug!("Metric Tag Filterlist transform stopped.");

        Ok(())
    }
}

enum TagFilterAction<'a> {
    Keep,
    Remove,
    Replace(&'a Tag),
}

#[inline]
fn filter_tag<'a>(tag: &Tag, filter: &'a CompiledFilter) -> TagFilterAction<'a> {
    let tag = tag.as_borrowed();
    if filter.is_exclude == filter.tag_names.contains(tag.name()) {
        return TagFilterAction::Remove;
    }

    let Some(allowlist) = filter.tag_value_allowlists.get(tag.name()) else {
        return TagFilterAction::Keep;
    };
    if tag
        .value()
        .is_some_and(|value| allowlist.allowed_values.contains(value))
    {
        return TagFilterAction::Keep;
    }

    match &allowlist.on_miss {
        CompiledMismatchAction::Remove => TagFilterAction::Remove,
        CompiledMismatchAction::Replace(replacement) if replacement.as_str() == tag.as_ref() => TagFilterAction::Keep,
        CompiledMismatchAction::Replace(replacement) => TagFilterAction::Replace(replacement),
    }
}

/// Filters the tags of a metric according to the compiled filter table.
///
/// Both instrumented tags and origin tags are filtered using the same key and value rules.
/// If the metric name isn't present in `filters`, the metric is left unchanged.
/// If filtering would not change any tags, the metric context is left untouched (zero allocations).
#[inline]
pub fn filter_metric_tags(
    metric: &mut Metric, state: &mut TagSetMutViewState, filters: &CompiledFilters,
) -> FilterMetricTagsOutcome {
    let Some(filter) = filters.get(metric.context().name().as_ref()) else {
        return FilterMetricTagsOutcome::RuleMiss;
    };

    let mut tag_replacements = Vec::new();
    let mut origin_tag_replacements = Vec::new();
    let mut tag_set_view = metric.context_mut().tags_mut_view(state);
    tag_set_view.retain_tags(|tag| match filter_tag(tag, filter) {
        TagFilterAction::Keep => true,
        TagFilterAction::Remove => false,
        TagFilterAction::Replace(replacement) => {
            tag_replacements.push(replacement.clone());
            false
        }
    });
    tag_set_view.retain_origin_tags(|tag| match filter_tag(tag, filter) {
        TagFilterAction::Keep => true,
        TagFilterAction::Remove => false,
        TagFilterAction::Replace(replacement) => {
            origin_tag_replacements.push(replacement.clone());
            false
        }
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

    if total_removed == 0 {
        FilterMetricTagsOutcome::NoChange
    } else {
        FilterMetricTagsOutcome::Modified {
            removed_tags: total_removed,
        }
    }
}

#[cfg(test)]
mod tests {
    use saluki_config::{dynamic::ConfigUpdate, ConfigurationLoader};
    use saluki_context::{
        tags::{Tag, TagSet},
        Context, TagSetMutViewState,
    };
    use saluki_core::data_model::event::metric::Metric;
    use saluki_metrics::{test::TestRecorder, MetricsBuilder};
    use serde_json::json;

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

    fn value_allowlist(tag_name: &str, values: &[&str]) -> StdHashMap<String, TagValueAllowlist> {
        [(
            tag_name.to_string(),
            TagValueAllowlist {
                values: values.iter().map(|value| (*value).to_string()).collect(),
                ..Default::default()
            },
        )]
        .into_iter()
        .collect()
    }

    fn replacing_value_allowlist(
        tag_name: &str, values: &[&str], replacement: &str,
    ) -> StdHashMap<String, TagValueAllowlist> {
        [(
            tag_name.to_string(),
            TagValueAllowlist {
                values: values.iter().map(|value| (*value).to_string()).collect(),
                on_miss: TagValueMismatchAction::Replace,
                replacement: replacement.to_string(),
            },
        )]
        .into_iter()
        .collect()
    }

    #[test]
    fn exclude_removes_listed_tags() {
        let entries = vec![MetricTagFilterEntry {
            metric_name: "my.dist".to_string(),
            action: FilterAction::Exclude,
            tags: vec!["env".to_string(), "host".to_string()],
            tag_value_allowlists: StdHashMap::default(),
        }];
        let filters = compile_filters(&entries).unwrap();

        let mut metric = distribution_metric("my.dist", &["env:prod", "service:web", "host:h1"]);
        let mut state = TagSetMutViewState::default();
        filter_metric_tags(&mut metric, &mut state, &filters);

        assert_eq!(tag_names(&metric), vec!["service:web"]);
    }

    #[test]
    fn include_keeps_only_listed_tags() {
        let entries = vec![MetricTagFilterEntry {
            metric_name: "my.dist".to_string(),
            action: FilterAction::Include,
            tags: vec!["env".to_string()],
            tag_value_allowlists: StdHashMap::default(),
        }];
        let filters = compile_filters(&entries).unwrap();

        let mut metric = distribution_metric("my.dist", &["env:prod", "service:web", "host:h1"]);
        let mut state = TagSetMutViewState::default();
        filter_metric_tags(&mut metric, &mut state, &filters);

        assert_eq!(tag_names(&metric), vec!["env:prod"]);
    }

    #[test]
    fn value_allowlist_keeps_allowed_values_and_removes_others() {
        let entries = vec![MetricTagFilterEntry {
            metric_name: "my.dist".to_string(),
            action: FilterAction::Exclude,
            tags: vec![],
            tag_value_allowlists: value_allowlist("customer_id", &["top-1", "top-2"]),
        }];
        let filters = compile_filters(&entries).unwrap();
        let mut state = TagSetMutViewState::default();

        let mut allowed = distribution_metric("my.dist", &["customer_id:top-1", "service:web"]);
        let allowed_outcome = filter_metric_tags(&mut allowed, &mut state, &filters);
        assert_eq!(allowed_outcome, FilterMetricTagsOutcome::NoChange);
        assert_eq!(tag_names(&allowed), vec!["customer_id:top-1", "service:web"]);

        let mut unlisted = distribution_metric("my.dist", &["customer_id:other", "service:web"]);
        let unlisted_outcome = filter_metric_tags(&mut unlisted, &mut state, &filters);
        assert_eq!(unlisted_outcome, FilterMetricTagsOutcome::Modified { removed_tags: 1 });
        assert_eq!(tag_names(&unlisted), vec!["service:web"]);
    }

    #[test]
    fn value_allowlist_replaces_unlisted_values() {
        let entries = vec![MetricTagFilterEntry {
            metric_name: "my.dist".to_string(),
            action: FilterAction::Exclude,
            tags: vec![],
            tag_value_allowlists: replacing_value_allowlist("customer_id", &["top-1"], "other"),
        }];
        let filters = compile_filters(&entries).unwrap();
        let mut metric = distribution_metric("my.dist", &["customer_id:unlisted", "service:web"]);
        let mut state = TagSetMutViewState::default();

        let outcome = filter_metric_tags(&mut metric, &mut state, &filters);

        assert_eq!(outcome, FilterMetricTagsOutcome::Modified { removed_tags: 1 });
        assert_eq!(tag_names(&metric), vec!["customer_id:other", "service:web"]);
    }

    #[test]
    fn value_allowlist_keeps_existing_replacement_value() {
        let entries = vec![MetricTagFilterEntry {
            metric_name: "my.dist".to_string(),
            action: FilterAction::Exclude,
            tags: vec![],
            tag_value_allowlists: replacing_value_allowlist("customer_id", &[], "other"),
        }];
        let filters = compile_filters(&entries).unwrap();
        let mut metric = distribution_metric("my.dist", &["customer_id:other"]);
        let mut state = TagSetMutViewState::default();

        let outcome = filter_metric_tags(&mut metric, &mut state, &filters);

        assert_eq!(outcome, FilterMetricTagsOutcome::NoChange);
        assert_eq!(tag_names(&metric), vec!["customer_id:other"]);
    }

    #[test]
    fn value_allowlist_replacement_deduplicates_multiple_unlisted_values() {
        let entries = vec![MetricTagFilterEntry {
            metric_name: "my.dist".to_string(),
            action: FilterAction::Exclude,
            tags: vec![],
            tag_value_allowlists: replacing_value_allowlist("customer_id", &[], "other"),
        }];
        let filters = compile_filters(&entries).unwrap();
        let mut metric = distribution_metric("my.dist", &["customer_id:a", "customer_id:b"]);
        let mut state = TagSetMutViewState::default();

        let outcome = filter_metric_tags(&mut metric, &mut state, &filters);

        assert_eq!(outcome, FilterMetricTagsOutcome::Modified { removed_tags: 2 });
        assert_eq!(tag_names(&metric), vec!["customer_id:other"]);
    }

    #[test]
    fn value_allowlist_removes_bare_and_empty_unlisted_values() {
        let entries = vec![MetricTagFilterEntry {
            metric_name: "my.dist".to_string(),
            action: FilterAction::Exclude,
            tags: vec![],
            tag_value_allowlists: value_allowlist("customer_id", &["top-1"]),
        }];
        let filters = compile_filters(&entries).unwrap();
        let mut metric = distribution_metric("my.dist", &["customer_id", "customer_id:", "service:web"]);
        let mut state = TagSetMutViewState::default();

        let outcome = filter_metric_tags(&mut metric, &mut state, &filters);

        assert_eq!(outcome, FilterMetricTagsOutcome::Modified { removed_tags: 2 });
        assert_eq!(tag_names(&metric), vec!["service:web"]);
    }

    #[test]
    fn include_filter_must_also_include_value_allowlisted_tag_key() {
        let entries = vec![MetricTagFilterEntry {
            metric_name: "my.dist".to_string(),
            action: FilterAction::Include,
            tags: vec!["service".to_string()],
            tag_value_allowlists: value_allowlist("customer_id", &["top-1"]),
        }];
        let filters = compile_filters(&entries).unwrap();
        let mut metric = distribution_metric("my.dist", &["customer_id:top-1", "service:web"]);
        let mut state = TagSetMutViewState::default();

        filter_metric_tags(&mut metric, &mut state, &filters);

        assert_eq!(tag_names(&metric), vec!["service:web"]);
    }

    #[test]
    fn non_matching_metric_unchanged() {
        let entries = vec![MetricTagFilterEntry {
            metric_name: "other.dist".to_string(),
            action: FilterAction::Exclude,
            tags: vec!["env".to_string()],
            tag_value_allowlists: StdHashMap::default(),
        }];
        let filters = compile_filters(&entries).unwrap();

        let mut metric = distribution_metric("my.dist", &["env:prod", "service:web"]);
        let mut state = TagSetMutViewState::default();
        filter_metric_tags(&mut metric, &mut state, &filters);

        assert_eq!(tag_names(&metric), vec!["env:prod", "service:web"]);
    }

    #[test]
    fn non_distribution_metric_unchanged() {
        let metric = counter_metric("my.counter", &["env:prod", "service:web"]);
        assert!(!metric.values().is_sketch(), "counter should not be a sketch");
        assert_eq!(tag_names(&metric), vec!["env:prod", "service:web"]);
    }

    #[test]
    fn count_exclude_removes_listed_tags() {
        let entries = vec![MetricTagFilterEntry {
            metric_name: "my.counter".to_string(),
            action: FilterAction::Exclude,
            tags: vec!["env".to_string(), "host".to_string()],
            tag_value_allowlists: StdHashMap::default(),
        }];
        let filters = compile_filters(&entries).unwrap();

        let mut metric = counter_metric("my.counter", &["env:prod", "service:web", "host:h1"]);
        let mut state = TagSetMutViewState::default();
        let outcome = filter_metric_tags(&mut metric, &mut state, &filters);

        assert_eq!(tag_names(&metric), vec!["service:web"]);
        assert_eq!(outcome, FilterMetricTagsOutcome::Modified { removed_tags: 2 });
    }

    #[test]
    fn count_include_keeps_only_listed_tags() {
        let entries = vec![MetricTagFilterEntry {
            metric_name: "my.counter".to_string(),
            action: FilterAction::Include,
            tags: vec!["env".to_string()],
            tag_value_allowlists: StdHashMap::default(),
        }];
        let filters = compile_filters(&entries).unwrap();

        let mut metric = counter_metric("my.counter", &["env:prod", "service:web", "host:h1"]);
        let mut state = TagSetMutViewState::default();
        let outcome = filter_metric_tags(&mut metric, &mut state, &filters);

        assert_eq!(tag_names(&metric), vec!["env:prod"]);
        assert_eq!(outcome, FilterMetricTagsOutcome::Modified { removed_tags: 2 });
    }

    #[test]
    fn count_non_matching_metric_unchanged() {
        let entries = vec![MetricTagFilterEntry {
            metric_name: "other.counter".to_string(),
            action: FilterAction::Exclude,
            tags: vec!["env".to_string()],
            tag_value_allowlists: StdHashMap::default(),
        }];
        let filters = compile_filters(&entries).unwrap();

        let mut metric = counter_metric("my.counter", &["env:prod", "service:web"]);
        let mut state = TagSetMutViewState::default();
        let outcome = filter_metric_tags(&mut metric, &mut state, &filters);

        assert_eq!(tag_names(&metric), vec!["env:prod", "service:web"]);
        assert_eq!(outcome, FilterMetricTagsOutcome::RuleMiss);
    }

    #[test]
    fn count_no_change_outcome_when_filter_matches_no_tags() {
        let entries = vec![MetricTagFilterEntry {
            metric_name: "my.counter".to_string(),
            action: FilterAction::Exclude,
            tags: vec!["region".to_string()],
            tag_value_allowlists: StdHashMap::default(),
        }];
        let filters = compile_filters(&entries).unwrap();

        let mut metric = counter_metric("my.counter", &["env:prod", "service:web"]);
        let mut state = TagSetMutViewState::default();
        let outcome = filter_metric_tags(&mut metric, &mut state, &filters);

        assert_eq!(tag_names(&metric), vec!["env:prod", "service:web"]);
        assert_eq!(outcome, FilterMetricTagsOutcome::NoChange);
    }

    #[test]
    fn bare_tag_excluded_by_name() {
        let entries = vec![MetricTagFilterEntry {
            metric_name: "my.dist".to_string(),
            action: FilterAction::Exclude,
            tags: vec!["production".to_string()],
            tag_value_allowlists: StdHashMap::default(),
        }];
        let filters = compile_filters(&entries).unwrap();

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
            tag_value_allowlists: StdHashMap::default(),
        }];
        let filters = compile_filters(&entries).unwrap();

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
            tag_value_allowlists: StdHashMap::default(),
        }];
        let filters = compile_filters(&entries).unwrap();

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
                tag_value_allowlists: StdHashMap::default(),
            },
            MetricTagFilterEntry {
                metric_name: "my.dist".to_string(),
                action: FilterAction::Exclude,
                tags: vec!["host".to_string()],
                tag_value_allowlists: StdHashMap::default(),
            },
        ];
        let filters = compile_filters(&entries).unwrap();

        let mut metric = distribution_metric("my.dist", &["env:prod", "host:h1", "service:web"]);
        let mut state = TagSetMutViewState::default();
        filter_metric_tags(&mut metric, &mut state, &filters);

        assert_eq!(tag_names(&metric), vec!["service:web"]);
    }

    #[test]
    fn merge_same_tag_value_allowlist_unions_values() {
        let entries = vec![
            MetricTagFilterEntry {
                metric_name: "my.dist".to_string(),
                action: FilterAction::Exclude,
                tags: vec![],
                tag_value_allowlists: value_allowlist("customer_id", &["top-1"]),
            },
            MetricTagFilterEntry {
                metric_name: "my.dist".to_string(),
                action: FilterAction::Exclude,
                tags: vec![],
                tag_value_allowlists: value_allowlist("customer_id", &["top-2"]),
            },
        ];
        let filters = compile_filters(&entries).unwrap();
        let mut state = TagSetMutViewState::default();

        for tag in ["customer_id:top-1", "customer_id:top-2"] {
            let mut metric = distribution_metric("my.dist", &[tag]);
            assert_eq!(
                filter_metric_tags(&mut metric, &mut state, &filters),
                FilterMetricTagsOutcome::NoChange
            );
        }
    }

    #[test]
    fn conflicting_value_replacements_are_rejected() {
        let entries = vec![
            MetricTagFilterEntry {
                metric_name: "my.dist".to_string(),
                action: FilterAction::Exclude,
                tags: vec![],
                tag_value_allowlists: replacing_value_allowlist("customer_id", &["top-1"], "other"),
            },
            MetricTagFilterEntry {
                metric_name: "my.dist".to_string(),
                action: FilterAction::Exclude,
                tags: vec![],
                tag_value_allowlists: replacing_value_allowlist("customer_id", &["top-2"], "aggregated"),
            },
        ];

        let error = compile_filters(&entries)
            .err()
            .expect("conflicting replacements must fail");

        assert!(error.to_string().contains("metric 'my.dist' and tag 'customer_id'"));
    }

    #[test]
    fn conflicting_value_mismatch_actions_are_rejected() {
        let entries = vec![
            MetricTagFilterEntry {
                metric_name: "my.dist".to_string(),
                action: FilterAction::Exclude,
                tags: vec![],
                tag_value_allowlists: value_allowlist("customer_id", &["top-1"]),
            },
            MetricTagFilterEntry {
                metric_name: "my.dist".to_string(),
                action: FilterAction::Exclude,
                tags: vec![],
                tag_value_allowlists: replacing_value_allowlist("customer_id", &["top-2"], "other"),
            },
        ];

        let error = compile_filters(&entries)
            .err()
            .expect("conflicting mismatch actions must fail");

        assert!(error.to_string().contains("metric 'my.dist' and tag 'customer_id'"));
    }

    #[test]
    fn merge_conflicting_actions_exclude_wins() {
        let entries = vec![
            MetricTagFilterEntry {
                metric_name: "my.dist".to_string(),
                action: FilterAction::Include,
                tags: vec!["env".to_string()],
                tag_value_allowlists: StdHashMap::default(),
            },
            MetricTagFilterEntry {
                metric_name: "my.dist".to_string(),
                action: FilterAction::Exclude,
                tags: vec!["host".to_string()],
                tag_value_allowlists: StdHashMap::default(),
            },
        ];
        let filters = compile_filters(&entries).unwrap();

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
                tag_value_allowlists: StdHashMap::default(),
            },
            MetricTagFilterEntry {
                metric_name: "my.dist".to_string(),
                action: FilterAction::Include,
                tags: vec!["env".to_string()],
                tag_value_allowlists: StdHashMap::default(),
            },
        ];
        let filters = compile_filters(&entries).unwrap();

        let mut metric = distribution_metric("my.dist", &["env:prod", "host:h1", "service:web"]);
        let mut state = TagSetMutViewState::default();
        filter_metric_tags(&mut metric, &mut state, &filters);

        assert_eq!(tag_names(&metric), vec!["env:prod", "service:web"]);
    }

    #[test]
    fn no_config_is_noop() {
        let filters = compile_filters(&[]).unwrap();
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
                tag_value_allowlists: StdHashMap::default(),
            },
            MetricTagFilterEntry {
                metric_name: "my.dist".to_string(),
                action: FilterAction::Exclude,
                tags: vec!["host".to_string()],
                tag_value_allowlists: StdHashMap::default(),
            },
        ];
        let filters = compile_filters(&entries).unwrap();

        assert!(!filters.contains_key(""));
        assert!(filters.contains_key("my.dist"));
    }

    #[test]
    fn missing_action_defaults_to_exclude() {
        let entry: MetricTagFilterEntry = serde_json::from_value(json!({
            "metric_name": "my.dist",
            "tags": ["env"]
        }))
        .unwrap();

        assert_eq!(entry.action, FilterAction::Exclude);
    }

    #[test]
    fn value_allowlist_defaults_to_remove_with_other_replacement() {
        let entry: MetricTagFilterEntry = serde_json::from_value(json!({
            "metric_name": "my.dist",
            "tags": [],
            "tag_value_allowlists": {
                "customer_id": { "values": ["top-1"] }
            }
        }))
        .unwrap();

        let allowlist = &entry.tag_value_allowlists["customer_id"];
        assert_eq!(allowlist.on_miss, TagValueMismatchAction::Remove);
        assert_eq!(allowlist.replacement, "other");
    }

    #[test]
    fn invalid_action_defaults_to_exclude() {
        let entry: MetricTagFilterEntry = serde_json::from_value(json!({
            "metric_name": "my.dist",
            "action": "invalid",
            "tags": ["env"]
        }))
        .unwrap();

        assert_eq!(entry.action, FilterAction::Exclude);
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
            tag_value_allowlists: StdHashMap::default(),
        }];
        let filters = compile_filters(&entries).unwrap();

        let mut metric =
            distribution_metric_with_origin_tags("my.dist", &["env:prod"], &["env:prod", "host:h1", "service:web"]);
        let mut state = TagSetMutViewState::default();
        filter_metric_tags(&mut metric, &mut state, &filters);

        assert_eq!(origin_tag_names(&metric), vec!["service:web"]);
    }

    #[test]
    fn value_allowlist_filters_origin_tags() {
        let entries = vec![MetricTagFilterEntry {
            metric_name: "my.dist".to_string(),
            action: FilterAction::Exclude,
            tags: vec![],
            tag_value_allowlists: value_allowlist("customer_id", &["top-1"]),
        }];
        let filters = compile_filters(&entries).unwrap();
        let mut metric = distribution_metric_with_origin_tags(
            "my.dist",
            &[],
            &["customer_id:top-1", "customer_id:other", "service:web"],
        );
        let mut state = TagSetMutViewState::default();

        filter_metric_tags(&mut metric, &mut state, &filters);

        assert_eq!(origin_tag_names(&metric), vec!["customer_id:top-1", "service:web"]);
    }

    #[test]
    fn value_allowlist_replaces_origin_tags() {
        let entries = vec![MetricTagFilterEntry {
            metric_name: "my.dist".to_string(),
            action: FilterAction::Exclude,
            tags: vec![],
            tag_value_allowlists: replacing_value_allowlist("customer_id", &["top-1"], "other"),
        }];
        let filters = compile_filters(&entries).unwrap();
        let mut metric = distribution_metric_with_origin_tags("my.dist", &[], &["customer_id:unlisted", "service:web"]);
        let mut state = TagSetMutViewState::default();

        filter_metric_tags(&mut metric, &mut state, &filters);

        assert_eq!(origin_tag_names(&metric), vec!["customer_id:other", "service:web"]);
    }

    #[test]
    fn include_keeps_only_listed_origin_tags() {
        let entries = vec![MetricTagFilterEntry {
            metric_name: "my.dist".to_string(),
            action: FilterAction::Include,
            tags: vec!["env".to_string()],
            tag_value_allowlists: StdHashMap::default(),
        }];
        let filters = compile_filters(&entries).unwrap();

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
            tag_value_allowlists: StdHashMap::default(),
        }];
        let filters = compile_filters(&entries).unwrap();

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
            tag_value_allowlists: StdHashMap::default(),
        }];
        let filters = compile_filters(&entries).unwrap();

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
            tag_value_allowlists: StdHashMap::default(),
        }];
        let filters = compile_filters(&entries).unwrap();

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
    async fn dynamic_update_partial_replaces_filter() {
        let (cfg, sender) = ConfigurationLoader::for_tests(Some(serde_json::json!({})), None, true).await;
        let sender = sender.expect("sender should exist");
        sender
            .send(ConfigUpdate::Snapshot(serde_json::json!({})))
            .await
            .unwrap();
        cfg.ready().await;

        let mut watcher = cfg.watch_for_updates("metric_tag_filterlist");

        sender
            .send(ConfigUpdate::Partial {
                key: "metric_tag_filterlist".to_string(),
                value: serde_json::json!([{
                    "metric_name": "my.dist",
                    "action": "exclude",
                    "tags": ["host"],
                    "tag_value_allowlists": {
                        "customer_id": {
                            "values": ["top-1"],
                            "on_miss": "replace",
                            "replacement": "aggregated"
                        }
                    }
                }]),
            })
            .await
            .unwrap();

        let (_, new_entries) = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            watcher.changed::<Vec<MetricTagFilterEntry>>(),
        )
        .await
        .expect("timed out waiting for metric_tag_filterlist update");

        let filters = compile_filters(new_entries.as_deref().unwrap_or(&[])).unwrap();

        let mut metric = distribution_metric("my.dist", &["customer_id:other", "env:prod", "host:h1", "service:web"]);
        let mut state = TagSetMutViewState::default();
        filter_metric_tags(&mut metric, &mut state, &filters);
        assert_eq!(
            tag_names(&metric),
            vec!["customer_id:aggregated", "env:prod", "service:web"]
        );
    }

    #[tokio::test]
    async fn dynamic_update_to_empty_clears_filter() {
        let (cfg, sender) = ConfigurationLoader::for_tests(Some(serde_json::json!({})), None, true).await;
        let sender = sender.expect("sender should exist");
        sender
            .send(ConfigUpdate::Snapshot(serde_json::json!({})))
            .await
            .unwrap();
        cfg.ready().await;

        let mut watcher = cfg.watch_for_updates("metric_tag_filterlist");

        sender
            .send(ConfigUpdate::Partial {
                key: "metric_tag_filterlist".to_string(),
                value: serde_json::json!([
                    { "metric_name": "my.dist", "action": "exclude", "tags": ["env"] }
                ]),
            })
            .await
            .unwrap();

        let _ = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            watcher.changed::<Vec<MetricTagFilterEntry>>(),
        )
        .await
        .expect("timed out waiting for initial metric_tag_filterlist update");

        sender
            .send(ConfigUpdate::Partial {
                key: "metric_tag_filterlist".to_string(),
                value: serde_json::json!([]),
            })
            .await
            .unwrap();

        let (_, new_entries) = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            watcher.changed::<Vec<MetricTagFilterEntry>>(),
        )
        .await
        .expect("timed out waiting for cleared metric_tag_filterlist update");

        let filters = compile_filters(new_entries.as_deref().unwrap_or(&[])).unwrap();

        let mut metric = distribution_metric("my.dist", &["env:prod", "service:web"]);
        let mut state = TagSetMutViewState::default();
        filter_metric_tags(&mut metric, &mut state, &filters);
        assert_eq!(tag_names(&metric), vec!["env:prod", "service:web"]);
    }

    #[tokio::test]
    async fn dynamic_update_snapshot_applies_filter() {
        let (cfg, sender) = ConfigurationLoader::for_tests(Some(serde_json::json!({})), None, true).await;
        let sender = sender.expect("sender should exist");
        sender
            .send(ConfigUpdate::Snapshot(serde_json::json!({})))
            .await
            .unwrap();
        cfg.ready().await;

        let mut watcher = cfg.watch_for_updates("metric_tag_filterlist");

        sender
            .send(ConfigUpdate::Snapshot(serde_json::json!({
                "metric_tag_filterlist": [
                    { "metric_name": "my.dist", "action": "include", "tags": ["service"] }
                ]
            })))
            .await
            .unwrap();

        let (_, new_entries) = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            watcher.changed::<Vec<MetricTagFilterEntry>>(),
        )
        .await
        .expect("timed out waiting for metric_tag_filterlist update");

        let filters = compile_filters(new_entries.as_deref().unwrap_or(&[])).unwrap();

        let mut metric = distribution_metric("my.dist", &["env:prod", "service:web", "host:h1"]);
        let mut state = TagSetMutViewState::default();
        filter_metric_tags(&mut metric, &mut state, &filters);
        assert_eq!(tag_names(&metric), vec!["service:web"]);
    }

    #[test]
    fn modified_filter_marks_tagsets_as_modified() {
        let entries = vec![MetricTagFilterEntry {
            metric_name: "my.dist".to_string(),
            action: FilterAction::Exclude,
            tags: vec!["host".to_string()],
            tag_value_allowlists: StdHashMap::default(),
        }];
        let filters = compile_filters(&entries).unwrap();

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
            tag_value_allowlists: StdHashMap::default(),
        }];
        let filters = compile_filters(&entries).unwrap();
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
}
