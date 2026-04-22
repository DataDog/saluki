use std::ops::Deref;

use async_trait::async_trait;
use facet::Facet;
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder};
use metrics::{Counter, Gauge};
use saluki_config::GenericConfiguration;
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
use serde::Deserialize;
use tokio::select;
use tracing::{debug, error};

const METRIC_FILTERLIST_SIZE_METRIC: &str = "metric_filterlist_size";
const METRIC_FILTERLIST_UPDATES_METRIC: &str = "metric_filterlist_updates_total";
const LISTENER_FILTERED_POINTS_METRIC: &str = "dogstatsd_listener_filtered_points_total";

/// DogStatsD prefix filter transform.
///
/// Appends a prefix to every metric if specified.
///
/// Checks if a metric name should be allowed.
#[derive(Deserialize, Facet)]
pub struct DogStatsDPrefixFilterConfiguration {
    #[serde(default, rename = "statsd_metric_namespace")]
    metric_prefix: String,

    #[serde(
        default = "default_metric_prefix_blocklist",
        rename = "statsd_metric_namespace_blocklist"
    )]
    metric_prefix_blocklist: Vec<String>,

    #[serde(default)]
    metric_filterlist: Vec<String>,

    #[serde(default)]
    metric_filterlist_match_prefix: bool,

    #[serde(default, rename = "statsd_metric_blocklist")]
    metric_blocklist: Vec<String>,

    #[serde(default, rename = "statsd_metric_blocklist_match_prefix")]
    metric_blocklist_match_prefix: bool,

    #[serde(skip)]
    #[facet(opaque)]
    configuration: Option<GenericConfiguration>,
}

fn default_metric_prefix_blocklist() -> Vec<String> {
    vec![
        "datadog.agent".to_string(),
        "datadog.dogstatsd".to_string(),
        "datadog.process".to_string(),
        "datadog.trace_agent".to_string(),
        "datadog.tracer".to_string(),
        "activemq".to_string(),
        "activemq_58".to_string(),
        "airflow".to_string(),
        "cassandra".to_string(),
        "confluent".to_string(),
        "hazelcast".to_string(),
        "hive".to_string(),
        "ignite".to_string(),
        "jboss".to_string(),
        "jvm".to_string(),
        "kafka".to_string(),
        "presto".to_string(),
        "sidekiq".to_string(),
        "solr".to_string(),
        "tomcat".to_string(),
        "runtime".to_string(),
    ]
}

impl DogStatsDPrefixFilterConfiguration {
    /// Creates a new `DogStatsDPrefixFilterConfiguration` from the given configuration.
    pub fn from_configuration(config: &GenericConfiguration) -> Result<Self, GenericError> {
        let mut typed_config: DogStatsDPrefixFilterConfiguration = config.as_typed()?;
        typed_config.configuration = Some(config.clone());
        Ok(typed_config)
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
        let mut metric_prefix = self.metric_prefix.clone();
        if !metric_prefix.is_empty() && !metric_prefix.ends_with(".") {
            metric_prefix.push('.');
        }
        let metrics_builder = MetricsBuilder::from_component_context(&context);
        let effective_filterlist = EffectiveFilterlist::from_configuration(self);
        let telemetry = FilterlistTelemetry::new(&metrics_builder);
        let mut filter = DogStatsDPrefixFilter {
            metric_prefix,
            metric_prefix_blocklist: self.metric_prefix_blocklist.clone(),
            blocklist: Blocklist::default(),
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

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct Blocklist {
    data: Vec<String>,
    match_prefix: bool,
}

impl Blocklist {
    fn new(data: &[String], match_prefix: bool) -> Self {
        let mut data = data.to_owned();
        data.sort();

        // Only keep values with unique prefixes.
        if match_prefix && !data.is_empty() {
            let mut i = 0;
            for j in 1..data.len() {
                if !data[j].starts_with(&data[i]) {
                    i += 1;
                    data[i] = data[j].clone(); // Move item to next position
                }
            }
            data.truncate(i + 1);
        }

        Blocklist { data, match_prefix }
    }

    fn contains(&self, name: &str) -> bool {
        if self.data.is_empty() {
            return false;
        }

        let i = self.data.binary_search_by(|k| k.as_str().cmp(name));

        // Check for prefix match when match_prefix is true
        if self.match_prefix {
            let index = i.unwrap_or_else(|idx| idx);
            if index > 0 && name.starts_with(&self.data[index - 1]) {
                return true;
            }
        }

        if let Ok(index) = i {
            return name == self.data[index];
        }

        false
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
            filterlist_size: builder.register_debug_gauge(METRIC_FILTERLIST_SIZE_METRIC),
            filterlist_updates: builder.register_debug_counter(METRIC_FILTERLIST_UPDATES_METRIC),
            listener_filtered_points: builder.register_debug_counter(LISTENER_FILTERED_POINTS_METRIC),
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

#[derive(Clone, Debug, Default)]
struct EffectiveFilterlist {
    metric_filterlist: Vec<String>,
    metric_filterlist_match_prefix: bool,
    metric_blocklist: Vec<String>,
    metric_blocklist_match_prefix: bool,
}

impl EffectiveFilterlist {
    fn from_configuration(config: &DogStatsDPrefixFilterConfiguration) -> Self {
        Self {
            metric_filterlist: config.metric_filterlist.clone(),
            metric_filterlist_match_prefix: config.metric_filterlist_match_prefix,
            metric_blocklist: config.metric_blocklist.clone(),
            metric_blocklist_match_prefix: config.metric_blocklist_match_prefix,
        }
    }

    // Matches Agent precedence in
    // https://github.com/DataDog/datadog-agent/blob/main/comp/filterlist/impl/filterlist.go:
    // prefer `metric_filterlist` when configured, otherwise fall back to legacy `statsd_metric_blocklist`.
    fn effective_values(&self) -> (&[String], bool) {
        if !self.metric_filterlist.is_empty() {
            (&self.metric_filterlist, self.metric_filterlist_match_prefix)
        } else {
            (&self.metric_blocklist, self.metric_blocklist_match_prefix)
        }
    }

    fn effective_len(&self) -> usize {
        self.effective_values().0.len()
    }

    fn to_blocklist(&self) -> Blocklist {
        let (values, match_prefix) = self.effective_values();
        Blocklist::new(values, match_prefix)
    }
}

pub struct DogStatsDPrefixFilter {
    metric_prefix: String,
    metric_prefix_blocklist: Vec<String>,
    blocklist: Blocklist,
    effective_filterlist: EffectiveFilterlist,
    telemetry: FilterlistTelemetry,
    configuration: Option<GenericConfiguration>,
}

impl DogStatsDPrefixFilter {
    fn sync_effective_blocklist(&mut self, count_update: bool) {
        self.blocklist = self.effective_filterlist.to_blocklist();
        self.telemetry
            .set_filterlist_size(self.effective_filterlist.effective_len());
        if count_update {
            self.telemetry.increment_filterlist_updates();
        }
    }

    fn update_metric_filterlist(&mut self, metric_filterlist: Vec<String>) {
        let count_update = !self.effective_filterlist.metric_filterlist.is_empty() || !metric_filterlist.is_empty();
        self.effective_filterlist.metric_filterlist = metric_filterlist;
        self.sync_effective_blocklist(count_update);
    }

    fn update_metric_blocklist(&mut self, metric_blocklist: Vec<String>) {
        let count_update = self.effective_filterlist.metric_filterlist.is_empty();
        self.effective_filterlist.metric_blocklist = metric_blocklist;
        self.sync_effective_blocklist(count_update);
    }

    fn update_metric_filterlist_match_prefix(&mut self, match_prefix: bool) {
        let count_update = !self.effective_filterlist.metric_filterlist.is_empty();
        self.effective_filterlist.metric_filterlist_match_prefix = match_prefix;
        self.sync_effective_blocklist(count_update);
    }

    fn update_metric_blocklist_match_prefix(&mut self, match_prefix: bool) {
        let count_update = self.effective_filterlist.metric_filterlist.is_empty();
        self.effective_filterlist.metric_blocklist_match_prefix = match_prefix;
        self.sync_effective_blocklist(count_update);
    }

    fn process_metric(&self, metric: &mut Metric) -> bool {
        let metric_name = metric.context().name().deref();

        if self.metric_prefix.is_empty() {
            if self.blocklist.contains(metric_name) {
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

            if self.blocklist.contains(&new_metric_name) {
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

        let config = self.configuration.as_ref().unwrap();
        let mut filterlist_watcher = config.watch_for_updates("metric_filterlist");
        let mut filterlist_match_prefix_watcher = config.watch_for_updates("metric_filterlist_match_prefix");
        let mut blocklist_watcher = config.watch_for_updates("statsd_metric_blocklist");
        let mut blocklist_match_prefix_watcher = config.watch_for_updates("statsd_metric_blocklist_match_prefix");

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
                (_, maybe_new_metric_filterlist) = filterlist_watcher.changed::<Vec<String>>() => {
                    if let Some(new_filterlist) = maybe_new_metric_filterlist {
                        debug!(?new_filterlist, "Updated metric filterlist.");
                        self.update_metric_filterlist(new_filterlist);
                    }
                },
                (_, maybe_new_filterlist_match_prefix) = filterlist_match_prefix_watcher.changed::<bool>() => {
                    if let Some(new_match_prefix) = maybe_new_filterlist_match_prefix {
                        debug!(match_prefix = new_match_prefix, "Updated metric filterlist match prefix.");
                        self.update_metric_filterlist_match_prefix(new_match_prefix);
                    }
                },
                (_, maybe_new_blocklist) = blocklist_watcher.changed::<Vec<String>>() => {
                    if let Some(new_blocklist) = maybe_new_blocklist {
                        debug!(?new_blocklist, "Updated metric blocklist.");
                        self.update_metric_blocklist(new_blocklist);
                    }
                },
                (_, maybe_new_blocklist_match_prefix) = blocklist_match_prefix_watcher.changed::<bool>() => {
                    if let Some(new_match_prefix) = maybe_new_blocklist_match_prefix {
                        debug!(match_prefix = new_match_prefix, "Updated metric blocklist match prefix.");
                        self.update_metric_blocklist_match_prefix(new_match_prefix);
                    }
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
    use saluki_config::{dynamic::ConfigUpdate, ConfigurationLoader};
    use saluki_metrics::{test::TestRecorder, MetricsBuilder};

    use super::*;

    #[test]
    fn test_metric_prefix_add() {
        let filter = DogStatsDPrefixFilter {
            metric_prefix: "foo.".to_string(),
            metric_prefix_blocklist: vec![],
            blocklist: Blocklist::default(),
            effective_filterlist: EffectiveFilterlist::default(),
            telemetry: FilterlistTelemetry::noop(),
            configuration: None,
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
            blocklist: Blocklist::default(),
            effective_filterlist: EffectiveFilterlist::default(),
            telemetry: FilterlistTelemetry::noop(),
            configuration: None,
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
            blocklist: Blocklist::new(&["foobar".to_string(), "test".to_string()], false),
            effective_filterlist: EffectiveFilterlist::default(),
            telemetry: FilterlistTelemetry::noop(),
            configuration: None,
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
            blocklist: Blocklist::new(&["foo.bar".to_string(), "test".to_string()], false),
            effective_filterlist: EffectiveFilterlist::default(),
            telemetry: FilterlistTelemetry::noop(),
            configuration: None,
        };

        let mut metric = Metric::gauge("bar", 1.0);
        assert!(!filter.process_metric(&mut metric));

        let filter = DogStatsDPrefixFilter {
            metric_prefix: "foo.".to_string(),
            metric_prefix_blocklist: vec!["foo".to_string()],
            blocklist: Blocklist::default(),
            effective_filterlist: EffectiveFilterlist::default(),
            telemetry: FilterlistTelemetry::noop(),
            configuration: None,
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
            blocklist: Blocklist::new(&["b".to_string(), "test".to_string()], true),
            effective_filterlist: EffectiveFilterlist::default(),
            telemetry: FilterlistTelemetry::noop(),
            configuration: None,
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
            blocklist: Blocklist::new(&["fo".to_string(), "test".to_string()], true),
            effective_filterlist: EffectiveFilterlist::default(),
            telemetry: FilterlistTelemetry::noop(),
            configuration: None,
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
            blocklist: Blocklist::new(&["foobar".to_string(), "test".to_string()], false),
            effective_filterlist: EffectiveFilterlist {
                metric_blocklist: vec!["foobar".to_string(), "test".to_string()],
                ..EffectiveFilterlist::default()
            },
            telemetry: FilterlistTelemetry::noop(),
            configuration: Some(cfg.clone()),
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
            blocklist: Blocklist::new(&["foo".to_string()], false),
            effective_filterlist: EffectiveFilterlist {
                metric_filterlist: vec!["foo".to_string()],
                metric_filterlist_match_prefix: false,
                ..EffectiveFilterlist::default()
            },
            telemetry: FilterlistTelemetry::noop(),
            configuration: Some(cfg.clone()),
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
        assert_eq!(filter.blocklist, Blocklist::new(&["foo".to_string()], true));
    }

    #[test]
    fn telemetry_only_counts_active_filterlist_updates() {
        let recorder = TestRecorder::default();
        let _local = set_default_local_recorder(&recorder);

        let telemetry = FilterlistTelemetry::new(&MetricsBuilder::default());
        let mut filter = DogStatsDPrefixFilter {
            metric_prefix: "".to_string(),
            metric_prefix_blocklist: vec![],
            blocklist: Blocklist::default(),
            effective_filterlist: EffectiveFilterlist {
                metric_filterlist: vec!["preferred".to_string()],
                metric_filterlist_match_prefix: false,
                metric_blocklist: vec!["legacy".to_string()],
                metric_blocklist_match_prefix: false,
            },
            telemetry,
            configuration: None,
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
            blocklist: Blocklist::default(),
            effective_filterlist: EffectiveFilterlist {
                metric_filterlist: vec!["foo".to_string()],
                metric_filterlist_match_prefix: true,
                metric_blocklist: vec![],
                metric_blocklist_match_prefix: false,
            },
            telemetry,
            configuration: None,
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
            blocklist: Blocklist::new(&["foo".to_string(), "bar".to_string()], true),
            effective_filterlist: EffectiveFilterlist::default(),
            telemetry,
            configuration: None,
        };

        let mut exact_metric = Metric::gauge("foo", 1.0);
        assert!(!filter.process_metric(&mut exact_metric));

        let mut prefix_metric = Metric::gauge("bar.baz", 1.0);
        assert!(!filter.process_metric(&mut prefix_metric));

        assert_eq!(recorder.counter(LISTENER_FILTERED_POINTS_METRIC), Some(2));
    }
}
