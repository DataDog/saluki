//! Metric Tag Filterlist synchronous transform.
//!
//! Removes or retains specific tags from distribution metrics based on per-metric configuration.
//! Supports both "exclude" (denylist) and "include" (allowlist) modes.
//!
//! Configuration is read from the `metric_tag_filterlist` key and can be updated at runtime via
//! Remote Config.

mod telemetry;

use async_trait::async_trait;
use foldhash::fast::RandomState as FoldHashState;
use hashbrown::{HashMap, HashSet};
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_config::GenericConfiguration;
use saluki_context::{tags::Tag, TagSetMutViewState};
use saluki_core::{
    components::{
        transforms::{Transform, TransformBuilder, TransformContext},
        ComponentContext,
    },
    data_model::event::{metric::Metric, EventType},
    observability::ComponentMetricsExt,
    topology::OutputDefinition,
};
use saluki_error::GenericError;
use saluki_metrics::MetricsBuilder;
use serde::{de::Deserializer, Deserialize};
use tokio::select;
use tracing::{debug, warn};

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

/// Compiled filter table: metric name → (is_exclude, set of tag key names).
pub type CompiledFilters = HashMap<String, (bool, HashSet<String, FoldHashState>), FoldHashState>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
/// Outcome of attempting to apply `metric_tag_filterlist` rules to a metric.
pub enum FilterMetricTagsOutcome {
    /// No rule existed for the metric name.
    RuleMiss,
    /// A rule existed, but applying it did not change any tags.
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

    #[serde(skip)]
    configuration: Option<GenericConfiguration>,
}

impl TagFilterlistConfiguration {
    /// Creates a new `TagFilterlistConfiguration` from the given configuration.
    pub fn from_configuration(config: &GenericConfiguration) -> Result<Self, GenericError> {
        let mut typed: Self = config.as_typed()?;
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

        Ok(Box::new(TagFilterlist {
            filters: compile_filters(&self.entries),
            configuration: self
                .configuration
                .clone()
                .expect("configuration must be set via from_configuration"),
            telemetry: Telemetry::new(&metrics_builder),
            view_state: TagSetMutViewState::new(),
        }))
    }
}

impl MemoryBounds for TagFilterlistConfiguration {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        builder.minimum().with_single_value::<TagFilterlist>("component struct");
    }
}

struct TagFilterlist {
    filters: CompiledFilters,
    configuration: GenericConfiguration,
    telemetry: Telemetry,
    view_state: TagSetMutViewState,
}

#[async_trait]
impl Transform for TagFilterlist {
    async fn run(mut self: Box<Self>, mut context: TransformContext) -> Result<(), GenericError> {
        let mut health = context.take_health_handle();
        health.mark_ready();

        let mut watcher = self.configuration.watch_for_updates("metric_tag_filterlist");

        debug!("Metric Tag Filterlist transform started.");

        loop {
            select! {
                _ = health.live() => continue,
                maybe_events = context.events().next() => match maybe_events {
                    Some(mut events) => {
                        for event in &mut events {
                            if let Some(metric) = event.try_as_metric_mut() {
                                if metric.values().is_sketch() {
                                    let outcome = filter_metric_tags(metric, &self.filters, &mut self.view_state);
                                    self.telemetry.record(outcome);
                                }
                            }
                        }
                        if let Err(e) = context.dispatcher().dispatch(events).await {
                            tracing::error!(error = %e, "Failed to dispatch events.");
                        }
                    }
                    None => break,
                },
                (_, new_entries) = watcher.changed::<Vec<MetricTagFilterEntry>>() => {
                    self.filters = compile_filters(new_entries.as_deref().unwrap_or(&[]));
                    debug!("Updated metric tag filterlist.");
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
/// If the metric name is not present in `filters`, the metric is left unchanged.
/// If filtering would not change any tags, the metric context is left untouched (zero allocations).
///
/// Uses a lazy copy-on-write view to scan tags in a single read-only pass and defer the
/// `Arc` clone + context key recomputation until tags are actually removed.
#[inline]
pub fn filter_metric_tags(
    metric: &mut Metric, filters: &CompiledFilters, view_state: &mut TagSetMutViewState,
) -> FilterMetricTagsOutcome {
    let Some((is_exclude, tag_names)) = filters.get(metric.context().name().as_ref()) else {
        return FilterMetricTagsOutcome::RuleMiss;
    };

    let is_exclude = *is_exclude;

    // Single read-only scan of both tag sets, collecting removal indices.
    // No Arc clone or rehash occurs until finish() — and only if something was flagged.
    let mut view = metric.context_mut().tags_mut_view(view_state);
    view.retain_tags(|tag| should_keep_tag(tag, is_exclude, tag_names));
    view.retain_origin_tags(|tag| should_keep_tag(tag, is_exclude, tag_names));

    match view.finish() {
        0 => FilterMetricTagsOutcome::NoChange,
        n => FilterMetricTagsOutcome::Modified { removed_tags: n },
    }
}

#[cfg(test)]
mod tests {
    use saluki_config::{dynamic::ConfigUpdate, ConfigurationLoader};
    use saluki_context::{
        tags::{Tag, TagSet},
        Context,
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

    #[test]
    fn exclude_removes_listed_tags() {
        let entries = vec![MetricTagFilterEntry {
            metric_name: "my.dist".to_string(),
            action: FilterAction::Exclude,
            tags: vec!["env".to_string(), "host".to_string()],
        }];
        let filters = compile_filters(&entries);

        let mut metric = distribution_metric("my.dist", &["env:prod", "service:web", "host:h1"]);
        filter_metric_tags(&mut metric, &filters, &mut TagSetMutViewState::new());

        assert_eq!(tag_names(&metric), vec!["service:web"]);
    }

    #[test]
    fn include_keeps_only_listed_tags() {
        let entries = vec![MetricTagFilterEntry {
            metric_name: "my.dist".to_string(),
            action: FilterAction::Include,
            tags: vec!["env".to_string()],
        }];
        let filters = compile_filters(&entries);

        let mut metric = distribution_metric("my.dist", &["env:prod", "service:web", "host:h1"]);
        filter_metric_tags(&mut metric, &filters, &mut TagSetMutViewState::new());

        assert_eq!(tag_names(&metric), vec!["env:prod"]);
    }

    #[test]
    fn non_matching_metric_unchanged() {
        let entries = vec![MetricTagFilterEntry {
            metric_name: "other.dist".to_string(),
            action: FilterAction::Exclude,
            tags: vec!["env".to_string()],
        }];
        let filters = compile_filters(&entries);

        let mut metric = distribution_metric("my.dist", &["env:prod", "service:web"]);
        filter_metric_tags(&mut metric, &filters, &mut TagSetMutViewState::new());

        assert_eq!(tag_names(&metric), vec!["env:prod", "service:web"]);
    }

    #[test]
    fn non_distribution_metric_unchanged() {
        let metric = counter_metric("my.counter", &["env:prod", "service:web"]);
        assert!(!metric.values().is_sketch(), "counter should not be a sketch");
        assert_eq!(tag_names(&metric), vec!["env:prod", "service:web"]);
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
        filter_metric_tags(&mut metric, &filters, &mut TagSetMutViewState::new());

        assert_eq!(tag_names(&metric), vec!["env:prod", "service:web"]);
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
        filter_metric_tags(&mut metric, &filters, &mut TagSetMutViewState::new());

        assert!(tag_names(&metric).is_empty());
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
        filter_metric_tags(&mut metric, &filters, &mut TagSetMutViewState::new());

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
        filter_metric_tags(&mut metric, &filters, &mut TagSetMutViewState::new());

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
        filter_metric_tags(&mut metric, &filters, &mut TagSetMutViewState::new());

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

        // "production" is a bare tag (no colon); tag.name() returns "production".
        let mut metric = distribution_metric("my.dist", &["production", "service:web"]);
        filter_metric_tags(&mut metric, &filters, &mut TagSetMutViewState::new());

        assert_eq!(tag_names(&metric), vec!["service:web"]);
    }

    #[test]
    fn no_config_is_noop() {
        let filters = compile_filters(&[]);
        let mut metric = distribution_metric("my.dist", &["env:prod", "service:web"]);
        filter_metric_tags(&mut metric, &filters, &mut TagSetMutViewState::new());
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
    fn missing_action_defaults_to_exclude() {
        let entry: MetricTagFilterEntry = serde_json::from_value(json!({
            "metric_name": "my.dist",
            "tags": ["env"]
        }))
        .unwrap();

        assert_eq!(entry.action, FilterAction::Exclude);
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
        }];
        let filters = compile_filters(&entries);

        let mut metric =
            distribution_metric_with_origin_tags("my.dist", &["env:prod"], &["env:prod", "host:h1", "service:web"]);
        filter_metric_tags(&mut metric, &filters, &mut TagSetMutViewState::new());

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
        filter_metric_tags(&mut metric, &filters, &mut TagSetMutViewState::new());

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
        filter_metric_tags(&mut metric, &filters, &mut TagSetMutViewState::new());

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

        filter_metric_tags(&mut metric1, &filters, &mut TagSetMutViewState::new());

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
        filter_metric_tags(&mut metric, &filters, &mut TagSetMutViewState::new());

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
        filter_metric_tags(&mut metric, &filters, &mut TagSetMutViewState::new());
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
        filter_metric_tags(&mut metric, &filters, &mut TagSetMutViewState::new());
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
        filter_metric_tags(&mut metric, &filters, &mut TagSetMutViewState::new());
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

        let outcome = filter_metric_tags(&mut metric, &filters, &mut TagSetMutViewState::new());

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

        let outcome = filter_metric_tags(&mut metric, &filters, &mut TagSetMutViewState::new());

        assert_eq!(outcome, FilterMetricTagsOutcome::NoChange);
        assert_eq!(tag_names(&metric), vec!["env:prod", "host:h1"]);
        assert!(!metric.context().tags().is_modified());
        assert!(!metric.context().origin_tags().is_modified());
    }
}
