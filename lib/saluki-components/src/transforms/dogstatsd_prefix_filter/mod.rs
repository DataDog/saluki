use std::ops::Deref;

use async_trait::async_trait;
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_config::GenericConfiguration;
use saluki_core::{
    components::{
        transforms::{Transform, TransformBuilder, TransformContext},
        ComponentContext,
    },
    topology::OutputDefinition,
};
use saluki_error::GenericError;
use saluki_event::{metric::Metric, DataType};
use serde::Deserialize;
use stringtheory::MetaString;
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
        default = "default_metric_prefix_blacklist",
        rename = "statsd_metric_namespace_blacklist"
    )]
    metric_prefix_blacklist: Vec<String>,

    #[serde(default, rename = "statsd_metric_blocklist")]
    metric_blocklist: Vec<String>,

    #[serde(default, rename = "statsd_metric_blocklist_match_prefix")]
    metric_blocklist_match_prefix: bool,
}

fn default_metric_prefix_blacklist() -> Vec<String> {
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
        Ok(config.as_typed()?)
    }
}

#[async_trait]
impl TransformBuilder for DogstatsDPrefixFilterConfiguration {
    fn input_data_type(&self) -> DataType {
        DataType::Metric
    }

    fn outputs(&self) -> &[OutputDefinition] {
        static OUTPUTS: &[OutputDefinition] = &[OutputDefinition::default_output(DataType::Metric)];
        OUTPUTS
    }

    async fn build(&self, _: ComponentContext) -> Result<Box<dyn Transform + Send>, GenericError> {
        Ok(Box::new(DogstatsDPrefixFilter {
            metric_prefix: self.metric_prefix.clone(),
            metric_prefix_blacklist: self.metric_prefix_blacklist.clone(),
            blocklist: Blocklist::new(&self.metric_blocklist, self.metric_blocklist_match_prefix),
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

    metric_prefix_blacklist: Vec<String>,

    blocklist: Blocklist,
}

impl DogstatsDPrefixFilter {
    fn enrich_metric(&self, mut metric: Metric) -> Option<Metric> {
        let metric_name = metric.context().name().deref();

        if self.metric_prefix.is_empty() {
            for s in &self.blocklist.data {
                if s == metric_name || self.blocklist.match_prefix && metric_name.starts_with(s) {
                    debug!("Metric {} excluded due to blocklist.", metric_name);
                    return None;
                }
            }
        } else {
            // Enrich metric with prefix if prefix is allowed.
            let new_metric_name = if self.is_excluded(metric_name) {
                metric.context().name().clone()
            } else {
                self.prefixed_metric(metric_name)
            };

            if self.blocklist.contains(&new_metric_name) {
                debug!("Metric {} excluded due to blocklist.", new_metric_name);
                return None;
            }

            // Update metric with new name.
            let new_context = metric.context().with_name(new_metric_name);
            let existing_context = metric.context_mut();
            *existing_context = new_context;
        }
        Some(metric)
    }

    fn is_excluded(&self, metric_name: &str) -> bool {
        !self.metric_prefix.is_empty()
            && self
                .metric_prefix_blacklist
                .iter()
                .any(|prefix| metric_name.starts_with(prefix))
    }

    fn prefixed_metric(&self, metric_name: &str) -> MetaString {
        if self.metric_prefix.ends_with(".") {
            MetaString::from(format!("{}{}", self.metric_prefix, metric_name))
        } else {
            MetaString::from(format!("{}.{}", self.metric_prefix, metric_name))
        }
    }
}

#[async_trait]
impl Transform for DogstatsDPrefixFilter {
    async fn run(mut self: Box<Self>, mut context: TransformContext) -> Result<(), GenericError> {
        let mut health = context.take_health_handle();
        health.mark_ready();

        debug!("DogStatsD Prefix Filter transform started.");

        loop {
            select! {
                _ = health.live() => continue,
                maybe_events = context.event_stream().next() => match maybe_events {
                    Some(events) => {
                        let mut buffered_forwarder = context.forwarder().buffered().expect("default output must always exist");

                        for event in events {
                            if let Some(metric) = event.try_into_metric() {
                                if let Some(new_metric) = self.enrich_metric(metric) {
                                    if let Err(e) = buffered_forwarder.push(saluki_event::Event::Metric(new_metric)).await {
                                        error!(error = %e, "Failed to forward event.");
                                    }
                                }
                            }
                        }

                        if let Err(e) = buffered_forwarder.flush().await {
                            error!(error = %e, "Failed to forward events.");
                        }

                    },
                    None => break,
                },
            }
        }

        debug!("DogStatsD Prefix Filter transform stopped.");

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use saluki_context::Context;

    use super::*;

    #[test]
    fn test_metric_prefix_add() {
        let filter = DogstatsDPrefixFilter {
            metric_prefix: "foo".to_string(),
            metric_prefix_blacklist: vec![],
            blocklist: Blocklist::default(),
        };
        let context = Context::from_static_parts("bar", &[]);
        let metric = Metric::gauge(context, 1.0);
        let new_metric = filter.enrich_metric(metric).unwrap();

        assert_eq!(new_metric.context().name(), "foo.bar");
    }

    #[test]
    fn test_metric_prefix_blacklist() {
        let filter = DogstatsDPrefixFilter {
            metric_prefix: "foo".to_string(),
            metric_prefix_blacklist: vec!["foo".to_string(), "bar".to_string()],
            blocklist: Blocklist::default(),
        };
        let context = Context::from_static_parts("barbar", &[]);
        let metric = Metric::gauge(context, 1.0);
        let new_metric = filter.enrich_metric(metric).unwrap();
        assert_eq!(new_metric.context().name(), "barbar");
    }

    #[test]
    fn test_metric_blocklist() {
        let filter = DogstatsDPrefixFilter {
            metric_prefix: "".to_string(),
            metric_prefix_blacklist: vec![],
            blocklist: Blocklist::new(&["foobar".to_string(), "test".to_string()], false),
        };
        let context = Context::from_static_parts("foobar", &[]);
        let metric = Metric::gauge(context, 1.0);
        let new_metric = filter.enrich_metric(metric);
        assert!(new_metric.is_none());

        let context = Context::from_static_parts("foo", &[]);
        let metric = Metric::gauge(context, 1.0);
        let new_metric = filter.enrich_metric(metric).unwrap();
        assert_eq!(new_metric.context().name(), "foo");
    }

    #[test]
    fn test_metric_blocklist_with_metric_prefix() {
        let filter = DogstatsDPrefixFilter {
            metric_prefix: "foo".to_string(),
            metric_prefix_blacklist: vec![],
            blocklist: Blocklist::new(&["foo.bar".to_string(), "test".to_string()], false),
        };
        let context = Context::from_static_parts("bar", &[]);
        let metric = Metric::gauge(context, 1.0);
        let new_metric = filter.enrich_metric(metric);
        assert!(new_metric.is_none());

        let filter = DogstatsDPrefixFilter {
            metric_prefix: "foo".to_string(),
            metric_prefix_blacklist: vec!["foo".to_string()],
            blocklist: Blocklist::default(),
        };
        let context = Context::from_static_parts("foo", &[]);
        let metric = Metric::gauge(context, 1.0);
        let new_metric = filter.enrich_metric(metric).unwrap();
        assert_eq!(new_metric.context().name(), "foo");
    }

    #[test]
    fn test_metric_match_prefix_without_added_prefix() {
        let filter = DogstatsDPrefixFilter {
            metric_prefix: "".to_string(),
            metric_prefix_blacklist: vec![],
            blocklist: Blocklist::new(&["b".to_string(), "test".to_string()], true),
        };
        let context = Context::from_static_parts("bar", &[]);
        let metric = Metric::gauge(context, 1.0);
        let new_metric = filter.enrich_metric(metric);
        // match prefix is true, "bar" has prefix "b"
        assert!(new_metric.is_none());

        let context = Context::from_static_parts("test", &[]);
        let metric = Metric::gauge(context, 1.0);
        let new_metric = filter.enrich_metric(metric);
        // match prefix is true, "test" has prefix "test"
        assert!(new_metric.is_none());
    }

    #[test]
    fn test_metric_match_prefix_with_added_prefix() {
        let filter = DogstatsDPrefixFilter {
            metric_prefix: "foo".to_string(),
            metric_prefix_blacklist: vec![],
            blocklist: Blocklist::new(&["fo".to_string(), "test".to_string()], true),
        };
        let context = Context::from_static_parts("bar", &[]);
        let metric = Metric::gauge(context, 1.0);
        let new_metric = filter.enrich_metric(metric);
        // new_metric is "foo.bar", match prefix is true, "foo.bar" has prefix "fo"
        assert!(new_metric.is_none());
    }
}
