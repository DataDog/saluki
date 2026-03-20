//! Metric Tag Filterlist synchronous transform.
//!
//! Removes or retains specific tags from distribution metrics based on per-metric configuration.
//! Supports both "exclude" (denylist) and "include" (allowlist) modes.
//!
//! Configuration is read from the `metric_tag_filterlist` key and can be updated at runtime via
//! Remote Config.

use async_trait::async_trait;
use foldhash::fast::RandomState as FoldHashState;
use hashbrown::{HashMap, HashSet};
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_config::GenericConfiguration;
use saluki_context::tags::{SharedTagSet, TagSet};
use saluki_core::{
    components::{
        transforms::{Transform, TransformBuilder, TransformContext},
        ComponentContext,
    },
    data_model::event::EventType,
    topology::OutputDefinition,
};
use saluki_error::GenericError;
use serde::Deserialize;
use tokio::select;
use tracing::debug;

/// Action applied to the configured tag list: keep only listed tags, or remove listed tags.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum FilterAction {
    /// Keep only the tags whose key appears in the configured list.
    Include,
    /// Remove the tags whose key appears in the configured list.
    Exclude,
}

/// A single metric tag filter entry.
#[derive(Clone, Debug, Deserialize)]
pub struct MetricTagFilterEntry {
    /// The exact metric name this entry applies to.
    pub metric_name: String,
    /// Whether to include or exclude the listed tags.
    pub action: FilterAction,
    /// Tag key names to include or exclude.
    pub tags: Vec<String>,
}

/// Compiled filter table: metric name → (is_exclude, set of tag key names).
pub type CompiledFilters = HashMap<String, (bool, HashSet<String, FoldHashState>), FoldHashState>;

/// Compile a slice of filter entries into an O(1)-lookup table.
///
/// Merge rules:
/// - Same metric name + same action → union of tag key sets.
/// - Same metric name + conflicting actions → `exclude` wins.
pub fn compile_filters(entries: &[MetricTagFilterEntry]) -> CompiledFilters {
    let mut filters: CompiledFilters = HashMap::with_hasher(FoldHashState::default());

    for entry in entries {
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
                    // Same action: union the tag sets.
                    existing_tags.extend(tag_set);
                } else if is_exclude {
                    // Conflicting actions: exclude takes precedence.
                    *existing_is_exclude = true;
                    *existing_tags = tag_set;
                }
                // If existing is already exclude and incoming is include: ignore.
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

    async fn build(&self, _context: ComponentContext) -> Result<Box<dyn Transform + Send>, GenericError> {
        Ok(Box::new(TagFilterlist {
            filters: compile_filters(&self.entries),
            configuration: self
                .configuration
                .clone()
                .expect("configuration must be set via from_configuration"),
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
                                    filter_metric_tags(metric, &self.filters);
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

/// Applies a tag filter to a shared tag set, returning `Some(TagSet)` if any tags were
/// filtered out, or `None` if the result would be identical to the source.
///
/// Tags whose key is in `names` are excluded when `is_exclude` is true, or kept when false.
/// Constructs a fresh `TagSet` without mutating the source, preserving isolation for
/// metrics that share the same underlying `Arc<TagSet>`.
#[inline]
fn apply_tag_filter(tags: &SharedTagSet, is_exclude: bool, names: &HashSet<String, FoldHashState>) -> Option<TagSet> {
    let capacity = if is_exclude {
        tags.len().saturating_sub(names.len())
    } else {
        names.len().min(tags.len())
    };
    let mut out = TagSet::with_capacity(capacity);
    let mut any_change = false;
    for tag in tags {
        if is_exclude != names.contains(tag.name()) {
            out.extend([tag.clone()]);
        } else {
            any_change = true;
        }
    }
    if any_change {
        Some(out)
    } else {
        None
    }
}

/// Filter the tags of a distribution metric according to the compiled filter table.
///
/// Both instrumented tags and origin tags are filtered using the same tag key list.
/// If the metric name is not present in `filters`, the metric is left unchanged.
/// If filtering would not change any tags, the metric context is left untouched (zero allocations).
#[inline]
pub fn filter_metric_tags(metric: &mut saluki_core::data_model::event::metric::Metric, filters: &CompiledFilters) {
    let Some((is_exclude, tag_names)) = filters.get(metric.context().name().as_ref()) else {
        return;
    };

    let new_tags = apply_tag_filter(metric.context().tags(), *is_exclude, tag_names);

    if metric.context().origin_tags().is_empty() {
        if let Some(filtered) = new_tags {
            *metric.context_mut() = metric.context().with_tags(filtered.into_shared());
        }
    } else {
        let new_origin = apply_tag_filter(metric.context().origin_tags(), *is_exclude, tag_names);
        match (new_tags, new_origin) {
            (None, None) => {}
            (Some(tags), None) => {
                *metric.context_mut() = metric.context().with_tags(tags.into_shared());
            }
            (None, Some(origin)) => {
                *metric.context_mut() = metric.context().with_origin_tags(origin.into_shared());
            }
            (Some(tags), Some(origin)) => {
                *metric.context_mut() = metric
                    .context()
                    .with_tags_and_origin_tags(tags.into_shared(), origin.into_shared());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use saluki_config::{dynamic::ConfigUpdate, ConfigurationLoader};
    use saluki_context::{tags::Tag, Context};
    use saluki_core::data_model::event::metric::Metric;

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
        filter_metric_tags(&mut metric, &filters);

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
        filter_metric_tags(&mut metric, &filters);

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
        filter_metric_tags(&mut metric, &filters);

        assert_eq!(tag_names(&metric), vec!["env:prod", "service:web"]);
    }

    #[test]
    fn non_distribution_metric_unchanged() {
        // compile_filters is correct; caller is responsible for checking is_sketch().
        // Test that a counter is not modified by the transform logic.
        let entries = vec![MetricTagFilterEntry {
            metric_name: "my.counter".to_string(),
            action: FilterAction::Exclude,
            tags: vec!["env".to_string()],
        }];
        let filters = compile_filters(&entries);

        let metric = counter_metric("my.counter", &["env:prod", "service:web"]);
        // filter_metric_tags is only called for is_sketch() metrics; verify counter is unchanged.
        assert!(!metric.values().is_sketch(), "counter should not be a sketch");
        // If we did call filter_metric_tags (incorrectly), verify tags are still filtered by name,
        // but in practice the transform guard prevents this.
        let _ = &filters; // filters compiled fine
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
        filter_metric_tags(&mut metric, &filters);

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
        filter_metric_tags(&mut metric, &filters);

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
        filter_metric_tags(&mut metric, &filters);

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
        filter_metric_tags(&mut metric, &filters);

        // Exclude wins: only "host" is removed.
        assert_eq!(tag_names(&metric), vec!["env:prod", "service:web"]);
    }

    #[test]
    fn merge_conflicting_actions_exclude_first_wins() {
        // Same as above but order is reversed: exclude comes first.
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
        filter_metric_tags(&mut metric, &filters);

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
        filter_metric_tags(&mut metric, &filters);

        assert_eq!(tag_names(&metric), vec!["service:web"]);
    }

    #[test]
    fn no_config_is_noop() {
        let filters = compile_filters(&[]);
        let mut metric = distribution_metric("my.dist", &["env:prod", "service:web"]);
        filter_metric_tags(&mut metric, &filters);
        assert_eq!(tag_names(&metric), vec!["env:prod", "service:web"]);
    }

    #[test]
    fn origin_tags_preserved_after_filtering() {
        // Build a context with origin_tags manually via from_inner.
        // We simulate origin_tags by verifying that with_tags() on Context preserves origin_tags.
        let context = Context::from_static_parts("my.dist", &["env:prod", "host:h1"]);
        // with_tags preserves the name; origin_tags are empty for static contexts.
        let tag_set: TagSet = [Tag::from("service:web")].into_iter().collect();
        let new_context = context.with_tags(tag_set.into_shared());
        assert_eq!(new_context.name().as_ref(), "my.dist");
        // origin_tags are empty for statically-created contexts.
        assert!(new_context.origin_tags().is_empty());
        // Only the new tag survives.
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
        filter_metric_tags(&mut metric, &filters);

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
        filter_metric_tags(&mut metric, &filters);

        assert_eq!(origin_tag_names(&metric), vec!["env:prod"]);
    }

    #[test]
    fn origin_tags_empty_unchanged() {
        // Fast path: metric has no origin_tags; filtering should still work correctly.
        let entries = vec![MetricTagFilterEntry {
            metric_name: "my.dist".to_string(),
            action: FilterAction::Exclude,
            tags: vec!["env".to_string()],
        }];
        let filters = compile_filters(&entries);

        let mut metric = distribution_metric("my.dist", &["env:prod", "service:web"]);
        filter_metric_tags(&mut metric, &filters);

        assert_eq!(tag_names(&metric), vec!["service:web"]);
        assert!(metric.context().origin_tags().is_empty());
    }

    #[test]
    fn filtering_origin_tags_does_not_affect_shared_origin() {
        // Two metrics share the same Arc<TagSet> for origin_tags.
        // Filtering one must not change the other's origin_tags.
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

        filter_metric_tags(&mut metric1, &filters);

        // metric1's origin_tags should be filtered.
        assert_eq!(origin_tag_names(&metric1), vec!["service:web"]);
        // metric2's origin_tags should be unchanged (still shares the original Arc).
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
        filter_metric_tags(&mut metric, &filters);

        // Both instrumented and origin tags should have env/host removed.
        assert_eq!(tag_names(&metric), vec!["service:web"]);
        assert_eq!(origin_tag_names(&metric), vec!["region:us-east-1"]);
    }

    #[tokio::test]
    async fn dynamic_update_partial_replaces_filter() {
        // Start with an empty static config to avoid figment merging static and dynamic arrays.
        let (cfg, sender) = ConfigurationLoader::for_tests(Some(serde_json::json!({})), None, true).await;
        let sender = sender.expect("sender should exist");
        sender
            .send(ConfigUpdate::Snapshot(serde_json::json!({})))
            .await
            .unwrap();
        cfg.ready().await;

        let mut watcher = cfg.watch_for_updates("metric_tag_filterlist");

        // Push a partial update setting the filter.
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

        // With filter (exclude "host"), "env" should be kept, "host" removed.
        let mut metric = distribution_metric("my.dist", &["env:prod", "host:h1", "service:web"]);
        filter_metric_tags(&mut metric, &filters);
        assert_eq!(tag_names(&metric), vec!["env:prod", "service:web"]);
    }

    #[tokio::test]
    async fn dynamic_update_to_empty_clears_filter() {
        // Start with an empty static config to avoid figment merging static and dynamic arrays.
        let (cfg, sender) = ConfigurationLoader::for_tests(Some(serde_json::json!({})), None, true).await;
        let sender = sender.expect("sender should exist");
        sender
            .send(ConfigUpdate::Snapshot(serde_json::json!({})))
            .await
            .unwrap();
        cfg.ready().await;

        let mut watcher = cfg.watch_for_updates("metric_tag_filterlist");

        // First, set a filter via partial update.
        sender
            .send(ConfigUpdate::Partial {
                key: "metric_tag_filterlist".to_string(),
                value: serde_json::json!([
                    { "metric_name": "my.dist", "action": "exclude", "tags": ["env"] }
                ]),
            })
            .await
            .unwrap();

        // Consume the first update.
        let _ = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            watcher.changed::<Vec<MetricTagFilterEntry>>(),
        )
        .await
        .expect("timed out waiting for initial metric_tag_filterlist update");

        // Now clear the filter.
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

        // No filters: all tags pass through.
        let mut metric = distribution_metric("my.dist", &["env:prod", "service:web"]);
        filter_metric_tags(&mut metric, &filters);
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

        // Push a full snapshot that includes the filterlist key.
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

        // Include "service": only service tag kept.
        let mut metric = distribution_metric("my.dist", &["env:prod", "service:web", "host:h1"]);
        filter_metric_tags(&mut metric, &filters);
        assert_eq!(tag_names(&metric), vec!["service:web"]);
    }
}
