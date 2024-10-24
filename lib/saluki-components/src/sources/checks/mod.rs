use std::collections::{BTreeMap, HashSet};
use std::hash::Hasher;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};
use std::{fmt::Display, time::Duration};

use async_trait::async_trait;
use fnv::FnvHasher;
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder};
use pyo3::prelude::*;
use python_scheduler::RunnableDecision;
use saluki_config::GenericConfiguration;
use saluki_context::{Context, Tag, TagSet};
use saluki_core::pooling::ObjectPool;
use saluki_core::{
    components::{
        sources::{Source, SourceBuilder, SourceContext},
        MetricsBuilder,
    },
    topology::{
        shutdown::{ComponentShutdownHandle, DynamicShutdownCoordinator, DynamicShutdownHandle},
        OutputDefinition,
    },
};
use saluki_error::{generic_error, GenericError};
use saluki_event::{metric::*, DataType, Event};
use saluki_health::Health;
use serde::{Deserialize, Serialize};
use snafu::Snafu;
use tokio::{select, sync::mpsc};
use tracing::{debug, error, info, trace, warn};

use crate::sources::checks::dispatcher::*;

mod dispatcher;
mod listener;
mod python_exposed_modules;
mod python_scheduler;

#[derive(Debug, Snafu)]
#[snafu(context(suffix(false)))]
enum Error {
    CantReadConfiguration { source: serde_yaml::Error },
}
/// Checks source.
///
/// Scans a directory for check configurations and emits them as things to run.
#[derive(Deserialize)]
pub struct ChecksConfiguration {
    /// The directory containing the check configurations.
    #[serde(default = "default_check_config_dirs")]
    check_config_dirs: Vec<String>,
}

fn default_check_config_dirs() -> Vec<String> {
    vec![
        "./dist/conf.d".to_string(),
        "/etc/datadog-agent/conf.d".to_string(),
        "etc-datadog-agent/conf.d".to_string(),
    ]
}

#[derive(Debug, Clone, Eq, PartialEq, Hash)]
struct CheckRequest {
    // Name is _not_ just for display, the 'name' field identifies which check should be instantiated
    // and run to satisfy this check request.
    // In python checks, this is the module name for the check.
    // In core checks, TBD.
    name: String,
    instances: Vec<CheckConfiguration>,
    init_config: CheckConfiguration,
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
    map: &BTreeMap<String, serde_yaml::Value>, p: &'py pyo3::Python,
) -> PyResult<Bound<'py, pyo3::types::PyDict>> {
    let dict = pyo3::types::PyDict::new_bound(*p);
    for (key, value) in map {
        let value = serde_value_to_pytype(value, p)?;
        dict.set_item(key, value)?;
    }
    Ok(dict)
}

/// Builds a unique ID for a check name and its configuration.
///
/// # Arguments
///
/// * `check_name` - The name of the check.
/// * `integration_config_digest` - Defined in agent-code in comp/core/autodiscovery/integration/config.go
/// * `instance` - Configuration for a specific check instance
/// * `init_config` - Shared initial configuration amongst this check
///
/// `integration_config_digest` is a hash implemented as follows:
///
/// // FastDigest returns an hash value representing the data stored in this configuration.
/// // Difference with Digest is that FastDigest does not consider that difference may appear inside Instances
/// // allowing to remove costly YAML Marshal/UnMarshal operations
/// // The ClusterCheck field is intentionally left out to keep a stable digest
/// // between the cluster-agent and the node-agents
/// func (c *Config) FastDigest() uint64 {
/// »···h := murmur3.New64()
/// »···_, _ = h.Write([]byte(c.Name))
/// »···for _, i := range c.Instances {
/// »···»···_, _ = h.Write([]byte(i))
/// »···}
/// »···_, _ = h.Write([]byte(c.InitConfig))
/// »···for _, i := range c.ADIdentifiers {
/// »···»···_, _ = h.Write([]byte(i))
/// »···}
/// »···_, _ = h.Write([]byte(c.NodeName))
/// »···_, _ = h.Write([]byte(c.LogsConfig))
/// »···_, _ = h.Write([]byte(c.ServiceID))
/// »···_, _ = h.Write([]byte(strconv.FormatBool(c.IgnoreAutodiscoveryTags)))
///
/// »···return h.Sum64()
/// }

/// # Returns
///
/// A unique `ID` as a `String`.
fn build_check_id(
    check_name: &str, integration_config_digest: u64, instance: &CheckConfiguration, init_config: &CheckConfiguration,
) -> String {
    // Convert the digest to big-endian bytes.
    let digest_bytes = integration_config_digest.to_be_bytes();

    // Initialize the FNV hasher.
    let mut hasher = FnvHasher::default();
    hasher.write(&digest_bytes);
    hasher.write(&instance.as_bytes());
    hasher.write(&init_config.as_bytes());

    // Compute the hash.
    let hash = hasher.finish();

    // Get the name from the instance.
    // this just needs to check for a 'name' field and return it
    let name = instance.name();

    // Build the ID based on whether the name is empty.
    if let Some(name) = name {
        format!("{}:{}:{:x}", check_name, name, hash)
    } else {
        format!("{}:{:x}", check_name, hash)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq, Hash)]
struct CheckConfiguration(BTreeMap<String, serde_yaml::Value>);

impl CheckConfiguration {
    fn min_collection_interval_ms(&self) -> u32 {
        self.0
            .get("min_collection_interval")
            .map(|v| v.as_i64().expect("min_collection_interval must be an integer") as u32)
            .unwrap_or_else(default_min_collection_interval_ms)
    }

    fn name(&self) -> Option<String> {
        match self.0.get("name") {
            Some(serde_yaml::Value::String(s)) => Some(s.clone()),
            _ => None,
        }
    }

    /// Serializes the configuration to bytes in a deterministic way.
    fn as_bytes(&self) -> Vec<u8> {
        // Convert HashMap to BTreeMap for deterministic ordering
        let map: BTreeMap<_, _> = self.0.iter().collect();

        // Serialize the BTreeMap to JSON bytes
        serde_json::to_vec(&map).unwrap()
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

                    let map: BTreeMap<String, serde_yaml::Value> = mapping
                        .into_iter()
                        .map(|(k, v)| (k.as_str().expect("Only string instance config keys").to_string(), v))
                        .collect();

                    CheckConfiguration(map)
                } else {
                    CheckConfiguration(BTreeMap::new())
                };

                for instance in read_yaml.instances.into_iter() {
                    let mapping: serde_yaml::Mapping = instance.to_owned();

                    let map: BTreeMap<String, serde_yaml::Value> = mapping
                        .into_iter()
                        .map(|(k, v)| (k.as_str().expect("Only string instance config keys").to_string(), v))
                        .collect();

                    checks_config.push(CheckConfiguration(map));
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

        let listener = listener::DirCheckRequestListener::from_paths(self.check_config_dirs.clone())?;

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

pub struct Checks {
    listeners: Vec<listener::DirCheckRequestListener>,
}

impl Checks {
    // consumes self
    async fn run_inner(self, context: SourceContext, global_shutdown: ComponentShutdownHandle) -> Result<(), ()> {
        let mut listener_shutdown_coordinator = DynamicShutdownCoordinator::default();
        let metrics = MetricsBuilder::from_component_context(context.component_context());
        let tlm = ChecksTelemetry {
            check_requests_dispatched: metrics.register_counter("checkrequests.dispatched"),
            check_instances_started: metrics.register_counter("checkinstances.started"),
        };

        let check_dispatcher_health = context.health_registry().register_component("check_dispatcher");
        let python_check_scheduler = context.health_registry().register_component("python_check_scheduler");

        let Checks { listeners } = self;

        let (check_run_requests_tx, check_run_requests_rx) = mpsc::channel(100);
        let (check_stop_requests_tx, check_stop_requests_rx) = mpsc::channel(100);
        let dispatcher = CheckDispatcher::new(
            check_run_requests_rx,
            check_stop_requests_rx,
            tlm,
            check_dispatcher_health,
            python_check_scheduler,
        )
        .expect("Could create");

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
                    if let Some(_unsent) = event_buffer.try_push(event) {
                        error!("Event buffer full, dropping event");
                        continue;
                    }
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
        let global_shutdown = context.take_shutdown_handle();
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
    _source_context: SourceContext, listener_context: listener::DirCheckListenerContext,
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
                info!("Received shutdown signal. Shutting down check listeners.");
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
                let runnable_check_request: RunnableCheckRequest = new_entity.into();
                info!("New Check request for {} received.", runnable_check_request.check_request.name);

                match submit_runnable_check_req.send(runnable_check_request).await {
                    Ok(_) => {
                        trace!("Check request submitted to dispatcher");
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
    fn can_run_check(&mut self, check_request: &RunnableCheckRequest) -> RunnableDecision;
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
        // Convert Vec<String> to Vec<Tag>
        let tags: Vec<Tag> = self.tags.into_iter().map(Tag::new).collect();

        // Convert Vec<Tag> to TagSet
        let tagset: TagSet = TagSet::new(tags);

        let context = Context::from_parts(self.name, tagset);
        let metadata = MetricMetadata::default();

        match self.metric_type {
            PyMetricType::Gauge => Ok(saluki_event::Event::Metric(Metric::from_parts(
                context,
                MetricValues::gauge(self.value),
                metadata,
            ))),
            PyMetricType::Counter => Ok(saluki_event::Event::Metric(Metric::from_parts(
                context,
                MetricValues::counter(self.value),
                metadata,
            ))),
            PyMetricType::Histogram => Ok(saluki_event::Event::Metric(Metric::from_parts(
                context,
                // TODO support more metric types
                MetricValues::gauge(self.value),
                metadata,
            ))),
            PyMetricType::Historate => Ok(saluki_event::Event::Metric(Metric::from_parts(
                context,
                // TODO what is historate? what do I do with it?
                MetricValues::gauge(self.value),
                metadata,
            ))),
            PyMetricType::MonotonicCount => Ok(saluki_event::Event::Metric(Metric::from_parts(
                context,
                // TODO incorrect handling of monotonic count
                MetricValues::counter(self.value),
                metadata,
            ))),
            PyMetricType::Rate => Ok(saluki_event::Event::Metric(Metric::from_parts(
                context,
                // TODO incorrect handling of rate
                MetricValues::counter(self.value),
                metadata,
            ))),
            PyMetricType::Count => Ok(saluki_event::Event::Metric(Metric::from_parts(
                context,
                // TODO incorrect handling of count
                MetricValues::counter(self.value),
                metadata,
            ))),
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
