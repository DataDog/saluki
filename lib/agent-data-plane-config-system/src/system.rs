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
/// the nested `DatadogConfiguration` deserializer never reads. `apply_env_overlay` relocates those
/// flat keys into their nested slots per `env_overlay` before the Datadog deserialize. It is applied
/// only to the Datadog input: `SalukiOnly` deserializes from the untouched value, so its keys are
/// out of scope here.
fn deserialize_sources(
    raw_map: &GenericConfiguration, env_overlay: EnvOverlayMode,
) -> Result<(DatadogConfiguration, SalukiOnly), ConfigurationSystemError> {
    let mut merged = raw_map.as_typed::<serde_json::Value>()?;
    let saluki_only = SalukiOnly::deserialize(&merged)?;
    apply_env_overlay(&mut merged, env_overlay);
    let datadog = DatadogConfiguration::deserialize(&merged)?;
    Ok((datadog, saluki_only))
}

/// Translates the Datadog and Saluki-only sources into one [`SalukiConfiguration`], returning every
/// error recorded while converting an individual Datadog value.
///
/// The Datadog `drive` feeds every supported key to a `DatadogTranslator`; a value that cannot be
/// converted leaves its field at the model default and records an error. The Saluki-only values
/// then seed their disjoint destinations, which cannot fail. The returned configuration is always
/// complete: every valid value is present, and every invalid one holds its default.
fn translate(
    datadog: &DatadogConfiguration, saluki_only: &SalukiOnly,
) -> (SalukiConfiguration, Option<TranslateErrors>) {
    let (mut config, errors) = DatadogTranslator::new(datadog).translate();
    saluki_only.seed(&mut config);
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
}
