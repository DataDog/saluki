//! Metric Tag Filterlist synchronous transform.
//!
//! Removes or retains specific tags from distribution and count metrics based on per-metric
//! configuration. Supports whole-tag filtering and value allow-listing.
//!
//! Whole-tag configuration is read from `metric_tag_filterlist` and can be updated at runtime via
//! Remote Config. Value allow-list configuration is read independently from the static
//! `metric_tag_value_allowlist` key.

mod telemetry;

use std::{collections::hash_map::Entry, num::NonZeroUsize, time::Duration};

use agent_data_plane_config::{
    domains::dogstatsd::{
        validate_metric_tag_value_allowlists, Domain as DogStatsDDomain, FilterAction, MetricTagFilterEntry,
        MetricTagValueAllowlistEntry, TagValueMismatchAction,
    },
    Live,
};
use async_trait::async_trait;
use saluki_common::{
    cache::{Cache, CacheBuilder},
    collections::{FastHashMap, FastHashSet},
};
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
use tokio::select;
use tracing::{debug, error};

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
    allowlist: CompiledTagValueAllowlist,
}

#[derive(Clone, Default)]
struct CompiledValuePrefixFilters {
    metric_prefixes: Vec<String>,
    filters_by_tag_name: FastHashMap<String, Vec<CompiledValuePrefixFilter>>,
}

impl CompiledValuePrefixFilters {
    fn sort_and_compact(&mut self) {
        self.metric_prefixes.sort_unstable();

        let mut compacted_prefixes = Vec::with_capacity(self.metric_prefixes.len());
        for prefix in self.metric_prefixes.drain(..) {
            if compacted_prefixes
                .last()
                .is_some_and(|existing: &String| prefix.starts_with(existing))
            {
                continue;
            }
            compacted_prefixes.push(prefix);
        }
        self.metric_prefixes = compacted_prefixes;

        for filters in self.filters_by_tag_name.values_mut() {
            filters.sort_unstable_by(|left, right| left.metric_prefix.cmp(&right.metric_prefix));
        }
    }

    fn has_rule_for_metric(&self, metric_name: &str) -> bool {
        find_matching_prefix(&self.metric_prefixes, metric_name, String::as_str).is_some()
    }

    fn get(&self, metric_name: &str, tag_name: &str) -> Option<&CompiledTagValueAllowlist> {
        let filters = self.filters_by_tag_name.get(tag_name)?;
        find_matching_prefix(filters, metric_name, |filter| filter.metric_prefix.as_str())
            .map(|filter| &filter.allowlist)
    }

    fn rule_count(&self) -> usize {
        self.filters_by_tag_name.values().map(Vec::len).sum()
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
/// Merge rules:
/// - Same metric name + same action → union of tag key sets.
/// - Same metric name + conflicting actions → `exclude` wins.
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

/// Adds value allow-list entries to a compiled filter table.
///
/// Metric prefixes for the same tag must not overlap, ensuring that a metric/tag pair matches at most one rule.
/// Rules for different tags may use the same or overlapping prefixes.
///
/// # Errors
///
/// Returns an error when an entry has invalid fields or when two entries for the same tag have overlapping metric
/// prefixes.
pub fn add_value_allowlists(
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

        filters
            .value_prefix_filters
            .metric_prefixes
            .push(entry.metric_prefix.clone());
        filters
            .value_prefix_filters
            .filters_by_tag_name
            .entry(entry.tag_name.clone())
            .or_default()
            .push(CompiledValuePrefixFilter {
                metric_prefix: entry.metric_prefix.clone(),
                allowlist,
            });
    }

    filters.value_prefix_filters.sort_and_compact();
    Ok(())
}

fn compile_all_filters(
    entries: &[MetricTagFilterEntry], value_allowlists: &[MetricTagValueAllowlistEntry],
) -> Result<CompiledFilters, GenericError> {
    let mut filters = compile_filters(entries);
    add_value_allowlists(&mut filters, value_allowlists)?;
    Ok(filters)
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
    /// replace locally configured value rules. This key is static in the initial implementation; changing it requires
    /// restarting ADP. The configuration boundary rejects invalid rules, and construction compiles them once so
    /// dynamic whole-tag updates cannot fail on unchanged static configuration.
    value_prefix_filters: CompiledValuePrefixFilters,

    /// Maximum number of entries in the per-context deduplication cache used by the tag filter.
    ///
    /// Configured via `data_plane.dogstatsd.aggregator_tag_filter_cache_capacity` in the agent config
    /// stream. High-throughput deployments with many unique metric contexts may benefit from
    /// increasing this value to reduce cache churn.
    ///
    /// Defaults to 100,000.
    context_cache_capacity: usize,
}

impl TagFilterlistConfiguration {
    /// Creates a new `TagFilterlistConfiguration` from typed DogStatsD configuration.
    ///
    /// # Errors
    ///
    /// Returns an error when the initial value allow-list rules are invalid.
    pub fn from_configuration(config: Live<DogStatsDDomain>) -> Result<Self, GenericError> {
        let entries = config.project(|config| &config.tag_filterlist);
        let value_prefix_filters = compile_all_filters(&[], &config.tag_value_allowlist)?.value_prefix_filters;
        Ok(Self {
            entries,
            value_prefix_filters,
            context_cache_capacity: config.aggregation.aggregator_tag_filter_cache_capacity,
        })
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
fn should_keep_tag(tag: &Tag, is_exclude: bool, names: &FastHashSet<String>) -> bool {
    is_exclude != names.contains(tag.as_borrowed().name())
}

#[inline]
fn filter_tag(
    tag: &Tag, key_filter: Option<&CompiledKeyFilter>, metric_name: &str, filters: &CompiledFilters,
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
    let Some(allowlist) = filters.value_prefix_filters.get(metric_name, tag.name()) else {
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

/// Filters the tags of a metric according to the compiled filter table.
///
/// Both instrumented tags and origin tags are filtered using the same key and value rules. Whole-tag filtering runs
/// first, so an `include` rule must include a value-filtered tag key for its allow-list to apply. Configuration
/// validation ensures that a metric and tag match at most one value rule. Value rules leave bare tags unchanged;
/// empty-string values are processed and can be retained by including `""` in the configured values.
///
/// The caller applies this function only to counters and sketch-backed metrics. If the metric name doesn't have an
/// exact whole-tag rule or a matching value-rule prefix, the metric is unchanged. If filtering would not change any
/// tags, the metric context is left untouched (zero allocations).
#[inline]
pub fn filter_metric_tags(
    metric: &mut Metric, state: &mut TagSetMutViewState, filters: &CompiledFilters,
) -> FilterMetricTagsOutcome {
    let metric_name = metric.context().name().clone();
    let key_filter = filters.key_filters.get(metric_name.as_ref());
    let has_value_rule = filters.value_prefix_filters.has_rule_for_metric(metric_name.as_ref());
    if key_filter.is_none() && !has_value_rule {
        return FilterMetricTagsOutcome::RuleMiss;
    }

    let mut tag_replacements = Vec::new();
    let mut origin_tag_replacements = Vec::new();
    let mut tag_set_view = metric.context_mut().tags_mut_view(state);
    tag_set_view.retain_tags(|tag| filter_tag(tag, key_filter, metric_name.as_ref(), filters, &mut tag_replacements));
    tag_set_view.retain_origin_tags(|tag| {
        filter_tag(
            tag,
            key_filter,
            metric_name.as_ref(),
            filters,
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
    use std::sync::Arc;

    use saluki_context::{
        tags::{Tag, TagSet},
        Context, TagSetMutViewState,
    };
    use saluki_core::accounting::{ComponentRegistry, MemoryLimiter};
    use saluki_core::components::ComponentSpawner;
    use saluki_core::components::{
        transforms::{TransformBuilder, TransformContext},
        ComponentContext,
    };
    use saluki_core::data_model::event::{
        metric::{Metric, MetricValues},
        Event,
    };
    use saluki_core::health::HealthRegistry;
    use saluki_core::runtime::{state::DataspaceRegistry, Supervisor};
    use saluki_core::topology::interconnect::{Consumer, Dispatcher};
    use saluki_core::topology::{EventsBuffer, OutputName, TopologyContext};
    use saluki_metrics::{test::TestRecorder, MetricsBuilder};
    use tokio::runtime::Handle;
    use tokio::sync::mpsc;

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
        )
        .expect("valid value rules should compile");
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
        )
        .expect("valid value rules should compile");
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
        )
        .expect("valid value rules should compile");
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
        )
        .expect("valid value rules should compile");
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
        )
        .expect("valid value rules should compile");
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
    fn overlapping_value_allowlist_prefixes_for_the_same_tag_are_rejected() {
        let broad = value_allowlist(
            "my.",
            "customer_id",
            &["top-1"],
            TagValueMismatchAction::Replace,
            "other",
        );
        let narrow = value_allowlist(
            "my.metric",
            "customer_id",
            &["top-2"],
            TagValueMismatchAction::Replace,
            "other",
        );

        let error = compile_all_filters(&[], &[broad, narrow])
            .err()
            .expect("overlapping prefixes for one tag must fail");
        assert!(error
            .to_string()
            .contains("metric prefixes 'my.' and 'my.metric' are configured for tag 'customer_id'"));

        compile_all_filters(
            &[],
            &[
                value_allowlist("my.", "customer_id", &[], TagValueMismatchAction::Remove, "other"),
                value_allowlist("my.", "region", &[], TagValueMismatchAction::Remove, "other"),
            ],
        )
        .expect("different tags may use the same prefix");
    }

    #[test]
    fn overlapping_value_allowlist_prefixes_for_different_tags_both_apply() {
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
                value_allowlist(
                    "my.metric",
                    "region",
                    &["us-east-1"],
                    TagValueMismatchAction::Remove,
                    "other",
                ),
            ],
        )
        .expect("overlapping prefixes for different tags should compile");
        let mut metric = distribution_metric("my.metric.requests", &["customer_id:long-tail", "region:us-west-2"]);
        let mut state = TagSetMutViewState::default();

        assert_eq!(
            filter_metric_tags(&mut metric, &mut state, &filters),
            FilterMetricTagsOutcome::Modified { removed_tags: 2 }
        );
        assert!(tag_names(&metric).is_empty());
    }

    #[test]
    fn empty_value_allowlist_prefixes_and_tag_names_are_rejected() {
        let empty_prefix = value_allowlist("", "customer_id", &[], TagValueMismatchAction::Remove, "other");
        let error = compile_all_filters(&[], &[empty_prefix])
            .err()
            .expect("an empty metric prefix must fail");
        assert!(error.to_string().contains("empty `metric_prefix`"));

        let empty_tag_name = value_allowlist("my.", "", &[], TagValueMismatchAction::Remove, "other");
        let error = compile_all_filters(&[], &[empty_tag_name])
            .err()
            .expect("an empty tag name must fail");
        assert!(error.to_string().contains("empty `tag_name`"));
    }

    #[test]
    fn value_allowlist_rejects_tag_names_containing_a_colon() {
        let entry = value_allowlist("my.", "customer:id", &[], TagValueMismatchAction::Remove, "other");
        let error = compile_all_filters(&[], &[entry])
            .err()
            .expect("a tag name containing a colon must fail");
        assert!(error.to_string().contains("tag name 'customer:id' contains ':'"));
    }

    #[test]
    fn value_allowlist_preserves_whitespace_in_configured_strings() {
        let filters = compile_all_filters(
            &[],
            &[value_allowlist(
                "my. ",
                "customer_id ",
                &[" top-1 "],
                TagValueMismatchAction::Replace,
                " other ",
            )],
        )
        .expect("valid value rules should compile");
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
        assert_eq!(tag_names(&replaced), vec!["customer_id : other "]);
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

        let filters = compile_all_filters(&key_entries, &value_entries).expect("rules should compile");
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

    #[test]
    fn context_cache_capacity_comes_from_typed_dogstatsd_configuration() {
        let mut config = DogStatsDDomain::default();
        config.aggregation.aggregator_tag_filter_cache_capacity = 512;
        let builder = TagFilterlistConfiguration::from_configuration(Live::new_fixed(config))
            .expect("typed configuration should be valid");
        assert_eq!(builder.context_cache_capacity, 512);
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
        let cell = Arc::new(arc_swap::ArcSwap::from_pointee(
            agent_data_plane_config::SalukiConfiguration::default(),
        ));
        let (tick_tx, tick_rx) = tokio::sync::watch::channel(());
        let mut entries = Live::new_dynamic(Arc::clone(&cell), tick_rx, |config| {
            &config.domains.dogstatsd.tag_filterlist
        });
        let value_allowlists = [value_allowlist(
            "my.",
            "customer_id",
            &["top-1"],
            TagValueMismatchAction::Remove,
            "other",
        )];
        let value_prefix_filters = compile_all_filters(&[], &value_allowlists)
            .expect("static value rules should compile")
            .value_prefix_filters;

        let mut updated = (*cell.load_full()).clone();
        updated.domains.dogstatsd.tag_filterlist = vec![MetricTagFilterEntry {
            metric_name: "my.metric".to_string(),
            action: FilterAction::Exclude,
            tags: vec!["host".to_string()],
        }];
        cell.store(Arc::new(updated));
        tick_tx.send_replace(());

        let new_entries = tokio::time::timeout(Duration::from_secs(2), entries.changed())
            .await
            .expect("timed out waiting for typed tag filter update");
        let filters = compile_filters_with_values(&new_entries, value_prefix_filters.clone());
        let mut metric = distribution_metric("my.metric", &["customer_id:long-tail", "host:h1", "service:web"]);
        let mut state = TagSetMutViewState::default();
        filter_metric_tags(&mut metric, &mut state, &filters);
        assert_eq!(tag_names(&metric), vec!["service:web"]);

        let mut updated = (*cell.load_full()).clone();
        updated.domains.dogstatsd.tag_filterlist.clear();
        cell.store(Arc::new(updated));
        tick_tx.send_replace(());

        let new_entries = tokio::time::timeout(Duration::from_secs(2), entries.changed())
            .await
            .expect("timed out waiting for cleared typed tag filter update");
        let filters = compile_filters_with_values(&new_entries, value_prefix_filters);
        let mut metric = distribution_metric("my.metric", &["customer_id:long-tail", "host:h1", "service:web"]);
        filter_metric_tags(&mut metric, &mut state, &filters);
        assert_eq!(tag_names(&metric), vec!["host:h1", "service:web"]);
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

        let mut config = DogStatsDDomain::default();
        config.aggregation.aggregator_tag_filter_cache_capacity = 100_000;
        config.tag_filterlist = vec![MetricTagFilterEntry {
            metric_name: "svc.latency".to_string(),
            action: FilterAction::Exclude,
            tags: vec!["host".to_string()],
        }];
        config.tag_value_allowlist = vec![value_allowlist(
            "svc.",
            "customer_id",
            &[],
            TagValueMismatchAction::Replace,
            "other",
        )];
        let builder = TagFilterlistConfiguration::from_configuration(Live::new_fixed(config))
            .expect("typed configuration should be valid");

        let component_context = ComponentContext::test_transform("tag_filterlist");
        let transform = builder
            .build(component_context.clone())
            .await
            .expect("tag filterlist should build");

        // Wire a dispatcher whose default output we can drain after the run loop completes.
        let mut dispatcher = Dispatcher::new(component_context.clone());
        dispatcher.add_output(OutputName::Default).expect("add default output");
        let (out_tx, mut out_rx) = mpsc::channel(4);
        dispatcher
            .attach_sender_to_output(&OutputName::Default, out_tx)
            .expect("attach default sender");

        // A distribution, a counter, and a gauge that all share the same (name, tags) context, followed by a repeat
        // of the distribution. The counter and the repeated distribution hit the cache entry created by the first
        // distribution.
        let tags = &["host:h1", "env:prod", "customer_id:long-tail"];
        let mut input = EventsBuffer::default();
        for event in [
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
        ] {
            assert!(input.try_push(event).is_none(), "input buffer should have capacity");
        }

        let (in_tx, in_rx) = mpsc::channel(4);
        let consumer = Consumer::new(component_context.clone(), in_rx);
        in_tx.send(input).await.expect("send input buffer");
        drop(in_tx); // Closing the input makes the run loop terminate deterministically.

        let topology_context = TopologyContext::new(
            Arc::from("test"),
            MemoryLimiter::noop(),
            HealthRegistry::new(),
            Handle::current(),
            DataspaceRegistry::new(),
        );
        let health = HealthRegistry::new()
            .register_component(&saluki_core::support::SubsystemIdentifier::from_dotted("test"))
            .expect("component was not previously registered");
        // This component doesn't spawn supervised children yet, so a spawner over a never-run supervisor is
        // sufficient. Anything that does spawn needs `TestComponentSupervisor` (saluki_core::components::test_util)
        // instead, otherwise the spawn fails with `SupervisorGone`.
        let supervisor_handle = Supervisor::new("test").expect("valid supervisor name").handle();
        let spawner = ComponentSpawner::new(supervisor_handle, Handle::current());

        let context = TransformContext::new(
            &topology_context,
            &component_context,
            ComponentRegistry::default(),
            health,
            dispatcher,
            consumer,
            spawner,
        );

        transform.run(context).await.expect("tag filterlist run should succeed");

        let mut dispatched: Vec<Metric> = Vec::new();
        while let Ok(buffer) = out_rx.try_recv() {
            for event in buffer {
                if let Event::Metric(metric) = event {
                    dispatched.push(metric);
                }
            }
        }

        // Nothing is dropped by the transform; order is preserved.
        assert_eq!(dispatched.len(), 4, "all four metrics should be dispatched");

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
    }
}
