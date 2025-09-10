use std::ops::Deref;

use async_trait::async_trait;
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_config::dynamic::ConfigChangeEvent;
use saluki_config::GenericConfiguration;
use saluki_core::data_model::event::{metric::Metric, EventType};
use saluki_core::{
    components::{
        transforms::{Transform, TransformBuilder, TransformContext},
        ComponentContext,
    },
    topology::OutputDefinition,
};
use saluki_error::GenericError;
use serde::Deserialize;
use tokio::select;
use tracing::{debug, error};

/// DogstatsD prefix filter transform.
///
/// Appends a prefix to every metric if specified.
///
/// Checks if a metric name should be allowed.
#[derive(Deserialize)]
pub struct DogstatsDPrefixFilterConfiguration {
    #[serde(default, rename = "statsd_metric_namespace")]
    metric_prefix: String,

    #[serde(
        default = "default_metric_prefix_blocklist",
        rename = "statsd_metric_namespace_blocklist"
    )]
    metric_prefix_blocklist: Vec<String>,

    #[serde(default, rename = "statsd_metric_blocklist")]
    metric_blocklist: Vec<String>,

    #[serde(default, rename = "statsd_metric_blocklist_match_prefix")]
    metric_blocklist_match_prefix: bool,

    #[serde(skip)]
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

impl DogstatsDPrefixFilterConfiguration {
    /// Creates a new `DogstatsdPrefixFilterConfiguration` from the given configuration.
    pub fn from_configuration(config: &GenericConfiguration) -> Result<Self, GenericError> {
        let mut typed_config: DogstatsDPrefixFilterConfiguration = config.as_typed()?;
        typed_config.configuration = Some(config.clone());
        Ok(typed_config)
    }
}

#[async_trait]
impl TransformBuilder for DogstatsDPrefixFilterConfiguration {
    fn input_event_type(&self) -> EventType {
        EventType::Metric
    }

    fn outputs(&self) -> &[OutputDefinition] {
        static OUTPUTS: &[OutputDefinition] = &[OutputDefinition::default_output(EventType::Metric)];
        OUTPUTS
    }

    async fn build(&self, _: ComponentContext) -> Result<Box<dyn Transform + Send>, GenericError> {
        // Ensure our metric prefix has a trailing period so that we don't have to check for, and possibly add it, when we're
        // actually processing metrics.
        let mut metric_prefix = self.metric_prefix.clone();
        if !metric_prefix.is_empty() && !metric_prefix.ends_with(".") {
            metric_prefix.push('.');
        }

        Ok(Box::new(DogstatsDPrefixFilter {
            metric_prefix,
            metric_prefix_blocklist: self.metric_prefix_blocklist.clone(),
            blocklist: Blocklist::new(&self.metric_blocklist, self.metric_blocklist_match_prefix),
            configuration: self.configuration.clone(),
        }))
    }
}

impl MemoryBounds for DogstatsDPrefixFilterConfiguration {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        // Capture the size of the heap allocation when the component is built.
        builder
            .minimum()
            .with_single_value::<DogstatsDPrefixFilter>("component struct");
    }
}

#[derive(Debug, Default)]
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

#[derive(Debug)]
pub struct DogstatsDPrefixFilter {
    metric_prefix: String,
    metric_prefix_blocklist: Vec<String>,
    blocklist: Blocklist,
    configuration: Option<GenericConfiguration>,
}

impl DogstatsDPrefixFilter {
    fn process_metric(&self, metric: &mut Metric) -> bool {
        let metric_name = metric.context().name().deref();

        if self.metric_prefix.is_empty() {
            for s in &self.blocklist.data {
                if s == metric_name || self.blocklist.match_prefix && metric_name.starts_with(s) {
                    debug!("Metric {} excluded due to blocklist.", metric_name);
                    return false;
                }
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
impl Transform for DogstatsDPrefixFilter {
    async fn run(mut self: Box<Self>, mut context: TransformContext) -> Result<(), GenericError> {
        let mut health = context.take_health_handle();
        health.mark_ready();

        let config = self.configuration.as_ref().unwrap();
        let mut config_updates = config.subscribe_for_updates();

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
                Ok(change) = async { config_updates.as_mut().unwrap().recv().await }, if config_updates.is_some() => {
                    let maybe_new_metric_blocklist = try_extract_blocklist_from_change(change);
                    if let Some(new_metric_blocklist) = maybe_new_metric_blocklist {
                        debug!(?new_metric_blocklist, "Updated metric blocklist.");
                        self.blocklist = Blocklist::new(&new_metric_blocklist, self.blocklist.match_prefix);
                    }
                },
            }
        }

        debug!("DogStatsD Prefix Filter transform stopped.");

        Ok(())
    }
}

fn try_extract_blocklist_from_change(change: ConfigChangeEvent) -> Option<Vec<String>> {
    match change {
        ConfigChangeEvent::Modified { key, new_value, .. } | ConfigChangeEvent::Added { key, value: new_value } => {
            if key == "statsd_metric_blocklist" {
                let new_metric_blocklist = value_to_blocklist_vec(&new_value);
                return Some(new_metric_blocklist);
            }
        }
    }
    None
}

fn value_to_blocklist_vec(value: &serde_json::Value) -> Vec<String> {
    value
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect::<Vec<String>>()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_metric_prefix_add() {
        let filter = DogstatsDPrefixFilter {
            metric_prefix: "foo.".to_string(),
            metric_prefix_blocklist: vec![],
            blocklist: Blocklist::default(),
            configuration: None,
        };

        let mut metric = Metric::gauge("bar", 1.0);
        assert!(filter.process_metric(&mut metric));
        assert_eq!(metric.context().name(), "foo.bar");
    }

    #[test]
    fn test_metric_prefix_blocklist() {
        let filter = DogstatsDPrefixFilter {
            metric_prefix: "foo".to_string(),
            metric_prefix_blocklist: vec!["foo".to_string(), "bar".to_string()],
            blocklist: Blocklist::default(),
            configuration: None,
        };

        let mut metric = Metric::gauge("barbar", 1.0);
        assert!(filter.process_metric(&mut metric));
        assert_eq!(metric.context().name(), "barbar");
    }

    #[test]
    fn test_metric_blocklist() {
        let filter = DogstatsDPrefixFilter {
            metric_prefix: "".to_string(),
            metric_prefix_blocklist: vec![],
            blocklist: Blocklist::new(&["foobar".to_string(), "test".to_string()], false),
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
        let filter = DogstatsDPrefixFilter {
            metric_prefix: "foo.".to_string(),
            metric_prefix_blocklist: vec![],
            blocklist: Blocklist::new(&["foo.bar".to_string(), "test".to_string()], false),
            configuration: None,
        };

        let mut metric = Metric::gauge("bar", 1.0);
        assert!(!filter.process_metric(&mut metric));

        let filter = DogstatsDPrefixFilter {
            metric_prefix: "foo.".to_string(),
            metric_prefix_blocklist: vec!["foo".to_string()],
            blocklist: Blocklist::default(),
            configuration: None,
        };

        let mut metric = Metric::gauge("foo", 1.0);
        assert!(filter.process_metric(&mut metric));
        assert_eq!(metric.context().name(), "foo");
    }

    #[test]
    fn test_metric_match_prefix_without_added_prefix() {
        let filter = DogstatsDPrefixFilter {
            metric_prefix: "".to_string(),
            metric_prefix_blocklist: vec![],
            blocklist: Blocklist::new(&["b".to_string(), "test".to_string()], true),
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
        let filter = DogstatsDPrefixFilter {
            metric_prefix: "foo".to_string(),
            metric_prefix_blocklist: vec![],
            blocklist: Blocklist::new(&["fo".to_string(), "test".to_string()], true),
            configuration: None,
        };

        // new_metric is "foo.bar", match prefix is true, "foo.bar" has prefix "fo"
        let mut metric = Metric::gauge("bar", 1.0);
        assert!(!filter.process_metric(&mut metric));
    }
}
