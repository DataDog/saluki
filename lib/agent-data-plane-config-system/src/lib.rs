//! Configuration system facade: loading, translation, lifecycle, views, and runtime updates.

mod remote_agent;
mod witness_impl;

use std::{path::PathBuf, sync::Arc, time::Duration};

use agent_data_plane_config::{
    BootstrapConfiguration, ConfigViews, ControlSalukiOnly, DatadogBootstrap, DatadogRuntimeAuthority,
    DogStatsDSalukiOnly, InternalConfigView, OtlpSalukiOnly, SalukiBootstrap, SalukiConfiguration,
    SalukiOnlyConfiguration, SourceConfigView, WorkloadSalukiOnly,
};
use bytesize::ByteSize;
use datadog_agent_config::{drive, DatadogConfiguration, DatadogRemapper, KEY_ALIASES};
use datadog_protos::agent::{config_event, ConfigSetting, ConfigSnapshot, ConfigUpdate as AgentConfigUpdate};
use futures::StreamExt as _;
pub use remote_agent::{Attachments, DatadogAgentConnection};
use saluki_common::deser::PermissiveBool;
use saluki_component_config::{
    DatadogForwarderConfig, DogStatsDDebugLogConfig, DogStatsDPostAggregateFilterConfig, DogStatsDPrefixFilterConfig,
    MrfConfig, ScopedConfig, TagFilterlistConfig,
};
use saluki_config_tools::{ConfigurationError, ConfigurationLoader, GenericConfiguration};
use saluki_error::{generic_error, GenericError};
use serde::Deserialize;
use serde_with::serde_as;
use tokio::{
    sync::{watch, Mutex},
    time::sleep,
};
use tracing::{debug, warn};
use witness_impl::Translator;

/// Inputs used when loading local configuration sources.
#[derive(Clone, Debug, Default)]
pub struct BootstrapInputs {
    /// Optional Datadog YAML file path.
    pub datadog_config_path: Option<PathBuf>,
    /// Optional Saluki-only YAML file path.
    pub saluki_config_path: Option<PathBuf>,
    /// Whether Datadog environment variables should be loaded.
    pub load_datadog_environment: bool,
    /// Whether Saluki environment variables should be loaded.
    pub load_saluki_environment: bool,
}

/// Facade entry point for configuration loading.
pub struct ConfigurationSystem;

impl ConfigurationSystem {
    /// Loads local sources once and returns the staged loaded object.
    pub async fn load(inputs: BootstrapInputs) -> Result<LoadedConfigurationSystem, ConfigurationError> {
        let mut datadog_loader = ConfigurationLoader::default()
            .with_key_aliases(KEY_ALIASES)
            .add_providers([DatadogRemapper::new()]);
        if let Some(path) = &inputs.datadog_config_path {
            datadog_loader = datadog_loader.from_yaml(path)?;
        }
        if inputs.load_datadog_environment {
            datadog_loader = datadog_loader.from_environment("DD")?;
        }
        let datadog = datadog_loader.into_generic().await?;

        let mut saluki_loader = ConfigurationLoader::default();
        if let Some(path) = &inputs.saluki_config_path {
            saluki_loader = saluki_loader.from_yaml(path)?;
        }
        if inputs.load_saluki_environment {
            saluki_loader = saluki_loader.from_environment("SALUKI")?;
        }
        let saluki = saluki_loader.into_generic().await?;

        let bootstrap = BootstrapConfiguration {
            datadog: parse_datadog_bootstrap(&datadog)?,
            saluki: parse_saluki_bootstrap(&saluki)?,
        };
        let saluki_only = parse_saluki_only(&saluki)?;

        Ok(LoadedConfigurationSystem {
            datadog,
            saluki,
            bootstrap,
            saluki_only,
        })
    }
}

/// Loaded local source snapshot. Consumed when runtime starts.
pub struct LoadedConfigurationSystem {
    datadog: GenericConfiguration,
    saluki: GenericConfiguration,
    bootstrap: BootstrapConfiguration,
    saluki_only: SalukiOnlyConfiguration,
}

impl LoadedConfigurationSystem {
    /// Returns the typed bootstrap slice.
    pub fn bootstrap(&self) -> &BootstrapConfiguration {
        &self.bootstrap
    }

    /// Starts runtime configuration and consumes the loaded snapshot.
    pub async fn start_runtime(
        self, authority: DatadogRuntimeAuthority,
    ) -> Result<StartedConfigurationSystem, GenericError> {
        let local_datadog = self.datadog.as_typed::<DatadogConfiguration>()?;
        let local_native = translate_datadog(&local_datadog, &self.saluki_only)?;

        let attachments = remote_agent::build_attachments(&self.datadog, &local_native.control, authority).await?;
        let stream_connection = (authority == DatadogRuntimeAuthority::Stream)
            .then(|| attachments.datadog_agent.clone())
            .flatten();

        let (source_snapshot, native, router_authority) = if let Some(connection) = stream_connection.clone() {
            let source_snapshot = read_initial_datadog_stream_snapshot(connection).await?;
            let datadog = source_snapshot.as_datadog_configuration()?;
            let native = translate_datadog(&datadog, &self.saluki_only)?;
            (source_snapshot, native, DatadogRuntimeAuthority::Stream)
        } else {
            (
                DatadogSourceSnapshot::from_json(self.datadog.as_typed::<serde_json::Value>()?),
                local_native,
                DatadogRuntimeAuthority::Local,
            )
        };

        let router = Arc::new(Mutex::new(ConfigUpdateRouter::new(
            native.clone(),
            self.saluki_only.clone(),
            router_authority,
        )));
        let handles = router.lock().await.handles();
        let views = ConfigViews {
            raw: SourceConfigView::new(scrub_json(source_snapshot.as_json())),
            internal: InternalConfigView::new(scrub_json(serde_json::to_value(&native)?)),
        };

        if let Some(connection) = stream_connection {
            tokio::spawn(run_config_stream_task(
                connection,
                source_snapshot,
                router.clone(),
                views.clone(),
            ));
        }

        Ok(StartedConfigurationSystem {
            saluki: native,
            handles,
            views,
            router,
            datadog_source: self.datadog,
            attachments,
            _saluki_source: self.saluki,
        })
    }
}

/// Started runtime configuration system.
pub struct StartedConfigurationSystem {
    saluki: SalukiConfiguration,
    handles: DynamicConfigHandles,
    views: ConfigViews,
    router: Arc<Mutex<ConfigUpdateRouter>>,
    datadog_source: GenericConfiguration,
    attachments: Attachments,
    _saluki_source: GenericConfiguration,
}

impl StartedConfigurationSystem {
    /// Returns an owned copy of the native configuration.
    pub fn saluki(&self) -> SalukiConfiguration {
        self.saluki.clone()
    }

    /// Returns a reference to the raw Datadog `GenericConfiguration`.
    pub fn datadog_config(&self) -> &GenericConfiguration {
        &self.datadog_source
    }

    /// Returns dynamic config handles for topology assembly.
    pub fn dynamic_handles(&self) -> DynamicConfigHandles {
        self.handles.clone()
    }

    /// Returns serialized config views.
    pub fn config_views(&self) -> ConfigViews {
        self.views.clone()
    }

    /// Returns typed attachments established during runtime start.
    pub fn attachments(&self) -> Attachments {
        self.attachments.clone()
    }

    /// Returns the internal router for tests.
    pub fn router(&self) -> Arc<Mutex<ConfigUpdateRouter>> {
        self.router.clone()
    }
}

#[derive(Clone, Debug)]
struct DatadogSourceSnapshot {
    json: serde_json::Value,
    origin: String,
    sequence_id: i32,
}

impl DatadogSourceSnapshot {
    fn from_json(json: serde_json::Value) -> Self {
        Self {
            json,
            origin: String::new(),
            sequence_id: 0,
        }
    }

    fn from_stream_snapshot(snapshot: ConfigSnapshot) -> Result<Self, GenericError> {
        let mut json = serde_json::Value::Object(serde_json::Map::new());
        for setting in snapshot.settings {
            apply_config_setting(&mut json, setting)?;
        }
        Ok(Self {
            json,
            origin: snapshot.origin,
            sequence_id: snapshot.sequence_id,
        })
    }

    fn apply_stream_update(&mut self, update: AgentConfigUpdate) -> Result<(), GenericError> {
        if !self.origin.is_empty()
            && !update.origin.is_empty()
            && self.origin == update.origin
            && update.sequence_id <= self.sequence_id
        {
            debug!(
                origin = %update.origin,
                update_sequence_id = update.sequence_id,
                current_sequence_id = self.sequence_id,
                "Ignoring stale Datadog Agent config update."
            );
            return Ok(());
        }

        let origin = update.origin;
        let sequence_id = update.sequence_id;
        let setting = update
            .setting
            .ok_or_else(|| generic_error!("Datadog Agent config update did not include a setting."))?;
        apply_config_update_setting(&mut self.json, setting)?;
        self.origin = origin;
        self.sequence_id = sequence_id;
        Ok(())
    }

    fn as_json(&self) -> serde_json::Value {
        self.json.clone()
    }

    fn as_datadog_configuration(&self) -> Result<DatadogConfiguration, GenericError> {
        Ok(serde_json::from_value(self.json.clone())?)
    }
}

async fn read_initial_datadog_stream_snapshot(
    connection: DatadogAgentConnection,
) -> Result<DatadogSourceSnapshot, GenericError> {
    let session_id = connection.session_id().wait_for_update().await;
    let mut client = connection.client();
    let mut stream = client.stream_config_events(&session_id);

    while let Some(result) = stream.next().await {
        let event = result?;
        match event.event {
            Some(config_event::Event::Snapshot(snapshot)) => {
                return DatadogSourceSnapshot::from_stream_snapshot(snapshot)
            }
            Some(config_event::Event::Update(_)) => {
                return Err(generic_error!(
                    "Datadog Agent config stream sent an update before the initial snapshot."
                ));
            }
            None => debug!("Datadog Agent config stream sent an empty event before the initial snapshot."),
        }
    }

    Err(generic_error!(
        "Datadog Agent config stream ended before sending the initial snapshot."
    ))
}

async fn run_config_stream_task(
    connection: DatadogAgentConnection, mut source_snapshot: DatadogSourceSnapshot,
    router: Arc<Mutex<ConfigUpdateRouter>>, views: ConfigViews,
) {
    loop {
        let session_id = connection.session_id().wait_for_update().await;
        let mut client = connection.client();
        let mut stream = client.stream_config_events(&session_id);
        debug!(%session_id, "Listening for Datadog Agent config stream updates.");

        while let Some(result) = stream.next().await {
            let event = match result {
                Ok(event) => event,
                Err(status) => {
                    warn!(?status, "Datadog Agent config stream failed; reconnecting.");
                    break;
                }
            };

            let accepted_source_update = match event.event {
                Some(config_event::Event::Snapshot(snapshot)) => {
                    match DatadogSourceSnapshot::from_stream_snapshot(snapshot) {
                        Ok(snapshot) => {
                            source_snapshot = snapshot;
                            true
                        }
                        Err(error) => {
                            warn!(error = %error, "Rejecting malformed Datadog Agent config snapshot.");
                            false
                        }
                    }
                }
                Some(config_event::Event::Update(update)) => match source_snapshot.apply_stream_update(update) {
                    Ok(()) => true,
                    Err(error) => {
                        warn!(error = %error, "Rejecting malformed Datadog Agent config update.");
                        false
                    }
                },
                None => false,
            };

            if !accepted_source_update {
                continue;
            }

            let datadog = match source_snapshot.as_datadog_configuration() {
                Ok(datadog) => datadog,
                Err(error) => {
                    warn!(error = %error, "Rejecting Datadog Agent config update that failed to parse.");
                    continue;
                }
            };

            let mut router = router.lock().await;
            match router.apply_datadog_source_snapshot(&datadog, &source_snapshot.as_json()) {
                Ok(_) => update_views(&views, &source_snapshot, router.current()),
                Err(error) => warn!(error = %error, "Rejecting Datadog Agent config update that failed translation."),
            }
        }

        sleep(Duration::from_secs(1)).await;
    }
}

fn apply_config_setting(root: &mut serde_json::Value, setting: ConfigSetting) -> Result<(), GenericError> {
    let value = setting.value.map(protobuf_value_to_json).ok_or_else(|| {
        generic_error!(
            "Datadog Agent config setting `{}` did not include a value.",
            setting.key
        )
    })?;
    upsert_json_path(root, &setting.key, value);
    Ok(())
}

fn apply_config_update_setting(root: &mut serde_json::Value, setting: ConfigSetting) -> Result<(), GenericError> {
    let value = setting.value.map(protobuf_value_to_json).ok_or_else(|| {
        generic_error!(
            "Datadog Agent config setting `{}` did not include a value.",
            setting.key
        )
    })?;
    if value.is_null() {
        delete_json_path(root, &setting.key);
    } else {
        upsert_json_path(root, &setting.key, value);
    }
    Ok(())
}

fn upsert_json_path(root: &mut serde_json::Value, path: &str, value: serde_json::Value) {
    if !root.is_object() {
        *root = serde_json::Value::Object(serde_json::Map::new());
    }

    let mut current = root;
    let mut segments = path.split('.').peekable();
    while let Some(segment) = segments.next() {
        let is_leaf = segments.peek().is_none();
        if is_leaf {
            current
                .as_object_mut()
                .expect("current value is an object")
                .insert(segment.to_string(), value);
            return;
        }

        let map = current.as_object_mut().expect("current value is an object");
        current = map
            .entry(segment.to_string())
            .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
        if !current.is_object() {
            *current = serde_json::Value::Object(serde_json::Map::new());
        }
    }
}

fn delete_json_path(root: &mut serde_json::Value, path: &str) {
    let mut current = root;
    let mut segments = path.split('.').peekable();
    while let Some(segment) = segments.next() {
        let is_leaf = segments.peek().is_none();
        if is_leaf {
            if let Some(map) = current.as_object_mut() {
                map.remove(segment);
            }
            return;
        }

        let Some(next) = current.get_mut(segment) else {
            return;
        };
        current = next;
    }
}

fn protobuf_value_to_json(value: prost_types::Value) -> serde_json::Value {
    match value.kind {
        Some(prost_types::value::Kind::NullValue(_)) | None => serde_json::Value::Null,
        Some(prost_types::value::Kind::NumberValue(value)) => serde_json::Number::from_f64(value)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null),
        Some(prost_types::value::Kind::StringValue(value)) => serde_json::Value::String(value),
        Some(prost_types::value::Kind::BoolValue(value)) => serde_json::Value::Bool(value),
        Some(prost_types::value::Kind::StructValue(value)) => serde_json::Value::Object(
            value
                .fields
                .into_iter()
                .map(|(key, value)| (key, protobuf_value_to_json(value)))
                .collect(),
        ),
        Some(prost_types::value::Kind::ListValue(value)) => {
            serde_json::Value::Array(value.values.into_iter().map(protobuf_value_to_json).collect())
        }
    }
}

fn update_views(views: &ConfigViews, source_snapshot: &DatadogSourceSnapshot, native: &SalukiConfiguration) {
    views.raw.update(scrub_json(source_snapshot.as_json()));
    match serde_json::to_value(native) {
        Ok(value) => views.internal.update(scrub_json(value)),
        Err(error) => warn!(error = %error, "Failed to update internal config view."),
    }
}

/// Bundle of fixed-or-live dynamic slices.
#[derive(Clone, Debug)]
pub struct DynamicConfigHandles {
    /// Forwarder config handle.
    pub forwarder: ScopedConfig<DatadogForwarderConfig>,
    /// MRF config handle.
    pub multi_region_failover: ScopedConfig<MrfConfig>,
    /// DogStatsD prefix filter config handle.
    pub dogstatsd_prefix_filter: ScopedConfig<DogStatsDPrefixFilterConfig>,
    /// DogStatsD tag filterlist config handle.
    pub dogstatsd_tag_filterlist: ScopedConfig<TagFilterlistConfig>,
    /// DogStatsD post-aggregate filter config handle.
    pub dogstatsd_post_aggregate_filter: ScopedConfig<DogStatsDPostAggregateFilterConfig>,
    /// DogStatsD debug log config handle.
    pub dogstatsd_debug_log: ScopedConfig<DogStatsDDebugLogConfig>,
    /// Runtime log-level handle.
    pub log_level: ScopedConfig<Option<String>>,
}

/// Routes accepted updates to native config slices.
pub struct ConfigUpdateRouter {
    current: SalukiConfiguration,
    saluki_only: SalukiOnlyConfiguration,
    authority: DatadogRuntimeAuthority,
    forwarder_tx: Option<watch::Sender<DatadogForwarderConfig>>,
    mrf_tx: Option<watch::Sender<MrfConfig>>,
    prefix_tx: Option<watch::Sender<DogStatsDPrefixFilterConfig>>,
    tag_filter_tx: Option<watch::Sender<TagFilterlistConfig>>,
    post_aggregate_filter_tx: Option<watch::Sender<DogStatsDPostAggregateFilterConfig>>,
    debug_log_tx: Option<watch::Sender<DogStatsDDebugLogConfig>>,
    log_level_tx: Option<watch::Sender<Option<String>>>,
}

impl ConfigUpdateRouter {
    /// Creates a router from the initial native config.
    pub fn new(
        initial: SalukiConfiguration, saluki_only: SalukiOnlyConfiguration, authority: DatadogRuntimeAuthority,
    ) -> Self {
        match authority {
            DatadogRuntimeAuthority::Local => Self {
                current: initial,
                saluki_only,
                authority,
                forwarder_tx: None,
                mrf_tx: None,
                prefix_tx: None,
                tag_filter_tx: None,
                post_aggregate_filter_tx: None,
                debug_log_tx: None,
                log_level_tx: None,
            },
            DatadogRuntimeAuthority::Stream => {
                let (_, forwarder_tx) = ScopedConfig::live(initial.components.forwarder.datadog.clone());
                let (_, mrf_tx) = ScopedConfig::live(initial.components.metrics.multi_region_failover.clone());
                let (_, prefix_tx) = ScopedConfig::live(initial.components.dogstatsd.prefix_filter.clone());
                let (_, tag_filter_tx) = ScopedConfig::live(initial.components.dogstatsd.tag_filterlist.clone());
                let (_, post_aggregate_filter_tx) =
                    ScopedConfig::live(initial.components.dogstatsd.post_aggregate_filter.clone());
                let (_, debug_log_tx) = ScopedConfig::live(initial.components.dogstatsd.debug_log.clone());
                let (_, log_level_tx) = ScopedConfig::live(initial.control.log_level.clone());
                Self {
                    current: initial,
                    saluki_only,
                    authority,
                    forwarder_tx: Some(forwarder_tx),
                    mrf_tx: Some(mrf_tx),
                    prefix_tx: Some(prefix_tx),
                    tag_filter_tx: Some(tag_filter_tx),
                    post_aggregate_filter_tx: Some(post_aggregate_filter_tx),
                    debug_log_tx: Some(debug_log_tx),
                    log_level_tx: Some(log_level_tx),
                }
            }
        }
    }

    /// Returns handles for the router's dynamic-capable slices.
    pub fn handles(&self) -> DynamicConfigHandles {
        match self.authority {
            DatadogRuntimeAuthority::Local => DynamicConfigHandles {
                forwarder: ScopedConfig::fixed(self.current.components.forwarder.datadog.clone()),
                multi_region_failover: ScopedConfig::fixed(
                    self.current.components.metrics.multi_region_failover.clone(),
                ),
                dogstatsd_prefix_filter: ScopedConfig::fixed(self.current.components.dogstatsd.prefix_filter.clone()),
                dogstatsd_tag_filterlist: ScopedConfig::fixed(self.current.components.dogstatsd.tag_filterlist.clone()),
                dogstatsd_post_aggregate_filter: ScopedConfig::fixed(
                    self.current.components.dogstatsd.post_aggregate_filter.clone(),
                ),
                dogstatsd_debug_log: ScopedConfig::fixed(self.current.components.dogstatsd.debug_log.clone()),
                log_level: ScopedConfig::fixed(self.current.control.log_level.clone()),
            },
            DatadogRuntimeAuthority::Stream => DynamicConfigHandles {
                forwarder: live_handle(
                    self.current.components.forwarder.datadog.clone(),
                    self.forwarder_tx.as_ref().expect("forwarder sender"),
                ),
                multi_region_failover: live_handle(
                    self.current.components.metrics.multi_region_failover.clone(),
                    self.mrf_tx.as_ref().expect("mrf sender"),
                ),
                dogstatsd_prefix_filter: live_handle(
                    self.current.components.dogstatsd.prefix_filter.clone(),
                    self.prefix_tx.as_ref().expect("prefix sender"),
                ),
                dogstatsd_tag_filterlist: live_handle(
                    self.current.components.dogstatsd.tag_filterlist.clone(),
                    self.tag_filter_tx.as_ref().expect("tag filter sender"),
                ),
                dogstatsd_post_aggregate_filter: live_handle(
                    self.current.components.dogstatsd.post_aggregate_filter.clone(),
                    self.post_aggregate_filter_tx
                        .as_ref()
                        .expect("post-aggregate filter sender"),
                ),
                dogstatsd_debug_log: live_handle(
                    self.current.components.dogstatsd.debug_log.clone(),
                    self.debug_log_tx.as_ref().expect("debug log sender"),
                ),
                log_level: live_handle(
                    self.current.control.log_level.clone(),
                    self.log_level_tx.as_ref().expect("log level sender"),
                ),
            },
        }
    }

    /// Retranslates a Datadog snapshot and routes changed native slices.
    pub fn apply_datadog_snapshot(&mut self, snapshot: &DatadogConfiguration) -> Result<bool, GenericError> {
        let next = translate_datadog(snapshot, &self.saluki_only)?;
        Ok(self.apply_native_snapshot(next))
    }

    /// Retranslates a source snapshot and routes changed native slices.
    pub fn apply_datadog_source_snapshot(
        &mut self, snapshot: &DatadogConfiguration, _source: &serde_json::Value,
    ) -> Result<bool, GenericError> {
        let next = translate_datadog(snapshot, &self.saluki_only)?;
        Ok(self.apply_native_snapshot(next))
    }

    fn apply_native_snapshot(&mut self, next: SalukiConfiguration) -> bool {
        let changed = self.route_changed_slices(&next);
        self.current = next;
        changed
    }

    /// Returns the last accepted native configuration.
    pub fn current(&self) -> &SalukiConfiguration {
        &self.current
    }

    fn route_changed_slices(&mut self, next: &SalukiConfiguration) -> bool {
        let mut changed = false;
        changed |= send_if_changed(
            &self.forwarder_tx,
            &self.current.components.forwarder.datadog,
            &next.components.forwarder.datadog,
        );
        changed |= send_if_changed(
            &self.mrf_tx,
            &self.current.components.metrics.multi_region_failover,
            &next.components.metrics.multi_region_failover,
        );
        changed |= send_if_changed(
            &self.prefix_tx,
            &self.current.components.dogstatsd.prefix_filter,
            &next.components.dogstatsd.prefix_filter,
        );
        changed |= send_if_changed(
            &self.tag_filter_tx,
            &self.current.components.dogstatsd.tag_filterlist,
            &next.components.dogstatsd.tag_filterlist,
        );
        changed |= send_if_changed(
            &self.post_aggregate_filter_tx,
            &self.current.components.dogstatsd.post_aggregate_filter,
            &next.components.dogstatsd.post_aggregate_filter,
        );
        changed |= send_if_changed(
            &self.debug_log_tx,
            &self.current.components.dogstatsd.debug_log,
            &next.components.dogstatsd.debug_log,
        );
        changed |= send_if_changed(
            &self.log_level_tx,
            &self.current.control.log_level,
            &next.control.log_level,
        );
        changed
    }
}

fn live_handle<T: Clone>(initial: T, tx: &watch::Sender<T>) -> ScopedConfig<T> {
    ScopedConfig::Live {
        initial,
        rx: tx.subscribe(),
    }
}

fn send_if_changed<T>(tx: &Option<watch::Sender<T>>, current: &T, next: &T) -> bool
where
    T: Clone + PartialEq,
{
    if current == next {
        return false;
    }
    if let Some(tx) = tx {
        let _ = tx.send(next.clone());
    }
    true
}

fn parse_datadog_bootstrap(config: &GenericConfiguration) -> Result<DatadogBootstrap, ConfigurationError> {
    Ok(DatadogBootstrap {
        log_level: config.try_get_typed("log_level")?,
        metrics_level: config.try_get_typed("metrics_level")?,
        log_format_json: read_permissive_bool(config, "log_format_json")?,
        log_format_rfc3339: read_permissive_bool(config, "log_format_rfc3339")?,
        log_to_console: read_permissive_bool(config, "log_to_console")?,
        log_to_syslog: read_permissive_bool(config, "log_to_syslog")?,
        syslog_rfc: read_permissive_bool(config, "syslog_rfc")?,
        syslog_uri: config.try_get_typed("syslog_uri")?,
        log_file_max_size_bytes: config
            .try_get_typed::<ByteSize>("log_file_max_size")?
            .map(|size| size.as_u64()),
        log_file_max_rolls: config.try_get_typed("log_file_max_rolls")?,
        disable_file_logging: read_permissive_bool(config, "disable_file_logging")?,
        data_plane_log_file: config.try_get_typed("data_plane.log_file")?,
        cmd_port: config.try_get_typed("cmd_port")?,
        auth_token_file_path: config.try_get_typed("auth_token_file_path")?,
        ipc_cert_file_path: config.try_get_typed("ipc_cert_file_path")?,
    })
}

fn read_permissive_bool(config: &GenericConfiguration, key: &str) -> Result<Option<bool>, ConfigurationError> {
    Ok(config.try_get_typed::<PermissiveBoolValue>(key)?.map(|v| v.0))
}

#[serde_as]
#[derive(Deserialize)]
struct PermissiveBoolValue(#[serde_as(as = "PermissiveBool")] bool);

fn parse_saluki_bootstrap(config: &GenericConfiguration) -> Result<SalukiBootstrap, ConfigurationError> {
    Ok(SalukiBootstrap {
        config_path: config.try_get_typed("config_path")?,
    })
}

fn parse_saluki_only(config: &GenericConfiguration) -> Result<SalukiOnlyConfiguration, ConfigurationError> {
    let defaults = SalukiOnlyConfiguration::default();
    Ok(SalukiOnlyConfiguration {
        control: ControlSalukiOnly {
            standalone_mode: config
                .try_get_typed("data_plane.standalone_mode")?
                .unwrap_or(defaults.control.standalone_mode),
            checks_enabled: config
                .try_get_typed("data_plane.checks.enabled")?
                .unwrap_or(defaults.control.checks_enabled),
            stop_timeout_secs: config
                .try_get_typed("data_plane.stop_timeout")?
                .or(defaults.control.stop_timeout_secs),
        },
        otlp: OtlpSalukiOnly {
            string_interner_size: config
                .try_get_typed("otlp.string_interner_size")?
                .unwrap_or(defaults.otlp.string_interner_size),
            cached_contexts_limit: config
                .try_get_typed("otlp.cached_contexts_limit")?
                .unwrap_or(defaults.otlp.cached_contexts_limit),
        },
        dogstatsd: DogStatsDSalukiOnly {
            string_interner_size_bytes: config
                .try_get_typed("dogstatsd.string_interner_size_bytes")?
                .unwrap_or(defaults.dogstatsd.string_interner_size_bytes),
            cached_contexts_limit: config
                .try_get_typed("dogstatsd.cached_contexts_limit")?
                .unwrap_or(defaults.dogstatsd.cached_contexts_limit),
        },
        workload: WorkloadSalukiOnly {
            enabled: config
                .try_get_typed("workload.enabled")?
                .unwrap_or(defaults.workload.enabled),
        },
    })
}

fn translate_datadog(
    datadog: &DatadogConfiguration, saluki_only: &SalukiOnlyConfiguration,
) -> Result<SalukiConfiguration, GenericError> {
    let mut translator = Translator::new(saluki_only.seed());
    drive(datadog, &mut translator)?;
    let mut native = translator.finish();
    saluki_only.apply_runtime_overrides(&mut native);
    Ok(native)
}

fn scrub_json(mut value: serde_json::Value) -> serde_json::Value {
    scrub_json_inner(&mut value);
    value
}

fn scrub_json_inner(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, value) in map {
                if is_sensitive_key(key) {
                    *value = serde_json::Value::String("********".to_string());
                } else {
                    scrub_json_inner(value);
                }
            }
        }
        serde_json::Value::Array(values) => {
            for value in values {
                scrub_json_inner(value);
            }
        }
        _ => {}
    }
}

fn is_sensitive_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    key.contains("api_key") || key.contains("app_key") || key.contains("token") || key.contains("password")
}

#[cfg(test)]
mod tests {
    use prost_types::value::Kind;
    use serde_json::json;

    use super::*;

    #[test]
    fn stream_updates_ignore_stale_same_origin_events_and_delete_nulls() {
        let mut snapshot = DatadogSourceSnapshot::from_stream_snapshot(ConfigSnapshot {
            origin: "agent-a".to_string(),
            sequence_id: 10,
            settings: vec![setting("log_level", string_value_proto("info"))],
        })
        .expect("initial snapshot should parse");

        snapshot
            .apply_stream_update(update("agent-a", 9, "log_level", string_value_proto("debug")))
            .expect("stale update should be ignored without failing");
        assert_eq!(snapshot.as_json()["log_level"], json!("info"));
        assert_eq!(snapshot.sequence_id, 10);

        snapshot
            .apply_stream_update(update("agent-a", 11, "log_level", string_value_proto("debug")))
            .expect("newer update should apply");
        assert_eq!(snapshot.as_json()["log_level"], json!("debug"));
        assert_eq!(snapshot.sequence_id, 11);

        snapshot
            .apply_stream_update(update("agent-a", 12, "log_level", null_value_proto()))
            .expect("null update should delete");
        assert!(snapshot.as_json().get("log_level").is_none());
    }

    #[test]
    fn datadog_witnesses_control_keys_and_saluki_only_control_overrides() {
        let saluki_only = SalukiOnlyConfiguration {
            control: ControlSalukiOnly {
                standalone_mode: true,
                checks_enabled: true,
                stop_timeout_secs: Some(7),
            },
            ..SalukiOnlyConfiguration::default()
        };
        let datadog: DatadogConfiguration = serde_json::from_value(json!({
            "auth_token_file_path": "/tmp/auth_token",
            "ipc_cert_file_path": "/tmp/ipc_cert.pem",
            "data_plane": {
                "enabled": true,
                "dogstatsd": { "enabled": false },
                "otlp": {
                    "enabled": true,
                    "proxy": {
                        "enabled": true,
                        "receiver": { "protocols": { "grpc": { "endpoint": "127.0.0.1:14319" } } }
                    }
                }
            }
        }))
        .expect("Datadog snapshot should parse");

        let native = translate_datadog(&datadog, &saluki_only).expect("snapshot should translate");

        assert!(native.control.enabled);
        assert!(native.control.standalone_mode);
        assert!(native.control.checks.enabled);
        assert!(!native.control.dogstatsd.enabled);
        assert!(native.control.otlp.native.enabled);
        assert!(native.control.otlp.proxy.enabled);
        assert_eq!(
            native.control.otlp.proxy.core_agent_otlp_grpc_endpoint,
            "127.0.0.1:14319"
        );
        assert_eq!(native.control.stop_timeout_millis, 7_000);
        assert_eq!(
            native.control.ipc_auth.auth_token_file_path.as_deref(),
            Some("/tmp/auth_token")
        );
        assert_eq!(
            native.control.ipc_auth.ipc_cert_file_path.as_deref(),
            Some("/tmp/ipc_cert.pem")
        );
    }

    #[test]
    fn stream_router_updates_live_typed_handles() {
        let mut initial = SalukiConfiguration::default();
        initial.control.log_level = Some("info".to_string());
        let mut router = ConfigUpdateRouter::new(
            initial,
            SalukiOnlyConfiguration::default(),
            DatadogRuntimeAuthority::Stream,
        );
        let handles = router.handles();
        let snapshot: DatadogConfiguration =
            serde_json::from_value(json!({"log_level": "debug"})).expect("Datadog snapshot should parse");

        assert!(router
            .apply_datadog_snapshot(&snapshot)
            .expect("snapshot should translate"));
        assert_eq!(handles.log_level.current(), Some("debug".to_string()));
    }

    fn update(origin: &str, sequence_id: i32, key: &str, value: prost_types::Value) -> AgentConfigUpdate {
        AgentConfigUpdate {
            origin: origin.to_string(),
            sequence_id,
            setting: Some(setting(key, value)),
        }
    }

    fn setting(key: &str, value: prost_types::Value) -> ConfigSetting {
        ConfigSetting {
            source: "test".to_string(),
            key: key.to_string(),
            value: Some(value),
        }
    }

    fn string_value_proto(value: &str) -> prost_types::Value {
        prost_types::Value {
            kind: Some(Kind::StringValue(value.to_string())),
        }
    }

    fn null_value_proto() -> prost_types::Value {
        prost_types::Value {
            kind: Some(Kind::NullValue(0)),
        }
    }
}
