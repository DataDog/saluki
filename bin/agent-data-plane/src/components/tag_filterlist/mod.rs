//! Metric Tag Filterlist synchronous transform.
//!
//! Removes or retains specific tags from distribution and count metrics based on per-metric
//! configuration. Supports both "exclude" (denylist) and "include" (allowlist) modes.
//!
//! Configuration is read from the `metric_tag_filterlist` key and can be updated at runtime via
//! Remote Config.

mod telemetry;

use std::{num::NonZeroUsize, time::Duration};

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
use saluki_error::GenericError;
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
}

/// Compiled filter table: metric name → (`is_exclude`, set of tag key names).
pub type CompiledFilters = HashMap<String, (bool, HashSet<String, FoldHashState>), FoldHashState>;

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
pub fn compile_filters(entries: &[MetricTagFilterEntry]) -> CompiledFilters {
    let mut filters: CompiledFilters = HashMap::with_hasher(FoldHashState::default());

    for entry in entries {
        if entry.metric_name.is_empty() {
            continue;
        }

        let is_exclude = entry.action == FilterAction::Exclude;
        let mut tag_set = HashSet::with_capacity_and_hasher(entry.tags.len(), FoldHashState::default());
        tag_set.extend(entry.tags.iter().cloned());

        match filters.entry(entry.metric_name.clone()) {
            hashbrown::hash_map::Entry::Vacant(e) => {
                e.insert((is_exclude, tag_set));
            }
            hashbrown::hash_map::Entry::Occupied(mut e) => {
                let (existing_is_exclude, existing_tags) = e.get_mut();
                if *existing_is_exclude == is_exclude {
                    existing_tags.extend(tag_set);
                } else if is_exclude {
                    *existing_is_exclude = true;
                    *existing_tags = tag_set;
                }
            }
        }
    }

    filters
}

/// Metric Tag Filterlist transform.
///
/// Removes or retains specific tags from distribution metrics based on per-metric configuration.
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
        let filters = compile_filters(&self.entries);
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
                    self.filters = compile_filters(new_entries.as_deref().unwrap_or(&[]));
                    self.context_cache = build_context_cache(self.context_cache_capacity);
                    self.telemetry.set_size(self.filters.len());
                    self.telemetry.increment_updates();
                    debug!(rules_loaded = self.filters.len(), "Updated metric tag filterlist.");
                },
            }
        }

        debug!("Metric Tag Filterlist transform stopped.");

        Ok(())
    }
}

#[inline]
fn should_keep_tag(tag: &Tag, is_exclude: bool, names: &HashSet<String, FoldHashState>) -> bool {
    is_exclude != names.contains(tag.as_borrowed().name())
}

/// Filter the tags of a distribution metric according to the compiled filter table.
///
/// Both instrumented tags and origin tags are filtered using the same tag key list.
/// If the metric name isn't present in `filters`, the metric is left unchanged.
/// If filtering would not change any tags, the metric context is left untouched (zero allocations).
#[inline]
pub fn filter_metric_tags(
    metric: &mut Metric, state: &mut TagSetMutViewState, filters: &CompiledFilters,
) -> FilterMetricTagsOutcome {
    let Some((is_exclude, tag_names)) = filters.get(metric.context().name().as_ref()) else {
        return FilterMetricTagsOutcome::RuleMiss;
    };

    let mut tag_set_view = metric.context_mut().tags_mut_view(state);
    tag_set_view.retain_tags(|tag| should_keep_tag(tag, *is_exclude, tag_names));
    tag_set_view.retain_origin_tags(|tag| should_keep_tag(tag, *is_exclude, tag_names));
    let total_removed = tag_set_view.finish();

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

    use saluki_config::{dynamic::ConfigUpdate, ConfigurationLoader};
    use saluki_context::{
        tags::{Tag, TagSet},
        Context, TagSetMutViewState,
    };
    use saluki_core::accounting::{ComponentRegistry, MemoryLimiter};
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
    use serde_json::json;
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

        assert!(!filters.contains_key(""));
        assert!(filters.contains_key("my.dist"));
    }

    #[test]
    fn filter_action_deserialize_maps_each_documented_branch() {
        // `FilterAction`'s custom `Deserialize` recognizes exactly `"include"`; the empty string, an explicit
        // null, and any unrecognized value all resolve to the documented `Exclude` default, and an omitted key
        // falls back to `Exclude` via `#[serde(default)]`. One case per branch.
        enum Action {
            Missing,
            Null,
            Value(&'static str),
        }

        struct Case {
            name: &'static str,
            action: Action,
            expected: FilterAction,
        }

        let cases = [
            Case {
                name: "explicit include",
                action: Action::Value("include"),
                expected: FilterAction::Include,
            },
            Case {
                name: "explicit exclude",
                action: Action::Value("exclude"),
                expected: FilterAction::Exclude,
            },
            Case {
                name: "empty string",
                action: Action::Value(""),
                expected: FilterAction::Exclude,
            },
            Case {
                name: "unrecognized value",
                action: Action::Value("invalid"),
                expected: FilterAction::Exclude,
            },
            Case {
                name: "explicit null",
                action: Action::Null,
                expected: FilterAction::Exclude,
            },
            Case {
                name: "omitted key",
                action: Action::Missing,
                expected: FilterAction::Exclude,
            },
        ];

        for case in cases {
            let mut value = json!({ "metric_name": "my.dist", "tags": ["env"] });
            match case.action {
                Action::Missing => {}
                Action::Null => value["action"] = serde_json::Value::Null,
                Action::Value(action) => value["action"] = json!(action),
            }

            let entry: MetricTagFilterEntry =
                serde_json::from_value(value).unwrap_or_else(|e| panic!("{}: deserialize failed: {e}", case.name));

            assert_eq!(entry.action, case.expected, "{}", case.name);
        }
    }

    #[tokio::test]
    async fn context_cache_capacity_defaults_to_100k_and_can_be_overridden() {
        // With no `data_plane.dogstatsd.aggregator_tag_filter_cache_capacity` key present, the capacity falls
        // back to the documented default of 100,000.
        let (default_config, _) = ConfigurationLoader::for_tests(Some(json!({})), None, false).await;
        let default_builder =
            TagFilterlistConfiguration::from_configuration(&default_config).expect("config should parse");
        assert_eq!(default_builder.context_cache_capacity, 100_000);

        // Setting the key overrides the default with the configured value.
        let (override_config, _) = ConfigurationLoader::for_tests(
            Some(json!({
                "data_plane": { "dogstatsd": { "aggregator_tag_filter_cache_capacity": 512 } }
            })),
            None,
            false,
        )
        .await;
        let override_builder =
            TagFilterlistConfiguration::from_configuration(&override_config).expect("config should parse");
        assert_eq!(override_builder.context_cache_capacity, 512);
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
                value: serde_json::json!([
                    { "metric_name": "my.dist", "action": "exclude", "tags": ["host"] }
                ]),
            })
            .await
            .unwrap();

        let (_, new_entries) = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            watcher.changed::<Vec<MetricTagFilterEntry>>(),
        )
        .await
        .expect("timed out waiting for metric_tag_filterlist update");

        let filters = compile_filters(new_entries.as_deref().unwrap_or(&[]));

        let mut metric = distribution_metric("my.dist", &["env:prod", "host:h1", "service:web"]);
        let mut state = TagSetMutViewState::default();
        filter_metric_tags(&mut metric, &mut state, &filters);
        assert_eq!(tag_names(&metric), vec!["env:prod", "service:web"]);
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

        let filters = compile_filters(new_entries.as_deref().unwrap_or(&[]));

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

        let filters = compile_filters(new_entries.as_deref().unwrap_or(&[]));

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

        let cfg_json = json!({
            "metric_tag_filterlist": [
                { "metric_name": "svc.latency", "action": "exclude", "tags": ["host"] }
            ]
        });
        let (config, _sender) = ConfigurationLoader::for_tests(Some(cfg_json), None, false).await;
        let builder = TagFilterlistConfiguration::from_configuration(&config).expect("config should parse");

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
        let tags = &["host:h1", "env:prod"];
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
        let supervisor_handle = Supervisor::new("test").expect("valid supervisor name").handle();

        let context = TransformContext::new(
            &topology_context,
            &component_context,
            ComponentRegistry::default(),
            health,
            dispatcher,
            consumer,
            supervisor_handle,
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
        assert_eq!(sorted_tags(&dispatched[0]), vec!["env:prod"]);
        // Counter (count metric) -> filtered via the cache-hit branch (shares the distribution's context entry).
        assert!(matches!(dispatched[1].values(), MetricValues::Counter(_)));
        assert_eq!(sorted_tags(&dispatched[1]), vec!["env:prod"]);
        // Gauge -> NOT a sketch and NOT a counter, so the type guard skips it and it passes through untouched.
        assert!(!dispatched[2].values().is_sketch());
        assert!(!matches!(dispatched[2].values(), MetricValues::Counter(_)));
        assert_eq!(
            sorted_tags(&dispatched[2]),
            vec!["env:prod", "host:h1"],
            "gauge metrics must not be filtered by the type guard"
        );
        // Repeated distribution -> filtered via the cache-hit branch, identical to the first distribution.
        assert!(dispatched[3].values().is_sketch());
        assert_eq!(sorted_tags(&dispatched[3]), vec!["env:prod"]);
    }
}
