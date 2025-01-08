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
use tokio::select;
use tracing::{debug, error};

#[derive(Deserialize)]
/// Name Transform.
pub struct NameFilterConfiguration {
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

impl NameFilterConfiguration {
    /// Creates a new `NameFilterConfiguration` from the given configuration.
    pub fn from_configuration(config: &GenericConfiguration) -> Result<Self, GenericError> {
        Ok(config.as_typed()?)
    }
}

#[async_trait]
impl TransformBuilder for NameFilterConfiguration {
    fn input_data_type(&self) -> DataType {
        DataType::Metric
    }

    fn outputs(&self) -> &[OutputDefinition] {
        static OUTPUTS: &[OutputDefinition] = &[OutputDefinition::default_output(DataType::Metric)];
        OUTPUTS
    }

    async fn build(&self, _: ComponentContext) -> Result<Box<dyn Transform + Send>, GenericError> {
        Ok(Box::new(NameFilter {
            metric_prefix: self.metric_prefix.clone(),
            metric_prefix_blacklist: self.metric_prefix_blacklist.clone(),
            blocklist: Blocklist::new(&self.metric_blocklist, self.metric_blocklist_match_prefix),
        }))
    }
}

impl MemoryBounds for NameFilterConfiguration {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        // Capture the size of the heap allocation when the component is built.
        builder.minimum().with_single_value::<NameFilter>();
    }
}

#[derive(Debug)]
struct Blocklist {
    data: Vec<String>,
    match_prefix: bool,
}

impl Blocklist {
    fn new(data: &[String], match_prefix: bool) -> Self {
        let mut data = data.to_owned();
        data.sort();

        if match_prefix && !data.is_empty() {
            let mut i = 0;
            for j in 1..data.len() {
                if !data[j].starts_with(&data[i]) {
                    i += 1;
                    data[i] = data[j].clone(); // Move item to next position
                }
            }
            data.truncate(i + 1); // Only keep valid elements
        }

        Blocklist { data, match_prefix }
    }

    fn test(&self, name: String) -> bool {
        if self.data.is_empty() {
            return false;
        }

        let i = self.data.binary_search(&name);

        // Check for prefix match when match_prefix is true
        if self.match_prefix {
            if let Ok(index) = i {
                if index > 0 && name.starts_with(&self.data[index - 1]) {
                    return true;
                }
            } else if i.is_err() {
                let index = i.unwrap_err();
                if index > 0 && name.starts_with(&self.data[index - 1]) {
                    return true;
                }
            }
        }

        if let Ok(index) = i {
            return name == self.data[index];
        }

        false
    }
}

#[derive(Debug)]
pub struct NameFilter {
    metric_prefix: String,

    metric_prefix_blacklist: Vec<String>,

    blocklist: Blocklist,
}

impl NameFilter {
    fn enrich_metric(&self, mut metric: Metric) -> Option<Metric> {
        let metric_name = metric.context().name().deref();
        let mut new_metric_name = metric_name.to_string();

        if !self.is_excluded(metric_name) {
            new_metric_name = format!("{}{}", self.metric_prefix, metric_name);
        }

        if self.blocklist.test(new_metric_name.clone()) {
            debug!("Metric {} excluded due to blocklist.", new_metric_name);
            return None;
        }

        // Update metric with new name.
        let new_context = metric.context().with_name(new_metric_name);
        let existing_context = metric.context_mut();
        *existing_context = new_context;

        Some(metric)
    }

    fn is_excluded(&self, metric_name: &str) -> bool {
        !self.metric_prefix.is_empty()
            && self
                .metric_prefix_blacklist
                .iter()
                .any(|prefix| metric_name.starts_with(prefix))
    }
}

#[async_trait]
impl Transform for NameFilter {
    async fn run(mut self: Box<Self>, mut context: TransformContext) -> Result<(), GenericError> {
        let mut health = context.take_health_handle();
        health.mark_ready();

        debug!("Agent metric name filter transform started.");

        loop {
            select! {
                _ = health.live() => continue,
                maybe_events = context.event_stream().next() => match maybe_events {
                    Some(events) => {
                        let mut buffered_forwarder = context.forwarder().buffered().expect("default output must always exist");

                        for event in events {
                            if let Some(metric) = event.try_into_metric() {
                                if let Some(metric) = self.enrich_metric(metric) {

                                let new_event = Metric::from_parts(
                                    metric.context().clone(),
                                    metric.values().clone(),
                                    metric.metadata().clone(),
                                );

                                if let Err(e) = buffered_forwarder.push(saluki_event::Event::Metric(new_event)).await {
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

        debug!("Agent metric name transform stopped.");

        Ok(())
    }
}
