//! Configuration system facade: loading, translation, lifecycle, views, and runtime updates.

mod witness_impl;

use std::path::PathBuf;

use agent_data_plane_config::{
    BootstrapConfiguration, ConfigViews, DatadogBootstrap, DatadogRuntimeAuthority, DogStatsDSalukiOnly,
    InternalConfigView, OtlpSalukiOnly, SalukiBootstrap, SalukiConfiguration, SalukiOnlyConfiguration,
    SourceConfigView, WorkloadSalukiOnly,
};
use datadog_agent_config::{drive, DatadogConfiguration, DatadogRemapper, KEY_ALIASES};
use saluki_component_config::{
    DatadogForwarderConfig, DogStatsDDebugLogConfig, DogStatsDPostAggregateFilterConfig, DogStatsDPrefixFilterConfig,
    MrfConfig, ScopedConfig, TagFilterlistConfig,
};
use saluki_config_tools::{ConfigurationError, ConfigurationLoader, GenericConfiguration};
use saluki_error::GenericError;
use tokio::sync::watch;
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
        let datadog = self.datadog.as_typed::<DatadogConfiguration>()?;
        let native = translate_datadog(&datadog, &self.saluki_only)?;
        let router = ConfigUpdateRouter::new(native.clone(), self.saluki_only.clone(), authority);
        let views = ConfigViews {
            raw: SourceConfigView::new(scrub_json(self.datadog.as_typed::<serde_json::Value>()?)),
            internal: InternalConfigView::new(scrub_json(serde_json::to_value(&native)?)),
        };

        Ok(StartedConfigurationSystem {
            saluki: native,
            handles: router.handles(),
            views,
            router,
            _saluki_source: self.saluki,
        })
    }
}

/// Started runtime configuration system.
pub struct StartedConfigurationSystem {
    saluki: SalukiConfiguration,
    handles: DynamicConfigHandles,
    views: ConfigViews,
    router: ConfigUpdateRouter,
    _saluki_source: GenericConfiguration,
}

impl StartedConfigurationSystem {
    /// Returns an owned copy of the native configuration.
    pub fn saluki(&self) -> SalukiConfiguration {
        self.saluki.clone()
    }

    /// Returns dynamic config handles for topology assembly.
    pub fn dynamic_handles(&self) -> DynamicConfigHandles {
        self.handles.clone()
    }

    /// Returns serialized config views.
    pub fn config_views(&self) -> ConfigViews {
        self.views.clone()
    }

    /// Returns the internal router for tests and future stream wiring.
    pub fn router(&mut self) -> &mut ConfigUpdateRouter {
        &mut self.router
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
            },
            DatadogRuntimeAuthority::Stream => {
                let (_, forwarder_tx) = ScopedConfig::live(initial.components.forwarder.datadog.clone());
                let (_, mrf_tx) = ScopedConfig::live(initial.components.metrics.multi_region_failover.clone());
                let (_, prefix_tx) = ScopedConfig::live(initial.components.dogstatsd.prefix_filter.clone());
                let (_, tag_filter_tx) = ScopedConfig::live(initial.components.dogstatsd.tag_filterlist.clone());
                let (_, post_aggregate_filter_tx) =
                    ScopedConfig::live(initial.components.dogstatsd.post_aggregate_filter.clone());
                let (_, debug_log_tx) = ScopedConfig::live(initial.components.dogstatsd.debug_log.clone());
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
            },
        }
    }

    /// Retranslates a Datadog snapshot and routes changed native slices.
    pub fn apply_datadog_snapshot(&mut self, snapshot: &DatadogConfiguration) -> Result<bool, GenericError> {
        let next = translate_datadog(snapshot, &self.saluki_only)?;
        let changed = self.route_changed_slices(&next);
        if changed {
            self.current = next;
        }
        Ok(changed)
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
        cmd_port: config.try_get_typed("cmd_port")?,
        auth_token_file_path: config.try_get_typed("auth_token_file_path")?,
    })
}

fn parse_saluki_bootstrap(config: &GenericConfiguration) -> Result<SalukiBootstrap, ConfigurationError> {
    Ok(SalukiBootstrap {
        config_path: config.try_get_typed("config_path")?,
    })
}

fn parse_saluki_only(config: &GenericConfiguration) -> Result<SalukiOnlyConfiguration, ConfigurationError> {
    let defaults = SalukiOnlyConfiguration::default();
    Ok(SalukiOnlyConfiguration {
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
    Ok(translator.finish())
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
