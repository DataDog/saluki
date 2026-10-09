//! Helpers for tests: configuration from YAML, the environment provider, and host facts.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use agent_data_plane_config::SalukiConfiguration;
use agent_data_plane_config_system::{ConfigurationSystem, EnvPrecedence, LoadedConfiguration};
use saluki_core::{
    accounting::ComponentRegistry,
    data_model::tags::{SharedTagSet, Tag},
    health::HealthRegistry,
};

use super::host_facts::{cluster_agent_configuration, host_tags_enabled, HostFacts};
use crate::internal::env::ADPEnvironmentProvider;

/// The configuration every test starts from: standalone, with the API key and hostname standalone mode requires.
const BASE: &str = "
api_key: lowering-primary-key
hostname: lowering-host
data_plane:
  standalone_mode: true
";

/// The wall-clock time the process starts at in tests.
pub(super) const PROCESS_START_SECS: u64 = 1_700_000_000;

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
    let mut config = serde_yaml::from_str::<serde_yaml::Value>(BASE).unwrap();
    for layer in layers.iter().filter(|layer| !layer.trim().is_empty()) {
        merge(
            &mut config,
            serde_yaml::from_str(layer).expect("test configuration is YAML"),
        );
    }
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("datadog.yaml");
    std::fs::write(&path, serde_yaml::to_string(&config).unwrap()).unwrap();
    LoadedConfiguration::load(&path, EnvPrecedence::Disabled)
        .await
        .expect("test configuration loads")
        .standalone()
        .await
        .expect("test configuration is valid")
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
        cluster_agent_endpoint: cluster_agent_configuration(config)
            .endpoint_and_token()
            .map(|(endpoint, _)| endpoint),
    }
}

/// Returns the wall-clock time the process starts at in tests.
pub(super) fn process_start() -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(PROCESS_START_SECS)
}
