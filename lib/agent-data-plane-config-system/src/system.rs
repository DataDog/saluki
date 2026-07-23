//! [`ConfigurationSystem`]: the runtime configuration, translated from the raw sources and kept
//! current as the Datadog Agent streams updates.

use std::collections::HashSet;
use std::sync::{Arc, OnceLock};

use agent_data_plane_config::{Live, SalukiConfiguration};
use arc_swap::ArcSwap;
use datadog_agent_config::{apply_env_overlay, DatadogConfiguration, EnvOverlayMode, TranslateErrors};
use saluki_config::dynamic::ConfigUpdate;
use saluki_config::{upsert, ConfigurationError, GenericConfiguration};
use serde::Deserialize;
use serde_json::Value;
use snafu::Snafu;
use tokio::sync::{mpsc, watch};
use tracing::{debug, warn};

use crate::saluki_env_overlay;
use crate::saluki_only::SalukiOnly;
use crate::translators::DatadogTranslator;

/// An error building the translated configuration from the raw sources.
#[derive(Debug, Snafu)]
pub enum Error {
    /// The configuration value could not be read from the raw configuration map.
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

    /// The Datadog Agent closed the configuration stream before sending the initial snapshot.
    #[snafu(display("configuration stream closed before the initial snapshot"))]
    StreamClosed,

    /// The typed base could not be built from the file and environment.
    #[snafu(display("failed to build the configuration base: {message}"))]
    Base {
        /// What went wrong reading the file, parsing YAML, or decoding an environment variable.
        message: String,
    },

    /// Translating the sources into the model failed on one or more keys.
    #[snafu(display("{source}"))]
    Translate {
        /// Every translation error recorded.
        source: TranslateErrors,
    },
}

type Result<T> = std::result::Result<T, Error>;

/// The runtime configuration, translated from the raw sources and kept current.
///
/// The configuration system is the single owner of the Datadog Agent's `ConfigUpdate` stream. It
/// folds each update onto the local source base to build the typed [`SalukiConfiguration`] directly,
/// and forwards the same update to a legacy [`GenericConfiguration`] compatibility map so
/// un-migrated components can still read by key. The current configuration lives in an [`ArcSwap`]
/// cell so readers load a whole, self-consistent version with no lock, while the update task
/// replaces it in one atomic store.
pub struct ConfigurationSystem {
    raw_map: GenericConfiguration,
    current: Arc<ArcSwap<SalukiConfiguration>>,
    // Fired once after each accepted update so live views wake and re-project. Shared with the
    // update task via `Arc` because `watch::Sender` is not `Clone` and both the system (to mint
    // views) and the task (to notify) need it.
    tick: Arc<watch::Sender<()>>,
}

impl ConfigurationSystem {
    /// Connected authority: takes ownership of the Datadog Agent's config stream, forwards each
    /// update to the compatibility map, and builds the typed model directly from the stream folded
    /// onto the local `base` (file + environment).
    ///
    /// Blocks for the first authoritative snapshot and is the strict startup gate: a snapshot that
    /// never arrives, cannot be deserialized, or fails translation aborts the boot. `async` because
    /// the update task requires a Tokio runtime; keeping that requirement visible here avoids a
    /// panic deep inside `tokio::spawn`.
    ///
    /// # Errors
    ///
    /// Returns an error if the stream closes before the first snapshot, or the initial configuration
    /// cannot be deserialized or translated.
    pub(crate) async fn connected(
        mut agent_rx: mpsc::Receiver<ConfigUpdate>, compat_tx: mpsc::Sender<ConfigUpdate>,
        compat_map: GenericConfiguration, base: Value, overlay: EnvOverlayMode,
    ) -> Result<Self> {
        // The first stream message is the authoritative initial snapshot.
        let first = agent_rx.recv().await.ok_or(Error::StreamClosed)?;

        // Fold it into the accumulating Agent layer and forward it to the compat map, then wait for
        // the compat map to apply it so `raw_map()` is populated before any consumer reads it.
        let mut agent = Value::Null;
        fold(&mut agent, &first);
        forward(&compat_tx, first).await;
        compat_map.ready().await;

        // Startup is the strict gate: this is the first, authoritative Agent snapshot, so any error
        // fails the boot and we never run on bad config. At runtime (see `agent_loop`) the same
        // check instead rejects the offending update and keeps the last-known-good configuration,
        // because a runtime update must never take the system down.
        let merged = deep_merge(base.clone(), agent.clone());
        let config = translate_strict(&merged, overlay)?;

        let current = Arc::new(ArcSwap::from_pointee(config));
        // The initial receiver is dropped immediately; `send_replace` works with zero receivers, and
        // each live view subscribes its own receiver from the sender.
        let (tick, _) = watch::channel(());
        let tick = Arc::new(tick);

        tokio::spawn(agent_loop(
            agent_rx,
            compat_tx,
            base,
            agent,
            Arc::clone(&current),
            Arc::clone(&tick),
            overlay,
        ));

        Ok(Self {
            raw_map: compat_map,
            current,
            tick,
        })
    }

    /// Installs a static configuration without an update task.
    ///
    /// Live views retain their initial values because this system sends no update notifications.
    pub(crate) fn standalone(compat_map: GenericConfiguration, config: SalukiConfiguration) -> Self {
        let current = Arc::new(ArcSwap::from_pointee(config));
        let (tick, _) = watch::channel(());
        Self {
            raw_map: compat_map,
            current,
            tick: Arc::new(tick),
        }
    }

    /// Returns a live view of the given projection of the current configuration. Narrow further with
    /// [`Live::project`]. This is the only way a consumer subscribes to runtime updates.
    pub fn live<T>(&self, project: impl for<'a> Fn(&'a SalukiConfiguration) -> &'a T + Send + Sync + 'static) -> Live<T>
    where
        T: Clone + PartialEq + 'static,
    {
        Live::new_dynamic(Arc::clone(&self.current), self.tick.subscribe(), project)
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

/// Owns the Datadog Agent config stream for the life of the process: validates each update against
/// the typed model, commits it on success, and forwards it to the by-key configuration view. Ends
/// when the stream closes.
///
/// Each update is processed individually (no burst collapse) so a rejection can be attributed to the
/// exact update that caused it. Updates are infrequent, so re-translating per update is cheap.
async fn agent_loop(
    mut agent_rx: mpsc::Receiver<ConfigUpdate>, compat_tx: mpsc::Sender<ConfigUpdate>, base: Value, mut agent: Value,
    current: Arc<ArcSwap<SalukiConfiguration>>, tick: Arc<watch::Sender<()>>, overlay: EnvOverlayMode,
) {
    while let Some(update) = agent_rx.recv().await {
        // Validate-then-commit: fold onto a tentative copy of the Agent layer and drive the typed
        // model from it. Only a fully successful update advances the committed layer, so a rejected
        // value never lingers to re-poison a later merge.
        let mut tentative = agent.clone();
        fold(&mut tentative, &update);
        let merged = deep_merge(base.clone(), tentative.clone());
        match translate_strict(&merged, overlay) {
            Ok(config) => {
                agent = tentative;
                current.store(Arc::new(config));
                tick.send_replace(());
                debug!("Applied configuration update.");
            }
            Err(e) => warn!(
                error = %e,
                "Rejected configuration update; keeping the last-known-good typed configuration. The \
                 compatibility map still receives this update, so the two representations may now \
                 differ, so that divergence indicates a defect in the typed configuration system, not \
                 in the configuration itself."
            ),
        }
        // The compatibility map receives every update faithfully, whether or not the typed path
        // accepted it: un-migrated components keep the Agent's permissive behavior during migration.
        // The updater owns the receiver; if it is gone, no un-migrated component is reading the
        // by-key view, so dropping the forward is fine.
        forward(&compat_tx, update).await;
    }
}

/// Folds one update into the accumulating Agent layer.
///
/// `Snapshot` replaces the layer; `Partial` applies one (possibly dotted) key via `upsert`, the same
/// handling the `saluki-config` updater uses, so the typed model and the compatibility map stay
/// consistent.
fn fold(agent: &mut Value, update: &ConfigUpdate) {
    match update {
        ConfigUpdate::Snapshot(state) => *agent = state.clone(),
        ConfigUpdate::Partial { key, value } => {
            if agent.is_null() {
                *agent = Value::Object(serde_json::Map::new());
            }
            upsert(agent, key, value.clone());
        }
    }
}

/// Forwards one update to the compatibility map's updater.
async fn forward(compat_tx: &mpsc::Sender<ConfigUpdate>, update: ConfigUpdate) {
    let _ = compat_tx.send(update).await;
}

/// Deserializes and translates merged source values, rejecting partially translated configuration.
///
/// # Errors
///
/// Returns an error if either source model cannot be deserialized or any key fails translation.
pub(crate) fn translate_strict(merged: &Value, overlay: EnvOverlayMode) -> Result<SalukiConfiguration> {
    let Sources { datadog, saluki } = deserialize_sources(merged, overlay)?;
    let (config, errors) = translate(&datadog, &saluki);
    if let Some(errors) = errors {
        return Err(Error::Translate { source: errors });
    }
    Ok(config)
}

/// Merges `overlay` onto `base`, with `overlay` winning. Objects merge recursively; every other
/// value (and any type mismatch) is replaced by the overlay's value.
//
// This is the authoritative-Agent merge: the Agent snapshot layered on the local base. The merge is
// schema-leaf-level, not structural: it recurses through sections (the intermediate path objects a
// leaf lives under) and replaces at every leaf. A map- or array-typed leaf (for example
// `additional_endpoints`) is therefore replaced wholesale by whichever source supplies it, never
// key-unioned across sources. The section-vs-leaf distinction comes from the schema's own leaf
// paths (see `section_paths`).
//
// TODO: A map/array-valued schema leaf is replaced wholesale when any source (file, environment, or
// the Agent config stream) supplies it. Verify this is the intended semantic for the remote Agent
// config stream: ADP is that stream's first consumer, so the correct behavior for a stream update to
// a map-shaped setting may not have been defined yet.
fn deep_merge(base: Value, overlay: Value) -> Value {
    merge_at(base, overlay, &mut Vec::new(), section_paths())
}

// Recurse only where `path` names a schema section; everywhere else the overlay value replaces the
// base value. This covers leaves (map, array, or scalar) and any unknown key not in the schema. As
// before, an array or a base/overlay type mismatch falls through to a wholesale replace.
fn merge_at(base: Value, overlay: Value, path: &mut Vec<String>, sections: &HashSet<Vec<String>>) -> Value {
    match (base, overlay) {
        (Value::Object(mut base_map), Value::Object(overlay_map)) => {
            for (key, overlay_value) in overlay_map {
                path.push(key.clone());
                let merged = match base_map.remove(&key) {
                    Some(base_value) if sections.contains(path.as_slice()) => {
                        merge_at(base_value, overlay_value, path, sections)
                    }
                    _ => overlay_value,
                };
                path.pop();
                base_map.insert(key, merged);
            }
            Value::Object(base_map)
        }
        (_, overlay) => overlay,
    }
}

// Every proper prefix of a modeled leaf path, across both source models. A path in this set is a
// section the merge recurses into; a path that is a leaf (or unknown) is absent, so the merge
// replaces there. Built once from the schema's own leaf tables, so it cannot drift from the models.
fn section_paths() -> &'static HashSet<Vec<String>> {
    static SECTIONS: OnceLock<HashSet<Vec<String>>> = OnceLock::new();
    SECTIONS.get_or_init(|| {
        let mut sections = HashSet::new();
        let mut add_prefixes = |segments: Vec<String>| {
            for end in 1..segments.len() {
                sections.insert(segments[..end].to_vec());
            }
        };
        for path in datadog_agent_config::datadog_leaf_paths() {
            add_prefixes(path.iter().map(|segment| (*segment).to_string()).collect());
        }
        for path in saluki_env_overlay::leaf_paths() {
            add_prefixes(path.to_vec());
        }
        sections
    })
}

/// The sources deserialized from the merged configuration value, separated by source authority.
struct Sources {
    datadog: DatadogConfiguration,
    saluki: SalukiOnly,
}

/// Deserializes both source models from the merged configuration value.
///
/// The source models use ordinary serde-compatible field types, so deserializing from
/// `serde_json::Value` preserves the values.
///
/// Environment variables reach the merged value as flat keys (`autoscaling_failover_enabled`), which
/// neither nested source reads. Each source has its own overlay applied before its deserialize, per
/// `overlay`: `saluki_env_overlay::apply` for the Saluki-only keys and `apply_env_overlay` for the
/// Datadog keys. The two cover disjoint key sets, so relocating one source's keys is inert for the
/// other.
fn deserialize_sources(merged: &Value, overlay: EnvOverlayMode) -> Result<Sources> {
    let mut merged = merged.clone();
    saluki_env_overlay::apply(&mut merged, overlay);
    let saluki = SalukiOnly::deserialize(&merged)?;
    apply_env_overlay(&mut merged, overlay);
    let datadog = DatadogConfiguration::deserialize(&merged)?;
    Ok(Sources { datadog, saluki })
}

/// Translates the Datadog and Saluki-only sources into one [`SalukiConfiguration`], returning every
/// error recorded while converting an individual Datadog value.
///
/// The Datadog `drive` feeds every supported key to a `DatadogTranslator`; a value that cannot be
/// converted leaves its field at the model default and records an error. The Saluki-only values
/// then seed their disjoint destinations, which cannot fail. The returned configuration is always
/// complete: every valid value is present, and every invalid one holds its default.
fn translate(datadog: &DatadogConfiguration, saluki: &SalukiOnly) -> (SalukiConfiguration, Option<TranslateErrors>) {
    let (mut config, errors) = DatadogTranslator::new(datadog).translate();
    saluki.seed(&mut config);
    (config, errors)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use agent_data_plane_config::domains::dogstatsd::OriginTagCardinality;
    use agent_data_plane_config::{Live, SalukiConfiguration};
    use datadog_agent_config::{DatadogConfiguration, EnvOverlayMode};
    use saluki_config::dynamic::ConfigUpdate;
    use saluki_config::ConfigurationLoader;
    use serde_json::{json, Value};
    use tokio::sync::mpsc;

    use super::{deep_merge, translate, translate_strict, ConfigurationSystem, Error, SalukiOnly};

    /// Builds a standalone system whose authority is the local sources (`file` + `env`).
    async fn standalone_system(
        file: Option<Value>, env: Option<&[(String, String)]>, overlay: EnvOverlayMode,
    ) -> Result<ConfigurationSystem, Error> {
        let (compat_map, _) = ConfigurationLoader::for_tests(file, env, false).await;
        let base = compat_map.as_typed::<Value>().expect("base extracts");
        let config = translate_strict(&base, overlay)?;
        Ok(ConfigurationSystem::standalone(compat_map, config))
    }

    /// Builds a connected system whose base is `base` and whose authority is the returned Agent
    /// stream. The initial (empty) snapshot is queued before the system blocks on it, so the caller
    /// gets back a stream ready for `Partial`/`Snapshot` updates.
    async fn connected_system(
        base: Value, overlay: EnvOverlayMode,
    ) -> (ConfigurationSystem, mpsc::Sender<ConfigUpdate>) {
        let (agent_tx, agent_rx) = mpsc::channel(100);
        let (compat_map, compat_tx) = ConfigurationLoader::for_tests(None, None, true).await;
        let compat_tx = compat_tx.expect("dynamic sender exists");
        agent_tx.send(ConfigUpdate::Snapshot(json!({}))).await.unwrap();
        let system = ConfigurationSystem::connected(agent_rx, compat_tx, compat_map, base, overlay)
            .await
            .expect("system builds");
        (system, agent_tx)
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
        let system = standalone_system(
            Some(json!({ "log_level": "warn", "dogstatsd_port": 9125 })),
            None,
            EnvOverlayMode::Fallback,
        )
        .await
        .expect("system builds");
        let config = system.config();

        assert_eq!(config.control.logging.level, "warn");
        assert_eq!(config.domains.dogstatsd.listeners.port, 9125);
    }

    #[tokio::test]
    async fn flat_env_key_overlays_onto_nested_datadog_slot() {
        // A flat, underscore-joined key (the shape an environment variable produces) is relocated
        // into the nested `autoscaling.failover.*` slot the Datadog deserializer reads. The string
        // list is split on whitespace.
        let system = standalone_system(
            Some(json!({
                "autoscaling_failover_enabled": true,
                "autoscaling_failover_metrics": "container.memory.usage container.cpu.usage",
            })),
            None,
            EnvOverlayMode::Fallback,
        )
        .await
        .expect("system builds");
        let config = system.config();

        assert!(config.shared.autoscaling_failover.enabled);
        assert_eq!(
            config.shared.autoscaling_failover.metrics,
            vec!["container.memory.usage".to_string(), "container.cpu.usage".to_string()]
        );
    }

    #[tokio::test]
    async fn saluki_only_dotted_env_key_seeds_the_model() {
        // A dotted Saluki-only key set only by environment variable arrives as a flat underscore-joined key
        // `data_plane_standalone_mode`, which the nested `SalukiOnly` never reads. The overlay must
        // relocate it so it seeds `control.standalone_mode`.
        let system = standalone_system(
            None,
            Some(&[("DATA_PLANE_STANDALONE_MODE".to_string(), "true".to_string())]),
            EnvOverlayMode::Fallback,
        )
        .await
        .expect("system builds");

        assert!(system.config().control.standalone_mode);
    }

    #[tokio::test]
    async fn disabled_overlay_leaves_flat_env_key_unread() {
        // With the overlay disabled, the flat key is never relocated, so the nested deserializer
        // does not see it and the field keeps its default.
        let system = standalone_system(
            Some(json!({ "autoscaling_failover_enabled": true })),
            None,
            EnvOverlayMode::Disabled,
        )
        .await
        .expect("system builds");

        assert!(!system.config().shared.autoscaling_failover.enabled);
    }

    #[tokio::test]
    async fn load_fails_on_translation_invalid_startup_config() {
        // Startup is the strict gate: a value figment accepts but the model rejects fails the load,
        // so the process never boots on bad config.
        let result = standalone_system(
            Some(json!({ "dogstatsd_tag_cardinality": "bogus" })),
            None,
            EnvOverlayMode::Fallback,
        )
        .await;

        assert!(matches!(result, Err(Error::Translate { .. })));
    }

    #[test]
    fn zero_otlp_trace_interner_size_is_rejected() {
        // Component builders used to discover this after translation. Reject zero before publishing
        // an invalid typed model.
        let error = translate_strict(
            &json!({ "otlp_config": { "traces": { "string_interner_size": 0 } } }),
            EnvOverlayMode::Fallback,
        )
        .expect_err("zero trace interner size should fail translation");

        assert!(matches!(error, Error::Deserialize { .. }));
        assert!(error.to_string().contains("value of bytes must be greater than zero"));
    }

    #[test]
    fn oversized_otlp_trace_interner_size_is_rejected() {
        let error = translate_strict(
            &json!({ "otlp_config": { "traces": { "string_interner_size": "2GiB" } } }),
            EnvOverlayMode::Fallback,
        )
        .expect_err("oversized trace interner should fail translation");

        assert!(matches!(error, Error::Deserialize { .. }));
        assert!(error.to_string().contains("must not exceed 1073741824 bytes"));
    }

    #[test]
    fn positive_otlp_trace_interner_size_is_accepted() {
        let config = translate_strict(
            &json!({ "otlp_config": { "traces": { "string_interner_size": "512KiB" } } }),
            EnvOverlayMode::Fallback,
        )
        .expect("positive trace interner size should translate");

        assert_eq!(config.domains.otlp.traces.string_interner_size.get(), 512 * 1024);
    }

    #[tokio::test]
    async fn standalone_loads_numeric_byte_size() {
        // A byte-size setting documented as accepting a bare integer (`10485760`) rather than a
        // string (`"10MB"`) must not abort the strict startup gate. The typed model normalizes it,
        // and the translator resolves it to the same byte count.
        let system = standalone_system(
            Some(json!({ "dogstatsd_log_file_max_size": 10485760 })),
            None,
            EnvOverlayMode::Fallback,
        )
        .await
        .expect("numeric byte size boots");

        assert_eq!(system.config().domains.dogstatsd.debug_log.log_file_max_size, 10485760);
    }

    #[tokio::test]
    async fn translation_invalid_update_is_rejected_keeping_last_known_good() {
        let (system, agent_tx) = connected_system(
            json!({ "log_level": "warn", "dogstatsd_tag_cardinality": "high" }),
            EnvOverlayMode::Fallback,
        )
        .await;
        assert_eq!(
            system.config().domains.dogstatsd.origin.tag_cardinality,
            OriginTagCardinality::High
        );

        // Send a translation-invalid update, then a valid update to a different field. Updates are
        // processed in order, so once the second is observed the first has already been handled.
        agent_tx
            .send(ConfigUpdate::Partial {
                key: "dogstatsd_tag_cardinality".to_string(),
                value: json!("bogus"),
            })
            .await
            .unwrap();
        agent_tx
            .send(ConfigUpdate::Partial {
                key: "log_level".to_string(),
                value: json!("error"),
            })
            .await
            .unwrap();

        await_config(&system, "the later valid update to take effect", |c| {
            c.control.logging.level == "error"
        })
        .await;
        // The invalid update was rejected whole: the field keeps its last-known-good value rather
        // than falling back to a default, and the later valid update still applied.
        assert_eq!(
            system.config().domains.dogstatsd.origin.tag_cardinality,
            OriginTagCardinality::High
        );
    }

    #[tokio::test]
    async fn converges_to_latest_value_under_burst() {
        let (system, agent_tx) = connected_system(json!({ "log_level": "info" }), EnvOverlayMode::Fallback).await;

        let burst = [
            "warn", "error", "debug", "trace", "info", "warn", "error", "debug", "trace", "info", "warn", "error",
            "debug", "trace", "info", "warn", "error", "debug", "trace",
        ];
        for (i, level) in burst.iter().enumerate() {
            agent_tx
                .send(ConfigUpdate::Partial {
                    key: "log_level".to_string(),
                    value: json!(level),
                })
                .await
                .unwrap();
            // Interleave a translation-invalid update mid-burst, then correct it. The invalid value
            // is rejected whole (last-known-good retained) rather than wedging the task, so the
            // baseline keeps converging on the latest valid value regardless of the transient bad one.
            if i == burst.len() / 2 {
                agent_tx
                    .send(ConfigUpdate::Partial {
                        key: "dogstatsd_tag_cardinality".to_string(),
                        value: json!("bogus"),
                    })
                    .await
                    .unwrap();
                agent_tx
                    .send(ConfigUpdate::Partial {
                        key: "dogstatsd_tag_cardinality".to_string(),
                        value: json!("high"),
                    })
                    .await
                    .unwrap();
            }
        }
        let final_level = "error";
        agent_tx
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
        let (system, agent_tx) = connected_system(
            json!({ "dogstatsd_metrics_stats_enable": false }),
            EnvOverlayMode::Fallback,
        )
        .await;
        let mut view = system.live(|c| &c.domains.dogstatsd.debug_log);
        assert!(!view.metrics_stats_enable);

        agent_tx
            .send(ConfigUpdate::Partial {
                key: "dogstatsd_metrics_stats_enable".to_string(),
                value: json!(true),
            })
            .await
            .unwrap();

        let updated = tokio::time::timeout(Duration::from_secs(2), view.changed())
            .await
            .expect("view observes the debug-log update");
        assert!(updated.metrics_stats_enable);
        // `Deref` reflects the value returned by the last `changed`.
        assert!(view.metrics_stats_enable);
    }

    #[tokio::test]
    async fn field_view_wakes_on_its_field() {
        // Projecting straight to a single field needs no schema change and no central registration:
        // the granularity is chosen at the call site.
        let (system, agent_tx) = connected_system(
            json!({ "dogstatsd_metrics_stats_enable": false }),
            EnvOverlayMode::Fallback,
        )
        .await;
        let mut stats = system.live(|c| &c.domains.dogstatsd.debug_log.metrics_stats_enable);
        assert!(!*stats);

        agent_tx
            .send(ConfigUpdate::Partial {
                key: "dogstatsd_metrics_stats_enable".to_string(),
                value: json!(true),
            })
            .await
            .unwrap();

        let updated = tokio::time::timeout(Duration::from_secs(2), stats.changed())
            .await
            .expect("field view observes its field's update");
        assert!(updated);
        assert!(*stats);
    }

    #[tokio::test]
    async fn fixed_view_never_changes() {
        let mut view: Live<bool> = Live::new_fixed(true);
        assert!(*view);
        // A fixed view never resolves, so this bound is deterministic rather than timing-dependent.
        assert!(tokio::time::timeout(Duration::from_millis(100), view.changed())
            .await
            .is_err());
    }

    #[tokio::test]
    async fn live_views_reflect_startup_configuration() {
        let system = standalone_system(
            Some(json!({ "dogstatsd_metrics_stats_enable": true })),
            None,
            EnvOverlayMode::Fallback,
        )
        .await
        .expect("system builds");
        let config = system.config();

        let debug_log = system.live(|c| &c.domains.dogstatsd.debug_log);
        assert_eq!(&*debug_log, &config.domains.dogstatsd.debug_log);

        let prefix_filter = system.live(|c| &c.domains.dogstatsd.prefix_filter);
        assert_eq!(&*prefix_filter, &config.domains.dogstatsd.prefix_filter);

        let multi_region_failover = system.live(|c| &c.domains.multi_region_failover);
        assert_eq!(&*multi_region_failover, &config.domains.multi_region_failover);
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
        let saluki: SalukiOnly = serde_json::from_value(json!({
            "dogstatsd_tcp_port": 8126,
        }))
        .expect("saluki-only source deserializes");

        let (config, errors) = translate(&datadog, &saluki);
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

    // A map-typed schema leaf (`additional_endpoints`) is replaced wholesale, not key-unioned: the
    // overlay's map fully supplants the base's rather than the two being merged.
    #[test]
    fn merge_replaces_a_map_typed_leaf_wholesale() {
        let base = json!({ "additional_endpoints": { "https://a.example.com": ["k1"] } });
        let overlay = json!({ "additional_endpoints": { "https://b.example.com": ["k2"] } });

        let merged = deep_merge(base, overlay);

        assert_eq!(
            merged,
            json!({ "additional_endpoints": { "https://b.example.com": ["k2"] } })
        );
    }

    // A schema section (`apm_config`) is recursed into: leaves from both sources coexist, and a leaf
    // set by both sources takes the overlay's value.
    #[test]
    fn merge_recurses_into_a_section_and_replaces_leaves_within_it() {
        let base = json!({ "apm_config": { "compute_stats_by_span_kind": true, "enable_rare_sampler": true } });
        let overlay = json!({ "apm_config": { "enable_rare_sampler": false } });

        let merged = deep_merge(base, overlay);

        assert_eq!(
            merged,
            json!({ "apm_config": { "compute_stats_by_span_kind": true, "enable_rare_sampler": false } })
        );
    }

    // A scalar leaf still replaces, and a colliding array leaf is replaced rather than adjoined.
    #[test]
    fn merge_replaces_scalar_and_array_leaves() {
        let base = json!({ "dogstatsd_port": 8125, "dogstatsd_mapper_profiles": ["a"] });
        let overlay = json!({ "dogstatsd_port": 9125, "dogstatsd_mapper_profiles": ["b"] });

        let merged = deep_merge(base, overlay);

        assert_eq!(
            merged,
            json!({ "dogstatsd_port": 9125, "dogstatsd_mapper_profiles": ["b"] })
        );
    }
}
