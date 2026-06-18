//! DogStatsD metric prefix and listener-side metric filter transform.

use async_trait::async_trait;
use metrics::{Counter, Gauge};
use resource_accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_component_config::{DogStatsDPrefixFilterConfig, ScopedConfig};
use saluki_core::data_model::event::{metric::Metric, EventType};
use saluki_core::{
    components::{
        transforms::{Transform, TransformBuilder, TransformContext},
        ComponentContext,
    },
    observability::ComponentMetricsExt as _,
    topology::OutputDefinition,
};
use saluki_error::GenericError;
use saluki_metrics::MetricsBuilder;
use tokio::select;
use tracing::{debug, error};

use crate::components::dogstatsd_filterlist::{Blocklist, EffectiveFilterlist};

const METRIC_FILTERLIST_SIZE_METRIC: &str = "metric_filterlist_size";
const METRIC_FILTERLIST_UPDATES_METRIC: &str = "metric_filterlist_updates_total";
const LISTENER_FILTERED_POINTS_METRIC: &str = "dogstatsd_listener_filtered_points_total";

/// DogStatsD prefix filter transform.
///
/// Appends a prefix to every metric if specified.
///
/// Checks if a metric name should be allowed.
pub struct DogStatsDPrefixFilterConfiguration {
    initial: DogStatsDPrefixFilterConfig,
    configuration: ScopedConfig<DogStatsDPrefixFilterConfig>,
}

impl DogStatsDPrefixFilterConfiguration {
    /// Creates a new `DogStatsDPrefixFilterConfiguration` from a native config handle.
    pub fn from_native(configuration: ScopedConfig<DogStatsDPrefixFilterConfig>) -> Self {
        let initial = configuration.current();
        Self { initial, configuration }
    }
}

#[async_trait]
impl TransformBuilder for DogStatsDPrefixFilterConfiguration {
    fn input_event_type(&self) -> EventType {
        EventType::Metric
    }

    fn outputs(&self) -> &[OutputDefinition<EventType>] {
        static OUTPUTS: &[OutputDefinition<EventType>] = &[OutputDefinition::default_output(EventType::Metric)];
        OUTPUTS
    }

    async fn build(&self, context: ComponentContext) -> Result<Box<dyn Transform + Send>, GenericError> {
        // Ensure our metric prefix has a trailing period so that we don't have to check for, and possibly add it, when we're
        // actually processing metrics.
        let mut metric_prefix = self.initial.metric_prefix.clone();
        if !metric_prefix.is_empty() && !metric_prefix.ends_with(".") {
            metric_prefix.push('.');
        }
        let metrics_builder = MetricsBuilder::from_component_context(&context);
        let effective_filterlist = EffectiveFilterlist::new(
            self.initial.metric_filterlist.clone(),
            self.initial.metric_filterlist_match_prefix,
            self.initial.metric_blocklist.clone(),
            self.initial.metric_blocklist_match_prefix,
        );
        let telemetry = FilterlistTelemetry::new(&metrics_builder);
        let mut filter = DogStatsDPrefixFilter {
            metric_prefix,
            metric_prefix_blocklist: self.initial.metric_prefix_blocklist.clone(),
            matcher: Blocklist::default(),
            effective_filterlist,
            telemetry,
            configuration: self.configuration.clone(),
        };
        filter.sync_effective_blocklist(false);

        Ok(Box::new(filter))
    }
}

impl MemoryBounds for DogStatsDPrefixFilterConfiguration {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        // Capture the size of the heap allocation when the component is built.
        builder
            .minimum()
            .with_single_value::<DogStatsDPrefixFilter>("component struct");
    }
}

#[derive(Clone)]
struct FilterlistTelemetry {
    filterlist_size: Gauge,
    filterlist_updates: Counter,
    listener_filtered_points: Counter,
}

impl FilterlistTelemetry {
    fn new(builder: &MetricsBuilder) -> Self {
        Self {
            filterlist_size: builder.register_gauge(METRIC_FILTERLIST_SIZE_METRIC),
            filterlist_updates: builder.register_counter(METRIC_FILTERLIST_UPDATES_METRIC),
            listener_filtered_points: builder.register_counter(LISTENER_FILTERED_POINTS_METRIC),
        }
    }

    #[cfg(test)]
    fn noop() -> Self {
        Self {
            filterlist_size: Gauge::noop(),
            filterlist_updates: Counter::noop(),
            listener_filtered_points: Counter::noop(),
        }
    }

    fn increment_filterlist_updates(&self) {
        self.filterlist_updates.increment(1);
    }

    fn increment_listener_filtered_points(&self) {
        self.listener_filtered_points.increment(1);
    }

    fn set_filterlist_size(&self, size: usize) {
        self.filterlist_size.set(size as f64);
    }
}

struct DogStatsDPrefixFilter {
    metric_prefix: String,
    metric_prefix_blocklist: Vec<String>,
    matcher: Blocklist,
    effective_filterlist: EffectiveFilterlist,
    telemetry: FilterlistTelemetry,
    configuration: ScopedConfig<DogStatsDPrefixFilterConfig>,
}

impl DogStatsDPrefixFilter {
    fn sync_effective_blocklist(&mut self, count_update: bool) {
        self.matcher = self.effective_filterlist.to_matcher();
        self.telemetry
            .set_filterlist_size(self.effective_filterlist.effective_len());
        if count_update {
            self.telemetry.increment_filterlist_updates();
        }
    }

    #[cfg(test)]
    fn update_metric_filterlist(&mut self, metric_filterlist: Vec<String>) {
        let count_update = self.effective_filterlist.metric_filterlist_is_active() || !metric_filterlist.is_empty();
        self.effective_filterlist.set_metric_filterlist(metric_filterlist);
        self.sync_effective_blocklist(count_update);
    }

    #[cfg(test)]
    fn update_metric_blocklist(&mut self, metric_blocklist: Vec<String>) {
        let count_update = !self.effective_filterlist.metric_filterlist_is_active();
        self.effective_filterlist.set_metric_blocklist(metric_blocklist);
        self.sync_effective_blocklist(count_update);
    }

    #[cfg(test)]
    fn update_metric_filterlist_match_prefix(&mut self, match_prefix: bool) {
        let count_update = self.effective_filterlist.metric_filterlist_is_active();
        self.effective_filterlist
            .set_metric_filterlist_match_prefix(match_prefix);
        self.sync_effective_blocklist(count_update);
    }

    fn apply_config(&mut self, config: DogStatsDPrefixFilterConfig) {
        self.effective_filterlist = EffectiveFilterlist::new(
            config.metric_filterlist,
            config.metric_filterlist_match_prefix,
            config.metric_blocklist,
            config.metric_blocklist_match_prefix,
        );
        self.sync_effective_blocklist(true);
    }

    fn process_metric(&self, metric: &mut Metric) -> bool {
        let metric_name = metric.context().name().as_ref();

        if self.metric_prefix.is_empty() {
            if self.matcher.contains(metric_name) {
                self.telemetry.increment_listener_filtered_points();
                debug!("Metric {} excluded due to blocklist.", metric_name);
                return false;
            }
        } else {
            // We don't want to prefix the metric if it has a prefix that is on our _prefix_ blocklist,
            // which ensures we don't prefix metrics that are already prefixed.
            let new_metric_name = if self.has_excluded_prefix(metric_name) {
                metric.context().name().clone()
            } else {
                let mut prefixed_metric_name = self.metric_prefix.clone();
                prefixed_metric_name.push_str(metric_name);
                prefixed_metric_name.into()
            };

            if self.matcher.contains(&new_metric_name) {
                self.telemetry.increment_listener_filtered_points();
                debug!("Metric {} excluded due to blocklist.", new_metric_name);
                return false;
            }

            // Update metric with new name.
            let new_context = metric.context().with_name(new_metric_name);
            let existing_context = metric.context_mut();
            *existing_context = new_context;
        }

        true
    }

    fn has_excluded_prefix(&self, metric_name: &str) -> bool {
        !self.metric_prefix.is_empty()
            && self
                .metric_prefix_blocklist
                .iter()
                .any(|prefix| metric_name.starts_with(prefix))
    }
}

#[async_trait]
impl Transform for DogStatsDPrefixFilter {
    async fn run(mut self: Box<Self>, mut context: TransformContext) -> Result<(), GenericError> {
        let mut health = context.take_health_handle();
        health.mark_ready();

        let mut configuration = self.configuration.clone();

        debug!("DogStatsD Prefix Filter transform started.");

        loop {
            select! {
                _ = health.live() => continue,
                maybe_events = context.events().next() => match maybe_events {
                    Some(mut events) => {
                        events.remove_if(|event| match event.try_as_metric_mut() {
                            // `process_metric` returns `true` if the metric should be kept, so we have to invert that
                            // here to match the predicate structure, which will _remove_ the event if `true` is returned.
                            Some(metric) => !self.process_metric(metric),
                            None => true,
                        });

                        if let Err(e) = context.dispatcher().dispatch(events).await {
                            error!(error = %e, "Failed to dispatch events.");
                        }
                    },
                    None => break,
                },
                _ = configuration.changed() => {
                    self.apply_config(configuration.current());
                    debug!("Updated DogStatsD prefix filter config.");
                },
            }
        }

        debug!("DogStatsD Prefix Filter transform stopped.");

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use metrics::set_default_local_recorder;
    use saluki_config_tools::{dynamic::ConfigUpdate, ConfigurationLoader};
    use saluki_metrics::{test::TestRecorder, MetricsBuilder};

    use super::*;

    #[test]
    fn test_metric_prefix_add() {
        let filter = DogStatsDPrefixFilter {
            metric_prefix: "foo.".to_string(),
            metric_prefix_blocklist: vec![],
            matcher: Blocklist::default(),
            effective_filterlist: EffectiveFilterlist::default(),
            telemetry: FilterlistTelemetry::noop(),
            configuration: ScopedConfig::fixed(DogStatsDPrefixFilterConfig::default()),
        };

        let mut metric = Metric::gauge("bar", 1.0);
        assert!(filter.process_metric(&mut metric));
        assert_eq!(metric.context().name(), "foo.bar");
    }

    #[test]
    fn test_metric_prefix_blocklist() {
        let filter = DogStatsDPrefixFilter {
            metric_prefix: "foo".to_string(),
            metric_prefix_blocklist: vec!["foo".to_string(), "bar".to_string()],
            matcher: Blocklist::default(),
            effective_filterlist: EffectiveFilterlist::default(),
            telemetry: FilterlistTelemetry::noop(),
            configuration: ScopedConfig::fixed(DogStatsDPrefixFilterConfig::default()),
        };

        let mut metric = Metric::gauge("barbar", 1.0);
        assert!(filter.process_metric(&mut metric));
        assert_eq!(metric.context().name(), "barbar");
    }

    #[test]
    fn test_metric_blocklist() {
        let filter = DogStatsDPrefixFilter {
            metric_prefix: "".to_string(),
            metric_prefix_blocklist: vec![],
            matcher: Blocklist::new(["foobar", "test"], false),
            effective_filterlist: EffectiveFilterlist::default(),
            telemetry: FilterlistTelemetry::noop(),
            configuration: ScopedConfig::fixed(DogStatsDPrefixFilterConfig::default()),
        };

        let mut metric = Metric::gauge("foobar", 1.0);
        assert!(!filter.process_metric(&mut metric));

        let mut metric = Metric::gauge("foo", 1.0);
        assert!(filter.process_metric(&mut metric));
        assert_eq!(metric.context().name(), "foo");
    }

    #[test]
    fn test_metric_blocklist_with_metric_prefix() {
        let filter = DogStatsDPrefixFilter {
            metric_prefix: "foo.".to_string(),
            metric_prefix_blocklist: vec![],
            matcher: Blocklist::new(["foo.bar", "test"], false),
            effective_filterlist: EffectiveFilterlist::default(),
            telemetry: FilterlistTelemetry::noop(),
            configuration: ScopedConfig::fixed(DogStatsDPrefixFilterConfig::default()),
        };

        let mut metric = Metric::gauge("bar", 1.0);
        assert!(!filter.process_metric(&mut metric));

        let filter = DogStatsDPrefixFilter {
            metric_prefix: "foo.".to_string(),
            metric_prefix_blocklist: vec!["foo".to_string()],
            matcher: Blocklist::default(),
            effective_filterlist: EffectiveFilterlist::default(),
            telemetry: FilterlistTelemetry::noop(),
            configuration: ScopedConfig::fixed(DogStatsDPrefixFilterConfig::default()),
        };

        let mut metric = Metric::gauge("foo", 1.0);
        assert!(filter.process_metric(&mut metric));
        assert_eq!(metric.context().name(), "foo");
    }

    #[test]
    fn test_metric_match_prefix_without_added_prefix() {
        let filter = DogStatsDPrefixFilter {
            metric_prefix: "".to_string(),
            metric_prefix_blocklist: vec![],
            matcher: Blocklist::new(["b", "test"], true),
            effective_filterlist: EffectiveFilterlist::default(),
            telemetry: FilterlistTelemetry::noop(),
            configuration: ScopedConfig::fixed(DogStatsDPrefixFilterConfig::default()),
        };

        // match prefix is true, "bar" has prefix "b"
        let mut metric = Metric::gauge("bar", 1.0);
        assert!(!filter.process_metric(&mut metric));

        // match prefix is true, "test" has prefix "test"
        let mut metric = Metric::gauge("test", 1.0);
        assert!(!filter.process_metric(&mut metric));
    }

    #[test]
    fn test_metric_match_prefix_with_added_prefix() {
        let filter = DogStatsDPrefixFilter {
            metric_prefix: "foo".to_string(),
            metric_prefix_blocklist: vec![],
            matcher: Blocklist::new(["fo", "test"], true),
            effective_filterlist: EffectiveFilterlist::default(),
            telemetry: FilterlistTelemetry::noop(),
            configuration: ScopedConfig::fixed(DogStatsDPrefixFilterConfig::default()),
        };

        // new_metric is "foo.bar", match prefix is true, "foo.bar" has prefix "fo"
        let mut metric = Metric::gauge("bar", 1.0);
        assert!(!filter.process_metric(&mut metric));
    }

    #[tokio::test]
    async fn test_metric_blocklist_dynamic_update() {
        let (cfg, sender) = ConfigurationLoader::for_tests(Some(serde_json::json!({})), None, true).await;
        let sender = sender.expect("sender should exist");
        sender
            .send(ConfigUpdate::Snapshot(serde_json::json!({})))
            .await
            .unwrap();

        cfg.ready().await;

        let mut filter = DogStatsDPrefixFilter {
            metric_prefix: "".to_string(),
            metric_prefix_blocklist: vec![],
            matcher: Blocklist::new(["foobar", "test"], false),
            effective_filterlist: EffectiveFilterlist::new(
                Vec::new(),
                false,
                vec!["foobar".to_string(), "test".to_string()],
                false,
            ),
            telemetry: FilterlistTelemetry::noop(),
            configuration: ScopedConfig::fixed(DogStatsDPrefixFilterConfig::default()),
        };

        let mut metric = Metric::gauge("foobar", 1.0);
        assert!(!filter.process_metric(&mut metric));

        let mut metric = Metric::gauge("foo", 1.0);
        assert!(filter.process_metric(&mut metric));
        assert_eq!(metric.context().name(), "foo");

        let mut blocklist_watcher = cfg.watch_for_updates("statsd_metric_blocklist");

        sender
            .send(ConfigUpdate::Partial {
                key: "statsd_metric_blocklist".to_string(),
                value: serde_json::json!(["foo".to_string()]),
            })
            .await
            .unwrap();

        let (_, new) = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            blocklist_watcher.changed::<Vec<String>>(),
        )
        .await
        .expect("timed out waiting for statsd_metric_blocklist update");

        assert_eq!(new, Some(vec!["foo".to_string()]));

        // Apply the dynamic update to the filter under test.
        filter.update_metric_blocklist(new.unwrap());

        // "foobar" is taken off the blocklist
        let mut metric = Metric::gauge("foobar", 1.0);
        assert!(filter.process_metric(&mut metric));
        assert_eq!(metric.context().name(), "foobar");

        // "foo" is added to the blocklist
        let mut metric = Metric::gauge("foo", 1.0);
        assert!(!filter.process_metric(&mut metric));

        let mut metric_filterlist_watcher = cfg.watch_for_updates("metric_filterlist");
        sender
            .send(ConfigUpdate::Partial {
                key: "metric_filterlist".to_string(),
                value: serde_json::json!(["baz".to_string()]),
            })
            .await
            .unwrap();

        let (_, new) = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            metric_filterlist_watcher.changed::<Vec<String>>(),
        )
        .await
        .expect("timed out waiting for metric_filterlist update");

        assert_eq!(new, Some(vec!["baz".to_string()]));

        // Apply the dynamic update to the filter under test.
        filter.update_metric_filterlist(new.unwrap());

        // "baz" is added to the filterlist
        let mut metric = Metric::gauge("baz", 1.0);
        assert!(!filter.process_metric(&mut metric));
    }

    #[tokio::test]
    async fn test_metric_filterlist_match_prefix_dynamic_update_is_applied() {
        let (cfg, sender) = ConfigurationLoader::for_tests(
            Some(serde_json::json!({
                "metric_filterlist": ["foo"],
                "metric_filterlist_match_prefix": false
            })),
            None,
            true,
        )
        .await;
        let sender = sender.expect("sender should exist");

        sender
            .send(ConfigUpdate::Snapshot(serde_json::json!({})))
            .await
            .unwrap();
        cfg.ready().await;

        let mut filter = DogStatsDPrefixFilter {
            metric_prefix: "".to_string(),
            metric_prefix_blocklist: vec![],
            matcher: Blocklist::new(["foo"], false),
            effective_filterlist: EffectiveFilterlist::new(vec!["foo".to_string()], false, Vec::new(), false),
            telemetry: FilterlistTelemetry::noop(),
            configuration: ScopedConfig::fixed(DogStatsDPrefixFilterConfig::default()),
        };

        let mut metric = Metric::gauge("foo.bar", 1.0);
        assert!(filter.process_metric(&mut metric));

        let mut match_prefix_watcher = cfg.watch_for_updates("metric_filterlist_match_prefix");
        sender
            .send(ConfigUpdate::Partial {
                key: "metric_filterlist_match_prefix".to_string(),
                value: serde_json::json!(true),
            })
            .await
            .unwrap();

        let (_, new) = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            match_prefix_watcher.changed::<bool>(),
        )
        .await
        .expect("timed out waiting for metric_filterlist_match_prefix update");

        assert_eq!(new, Some(true));

        // Apply the dynamic update to the filter under test.
        filter.update_metric_filterlist_match_prefix(new.unwrap());

        let mut metric = Metric::gauge("foo.bar", 1.0);
        assert!(!filter.process_metric(&mut metric));
        assert_eq!(filter.matcher, Blocklist::new(["foo"], true));
    }

    #[test]
    fn telemetry_only_counts_active_filterlist_updates() {
        let recorder = TestRecorder::default();
        let _local = set_default_local_recorder(&recorder);

        let telemetry = FilterlistTelemetry::new(&MetricsBuilder::default());
        let mut filter = DogStatsDPrefixFilter {
            metric_prefix: "".to_string(),
            metric_prefix_blocklist: vec![],
            matcher: Blocklist::default(),
            effective_filterlist: EffectiveFilterlist::new(
                vec!["preferred".to_string()],
                false,
                vec!["legacy".to_string()],
                false,
            ),
            telemetry,
            configuration: ScopedConfig::fixed(DogStatsDPrefixFilterConfig::default()),
        };

        filter.sync_effective_blocklist(false);
        filter.update_metric_blocklist(vec!["ignored".to_string(), "still_ignored".to_string()]);

        assert_eq!(recorder.counter(METRIC_FILTERLIST_UPDATES_METRIC), Some(0));
        assert_eq!(recorder.gauge(METRIC_FILTERLIST_SIZE_METRIC), Some(1.0));

        let mut metric = Metric::gauge("preferred", 1.0);
        assert!(!filter.process_metric(&mut metric));

        let mut metric = Metric::gauge("ignored", 1.0);
        assert!(filter.process_metric(&mut metric));

        filter.update_metric_filterlist(Vec::new());

        assert_eq!(recorder.counter(METRIC_FILTERLIST_UPDATES_METRIC), Some(1));
        assert_eq!(recorder.gauge(METRIC_FILTERLIST_SIZE_METRIC), Some(2.0));

        let mut metric = Metric::gauge("ignored", 1.0);
        assert!(!filter.process_metric(&mut metric));
    }

    #[test]
    fn telemetry_counts_active_reconfiguration_even_if_matcher_is_unchanged() {
        let recorder = TestRecorder::default();
        let _local = set_default_local_recorder(&recorder);

        let telemetry = FilterlistTelemetry::new(&MetricsBuilder::default());
        let mut filter = DogStatsDPrefixFilter {
            metric_prefix: "".to_string(),
            metric_prefix_blocklist: vec![],
            matcher: Blocklist::default(),
            effective_filterlist: EffectiveFilterlist::new(vec!["foo".to_string()], true, Vec::new(), false),
            telemetry,
            configuration: ScopedConfig::fixed(DogStatsDPrefixFilterConfig::default()),
        };

        filter.sync_effective_blocklist(false);
        filter.update_metric_filterlist(vec!["foo".to_string(), "foobar".to_string()]);

        assert_eq!(recorder.counter(METRIC_FILTERLIST_UPDATES_METRIC), Some(1));
        assert_eq!(recorder.gauge(METRIC_FILTERLIST_SIZE_METRIC), Some(2.0));

        let mut metric = Metric::gauge("foobar.baz", 1.0);
        assert!(!filter.process_metric(&mut metric));
    }

    #[test]
    fn telemetry_counts_listener_filtered_points() {
        let recorder = TestRecorder::default();
        let _local = set_default_local_recorder(&recorder);

        let telemetry = FilterlistTelemetry::new(&MetricsBuilder::default());
        let filter = DogStatsDPrefixFilter {
            metric_prefix: "".to_string(),
            metric_prefix_blocklist: vec![],
            matcher: Blocklist::new(["foo", "bar"], true),
            effective_filterlist: EffectiveFilterlist::default(),
            telemetry,
            configuration: ScopedConfig::fixed(DogStatsDPrefixFilterConfig::default()),
        };

        let mut exact_metric = Metric::gauge("foo", 1.0);
        assert!(!filter.process_metric(&mut exact_metric));

        let mut prefix_metric = Metric::gauge("bar.baz", 1.0);
        assert!(!filter.process_metric(&mut prefix_metric));

        assert_eq!(recorder.counter(LISTENER_FILTERED_POINTS_METRIC), Some(2));
    }
}

#[cfg(test)]
mod config_smoke {
    use datadog_agent_config::{DatadogRemapper, KEY_ALIASES};
    use datadog_agent_config_testing::config_registry::structs;
    use datadog_agent_config_testing::run_config_smoke_tests;
    use serde_json::json;

    #[tokio::test]
    #[ignore = "legacy raw config smoke test no longer matches the native config path"]
    async fn smoke_test() {
        run_config_smoke_tests(
            structs::DOGSTATSD_PREFIX_FILTER_CONFIGURATION,
            &[],
            json!({}),
            |cfg| cfg.as_typed::<serde_json::Value>().expect("config should deserialize"),
            KEY_ALIASES,
            DatadogRemapper::new,
        )
        .await
    }
}
