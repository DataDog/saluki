//! Helpers for tests: configuration from YAML, the environment provider, and host facts.

use std::{
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use agent_data_plane_config::{Live, SalukiConfiguration};
use agent_data_plane_config_system::{
    ConfigurationSystem, ConfigurationUpdates, EnvPrecedence, Error as ConfigurationError, LoadedConfiguration,
};
use arc_swap::ArcSwap;
use saluki_config::dynamic::{ConfigSetting, ConfigUpdate};
use saluki_core::{
    accounting::ComponentRegistry,
    data_model::tags::{SharedTagSet, Tag},
    health::HealthRegistry,
};
use saluki_metric_plan::operators::OperatorRegistry;
use tokio::sync::{mpsc, watch};

use super::{
    host_facts::{cluster_agent_configuration, host_tags_enabled, HostFacts},
    installer::ProgrammableMetrics,
    lanes::{LaneCounts, LaneSettings},
    lowering::Lowering,
};
use crate::internal::env::ADPEnvironmentProvider;

/// The configuration every test starts from: standalone, with the API key and hostname standalone mode requires.
const BASE: &str = "
api_key: lowering-primary-key
hostname: lowering-host
data_plane:
  standalone_mode: true
";

/// The lanes every test runs the metrics path on unless its configuration sets them: none, the layout before lanes,
/// rather than the default automatic counts, which follow the cores and decoders of the host running the test. Tests
/// of lanes set `lanes.mode: auto` and the counts they need, and
/// [`configuration_system_with_default_lanes`] keeps the defaults.
const LANES_OFF: &str = "
experimental:
  programmable_metrics:
    lanes:
      mode: off
";

/// The wall-clock time the process starts at in tests.
pub(super) const PROCESS_START_SECS: u64 = 1_700_000_000;

/// The workers of the primary runtime that the automatic lane counts of tests follow, so that a test's lanes do not
/// depend on the host running it: four, on which every kind of lane runs more than one lane.
pub(super) const TEST_WORKERS: usize = 4;

/// The host tags host facts carry in tests, when host tags enrichment is enabled.
pub(super) const HOST_TAGS: [&str; 2] = ["cloud_provider:example", "zone:a"];

/// Merges `overlay` into `base`: mappings merge member by member, and anything else in `overlay` replaces `base`.
fn merge(base: &mut serde_yaml::Value, overlay: serde_yaml::Value) {
    match (base, overlay) {
        (serde_yaml::Value::Mapping(base), serde_yaml::Value::Mapping(overlay)) => {
            for (key, value) in overlay {
                match base.get_mut(&key) {
                    Some(existing) => merge(existing, value),
                    None => {
                        base.insert(key, value);
                    }
                }
            }
        }
        (base, overlay) => *base = overlay,
    }
}

/// Loads the configuration system for the YAML configuration `yaml`, merged over the base configuration, with
/// environment variables ignored.
///
/// The local sources are the authority, as in standalone mode, even when `yaml` turns standalone mode off: this is how
/// tests describe a connected process without a Core Agent.
pub(super) async fn configuration_system(yaml: &str) -> ConfigurationSystem {
    layered_configuration_system(&[yaml]).await
}

/// Loads the configuration system for the YAML configurations `layers`, each merged over the previous ones and the
/// first over the base configuration, as [`configuration_system`] does for one.
pub(super) async fn layered_configuration_system(layers: &[&str]) -> ConfigurationSystem {
    loaded_configuration(layers)
        .await
        .standalone()
        .await
        .expect("test configuration is valid")
}

/// Loads the configuration system for the YAML configuration `yaml`, as [`configuration_system`] does, but with the
/// default lanes rather than none ([`LANES_OFF`]): the automatic counts unless `yaml` sets others.
pub(super) async fn configuration_system_with_default_lanes(yaml: &str) -> ConfigurationSystem {
    load_over(BASE, &[yaml])
        .await
        .expect("test configuration loads")
        .standalone()
        .await
        .expect("test configuration is valid")
}

/// Loads the local configuration for the YAML configurations `layers`, as [`layered_configuration_system`] does.
async fn loaded_configuration(layers: &[&str]) -> LoadedConfiguration {
    load(layers).await.expect("test configuration loads")
}

/// Loads the local configuration for the YAML configurations `layers`, each merged over the previous ones and the
/// first over the base configuration and the lanes of tests ([`LANES_OFF`]).
async fn load(layers: &[&str]) -> Result<LoadedConfiguration, ConfigurationError> {
    let base = [BASE, LANES_OFF].join("");
    load_over(&base, layers).await
}

/// Loads the local configuration for the YAML configurations `layers`, each merged over the previous ones and the
/// first over `base`.
async fn load_over(base: &str, layers: &[&str]) -> Result<LoadedConfiguration, ConfigurationError> {
    let mut config = serde_yaml::from_str::<serde_yaml::Value>(base).unwrap();
    for layer in layers.iter().filter(|layer| !layer.trim().is_empty()) {
        merge(
            &mut config,
            serde_yaml::from_str(layer).expect("test configuration is YAML"),
        );
    }
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("datadog.yaml");
    std::fs::write(&path, serde_yaml::to_string(&config).unwrap()).unwrap();
    LoadedConfiguration::load(&path, EnvPrecedence::Disabled).await
}

/// Returns why the YAML configuration `yaml`, merged over the base configuration, fails to load.
///
/// # Panics
///
/// Panics if the configuration loads.
pub(super) async fn configuration_error(yaml: &str) -> String {
    match load(&[yaml]).await {
        Ok(_) => panic!("the configuration loads: {}", yaml),
        Err(error) => format!("{:#}", error),
    }
}

/// Starts the configuration system on the connected path, as `run.rs` does with the Core Agent: the local
/// configuration `yaml`, merged over the base configuration, overlaid by what the configuration stream sends, which
/// starts with an empty snapshot. Returns the system, its updates, which apply what the stream sends next once they
/// run, and the stream, into which a test pushes updates.
///
/// The base configuration keeps standalone mode on, so the topology has the standalone shape (no liveness source and
/// no host tags, which need a Core Agent); the configuration still changes only through the stream.
pub(super) async fn connected_configuration_system(
    yaml: &str,
) -> (ConfigurationSystem, ConfigurationUpdates, mpsc::Sender<ConfigUpdate>) {
    connected_layered_configuration_system(&[yaml]).await
}

/// Starts the configuration system on the connected path as [`connected_configuration_system`] does, from the YAML
/// configurations `layers`, each merged over the previous ones and the first over the base configuration: a layer can
/// add keys under a section an earlier one has, such as `experimental`, which concatenating them as text cannot.
pub(super) async fn connected_layered_configuration_system(
    layers: &[&str],
) -> (ConfigurationSystem, ConfigurationUpdates, mpsc::Sender<ConfigUpdate>) {
    let loaded = loaded_configuration(layers).await;
    let (stream, updates) = mpsc::channel(64);
    stream
        .send(ConfigUpdate::snapshot([]))
        .await
        .expect("the stream is open");
    let (system, applied) = loaded.run(updates).await.expect("the initial snapshot is accepted");
    (system, applied, stream)
}

/// Returns an update of the configuration stream that sets the Datadog key `key` to `value`.
pub(super) fn update(key: &str, value: serde_json::Value) -> ConfigUpdate {
    ConfigUpdate::Partial(ConfigSetting::explicit(key, value))
}

/// Returns the configuration for the YAML configuration `yaml`, merged over the base configuration.
pub(super) async fn configuration(yaml: &str) -> SalukiConfiguration {
    layered_configuration(&[yaml]).await
}

/// Returns the configuration for the YAML configurations `layers`, as [`layered_configuration_system`] loads it.
pub(super) async fn layered_configuration(layers: &[&str]) -> SalukiConfiguration {
    SalukiConfiguration::clone(&layered_configuration_system(layers).await.config())
}

/// Returns an environment provider for `config` that needs no Core Agent: a fixed host, and no workload metadata.
pub(super) async fn environment_provider(config: &SalukiConfiguration) -> ADPEnvironmentProvider {
    let (provider, _) = ADPEnvironmentProvider::from_configuration(
        true,
        config.control.ipc.remote_agent_string_interner_size_bytes,
        &config.shared.environment,
        None,
        &ComponentRegistry::default(),
        &HealthRegistry::new(),
    )
    .await
    .expect("the base configuration has a hostname");
    provider
}

/// Returns the host facts of a process started at [`PROCESS_START_SECS`] with `config`, carrying [`HOST_TAGS`] when
/// host tags enrichment is enabled, as fetching them from the Core Agent would.
pub(super) fn facts(config: &SalukiConfiguration) -> HostFacts {
    HostFacts {
        process_start: process_start(),
        hostname: "lowering-host".to_string(),
        standalone: config.control.standalone_mode,
        host_tags: host_tags_enabled(config)
            .then(|| HOST_TAGS.iter().map(|tag| Tag::from(*tag)).collect::<SharedTagSet>()),
        cluster_agent_endpoint: cluster_agent_configuration(config).endpoint(),
    }
}

/// Readies TLS for the clients the delivery hosts of a test topology build: the default cryptography provider, and a
/// default root certificate store with no certificate, since a test's recipients never connect to a TLS endpoint.
pub(super) fn init_test_tls() {
    let _ = saluki_tls::initialize_default_crypto_provider();
    let _ = saluki_tls::initialize_empty_root_certificates();
}

/// Returns the frame hosts of a process with every fixed host, the Cluster Agent's included, for tests that lay a
/// lowered plan out themselves.
pub(super) fn frame_hosts() -> saluki_metric_plan::compile::FrameHosts {
    super::resources::FixedHosts::every().frame_hosts()
}

/// Returns the wall-clock time the process starts at in tests.
pub(super) fn process_start() -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(PROCESS_START_SECS)
}

/// A configuration that a test changes, as the configuration system does on an update.
pub(super) struct ConfigCell {
    cell: Arc<ArcSwap<SalukiConfiguration>>,
    tick: watch::Sender<()>,
}

impl ConfigCell {
    /// Creates a cell holding `config`.
    pub(super) fn new(config: SalukiConfiguration) -> Self {
        let (tick, _) = watch::channel(());
        Self {
            cell: Arc::new(ArcSwap::from_pointee(config)),
            tick,
        }
    }

    /// Returns a live view of the whole configuration, which wakes on every change.
    pub(super) fn live(&self) -> Live<SalukiConfiguration> {
        Live::new_dynamic(Arc::clone(&self.cell), self.tick.subscribe(), |config| config)
    }

    /// Replaces the configuration, waking every live view.
    pub(super) fn set(&self, config: SalukiConfiguration) {
        self.cell.store(Arc::new(config));
        self.tick.send_replace(());
    }
}

/// Prepares the programmable metrics path of a process started with `config` and the test host facts (see
/// [`facts`]), whose installer wakes on `live`; `None` if the configuration requires no metrics pipeline.
pub(super) fn programmable_metrics(
    config: &SalukiConfiguration, live: Live<SalukiConfiguration>, registry: &ComponentRegistry,
) -> Option<ProgrammableMetrics> {
    programmable_metrics_with_operators(config, live, registry, OperatorRegistry::native())
}

/// Prepares the programmable metrics path as [`programmable_metrics`] does, with the operators of `operators` in place
/// of the native ones.
///
/// # Panics
///
/// Panics if the configuration does not lower, or the metrics path cannot be prepared from it.
pub(super) fn programmable_metrics_with_operators(
    config: &SalukiConfiguration, live: Live<SalukiConfiguration>, registry: &ComponentRegistry,
    operators: OperatorRegistry,
) -> Option<ProgrammableMetrics> {
    // The lanes `config` asks for, on as many workers as the test topology's runtime would have.
    let lanes = LaneSettings::from_config(config, TEST_WORKERS).counts;
    prepared(config, live, registry, operators, lanes)
}

/// Prepares the programmable metrics path as [`programmable_metrics`] does, on `lanes` rather than the lanes `config`
/// asks for.
///
/// # Panics
///
/// Panics if the configuration does not lower, or the metrics path cannot be prepared from it.
pub(super) fn programmable_metrics_on(
    config: &SalukiConfiguration, live: Live<SalukiConfiguration>, registry: &ComponentRegistry, lanes: LaneCounts,
) -> Option<ProgrammableMetrics> {
    prepared(config, live, registry, OperatorRegistry::native(), lanes)
}

/// Prepares the programmable metrics path as [`programmable_metrics_with_operators`] does, on `lanes` rather than the
/// lanes `config` asks for.
///
/// # Panics
///
/// Panics if the configuration does not lower, or the metrics path cannot be prepared from it.
pub(super) fn programmable_metrics_with_operators_on(
    config: &SalukiConfiguration, live: Live<SalukiConfiguration>, registry: &ComponentRegistry,
    operators: OperatorRegistry, lanes: LaneCounts,
) -> Option<ProgrammableMetrics> {
    prepared(config, live, registry, operators, lanes)
}

fn prepared(
    config: &SalukiConfiguration, live: Live<SalukiConfiguration>, registry: &ComponentRegistry,
    operators: OperatorRegistry, lanes: LaneCounts,
) -> Option<ProgrammableMetrics> {
    let facts = facts(config);
    let lowering = Lowering::new(Arc::new(config.clone()), facts.clone()).expect("the configuration lowers")?;
    Some(
        ProgrammableMetrics::from_lowering(lowering, facts, config, live, registry, operators, lanes)
            .expect("the programmable metrics path is prepared"),
    )
}
