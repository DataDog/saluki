//! Metric Tag Filterlist: configuration conversion for the tag projection kernel, and the legacy transform.
//!
//! Removes or retains specific tags from distribution and count metrics based on per-metric
//! configuration. Supports whole-tag filtering and value allow-listing.
//!
//! Whole-tag configuration is read from `metric_tag_filterlist` and can be updated at runtime via
//! Remote Config. Value allow-list configuration is read independently from the static
//! `metric_tag_value_allowlist` key.
//!
//! Both forms of filtering exist to merge samples that differ only in a filtered tag into a single aggregated
//! context, so they only apply to metrics that are aggregated. The `dogstatsd_no_agg_split` timestamp split routes
//! timestamped metrics around it, leaving their tags untouched.
//!
//! The filtering itself is the tag projection kernel's (`saluki_metric_kernels::tag_projection`), which the
//! plan-driven topology runs in a region. This module converts configuration into the kernel's rules
//! ([`tag_filter_rules`], [`tag_value_rules`], [`value_rules`]). The legacy transform, `TagFilterlistConfiguration`,
//! which replaced its rules when the whole-tag configuration changed, is compiled only for tests, as a differential
//! reference.

use agent_data_plane_config::domains::dogstatsd::{
    FilterAction, MetricTagFilterEntry, MetricTagValueAllowlistEntry, TagValueMismatchAction,
};
#[cfg(test)]
use agent_data_plane_config::Live;
use agent_data_plane_config_system::describe_invalid_tag_value_allowlist;
#[cfg(test)]
use async_trait::async_trait;
#[cfg(test)]
use saluki_core::{
    accounting::{MemoryBounds, MemoryBoundsBuilder},
    components::{
        transforms::{Transform, TransformBuilder, TransformContext},
        BuildContext,
    },
    data_model::event::EventType,
    observability::ComponentMetricsExt,
    topology::OutputDefinition,
};
use saluki_error::{generic_error, GenericError};
#[cfg(test)]
use saluki_metric_kernels::tag_projection::{
    ContextMemo, TagProjection, TagProjectionRules, TagProjectionTelemetry, CONTEXT_CACHE_ENTRY_BYTES,
    NATIVE_CONTEXT_CACHE_ID,
};
use saluki_metric_kernels::{
    params::{TagFilterAction, TagFilterRule, TagValueMismatch, TagValueRule},
    tag_projection::ValueRules,
};
#[cfg(test)]
use saluki_metrics::MetricsBuilder;
#[cfg(test)]
use tokio::select;
#[cfg(test)]
use tracing::{debug, error};

/// Converts whole-tag filter entries into tag projection rules.
pub(crate) fn tag_filter_rules(entries: &[MetricTagFilterEntry]) -> Vec<TagFilterRule> {
    entries
        .iter()
        .map(|entry| TagFilterRule {
            metric_name: entry.metric_name.clone(),
            action: match entry.action {
                FilterAction::Include => TagFilterAction::Include,
                FilterAction::Exclude => TagFilterAction::Exclude,
            },
            tags: entry.tags.clone(),
        })
        .collect()
}

/// Converts tag value allow-list entries into tag projection value rules.
///
/// An entry's replacement only applies when it replaces mismatched values; an entry that removes them has none.
pub(crate) fn tag_value_rules(entries: &[MetricTagValueAllowlistEntry]) -> Vec<TagValueRule> {
    entries
        .iter()
        .map(|entry| TagValueRule {
            metric_prefix: entry.metric_prefix.clone(),
            tag_name: entry.tag_name.clone(),
            values: entry.values.clone(),
            on_miss: match entry.on_miss {
                TagValueMismatchAction::Remove => TagValueMismatch::Remove,
                TagValueMismatchAction::Replace => TagValueMismatch::Replace(entry.replacement.clone()),
            },
        })
        .collect()
}

/// Compiles tag value allow-list entries into the tag projection kernel's value rules.
///
/// # Errors
///
/// Returns an error, worded in terms of the `metric_tag_value_allowlist` configuration key, when the rules are
/// invalid. Configuration loading rejects such rules first, with the same message.
pub(crate) fn value_rules(entries: &[MetricTagValueAllowlistEntry]) -> Result<ValueRules, GenericError> {
    ValueRules::new(&tag_value_rules(entries))
        .map_err(|problem| generic_error!("{}", describe_invalid_tag_value_allowlist(&problem)))
}

#[cfg(test)]
fn compile_filters(entries: &[MetricTagFilterEntry], value_rules: &ValueRules) -> TagProjectionRules {
    TagProjectionRules::new(&tag_filter_rules(entries), value_rules.clone())
}

/// Metric Tag Filterlist transform: the legacy component, a differential reference for tests.
///
/// Removes, retains, or replaces tags on counter and sketch-backed metrics based on per-metric configuration. Gauges
/// and other metric types pass through unchanged. Whole-tag rules are read from `metric_tag_filterlist` and support
/// runtime updates via Remote Config. Value rules are read from the static `metric_tag_value_allowlist` key and match
/// metric names after DogStatsD mapper rewrites and metric namespace prefixing.
#[cfg(test)]
pub struct TagFilterlistConfiguration {
    entries: Live<Vec<MetricTagFilterEntry>>,

    /// Compiled per-metric-prefix tag value allow-list rules.
    ///
    /// Configured independently from `metric_tag_filterlist` so a remotely configured whole-tag filter does not
    /// replace locally configured value rules. This key is static in the initial implementation; changing it requires
    /// restarting ADP. The configuration boundary rejects invalid rules, and construction compiles them once so
    /// dynamic whole-tag updates cannot fail on unchanged static configuration.
    value_rules: ValueRules,

    /// Maximum number of entries in the per-context deduplication cache used by the tag filter.
    ///
    /// Configured via `data_plane.dogstatsd.aggregator_tag_filter_cache_capacity` in the agent config
    /// stream. High-throughput deployments with many unique metric contexts may benefit from
    /// increasing this value to reduce cache churn.
    ///
    /// Defaults to 100,000.
    context_cache_capacity: usize,
}

#[cfg(test)]
impl TagFilterlistConfiguration {
    /// Creates a new `TagFilterlistConfiguration`.
    ///
    /// # Errors
    ///
    /// Returns an error when the initial value allow-list rules are invalid.
    pub fn new(
        entries: Live<Vec<MetricTagFilterEntry>>, value_allowlists: &[MetricTagValueAllowlistEntry],
        context_cache_capacity: usize,
    ) -> Result<Self, GenericError> {
        let value_rules = value_rules(value_allowlists)?;
        Ok(Self {
            entries,
            value_rules,
            context_cache_capacity,
        })
    }
}

#[cfg(test)]
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
        let projection = TagProjection::new(
            compile_filters(&self.entries, &self.value_rules),
            Some(ContextMemo::new(NATIVE_CONTEXT_CACHE_ID, self.context_cache_capacity)),
            TagProjectionTelemetry::new(&metrics_builder),
        );

        Ok(Box::new(TagFilterlist {
            projection,
            entries: self.entries.clone(),
            value_rules: self.value_rules.clone(),
            context_cache_capacity: self.context_cache_capacity,
        }))
    }
}

#[cfg(test)]
impl MemoryBounds for TagFilterlistConfiguration {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        builder.minimum().with_single_value::<TagFilterlist>("component struct");

        builder
            .firm()
            .with_fixed_amount("context cache", self.context_cache_capacity * CONTEXT_CACHE_ENTRY_BYTES);
    }
}

#[cfg(test)]
struct TagFilterlist {
    projection: TagProjection,
    entries: Live<Vec<MetricTagFilterEntry>>,
    value_rules: ValueRules,
    context_cache_capacity: usize,
}

#[cfg(test)]
#[async_trait]
impl Transform for TagFilterlist {
    async fn run(mut self: Box<Self>, mut context: TransformContext) -> Result<(), GenericError> {
        let mut health = context.take_health_handle();
        health.mark_ready();

        debug!("Metric Tag Filterlist transform started.");

        loop {
            select! {
                _ = health.live() => continue,
                maybe_events = context.events().next() => match maybe_events {
                    Some(mut events) => {
                        self.projection.apply_buffer(&mut events);
                        if let Err(e) = context.dispatcher().dispatch(events).await {
                            error!(error = %e, "Failed to dispatch events.");
                        }
                    }
                    None => break,
                },
                new_entries = self.entries.changed() => {
                    // New rules start with an empty memo, so that no result of the old rules is served.
                    self.projection.replace_rules(
                        compile_filters(&new_entries, &self.value_rules),
                        Some(ContextMemo::new(NATIVE_CONTEXT_CACHE_ID, self.context_cache_capacity)),
                    );
                    debug!(rules_loaded = self.projection.rule_count(), "Updated metric tag filterlist.");
                },
            }
        }

        debug!("Metric Tag Filterlist transform stopped.");

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Duration};

    use metrics::set_default_local_recorder;
    use saluki_core::{
        accounting::{ComponentRegistry, MemoryLimiter},
        components::{
            transforms::{TransformBuilder, TransformContext},
            BuildContext, ComponentContext,
        },
        data_model::event::{
            metric::{
                context::{Context, TagSetMutViewState},
                Metric, MetricValues,
            },
            Event,
        },
        health::HealthRegistry,
        runtime::state::{DataspaceRegistry, ResourceRegistry},
        topology::{
            interconnect::{Consumer, Dispatcher},
            EventsBuffer, OutputName, TopologyContext,
        },
    };
    use saluki_metric_kernels::tag_projection::filter_metric_tags;
    use saluki_metrics::test::TestRecorder;
    use tokio::runtime::Handle;
    use tokio::sync::mpsc;

    use super::*;

    // The filtering itself is tested with the tag projection kernel in `saluki_metric_kernels::tag_projection`. These
    // tests cover what this transform adds: converting its configuration, and its run loop.

    fn distribution_metric(name: &'static str, tags: &[&'static str]) -> Metric {
        let context = Context::from_static_parts(name, tags);
        Metric::distribution(context, 1.0)
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
    fn configuration_converts_into_tag_projection_rules() {
        let entries = [
            MetricTagFilterEntry {
                metric_name: "my.metric".to_string(),
                action: FilterAction::Exclude,
                tags: vec!["host".to_string()],
            },
            MetricTagFilterEntry {
                metric_name: "my.other".to_string(),
                action: FilterAction::Include,
                tags: vec![],
            },
        ];
        assert_eq!(
            tag_filter_rules(&entries),
            [
                TagFilterRule {
                    metric_name: "my.metric".to_string(),
                    action: TagFilterAction::Exclude,
                    tags: vec!["host".to_string()],
                },
                TagFilterRule {
                    metric_name: "my.other".to_string(),
                    action: TagFilterAction::Include,
                    tags: vec![],
                },
            ]
        );

        // A replacement only reaches the rule when mismatched values are replaced.
        let value_allowlists = [
            value_allowlist(
                "my.",
                "customer_id",
                &["top-1"],
                TagValueMismatchAction::Replace,
                " other ",
            ),
            value_allowlist("their.", "region", &[], TagValueMismatchAction::Remove, "other"),
        ];
        assert_eq!(
            tag_value_rules(&value_allowlists),
            [
                TagValueRule {
                    metric_prefix: "my.".to_string(),
                    tag_name: "customer_id".to_string(),
                    values: vec!["top-1".to_string()],
                    on_miss: TagValueMismatch::Replace(" other ".to_string()),
                },
                TagValueRule {
                    metric_prefix: "their.".to_string(),
                    tag_name: "region".to_string(),
                    values: vec![],
                    on_miss: TagValueMismatch::Remove,
                },
            ]
        );
    }

    #[test]
    fn invalid_value_allowlists_fail_construction() {
        let overlapping = [
            value_allowlist("my.", "customer_id", &[], TagValueMismatchAction::Remove, "other"),
            value_allowlist("my.metric", "region", &[], TagValueMismatchAction::Remove, "other"),
        ];
        let error = TagFilterlistConfiguration::new(Live::new_fixed(Vec::new()), &overlapping, 10)
            .err()
            .expect("overlapping distinct prefixes must fail");
        // The message configuration loading gives for the same rules, in terms of the configuration keys.
        assert_eq!(
            error.to_string(),
            "overlapping metric prefixes 'my.' and 'my.metric' are configured; configure distinct prefixes that do \
             not overlap"
        );
    }

    #[test]
    fn memory_bounds_account_for_the_context_cache() {
        let config = TagFilterlistConfiguration::new(Live::new_fixed(Vec::new()), &[], 1000).unwrap();
        let registry = ComponentRegistry::default();
        config.specify_bounds(
            &mut registry.bounds_builder(&saluki_core::support::SubsystemIdentifier::from_dotted("test")),
        );
        let bounds = registry.as_bounds();
        assert_eq!(
            bounds.total_firm_limit_bytes() - bounds.total_minimum_required_bytes(),
            1000 * 64
        );
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
        let value_rules =
            ValueRules::new(&tag_value_rules(&value_allowlists)).expect("static value rules should compile");

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
        let filters = compile_filters(&new_entries, &value_rules);
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
        let filters = compile_filters(&new_entries, &value_rules);
        let mut metric = distribution_metric("my.metric", &["customer_id:long-tail", "host:h1", "service:web"]);
        filter_metric_tags(&mut metric, &mut state, &filters);
        assert_eq!(tag_names(&metric), vec!["host:h1", "service:web"]);
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
        let builder = TagFilterlistConfiguration::new(Live::new_fixed(entries), &value_allowlists, 100_000)
            .expect("typed configuration should be valid");

        let component_context = ComponentContext::test_transform("tag_filterlist");
        let transform = builder
            .build(BuildContext::new(component_context.clone(), ResourceRegistry::new()))
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

        let context = TransformContext::new(
            &topology_context,
            &component_context,
            ComponentRegistry::default(),
            health,
            dispatcher,
            consumer,
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

    #[tokio::test]
    async fn the_run_loop_replaces_rules_and_memo_on_live_updates() {
        let recorder = TestRecorder::default();
        let _local = set_default_local_recorder(&recorder);
        let cell = Arc::new(arc_swap::ArcSwap::from_pointee(
            agent_data_plane_config::SalukiConfiguration::default(),
        ));
        let (tick_tx, tick_rx) = tokio::sync::watch::channel(());
        let entries = Live::new_dynamic(Arc::clone(&cell), tick_rx, |config| {
            &config.domains.dogstatsd.tag_filterlist
        });
        let builder = TagFilterlistConfiguration::new(entries, &[], 100).expect("typed configuration should be valid");
        let component_context = ComponentContext::test_transform("tag_filterlist");
        let transform = builder
            .build(BuildContext::new(component_context.clone(), ResourceRegistry::new()))
            .await
            .expect("tag filterlist should build");

        let mut dispatcher = Dispatcher::new(component_context.clone());
        dispatcher.add_output(OutputName::Default).expect("add default output");
        let (out_tx, mut out_rx) = mpsc::channel(4);
        dispatcher
            .attach_sender_to_output(&OutputName::Default, out_tx)
            .expect("attach default sender");
        let (in_tx, in_rx) = mpsc::channel(4);
        let consumer = Consumer::new(component_context.clone(), in_rx);
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
        let context = TransformContext::new(
            &topology_context,
            &component_context,
            ComponentRegistry::default(),
            health,
            dispatcher,
            consumer,
        );
        let running = tokio::spawn(transform.run(context));

        let batch = || {
            let mut input = EventsBuffer::default();
            let metric = distribution_metric("my.metric", &["host:h1", "service:web"]);
            assert!(input.try_push(Event::Metric(metric)).is_none());
            input
        };
        let received_tags = |buffer: EventsBuffer| {
            buffer
                .into_iter()
                .filter_map(Event::try_into_metric)
                .map(|metric| tag_names(&metric))
                .collect::<Vec<_>>()
        };

        // No rules yet: the metric passes unchanged, and the memo holds nothing for it.
        in_tx.send(batch()).await.unwrap();
        let first = tokio::time::timeout(Duration::from_secs(2), out_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(received_tags(first), [vec!["host:h1", "service:web"]]);

        // A rule arrives: the same context is now projected.
        let mut updated = (*cell.load_full()).clone();
        updated.domains.dogstatsd.tag_filterlist = vec![MetricTagFilterEntry {
            metric_name: "my.metric".to_string(),
            action: FilterAction::Exclude,
            tags: vec!["host".to_string()],
        }];
        cell.store(Arc::new(updated));
        tick_tx.send_replace(());
        // The update and the next batch race in the run loop, so send until the rule shows.
        let mut projected = false;
        for _ in 0..50 {
            in_tx.send(batch()).await.unwrap();
            let received = tokio::time::timeout(Duration::from_secs(2), out_rx.recv())
                .await
                .unwrap()
                .unwrap();
            if received_tags(received) == [vec!["service:web"]] {
                projected = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(projected, "the updated rule never applied");
        let key = |name: &'static str| {
            metrics::Key::from_parts(
                name,
                &[
                    ("component_id", "tag_filterlist"),
                    ("component_type", component_context.component_type().as_str()),
                ],
            )
        };
        assert_eq!(recorder.counter(key("tag_filterlist_updates_total")), Some(1));
        assert_eq!(recorder.gauge(key("tag_filterlist_size")), Some(1.0));

        drop(in_tx);
        running.await.unwrap().expect("tag filterlist run should succeed");
    }
}
