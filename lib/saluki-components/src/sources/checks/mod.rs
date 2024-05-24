use async_trait::async_trait;
use async_walkdir::{Filtering, WalkDir};
use futures::StreamExt as _;
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder};
use pyo3::prelude::*;
use saluki_config::GenericConfiguration;
use saluki_core::{
    components::{Source, SourceBuilder, SourceContext},
    topology::{
        shutdown::{ComponentShutdownHandle, DynamicShutdownCoordinator, DynamicShutdownHandle},
        OutputDefinition,
    },
};
use saluki_env::time::get_unix_timestamp;
use saluki_error::{generic_error, GenericError};
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
mod python_scheduler;

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
    // Name is _not_ just for display, the 'name' field identifies which check should be instantiated
    // and run to satisfy this check request.
    // In python checks, this is the module name for the check.
    // In core checks, TBD.
    name: String,
    instances: Vec<CheckInstanceConfiguration>,
    init_config: CheckInitConfiguration,
    source: CheckSource,
}

impl Display for CheckRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Name: {} Source: {} # Instances: {}",
            self.name,
            self.source,
            self.instances.len(),
        )?;

        Ok(())
    }
}

impl CheckRequest {
    fn to_runnable_request(&self) -> Result<RunnableCheckRequest, Error> {
        Ok(RunnableCheckRequest {
            check_request: self.clone(),
            check_source_code: None,
        })
    }

    // Ideally this matches
    // https://github.com/DataDog/datadog-agent/blob/b039ea43d3168f521e8ea3e8356a0e84eec170d1/comp/core/autodiscovery/integration/config.go#L362
    // but for now, lets just do this
    // I don't really need this, just adding where I think it should go
    fn _digest(&self) -> String {
        let mut digest = self.name.clone();
        let init_config_str = self.init_config.0.len().to_string();
        digest.push_str(&init_config_str);
        let instance_str = self.instances.len().to_string();
        digest.push_str(&instance_str);
        digest
    }
}

/// This exists mostly for unit tests at this point
/// its useful to be able to specify literal code to run
struct RunnableCheckRequest {
    check_request: CheckRequest,
    check_source_code: Option<String>,
}

impl Display for RunnableCheckRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Request: {}", self.check_request)
    }
}

impl From<CheckRequest> for RunnableCheckRequest {
    fn from(check_request: CheckRequest) -> Self {
        Self {
            check_request,
            check_source_code: None,
        }
    }
}

#[derive(Debug, Deserialize)]
struct YamlCheckConfiguration {
    name: Option<String>,
    init_config: Option<serde_yaml::Mapping>,
    instances: Vec<serde_yaml::Mapping>,
}

fn map_to_pydict<'py>(
    map: &HashMap<String, serde_yaml::Value>, p: &'py pyo3::Python,
) -> PyResult<Bound<'py, pyo3::types::PyDict>> {
    let dict = pyo3::types::PyDict::new_bound(*p);
    for (key, value) in map {
        let value = serde_value_to_pytype(value, p)?;
        dict.set_item(key, value)?;
    }
    Ok(dict)
}

#[derive(Debug, Clone, Default)]
struct CheckInitConfiguration(HashMap<String, serde_yaml::Value>);

impl CheckInitConfiguration {
    fn to_pydict<'py>(&self, p: &'py pyo3::Python) -> Bound<'py, pyo3::types::PyDict> {
        map_to_pydict(&self.0, p).expect("Could convert")
    }
}

#[derive(Debug, Clone)]
struct CheckInstanceConfiguration(HashMap<String, serde_yaml::Value>);

impl CheckInstanceConfiguration {
    fn min_collection_interval_ms(&self) -> u32 {
        self.0
            .get("min_collection_interval")
            .map(|v| v.as_i64().expect("min_collection_interval must be an integer") as u32)
            .unwrap_or_else(default_min_collection_interval_ms)
    }

    fn to_pydict<'py>(&self, p: &'py pyo3::Python) -> Bound<'py, pyo3::types::PyDict> {
        map_to_pydict(&self.0, p).expect("Could convert")
    }
}

fn serde_value_to_pytype<'py>(value: &serde_yaml::Value, p: &'py pyo3::Python) -> PyResult<Bound<'py, pyo3::PyAny>> {
    match value {
        serde_yaml::Value::String(s) => Ok(s.into_py(*p).into_bound(*p)),
        serde_yaml::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Ok(i.into_py(*p).into_bound(*p))
            } else if let Some(f) = n.as_f64() {
                Ok(f.into_py(*p).into_bound(*p))
            } else {
                unreachable!("Number is neither i64 nor f64")
            }
        }
        serde_yaml::Value::Bool(b) => Ok(b.into_py(*p).into_bound(*p)),
        serde_yaml::Value::Sequence(s) => {
            let list = pyo3::types::PyList::empty_bound(*p);
            for item in s {
                let item = serde_value_to_pytype(item, p)?;
                list.append(item)?;
            }
            Ok(list.into_any())
        }
        serde_yaml::Value::Mapping(m) => {
            let dict = pyo3::types::PyDict::new_bound(*p);
            for (key, value) in m {
                let value = serde_value_to_pytype(value, p)?;
                let key = serde_value_to_pytype(key, p)?;
                dict.set_item(key, value)?;
            }
            Ok(dict.into_any())
        }
        serde_yaml::Value::Null => Ok(pyo3::types::PyNone::get_bound(*p).to_owned().into_any()),
        serde_yaml::Value::Tagged(_) => Err(generic_error!("Tagged values are not supported").into()),
    }
}

fn default_min_collection_interval_ms() -> u32 {
    15_000
}

#[derive(Debug, Clone, Eq, Hash, PartialEq, PartialOrd, Ord)]
struct YamlCheck {
    found_path: Option<PathBuf>,
    name: String,
    contents: String,
}

impl YamlCheck {
    pub fn new<N, C>(name: N, contents: C, found_path: Option<PathBuf>) -> YamlCheck
    where
        N: Into<String>,
        C: Into<String>,
    {
        Self {
            name: name.into(),
            contents: contents.into(),
            found_path,
        }
    }
}

#[derive(Debug, Clone, Eq, Hash, PartialEq, PartialOrd, Ord)]
enum CheckSource {
    Yaml(YamlCheck),
}

impl CheckSource {
    fn to_check_request(&self) -> Result<CheckRequest, Error> {
        match self {
            CheckSource::Yaml(y) => {
                let YamlCheck {
                    name,
                    contents,
                    found_path: _,
                } = y;
                let mut checks_config = Vec::new();
                let read_yaml: YamlCheckConfiguration = match serde_yaml::from_str(contents) {
                    Ok(read) => read,
                    Err(e) => {
                        error!(%e, "Can't decode yaml as check configuration: {contents}");
                        return Err(Error::CantReadConfiguration { source: e });
                    }
                };
                let init_config = if let Some(init_config) = read_yaml.init_config {
                    let mapping: serde_yaml::Mapping = init_config;

                    let map: HashMap<String, serde_yaml::Value> = mapping
                        .into_iter()
                        .map(|(k, v)| (k.as_str().expect("Only string instance config keys").to_string(), v))
                        .collect();

                    CheckInitConfiguration(map)
                } else {
                    CheckInitConfiguration(HashMap::new())
                };

                for instance in read_yaml.instances.into_iter() {
                    let mapping: serde_yaml::Mapping = instance.to_owned();

                    let map: HashMap<String, serde_yaml::Value> = mapping
                        .into_iter()
                        .map(|(k, v)| (k.as_str().expect("Only string instance config keys").to_string(), v))
                        .collect();

                    checks_config.push(CheckInstanceConfiguration(map));
                }

                let check_name = match &read_yaml.name {
                    Some(yaml_name) => {
                        if yaml_name != name {
                            warn!("Name in yaml file does not match file name: {} != {}", yaml_name, name);
                            warn!("Using 'name' field value as the loadable_name.");
                        }
                        yaml_name.clone()
                    }
                    None => name.clone(),
                };
                Ok(CheckRequest {
                    name: check_name,
                    instances: checks_config,
                    init_config,
                    source: self.clone(),
                })
            }
        }
    }
}

impl Display for CheckSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CheckSource::Yaml(YamlCheck {
                name,
                contents: _,
                found_path: _,
            }) => write!(f, "YamlCheckName: {name}"),
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

impl MemoryBounds for ChecksConfiguration {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        // We allocate nothing up front
        builder.minimum().with_fixed_amount(0);
    }
}

struct CheckDispatcher {
    python_scheduler: python_scheduler::PythonCheckScheduler,
    check_metrics_rx: mpsc::Receiver<CheckMetric>,
    check_run_requests: mpsc::Receiver<RunnableCheckRequest>,
    check_stop_requests: mpsc::Receiver<CheckRequest>,
}

impl CheckDispatcher {
    fn new(
        check_run_requests: mpsc::Receiver<RunnableCheckRequest>, check_stop_requests: mpsc::Receiver<CheckRequest>,
    ) -> Result<Self, GenericError> {
        let (check_metrics_tx, check_metrics_rx) = mpsc::channel(10_000_000);
        Ok(Self {
            check_metrics_rx,
            check_run_requests,
            check_stop_requests,
            python_scheduler: python_scheduler::PythonCheckScheduler::new(check_metrics_tx.clone())?,
        })
    }

    /// Listens for check requests and dispatches each one to a check scheduler
    /// that can handle the requested 'check'.
    /// If multiple schedulers can handle a check, it is dispatched to the first one.
    fn run(self) -> mpsc::Receiver<CheckMetric> {
        info!("Check dispatcher started.");

        let CheckDispatcher {
            mut check_run_requests,
            mut check_stop_requests,
            mut python_scheduler,
            check_metrics_rx,
        } = self;
        tokio::spawn(async move {
            loop {
                select! {
                    Some(check_request) = check_run_requests.recv() => {
                        info!("Dispatching check request: {}", check_request);
                        if python_scheduler.can_run_check(&check_request) {
                            match python_scheduler.run_check(&check_request) {
                                Ok(_) => {
                                    debug!("Check request dispatched: {}", check_request);
                                }
                                Err(e) => {
                                    error!("Error dispatching check request: {}", e);
                                }
                            }
                        } else {
                            warn!("Check request not dispatched: {}", check_request);
                        }
                    },
                    Some(check_request) = check_stop_requests.recv() => {
                        info!("Stopping check request: {}", check_request);
                        // TODO check wihch one is running it and then stop it on that one
                        python_scheduler.stop_check(check_request);
                    }
                }
            }
        });

        check_metrics_rx
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

        let (check_run_requests_tx, check_run_requests_rx) = mpsc::channel(100);
        let (check_stop_requests_tx, check_stop_requests_rx) = mpsc::channel(100);
        let dispatcher = CheckDispatcher::new(check_run_requests_rx, check_stop_requests_rx).expect("Could create");

        let mut joinset = tokio::task::JoinSet::new();
        // Run the local task set.
        // For each listener, spawn a dedicated task to run it.
        for listener in listeners {
            let listener_context = listener::DirCheckListenerContext {
                shutdown_handle: listener_shutdown_coordinator.register(),
                submit_runnable_check_req: check_run_requests_tx.clone(),
                submit_stop_check_req: check_stop_requests_tx.clone(),
                listener,
            };

            joinset.spawn(process_listener(context.clone(), listener_context));
        }

        info!("Check source started.");

        let mut check_metrics_rx = dispatcher.run();

        let check_shutdown = listener_shutdown_coordinator.register();
        tokio::spawn(async {
            global_shutdown.await;
            listener_shutdown_coordinator.shutdown().await;
        });

        tokio::pin!(check_shutdown);

        loop {
            select! {
                Some(check_metric) = check_metrics_rx.recv() => {
                    info!("Received check metric: {:?}", check_metric);
                    let mut event_buffer = context.event_buffer_pool().acquire().await;
                    let event: Event = check_metric.try_into().expect("can't convert");
                    event_buffer.push(event);
                    if let Err(e) = context.forwarder().forward(event_buffer).await {
                        error!(error = %e, "Failed to forward check metrics.");
                    }
                }
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
                _ = &mut check_shutdown => {
                    info!("Stopping Check source...");
                    break;
                }
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
        submit_runnable_check_req,
        submit_stop_check_req,
        mut listener,
    } = listener_context;
    tokio::pin!(shutdown_handle);

    let stream_shutdown_coordinator = DynamicShutdownCoordinator::default();

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
            Some(new_entity) = new_entities.recv() => {
                let check_request = match new_entity.to_runnable_request() {
                    Ok(check_request) => check_request,
                    Err(e) => {
                        error!("Can't convert check source to runnable request: {}", e);
                        continue;
                    }
                };

                match submit_runnable_check_req.send(check_request).await {
                    Ok(_) => {
                        debug!("Check request submitted to dispatcher");
                    }
                    Err(e) => {
                        error!("Error running check: {}", e);
                    }
                }
            }
            Some(deleted_entity) = deleted_entities.recv() => {
                submit_stop_check_req.send(deleted_entity).await.expect("Could send");
            }
        }
    }

    stream_shutdown_coordinator.shutdown().await;

    info!("Check listener stopped.");

    Ok(())
}

trait CheckScheduler {
    fn can_run_check(&self, check_request: &RunnableCheckRequest) -> bool;
    fn run_check(&mut self, check_request: &RunnableCheckRequest) -> Result<(), GenericError>;
    fn stop_check(&mut self, check_name: CheckRequest);
}

#[derive(Debug, Clone, Copy, PartialEq)]
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
#[derive(Debug, PartialEq)]
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
