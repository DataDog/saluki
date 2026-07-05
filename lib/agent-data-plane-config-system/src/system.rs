//! [`ConfigurationSystem`]: the runtime configuration, translated from the raw source map and kept
//! current as the map streams updates.

use std::sync::Arc;

use agent_data_plane_config::{Live, SalukiConfiguration};
use arc_swap::ArcSwap;
use datadog_agent_config::{apply_env_overlay, DatadogConfiguration, EnvOverlayMode, TranslateErrors};
use saluki_config::dynamic::ConfigChangeEvent;
use saluki_config::{ConfigurationError, GenericConfiguration};
use serde::Deserialize;
use snafu::Snafu;
use tokio::sync::{broadcast, watch};
use tracing::{debug, warn};

use crate::saluki_env_overlay;
use crate::saluki_only::SalukiOnly;
use crate::translators::DatadogTranslator;

/// An error building the translated configuration from the raw source map.
#[derive(Debug, Snafu)]
pub enum ConfigurationSystemError {
    /// The merged configuration value could not be read from the raw configuration map.
    #[snafu(context(false), display("{source}"))]
    Source {
        /// The underlying configuration error.
        source: ConfigurationError,
    },

    /// A source model could not be deserialized from the merged configuration value.
    #[snafu(context(false), display("{source}"))]
    Deserialize {
        /// The underlying deserialization error.
        source: serde_json::Error,
    },

    /// Translating the sources into the model failed on one or more keys.
    #[snafu(display("{source}"))]
    Translate {
        /// Every translation error recorded.
        source: TranslateErrors,
    },
}

/// The runtime configuration, translated from the raw source map and kept current.
///
/// Holds the raw source map, the current translated [`SalukiConfiguration`], and (when the source
/// map is dynamic) a background task that re-translates on every committed source update. The
/// current configuration lives in an [`ArcSwap`] cell so readers load a whole, self-consistent
/// version with no lock, while the background task replaces it in one atomic store.
pub struct ConfigurationSystem {
    raw_map: GenericConfiguration,
    current: Arc<ArcSwap<SalukiConfiguration>>,
    // Fired once after each accepted update so live views wake and re-project. Shared with the
    // router task via `Arc` because `watch::Sender` is not `Clone` and both the system (to mint
    // views) and the task (to notify) need it.
    tick: Arc<watch::Sender<()>>,
}

impl ConfigurationSystem {
    /// Translates `raw_map` into the initial configuration and, if the map streams updates, starts
    /// the task that keeps it current.
    ///
    /// Translation always yields a complete configuration; individual values that cannot be
    /// converted keep their defaults and are logged.
    ///
    /// # Errors
    ///
    /// Returns an error if the raw map cannot be deserialized into the source models.
    pub fn load(raw_map: GenericConfiguration, env_overlay: EnvOverlayMode) -> Result<Self, ConfigurationSystemError> {
        let (datadog, saluki_only) = deserialize_sources(&raw_map, env_overlay)?;
        let (config, errors) = translate(&datadog, &saluki_only);

        // Startup and runtime take opposite stances on translation errors. Startup is the strict
        // gate: this is the first, authoritative Agent snapshot, so any error fails the load and we
        // never boot on bad config. Runtime updates (see `apply`) must not fail or panic and take
        // the system down, and the bad value is already committed to the shared map, so they log
        // and keep operating on the valid config. The real fix is to sanitize Agent input before it
        // enters the shared map, a larger change deferred until this system proves itself.
        // TODO: sanitize source input before it reaches the shared config map.
        if let Some(errors) = errors {
            return Err(ConfigurationSystemError::Translate { source: errors });
        }

        let current = Arc::new(ArcSwap::from_pointee(config));
        // The initial receiver is dropped immediately; `send_replace` works with zero receivers, and
        // each live view subscribes its own receiver from the sender.
        let (tick, _) = watch::channel(());
        let tick = Arc::new(tick);

        // A dynamic source map broadcasts one event per committed key change. The task ignores the
        // event contents and re-reads the whole committed map, so it only exists when updates can
        // arrive; a static map has no sender and needs no task.
        if let Some(rx) = raw_map.subscribe_for_updates() {
            spawn_router(
                rx,
                raw_map.clone(),
                Arc::clone(&current),
                Arc::clone(&tick),
                env_overlay,
            );
        }

        Ok(Self { raw_map, current, tick })
    }

    /// Returns a live view of the given projection of the current configuration. Narrow further with
    /// [`Live::project`]. This is the only way a consumer subscribes to runtime updates.
    pub fn live<T>(&self, project: impl for<'a> Fn(&'a SalukiConfiguration) -> &'a T + Send + Sync + 'static) -> Live<T>
    where
        T: Clone + PartialEq + 'static,
    {
        Live::dynamic(Arc::clone(&self.current), self.tick.subscribe(), project)
    }

    /// Loads the current translated configuration.
    ///
    /// The returned guard pins one whole version; a concurrent refresh never tears the read.
    pub fn config(&self) -> arc_swap::Guard<Arc<SalukiConfiguration>> {
        self.current.load()
    }

    /// Returns a shared handle to the current-configuration cell for readers that load it
    /// independently.
    pub fn current_handle(&self) -> Arc<ArcSwap<SalukiConfiguration>> {
        Arc::clone(&self.current)
    }

    /// Returns the raw source map for consumers that read configuration by key.
    pub fn raw_map(&self) -> GenericConfiguration {
        self.raw_map.clone()
    }
}

/// Spawns the task that keeps `current` up to date as `raw_map` commits updates.
fn spawn_router(
    rx: broadcast::Receiver<ConfigChangeEvent>, raw_map: GenericConfiguration,
    current: Arc<ArcSwap<SalukiConfiguration>>, tick: Arc<watch::Sender<()>>, env_overlay: EnvOverlayMode,
) {
    tokio::spawn(router_loop(rx, raw_map, current, tick, env_overlay));
}

/// Re-translates the committed source map whenever it changes and stores the result.
///
/// Blocks until an update commits, drains any further updates that piled up without translating
/// between them, then translates the committed map once. Update events carry no payload the task
/// needs, so a dropped or lagged event is just another signal to re-read the committed map. The
/// task ends when the source map's sender is gone, since no further updates can arrive.
async fn router_loop(
    mut rx: broadcast::Receiver<ConfigChangeEvent>, raw_map: GenericConfiguration,
    current: Arc<ArcSwap<SalukiConfiguration>>, tick: Arc<watch::Sender<()>>, env_overlay: EnvOverlayMode,
) {
    loop {
        match rx.recv().await {
            Ok(_) => {}
            Err(broadcast::error::RecvError::Lagged(_)) => {}
            Err(broadcast::error::RecvError::Closed) => return,
        }
        // Collapse a burst of committed changes into a single re-translation.
        loop {
            match rx.try_recv() {
                Ok(_) => continue,
                Err(broadcast::error::TryRecvError::Empty) => break,
                Err(broadcast::error::TryRecvError::Lagged(_)) => continue,
                Err(broadcast::error::TryRecvError::Closed) => break,
            }
        }
        apply(&current, &raw_map, &tick, env_overlay);
    }
}

/// Re-translates the committed `raw_map` and stores the result as the current configuration.
///
/// Never fails: unlike `load`'s startup gate, a runtime update cannot take the system down. A source
/// that cannot be deserialized retains the current configuration (no partial value is recoverable).
/// A translation error does not: the invalid value keeps its default, the complete config is stored
/// and logged, and later valid updates still take effect, so one bad value never wedges the system.
fn apply(
    current: &ArcSwap<SalukiConfiguration>, raw_map: &GenericConfiguration, tick: &watch::Sender<()>,
    env_overlay: EnvOverlayMode,
) {
    let (datadog, saluki_only) = match deserialize_sources(raw_map, env_overlay) {
        Ok(sources) => sources,
        Err(e) => {
            warn!(error = %e, "Rejecting configuration update: sources could not be deserialized. Retaining current configuration.");
            return;
        }
    };
    let (next, errors) = translate(&datadog, &saluki_only);
    if let Some(errors) = errors {
        warn!(%errors, "Configuration update had translation errors; invalid values kept their defaults. Applying the remaining valid configuration.");
    }
    // Store the new config, then notify. A woken `Live` view reads this same cell, so store-before-
    // notify guarantees it sees the stored value; the baseline (`/config/internal`, `current_handle`)
    // and every view share this one source of truth, so they cannot disagree.
    current.store(Arc::new(next));
    tick.send_replace(());
    debug!("Applied configuration update.");
}

/// Deserializes both source models from the committed configuration map.
///
/// figment merges every provider into one value; we then deserialize each source model from that
/// merged `serde_json::Value` rather than calling figment's per-type `extract`, which would
/// instantiate the giant generated `DatadogConfiguration` visitor through figment once per model
/// (measured ~1.6 MiB of binary). The source models use no figment magic types (path fields are
/// plain strings), so nothing is lost.
///
/// Transitional: this seam goes away when `GenericConfiguration` is eliminated.
///
/// Environment variables reach the merged value as flat keys (`autoscaling_failover_enabled`), which
/// neither nested source reads. Each source has its own overlay applied before its deserialize, per
/// `env_overlay`: `saluki_env_overlay::apply` for the Saluki-only keys and
/// `apply_env_overlay` for the Datadog keys. The two cover disjoint key sets, so relocating one
/// source's keys is inert for the other.
fn deserialize_sources(
    raw_map: &GenericConfiguration, env_overlay: EnvOverlayMode,
) -> Result<(DatadogConfiguration, SalukiOnly), ConfigurationSystemError> {
    let mut merged = raw_map.as_typed::<serde_json::Value>()?;
    saluki_env_overlay::apply(&mut merged, env_overlay);
    let saluki_only = SalukiOnly::deserialize(&merged)?;
    apply_env_overlay(&mut merged, env_overlay);
    normalize_datadog_input_forms(&mut merged);
    let datadog = DatadogConfiguration::deserialize(&merged)?;
    Ok((datadog, saluki_only))
}

/// Coerces the handful of Datadog keys whose accepted input shapes are wider than the generated
/// `DatadogConfiguration` deserializer allows, restoring the alternate scalar forms the deleted
/// component serde used to accept. Env-var-sourced values land in `merged` as flat string keys, so
/// running here (after `apply_env_overlay`, before `DatadogConfiguration::deserialize`) catches the
/// env-var forms too. Scoped to the specific keys below; not a general coercion framework.
fn normalize_datadog_input_forms(merged: &mut serde_json::Value) {
    let Some(object) = merged.as_object_mut() else {
        return;
    };

    // `dogstatsd_eol_required` is a `Vec<String>` in the schema, but the Agent passes list-typed env
    // vars as a single space-separated string. Split it into an array (matching
    // `deserialize_space_separated_or_seq`: split on any run of whitespace, dropping empty tokens).
    if let Some(value @ serde_json::Value::String(_)) = object.get_mut("dogstatsd_eol_required") {
        let tokens = value
            .as_str()
            .expect("value matched String")
            .split_whitespace()
            .map(|token| serde_json::Value::String(token.to_owned()))
            .collect();
        *value = serde_json::Value::Array(tokens);
    }

    // `dogstatsd_mapper_profiles` is a `Vec<Value>` in the schema, but the Agent passes it as a JSON
    // string when set via env var. Parse the string into the array the translator expects. If it is
    // not valid JSON, leave the string in place so the downstream deserializer surfaces the error.
    if let Some(value @ serde_json::Value::String(_)) = object.get_mut("dogstatsd_mapper_profiles") {
        if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(value.as_str().expect("value matched String")) {
            *value = parsed;
        }
    }

    // `additional_endpoints` is a `HashMap<String, Vec<String>>` in the schema, but the deleted
    // component serde accepted two wider shapes the generated deserializer rejects: the whole value
    // as a JSON string (the form an env var produces, for example
    // `DD_ADDITIONAL_ENDPOINTS='{"https://app.datadoghq.com":["key"]}'`), and a bare string in place
    // of a one-element key list for a host. Restore both so dual-shipping config keeps deserializing.
    // First parse the JSON-string form into the object the deserializer expects; if it is not valid
    // JSON, leave the string in place so the downstream deserializer surfaces the error.
    if let Some(value @ serde_json::Value::String(_)) = object.get_mut("additional_endpoints") {
        if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(value.as_str().expect("value matched String")) {
            *value = parsed;
        }
    }
    // Then wrap any bare-string host value into a one-element array (matching the old `OneOrMany`).
    if let Some(serde_json::Value::Object(map)) = object.get_mut("additional_endpoints") {
        for keys in map.values_mut() {
            if matches!(keys, serde_json::Value::String(_)) {
                let single = std::mem::take(keys);
                *keys = serde_json::Value::Array(vec![single]);
            }
        }
    }
}

/// Translates the Datadog and Saluki-only sources into one [`SalukiConfiguration`], returning every
/// error recorded while converting an individual Datadog value.
///
/// The Saluki-only values seed the base first (defaults plus any Saluki-only knob). The Datadog
/// `drive` then overlays every supported key and is authoritative for every field it owns: a value
/// that cannot be converted leaves its field at the base value and records an error. The returned
/// configuration is always complete: every valid value is present, and every invalid one holds its
/// default.
fn translate(
    datadog: &DatadogConfiguration, saluki_only: &SalukiOnly,
) -> (SalukiConfiguration, Option<TranslateErrors>) {
    let mut base = SalukiConfiguration::default();
    saluki_only.seed(&mut base);
    DatadogTranslator::new(datadog, base).translate()
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use agent_data_plane_config::domains::dogstatsd::OriginTagCardinality;
    use agent_data_plane_config::{Live, SalukiConfiguration};
    use datadog_agent_config::{DatadogConfiguration, EnvOverlayMode};
    use saluki_config::dynamic::ConfigUpdate;
    use saluki_config::ConfigurationLoader;
    use serde_json::json;

    use super::{translate, ConfigurationSystem, ConfigurationSystemError, SalukiOnly};

    /// Waits until the sibling receiver observes the commit of `key`, so the source map is known to
    /// hold the updated value before the current configuration is inspected.
    async fn await_commit(rx: &mut tokio::sync::broadcast::Receiver<super::ConfigChangeEvent>, key: &str) {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                match rx.recv().await {
                    Ok(ev) if ev.key == key => break,
                    Ok(_) => continue,
                    Err(e) => panic!("update channel closed before `{key}`: {e}"),
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for `{key}` commit"));
    }

    /// Polls the current configuration until `predicate` holds, failing if it never does.
    async fn await_config(system: &ConfigurationSystem, what: &str, predicate: impl Fn(&SalukiConfiguration) -> bool) {
        tokio::time::timeout(Duration::from_secs(2), async {
            while !predicate(&system.config()) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
    }

    #[tokio::test]
    async fn startup_current_reflects_translation() {
        let (raw_map, _) = ConfigurationLoader::for_tests(
            Some(json!({ "log_level": "warn", "dogstatsd_port": 9125 })),
            None,
            false,
        )
        .await;

        let system = ConfigurationSystem::load(raw_map, EnvOverlayMode::Fallback).expect("system builds");
        let config = system.config();

        assert_eq!(config.control.logging.level, "warn");
        assert_eq!(config.domains.dogstatsd.listeners.port, 9125);
    }

    #[tokio::test]
    async fn flat_env_key_overlays_onto_nested_datadog_slot() {
        // A flat, underscore-joined key (the shape an environment variable produces) is relocated
        // into the nested `autoscaling.failover.*` slot the Datadog deserializer reads. The string
        // list is split on whitespace.
        let (raw_map, _) = ConfigurationLoader::for_tests(
            Some(json!({
                "autoscaling_failover_enabled": true,
                "autoscaling_failover_metrics": "container.memory.usage container.cpu.usage",
            })),
            None,
            false,
        )
        .await;

        let system = ConfigurationSystem::load(raw_map, EnvOverlayMode::Fallback).expect("system builds");
        let config = system.config();

        assert!(config.shared.autoscaling_failover.enabled);
        assert_eq!(
            config.shared.autoscaling_failover.metrics,
            vec!["container.memory.usage".to_string(), "container.cpu.usage".to_string()]
        );
    }

    #[tokio::test]
    async fn saluki_only_dotted_env_key_seeds_the_model() {
        // A dotted Saluki-only key set only by environment variable arrives as the flat figment key
        // `data_plane_standalone_mode`, which the nested `SalukiOnly` never reads. The overlay must
        // relocate it so it seeds `control.standalone_mode`.
        let (raw_map, _) = ConfigurationLoader::for_tests(
            None,
            Some(&[("DATA_PLANE_STANDALONE_MODE".to_string(), "true".to_string())]),
            false,
        )
        .await;
        raw_map.ready().await;

        let system = ConfigurationSystem::load(raw_map, EnvOverlayMode::Fallback).expect("system builds");

        assert!(system.config().control.standalone_mode);
    }

    #[tokio::test]
    async fn disabled_overlay_leaves_flat_env_key_unread() {
        // With the overlay disabled, the flat key is never relocated, so the nested deserializer
        // does not see it and the field keeps its default.
        let (raw_map, _) =
            ConfigurationLoader::for_tests(Some(json!({ "autoscaling_failover_enabled": true })), None, false).await;

        let system = ConfigurationSystem::load(raw_map, EnvOverlayMode::Disabled).expect("system builds");

        assert!(!system.config().shared.autoscaling_failover.enabled);
    }

    #[tokio::test]
    async fn load_fails_on_translation_invalid_startup_config() {
        // Startup is the strict gate: a value figment accepts but the model rejects fails the load,
        // so the process never boots on bad config.
        let (raw_map, _) =
            ConfigurationLoader::for_tests(Some(json!({ "dogstatsd_tag_cardinality": "bogus" })), None, false).await;

        let result = ConfigurationSystem::load(raw_map, EnvOverlayMode::Fallback);

        assert!(matches!(result, Err(ConfigurationSystemError::Translate { .. })));
    }

    #[tokio::test]
    async fn dogstatsd_unsigned_values_reject_negative_startup_input() {
        let keys = [
            "dogstatsd_buffer_size",
            "dogstatsd_capture_depth",
            "dogstatsd_context_expiry_seconds",
            "dogstatsd_mapper_cache_size",
            "dogstatsd_port",
            "dogstatsd_so_rcvbuf",
            "dogstatsd_string_interner_size",
            "statsd_forward_port",
        ];

        for key in keys {
            let mut values = serde_json::Map::new();
            values.insert(key.to_string(), json!(-1));
            let (raw_map, _) =
                ConfigurationLoader::for_tests(Some(serde_json::Value::Object(values)), None, false).await;

            let result = ConfigurationSystem::load(raw_map, EnvOverlayMode::Fallback);
            let Err(error) = result else {
                panic!("negative `{key}` should fail startup translation");
            };
            let message = error.to_string();
            assert!(matches!(error, ConfigurationSystemError::Translate { .. }));
            assert!(message.contains(key), "error should identify `{key}`: {message}");
            assert!(
                message.contains("-1"),
                "error should include the invalid value: {message}"
            );
        }
    }

    #[tokio::test]
    async fn dogstatsd_ports_reject_values_above_u16_max() {
        for key in ["dogstatsd_port", "statsd_forward_port"] {
            let mut values = serde_json::Map::new();
            values.insert(key.to_string(), json!(u16::MAX as u64 + 1));
            let (raw_map, _) =
                ConfigurationLoader::for_tests(Some(serde_json::Value::Object(values)), None, false).await;

            let result = ConfigurationSystem::load(raw_map, EnvOverlayMode::Fallback);
            let Err(error) = result else {
                panic!("out-of-range `{key}` should fail startup translation");
            };
            let message = error.to_string();
            assert!(matches!(error, ConfigurationSystemError::Translate { .. }));
            assert!(message.contains(key), "error should identify `{key}`: {message}");
            assert!(
                message.contains("65536"),
                "error should include the invalid value: {message}"
            );
        }
    }

    #[tokio::test]
    async fn dogstatsd_numeric_boundary_values_translate() {
        let (raw_map, _) = ConfigurationLoader::for_tests(
            Some(json!({
                "dogstatsd_buffer_size": 0,
                "dogstatsd_capture_depth": 0,
                "dogstatsd_context_expiry_seconds": 0,
                "dogstatsd_mapper_cache_size": 0,
                "dogstatsd_port": u16::MAX,
                "dogstatsd_so_rcvbuf": 0,
                "dogstatsd_string_interner_size": 0,
                "statsd_forward_port": u16::MAX,
            })),
            None,
            false,
        )
        .await;

        let system = ConfigurationSystem::load(raw_map, EnvOverlayMode::Fallback).expect("boundary values translate");
        let dogstatsd = &system.config().domains.dogstatsd;
        assert_eq!(dogstatsd.listeners.buffer_size, 0);
        assert_eq!(dogstatsd.listeners.capture_depth, 0);
        assert_eq!(dogstatsd.aggregation.context_expiry_seconds, 0);
        assert_eq!(dogstatsd.mapper.cache_size, 0);
        assert_eq!(dogstatsd.listeners.port, u16::MAX);
        assert_eq!(dogstatsd.listeners.so_rcvbuf, 0);
        assert_eq!(dogstatsd.contexts.string_interner_size, 0);
        assert_eq!(dogstatsd.listeners.forward_port, u16::MAX);
    }

    #[tokio::test]
    async fn mapper_profile_missing_mappings_fails_startup_translation() {
        let (raw_map, _) = ConfigurationLoader::for_tests(
            Some(json!({
                "dogstatsd_mapper_profiles": [{ "name": "p", "prefix": "x" }],
            })),
            None,
            false,
        )
        .await;

        let result = ConfigurationSystem::load(raw_map, EnvOverlayMode::Fallback);
        let Err(error) = result else {
            panic!("a mapper profile without `mappings` should fail startup translation");
        };
        let message = error.to_string();
        assert!(matches!(error, ConfigurationSystemError::Translate { .. }));
        assert!(message.contains("dogstatsd_mapper_profiles"));
        assert!(message.contains("missing field `mappings`"));
    }

    #[tokio::test]
    async fn mapper_profile_explicit_empty_mappings_is_valid() {
        let (raw_map, _) = ConfigurationLoader::for_tests(
            Some(json!({
                "dogstatsd_mapper_profiles": [{ "name": "p", "prefix": "x", "mappings": [] }],
            })),
            None,
            false,
        )
        .await;

        let system = ConfigurationSystem::load(raw_map, EnvOverlayMode::Fallback).expect("empty mappings translate");
        let profiles = &system.config().domains.dogstatsd.mapper.profiles;
        assert_eq!(profiles.len(), 1);
        assert_eq!(profiles[0].name, "p");
        assert_eq!(profiles[0].prefix, "x");
        assert!(profiles[0].mappings.is_empty());
    }

    /// `dogstatsd_eol_required` accepts a space-separated string (the Agent's list-typed env-var
    /// form) as well as a native array, both landing on `listeners.eol_required`.
    #[tokio::test]
    async fn eol_required_accepts_space_separated_string_or_array() {
        for value in [json!("udp uds"), json!(["udp", "uds"])] {
            let (raw_map, _) =
                ConfigurationLoader::for_tests(Some(json!({ "dogstatsd_eol_required": value })), None, false).await;

            let system = ConfigurationSystem::load(raw_map, EnvOverlayMode::Fallback).expect("eol_required translates");
            assert_eq!(
                system.config().domains.dogstatsd.listeners.eol_required,
                vec!["udp", "uds"]
            );
        }
    }

    /// `dogstatsd_mapper_profiles` accepts a JSON string (the Agent's env-var form) as well as a
    /// native array, both landing on `mapper.profiles`.
    #[tokio::test]
    async fn mapper_profiles_accepts_json_string_or_array() {
        for value in [
            json!("[{\"name\":\"p\",\"prefix\":\"x\",\"mappings\":[]}]"),
            json!([{ "name": "p", "prefix": "x", "mappings": [] }]),
        ] {
            let (raw_map, _) =
                ConfigurationLoader::for_tests(Some(json!({ "dogstatsd_mapper_profiles": value })), None, false).await;

            let system =
                ConfigurationSystem::load(raw_map, EnvOverlayMode::Fallback).expect("mapper profiles translate");
            let profiles = &system.config().domains.dogstatsd.mapper.profiles;
            assert_eq!(profiles.len(), 1);
            assert_eq!(profiles[0].name, "p");
            assert_eq!(profiles[0].prefix, "x");
        }
    }

    /// `additional_endpoints` accepts a JSON string (the Agent's env-var form) and a bare string in
    /// place of a one-element key list, as well as the native map-of-lists, all landing on
    /// `shared.endpoints.additional_endpoints`.
    #[tokio::test]
    async fn additional_endpoints_accepts_json_string_scalar_or_map() {
        let expected: std::collections::HashMap<String, Vec<String>> =
            [("https://app.datadoghq.com".to_string(), vec!["key".to_string()])]
                .into_iter()
                .collect();

        for value in [
            // Env-var form: the whole map arrives as a JSON string.
            json!("{\"https://app.datadoghq.com\":[\"key\"]}"),
            // Env-var form with a bare string value in place of a one-element key list.
            json!("{\"https://app.datadoghq.com\":\"key\"}"),
            // Native map with a bare string in place of a one-element key list.
            json!({ "https://app.datadoghq.com": "key" }),
            // Native map of key lists.
            json!({ "https://app.datadoghq.com": ["key"] }),
        ] {
            let (raw_map, _) =
                ConfigurationLoader::for_tests(Some(json!({ "additional_endpoints": value })), None, false).await;

            let system =
                ConfigurationSystem::load(raw_map, EnvOverlayMode::Fallback).expect("additional_endpoints translates");
            assert_eq!(system.config().shared.endpoints.additional_endpoints, expected);
        }
    }

    #[tokio::test]
    async fn mapper_profile_other_required_fields_remain_required() {
        let cases = [
            (json!({ "prefix": "x", "mappings": [] }), "missing field `name`"),
            (json!({ "name": "p", "mappings": [] }), "missing field `prefix`"),
            (
                json!({
                    "name": "p",
                    "prefix": "x",
                    "mappings": [{ "name": "mapped" }],
                }),
                "missing field `match`",
            ),
            (
                json!({
                    "name": "p",
                    "prefix": "x",
                    "mappings": [{ "match": "x.*" }],
                }),
                "missing field `name`",
            ),
        ];

        for (profile, expected) in cases {
            let (raw_map, _) =
                ConfigurationLoader::for_tests(Some(json!({ "dogstatsd_mapper_profiles": [profile] })), None, false)
                    .await;

            let result = ConfigurationSystem::load(raw_map, EnvOverlayMode::Fallback);
            let Err(error) = result else {
                panic!("mapper profile should fail with {expected}");
            };
            let message = error.to_string();
            assert!(matches!(error, ConfigurationSystemError::Translate { .. }));
            assert!(message.contains(expected), "expected `{expected}` in: {message}");
        }
    }

    #[tokio::test]
    async fn translation_invalid_update_applies_with_defaults_and_preserves_valid_values() {
        let (raw_map, sender) = ConfigurationLoader::for_tests(
            Some(json!({ "log_level": "warn", "dogstatsd_tag_cardinality": "high" })),
            None,
            true,
        )
        .await;
        let sender = sender.expect("dynamic sender exists");
        sender.send(ConfigUpdate::Snapshot(json!({}))).await.unwrap();
        raw_map.ready().await;

        let system = ConfigurationSystem::load(raw_map.clone(), EnvOverlayMode::Fallback).expect("system builds");
        assert_eq!(
            system.config().domains.dogstatsd.origin.tag_cardinality,
            OriginTagCardinality::High
        );

        let mut rx = raw_map.subscribe_for_updates().expect("updates enabled");
        // A translation-invalid value no longer wedges the system: the update is applied, the
        // invalid field falls back to its default, and unrelated valid values are preserved.
        sender
            .send(ConfigUpdate::Partial {
                key: "dogstatsd_tag_cardinality".to_string(),
                value: json!("bogus"),
            })
            .await
            .unwrap();
        await_commit(&mut rx, "dogstatsd_tag_cardinality").await;

        await_config(&system, "the invalid value to fall back to its default", |c| {
            c.domains.dogstatsd.origin.tag_cardinality == OriginTagCardinality::Low
        })
        .await;
        assert_eq!(system.config().control.logging.level, "warn");
    }

    #[tokio::test]
    async fn converges_to_latest_value_under_burst() {
        let (raw_map, sender) = ConfigurationLoader::for_tests(Some(json!({ "log_level": "info" })), None, true).await;
        let sender = sender.expect("dynamic sender exists");
        sender.send(ConfigUpdate::Snapshot(json!({}))).await.unwrap();
        raw_map.ready().await;

        let system = ConfigurationSystem::load(raw_map.clone(), EnvOverlayMode::Fallback).expect("system builds");

        let burst = [
            "warn", "error", "debug", "trace", "info", "warn", "error", "debug", "trace", "info", "warn", "error",
            "debug", "trace", "info", "warn", "error", "debug", "trace",
        ];
        for (i, level) in burst.iter().enumerate() {
            sender
                .send(ConfigUpdate::Partial {
                    key: "log_level".to_string(),
                    value: json!(level),
                })
                .await
                .unwrap();
            // Interleave a translation-invalid update mid-burst, then correct it. The invalid value
            // no longer wedges the task: each re-translation still applies, so the baseline keeps
            // converging on the latest valid value regardless of the transient bad one.
            if i == burst.len() / 2 {
                sender
                    .send(ConfigUpdate::Partial {
                        key: "dogstatsd_tag_cardinality".to_string(),
                        value: json!("bogus"),
                    })
                    .await
                    .unwrap();
                sender
                    .send(ConfigUpdate::Partial {
                        key: "dogstatsd_tag_cardinality".to_string(),
                        value: json!("high"),
                    })
                    .await
                    .unwrap();
            }
        }
        let final_level = "error";
        sender
            .send(ConfigUpdate::Partial {
                key: "log_level".to_string(),
                value: json!(final_level),
            })
            .await
            .unwrap();

        await_config(
            &system,
            "the current configuration to converge to the final value",
            |c| c.control.logging.level == final_level,
        )
        .await;
        assert_eq!(system.config().control.logging.level, final_level);
    }

    #[tokio::test]
    async fn live_view_observes_debug_log_update() {
        let (raw_map, sender) =
            ConfigurationLoader::for_tests(Some(json!({ "dogstatsd_metrics_stats_enable": false })), None, true).await;
        let sender = sender.expect("dynamic sender exists");
        sender.send(ConfigUpdate::Snapshot(json!({}))).await.unwrap();
        raw_map.ready().await;

        let system = ConfigurationSystem::load(raw_map.clone(), EnvOverlayMode::Fallback).expect("system builds");
        let mut view = system.live(|c| &c.domains.dogstatsd.debug_log);
        assert!(!view.current().metrics_stats_enable);

        sender
            .send(ConfigUpdate::Partial {
                key: "dogstatsd_metrics_stats_enable".to_string(),
                value: json!(true),
            })
            .await
            .unwrap();

        tokio::time::timeout(Duration::from_secs(2), view.changed())
            .await
            .expect("view observes the debug-log update");
        assert!(view.current().metrics_stats_enable);
        // `Deref` reflects the value observed at the last `changed`.
        assert!(view.metrics_stats_enable);
    }

    #[tokio::test]
    async fn field_view_wakes_on_its_field() {
        // Projecting straight to a single field needs no schema change and no central registration:
        // the granularity is chosen at the call site.
        let (raw_map, sender) =
            ConfigurationLoader::for_tests(Some(json!({ "dogstatsd_metrics_stats_enable": false })), None, true).await;
        let sender = sender.expect("dynamic sender exists");
        sender.send(ConfigUpdate::Snapshot(json!({}))).await.unwrap();
        raw_map.ready().await;

        let system = ConfigurationSystem::load(raw_map.clone(), EnvOverlayMode::Fallback).expect("system builds");
        let mut stats = system.live(|c| &c.domains.dogstatsd.debug_log.metrics_stats_enable);
        assert!(!stats.current());

        sender
            .send(ConfigUpdate::Partial {
                key: "dogstatsd_metrics_stats_enable".to_string(),
                value: json!(true),
            })
            .await
            .unwrap();

        tokio::time::timeout(Duration::from_secs(2), stats.changed())
            .await
            .expect("field view observes its field's update");
        assert!(stats.current());
        assert!(*stats);
    }

    #[tokio::test]
    async fn fixed_view_never_changes() {
        let mut view: Live<bool> = Live::fixed(true);
        assert!(view.current());
        assert!(*view);
        // A fixed view never resolves, so this bound is deterministic rather than timing-dependent.
        assert!(tokio::time::timeout(Duration::from_millis(100), view.changed())
            .await
            .is_err());
    }

    #[tokio::test]
    async fn live_views_reflect_startup_configuration() {
        let (raw_map, _) =
            ConfigurationLoader::for_tests(Some(json!({ "dogstatsd_metrics_stats_enable": true })), None, false).await;

        let system = ConfigurationSystem::load(raw_map, EnvOverlayMode::Fallback).expect("system builds");
        let config = system.config();

        assert_eq!(
            system.live(|c| &c.domains.dogstatsd.debug_log).current(),
            config.domains.dogstatsd.debug_log
        );
        assert_eq!(
            system.live(|c| &c.domains.dogstatsd.prefix_filter).current(),
            config.domains.dogstatsd.prefix_filter
        );
        assert_eq!(
            system.live(|c| &c.domains.multi_region_failover).current(),
            config.domains.multi_region_failover
        );
    }

    #[tokio::test]
    async fn empty_config_preserves_otlp_receiver_effective_defaults() {
        // Regression (PR #1989): the OTLP receiver keys are witnessed against the vendored Datadog
        // schema, whose defaults (localhost:4317/4318, logs disabled) diverge from ADP's historical
        // effective defaults. With the keys flagged `saluki_overrides_default`, an empty raw config
        // must resolve to ADP's defaults, not the Agent's.
        let (raw_map, _) = ConfigurationLoader::for_tests(Some(json!({})), None, false).await;

        let system = ConfigurationSystem::load(raw_map, EnvOverlayMode::Fallback).expect("system builds");
        let receiver = &system.config().domains.otlp.receiver;

        assert!(receiver.logs_enabled, "OTLP logs default to enabled");
        assert_eq!(receiver.grpc.endpoint, "0.0.0.0:4317");
        assert_eq!(receiver.http.endpoint, "0.0.0.0:4318");
    }

    #[tokio::test]
    async fn explicit_otlp_logs_enabled_false_is_preserved() {
        // An explicit `false` must still win over the ADP default: the flagged key is generated as
        // an absence-aware Option, so a present value round-trips.
        let (raw_map, _) = ConfigurationLoader::for_tests(
            Some(json!({ "otlp_config": { "logs": { "enabled": false } } })),
            None,
            false,
        )
        .await;

        let system = ConfigurationSystem::load(raw_map, EnvOverlayMode::Fallback).expect("system builds");

        assert!(!system.config().domains.otlp.receiver.logs_enabled);
    }

    #[tokio::test]
    async fn explicit_otlp_receiver_endpoints_pass_through() {
        // Explicit endpoints must pass through unchanged for both native and proxy modes (both read
        // these same model fields).
        let (raw_map, _) = ConfigurationLoader::for_tests(
            Some(json!({
                "otlp_config": {
                    "receiver": {
                        "protocols": {
                            "grpc": { "endpoint": "10.0.0.1:5317" },
                            "http": { "endpoint": "10.0.0.1:5318" },
                        }
                    }
                }
            })),
            None,
            false,
        )
        .await;

        let system = ConfigurationSystem::load(raw_map, EnvOverlayMode::Fallback).expect("system builds");
        let receiver = &system.config().domains.otlp.receiver;

        assert_eq!(receiver.grpc.endpoint, "10.0.0.1:5317");
        assert_eq!(receiver.http.endpoint, "10.0.0.1:5318");
    }

    #[tokio::test]
    async fn otlp_logs_enabled_env_var_is_honored() {
        // The environment-variable form (DD_OTLP_CONFIG_LOGS_ENABLED, here under the TEST_ prefix)
        // must still reach the flagged key and win over the ADP default.
        let (raw_map, _) = ConfigurationLoader::for_tests(
            None,
            Some(&[("OTLP_CONFIG_LOGS_ENABLED".to_string(), "false".to_string())]),
            false,
        )
        .await;
        raw_map.ready().await;

        let system = ConfigurationSystem::load(raw_map, EnvOverlayMode::Fallback).expect("system builds");

        assert!(!system.config().domains.otlp.receiver.logs_enabled);
    }

    #[test]
    fn translate_small_map_through_witness_and_seed() {
        // A small raw Datadog source map exercising a scalar conversion, an enum parse, a
        // duration parse, and the raw endpoint inputs.
        let datadog: DatadogConfiguration = serde_json::from_value(json!({
            "api_key": "abc",
            "dd_url": "https://custom.example.com",
            "dogstatsd_port": 9125,
            "dogstatsd_tag_cardinality": "high",
            "expected_tags_duration": "15s",
            "telemetry": { "dogstatsd_origin": true },
        }))
        .expect("datadog source deserializes");

        // A small Saluki-only source setting one seeded field.
        let saluki_only: SalukiOnly = serde_json::from_value(json!({
            "dogstatsd_tcp_port": 8126,
        }))
        .expect("saluki-only source deserializes");

        let (config, errors) = translate(&datadog, &saluki_only);
        assert!(errors.is_none(), "translation of a valid map records no error");

        // Driven scalar conversion: i64 -> u16.
        assert_eq!(config.domains.dogstatsd.listeners.port, 9125);
        // Driven enum parse.
        assert_eq!(
            config.domains.dogstatsd.origin.tag_cardinality,
            OriginTagCardinality::High
        );
        // Driven `format: duration` parse: a Go duration string becomes a `Duration`.
        assert_eq!(config.shared.tags.expected_tags_duration, Duration::from_secs(15));
        // Driven bool in a nested Datadog section.
        assert!(config.domains.dogstatsd.telemetry.origin_breakdown);
        // Raw endpoint inputs: carried through without resolution (see #1965).
        assert_eq!(config.shared.endpoints.api_key, "abc");
        assert_eq!(
            config.shared.endpoints.dd_url.as_deref(),
            Some("https://custom.example.com")
        );
        // Seeded Saluki-only field.
        assert_eq!(config.domains.dogstatsd.listeners.tcp_port, 8126);
    }

    #[test]
    fn cluster_agent_connection_values_are_trimmed() {
        let datadog: DatadogConfiguration = serde_json::from_value(json!({
            "cluster_agent": {
                "enabled": true,
                "url": "  https://cluster-agent.example.com  ",
                "auth_token": " secret-token ",
                "kubernetes_service_name": "   ",
            },
        }))
        .expect("datadog source deserializes");
        let saluki_only = SalukiOnly::default();

        let (config, errors) = translate(&datadog, &saluki_only);
        assert!(errors.is_none(), "translation of a valid map records no error");

        let cluster_agent = &config.shared.cluster_agent;
        assert!(cluster_agent.enabled);
        assert_eq!(cluster_agent.url.as_deref(), Some("https://cluster-agent.example.com"));
        assert_eq!(cluster_agent.auth_token.as_deref(), Some("secret-token"));
        assert_eq!(cluster_agent.kubernetes_service_name.as_deref(), Some(""));
    }

    #[test]
    fn cluster_agent_empty_kubernetes_service_name_is_preserved() {
        let datadog: DatadogConfiguration = serde_json::from_value(json!({
            "cluster_agent": {
                "kubernetes_service_name": "",
            },
        }))
        .expect("datadog source deserializes");
        let saluki_only = SalukiOnly::default();

        let (config, errors) = translate(&datadog, &saluki_only);
        assert!(errors.is_none(), "translation of a valid map records no error");

        assert_eq!(config.shared.cluster_agent.kubernetes_service_name.as_deref(), Some(""));
    }

    #[test]
    fn cluster_agent_kubernetes_service_name_defaults_from_schema() {
        // When the key is absent, the Datadog schema default flows in via `drive`.
        let datadog = DatadogConfiguration::default();
        let saluki_only = SalukiOnly::default();

        let (config, errors) = translate(&datadog, &saluki_only);
        assert!(errors.is_none(), "translation of a valid map records no error");

        assert!(!config.shared.cluster_agent.enabled);
        assert_eq!(
            config.shared.cluster_agent.kubernetes_service_name.as_deref(),
            Some("datadog-cluster-agent")
        );
    }
}
