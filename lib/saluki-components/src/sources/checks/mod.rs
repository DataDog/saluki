use async_trait::async_trait;
use async_walkdir::{DirEntry, Filtering, WalkDir};
use futures::StreamExt as _;
use saluki_config::GenericConfiguration;
use saluki_core::{
    components::{Source, SourceBuilder, SourceContext},
    topology::{
        shutdown::{ComponentShutdownHandle, DynamicShutdownCoordinator, DynamicShutdownHandle},
        OutputDefinition,
    },
};
use saluki_env::time::get_unix_timestamp;
use saluki_error::GenericError;
use saluki_event::{metric::*, DataType, Event};
use serde::Deserialize;
use snafu::Snafu;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};
use std::{collections::HashSet, io};
use std::{fmt::Display, time::Duration};
use tokio::{select, sync::mpsc};
use tracing::{debug, error, info, warn};

mod listener;
mod python_exposed_modules;
mod scheduler;

#[derive(Debug, Snafu)]
#[snafu(context(suffix(false)))]
enum Error {
    #[snafu(display("Directory incorrect"))]
    DirectoryIncorrect {
        source: io::Error,
    },
    CantReadConfiguration {
        source: serde_yaml::Error,
    },
    NoSourceAvailable {
        reason: String,
    },
    Python {
        reason: String,
    },
}
/// Checks source.
///
/// Scans a directory for check configurations and emits them as things to run.
#[derive(Deserialize)]
pub struct ChecksConfiguration {
    /// The directory containing the check configurations.
    #[serde(default = "default_check_config_dir")]
    check_config_dir: String,
}

fn default_check_config_dir() -> String {
    "./dist/conf.d".to_string()
}

#[derive(Debug, Clone)]
struct CheckRequest {
    name: Option<String>,
    instances: Vec<CheckInstanceConfiguration>,
    init_config: Option<serde_yaml::Value>,
    source: CheckSource,
}

impl Display for CheckRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Source: {} # Instances: {}", self.source, self.instances.len(),)?;
        if let Some(name) = &self.name {
            write!(f, " Name: {}", name)?
        }

        Ok(())
    }
}

impl CheckRequest {
    fn to_runnable_request(&self) -> Result<RunnableCheckRequest, Error> {
        // The concept of a RunnableRequest needs to be expanded
        // to look for modules available in the py runtime
        // This implies that this step needs to happen later in the process
        // Logic to emulate:
        // Idea, the RunnableCheckRequest can have an "expected_module_name" field
        // or something like that
        // That does imply that the `run` of a `runnablecheckrequest` can fail, but that is already the case
        // https://github.com/DataDog/datadog-agent/blob/e8de27352093e0d5f828cf86988d186a3501b525/pkg/collector/python/loader.go#L110-L112
        let name = if let Some(name) = &self.name {
            name.clone()
        } else {
            self.source.to_check_name()?
        };
        let check_source_code: Option<PathBuf> = match &self.source {
            CheckSource::Yaml(path) => find_sibling_py_file(path),
        };
        Ok(RunnableCheckRequest {
            check_request: self.clone(),
            check_name: name,
            check_source_code,
        })
    }
}

struct RunnableCheckRequest {
    check_request: CheckRequest,
    check_name: String,
    check_source_code: Option<PathBuf>,
}

impl Display for RunnableCheckRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Request: {} CheckSource: {}", self.check_request, self.check_name)
    }
}

#[derive(Debug, Deserialize)]
struct YamlCheckConfiguration {
    name: Option<String>,
    init_config: Option<serde_yaml::Value>,
    instances: Vec<YamlCheckInstance>,
}

#[derive(Debug, Deserialize)]
struct YamlCheckInstance {
    #[serde(default = "default_min_collection_interval_ms")]
    min_collection_interval: u32,
    // todo support arbitrary other fields
}

// checks configuration
#[derive(Debug, Clone, Eq, Hash, PartialEq)]
struct CheckInstanceConfiguration {
    min_collection_interval_ms: u32,
}

fn default_min_collection_interval_ms() -> u32 {
    15_000
}

#[derive(Debug, Clone, Eq, Hash, PartialEq)]
enum CheckSource {
    Yaml(PathBuf),
}

impl CheckSource {
    fn to_check_name(&self) -> Result<String, Error> {
        match self {
            CheckSource::Yaml(path) => {
                let mut path = path.clone();
                path.set_extension(""); // trim off the file extension
                let filename = path.file_name().expect("No error");
                let name = filename.to_string_lossy().to_string();
                Ok(name)
            }
        }
    }
    fn to_check_request(&self) -> Result<CheckRequest, Error> {
        match self {
            CheckSource::Yaml(path) => {
                let file = std::fs::File::open(path).expect("No error");
                let mut checks_config = Vec::new();
                let read_yaml: YamlCheckConfiguration = match serde_yaml::from_reader(file) {
                    Ok(read) => read,
                    Err(e) => {
                        debug!("can't read configuration at {}: {}", path.display(), e);
                        return Err(Error::CantReadConfiguration { source: e });
                    }
                };

                for instance in read_yaml.instances.into_iter() {
                    checks_config.push(CheckInstanceConfiguration {
                        min_collection_interval_ms: instance.min_collection_interval,
                    });
                }

                Ok(CheckRequest {
                    name: read_yaml.name,
                    instances: checks_config,
                    init_config: read_yaml.init_config,
                    source: self.clone(),
                })
            }
        }
    }
}

impl Display for CheckSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CheckSource::Yaml(path) => write!(f, "{}", path.display()),
        }
    }
}

impl ChecksConfiguration {
    /// Creates a new `ChecksConfiguration` from the given configuration.
    pub fn from_configuration(config: &GenericConfiguration) -> Result<Self, GenericError> {
        Ok(config.as_typed()?)
    }

    async fn build_listeners(&self) -> Result<Vec<listener::DirCheckRequestListener>, Error> {
        let mut listeners = Vec::new();

        let listener = listener::DirCheckRequestListener::from_path(&self.check_config_dir)?;

        listeners.push(listener);

        Ok(listeners)
    }
}

#[async_trait]
impl SourceBuilder for ChecksConfiguration {
    async fn build(&self) -> Result<Box<dyn Source + Send>, GenericError> {
        let listeners = self.build_listeners().await?;

        Ok(Box::new(Checks { listeners }))
    }

    fn outputs(&self) -> &[OutputDefinition] {
        static OUTPUTS: &[OutputDefinition] = &[OutputDefinition::default_output(DataType::Metric)];

        OUTPUTS
    }
}

pub struct Checks {
    listeners: Vec<listener::DirCheckRequestListener>,
}

impl Checks {
    // consumes self
    async fn run_inner(self, context: SourceContext, global_shutdown: ComponentShutdownHandle) -> Result<(), ()> {
        let mut listener_shutdown_coordinator = DynamicShutdownCoordinator::default();

        let Checks { listeners } = self;

        let mut joinset = tokio::task::JoinSet::new();
        // Run the local task set.
        // For each listener, spawn a dedicated task to run it.
        for listener in listeners {
            let listener_context = listener::DirCheckListenerContext {
                shutdown_handle: listener_shutdown_coordinator.register(),
                listener,
            };

            joinset.spawn(process_listener(context.clone(), listener_context));
        }

        info!("Check source started.");

        select! {
            j = joinset.join_next() => {
                match j {
                    Some(Ok(_)) => {
                        debug!("Check source task set exited normally.");
                    }
                    Some(Err(e)) => {
                        error!("Check source task set exited unexpectedly: {:?}", e);
                    }
                    None => {
                        // set is empty, all good here
                    }
                }
            },
            _ = global_shutdown => {
                info!("Stopping Check source...");

                listener_shutdown_coordinator.shutdown().await;
            }
        }

        info!("Check source stopped.");

        Ok(())
    }
}

#[async_trait]
impl Source for Checks {
    async fn run(mut self: Box<Self>, mut context: SourceContext) -> Result<(), ()> {
        let global_shutdown = context
            .take_shutdown_handle()
            .expect("should never fail to take shutdown handle");
        match self.run_inner(context, global_shutdown).await {
            Ok(_) => Ok(()),
            Err(e) => {
                error!("Check source failed: {:?}", e);
                Err(())
            }
        }
    }
}

async fn process_listener(
    source_context: SourceContext, listener_context: listener::DirCheckListenerContext,
) -> Result<(), GenericError> {
    let listener::DirCheckListenerContext {
        shutdown_handle,
        mut listener,
    } = listener_context;
    tokio::pin!(shutdown_handle);

    let stream_shutdown_coordinator = DynamicShutdownCoordinator::default();

    // Note: This architecture has a single CheckScheduler per listener
    // which is likely not the design we want long term
    let (check_metrics_tx, mut check_metrics_rx) = mpsc::channel(10_000_000);
    let mut scheduler = scheduler::CheckScheduler::new(check_metrics_tx)?;

    info!("Check listener started.");
    let (mut new_entities, mut deleted_entities) = listener.subscribe();
    loop {
        select! {
            _ = &mut shutdown_handle => {
                debug!("Received shutdown signal. Waiting for existing stream handlers to finish...");
                break;
            }
            _ = tokio::time::sleep(tokio::time::Duration::from_secs(1)) => {
                match listener.update_check_entities().await {
                    Ok(_) => {}
                    Err(e) => {
                        error!("Error updating check entities: {}", e);
                    }
                }
            }
            Some(check_metric) = check_metrics_rx.recv() => {
                let mut event_buffer = source_context.event_buffer_pool().acquire().await;
                let event: Event = check_metric.try_into().expect("can't convert");
                event_buffer.push(event);
                if let Err(e) = source_context.forwarder().forward(event_buffer).await {
                    error!(error = %e, "Failed to forward check metrics.");
                }
            }
            Some(new_entity) = new_entities.recv() => {
                let check_request = match new_entity.to_runnable_request() {
                    Ok(check_request) => check_request,
                    Err(e) => {
                        error!("Can't convert check source to runnable request: {}", e);
                        continue;
                    }
                };

                info!("Running a check request: {check_request}");

                match scheduler.run_check(check_request) {
                    Ok(_) => {
                        debug!("Check request succeeded, instances have been queued");
                    }
                    Err(e) => {
                        error!("Error running check: {}", e);
                    }
                }
            }
            Some(deleted_entity) = deleted_entities.recv() => {
                scheduler.stop_check(deleted_entity);
            }
        }
    }

    stream_shutdown_coordinator.shutdown().await;

    info!("Check listener stopped.");

    Ok(())
}

/// Given a yaml config, find the corresponding python source code
/// Currently only looks in the same directory, no support for `checks.d` or `mycheck.d` directories
fn find_sibling_py_file(check_yaml_path: &Path) -> Option<PathBuf> {
    let mut check_rel_filepath = check_yaml_path.to_path_buf();
    check_rel_filepath.set_extension("py");
    if check_rel_filepath.exists() {
        return Some(check_rel_filepath);
    }

    None
}

#[derive(Clone, Copy)]
pub enum PyMetricType {
    Gauge = 0,
    Rate,
    Count,
    MonotonicCount,
    Counter,
    Histogram,
    Historate,
}

impl From<i32> for PyMetricType {
    fn from(v: i32) -> Self {
        match v {
            0 => PyMetricType::Gauge,
            1 => PyMetricType::Rate,
            2 => PyMetricType::Count,
            3 => PyMetricType::MonotonicCount,
            4 => PyMetricType::Counter,
            5 => PyMetricType::Histogram,
            6 => PyMetricType::Historate,
            _ => {
                warn!("Unknown metric type: {}, considering it as a gauge", v);
                PyMetricType::Gauge
            }
        }
    }
}

#[derive(Debug, Snafu)]
#[snafu(context(suffix(false)))]
pub enum AggregatorError {
    UnsupportedType {},
}

/// CheckMetric are used to transmit metrics from python check execution results
/// to forward in the saluki's pipeline.
pub struct CheckMetric {
    name: String,
    metric_type: PyMetricType,
    value: f64,
    tags: Vec<String>,
}

impl TryInto<Event> for CheckMetric {
    type Error = AggregatorError;

    fn try_into(self) -> Result<Event, Self::Error> {
        let tags: MetricTags = self.tags.into();

        let context = MetricContext { name: self.name, tags };
        let metadata = MetricMetadata::from_timestamp(get_unix_timestamp());

        match self.metric_type {
            PyMetricType::Gauge => Ok(saluki_event::Event::Metric(Metric::from_parts(
                context,
                MetricValue::Gauge { value: self.value },
                metadata,
            ))),
            PyMetricType::Counter => Ok(saluki_event::Event::Metric(Metric::from_parts(
                context,
                MetricValue::Counter { value: self.value },
                metadata,
            ))),
            // TODO(remy): rest of the types
            _ => Err(AggregatorError::UnsupportedType {}),
        }
    }
}

impl Clone for CheckMetric {
    fn clone(&self) -> Self {
        Self {
            name: self.name.clone(),
            metric_type: self.metric_type,
            value: self.value,
            tags: self.tags.clone(),
        }
    }
}
