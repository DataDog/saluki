//! [`ConfigurationSystem`]: the runtime configuration, translated from the raw source map and kept
//! current as the map streams updates.

use std::sync::Arc;

use agent_data_plane_config::SalukiConfiguration;
use arc_swap::ArcSwap;
use datadog_agent_config::{DatadogConfiguration, TranslateError};
use saluki_config::dynamic::ConfigChangeEvent;
use saluki_config::{ConfigurationError, GenericConfiguration};
use snafu::Snafu;
use tokio::sync::broadcast;
use tracing::{debug, warn};

use crate::saluki_only::SalukiOnly;
use crate::translators::{ConfigTranslator, DatadogTranslator};

/// Translates the Datadog and Saluki-only sources into one [`SalukiConfiguration`], returning any
/// error recorded while converting an individual Datadog value.
///
/// The Datadog `drive` feeds every supported key to a `DatadogTranslator`; a value that cannot be
/// converted leaves its field at the model default and records an error. The Saluki-only values
/// then seed their disjoint destinations, which cannot fail. The returned configuration is always
/// complete: every valid value is present, and every invalid one holds its default.
fn translate(
    datadog: &DatadogConfiguration, saluki_only: &SalukiOnly,
) -> (SalukiConfiguration, Option<TranslateError>) {
    let (mut config, error) = DatadogTranslator::new(datadog).translate();
    saluki_only.seed(&mut config);
    (config, error)
}

/// An error building the translated configuration from the raw source map.
#[derive(Debug, Snafu)]
pub enum ConfigurationSystemError {
    /// A source model could not be deserialized from the raw configuration map.
    #[snafu(context(false), display("{source}"))]
    Source {
        /// The underlying deserialization error.
        source: ConfigurationError,
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
    pub fn load(raw_map: GenericConfiguration) -> Result<Self, ConfigurationSystemError> {
        let datadog = raw_map.as_typed::<DatadogConfiguration>()?;
        let saluki_only = raw_map.as_typed::<SalukiOnly>()?;
        let (config, error) = translate(&datadog, &saluki_only);
        if let Some(error) = error {
            warn!(%error, "Configuration translation reported errors at startup; invalid values kept their defaults.");
        }

        let current = Arc::new(ArcSwap::from_pointee(config));

        // A dynamic source map broadcasts one event per committed key change. The task ignores the
        // event contents and re-reads the whole committed map, so it only exists when updates can
        // arrive; a static map has no sender and needs no task.
        if let Some(rx) = raw_map.subscribe_for_updates() {
            spawn_router(rx, raw_map.clone(), Arc::clone(&current));
        }

        Ok(Self { raw_map, current })
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
    current: Arc<ArcSwap<SalukiConfiguration>>,
) {
    tokio::spawn(router_loop(rx, raw_map, current));
}

/// Re-translates the committed source map whenever it changes and stores the result.
///
/// Blocks until an update commits, drains any further updates that piled up without translating
/// between them, then translates the committed map once. Update events carry no payload the task
/// needs, so a dropped or lagged event is just another signal to re-read the committed map. The
/// task ends when the source map's sender is gone, since no further updates can arrive.
async fn router_loop(
    mut rx: broadcast::Receiver<ConfigChangeEvent>, raw_map: GenericConfiguration,
    current: Arc<ArcSwap<SalukiConfiguration>>,
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
        apply(&current, &raw_map);
    }
}

/// Re-translates the committed `raw_map` and stores the result as the current configuration.
///
/// A source model that cannot be deserialized leaves the current configuration unchanged, since no
/// partial value can be recovered from it. Translation, by contrast, always yields a complete
/// configuration: individual values that cannot be converted keep their defaults and are logged,
/// and the result is stored so that every valid value -- including later updates -- still takes
/// effect. A single invalid value therefore never wedges subsequent updates.
pub(crate) fn apply(current: &ArcSwap<SalukiConfiguration>, raw_map: &GenericConfiguration) {
    let datadog = match raw_map.as_typed::<DatadogConfiguration>() {
        Ok(c) => c,
        Err(e) => {
            warn!(error = %e, "Rejecting configuration update: the Datadog source could not be deserialized. Retaining current configuration.");
            return;
        }
    };
    let saluki_only = match raw_map.as_typed::<SalukiOnly>() {
        Ok(c) => c,
        Err(e) => {
            warn!(error = %e, "Rejecting configuration update: the Saluki-only source could not be deserialized. Retaining current configuration.");
            return;
        }
    };
    let (next, error) = translate(&datadog, &saluki_only);
    if let Some(error) = error {
        warn!(%error, "Configuration update had translation errors; invalid values kept their defaults. Applying the remaining valid configuration.");
    }
    current.store(Arc::new(next));
    debug!("Applied configuration update.");
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use agent_data_plane_config::domains::dogstatsd::OriginTagCardinality;
    use agent_data_plane_config::SalukiConfiguration;
    use datadog_agent_config::DatadogConfiguration;
    use saluki_config::dynamic::ConfigUpdate;
    use saluki_config::ConfigurationLoader;
    use serde_json::json;

    use super::{translate, ConfigurationSystem, SalukiOnly};

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

        let system = ConfigurationSystem::load(raw_map).expect("system builds");
        let config = system.config();

        assert_eq!(config.control.logging.level, "warn");
        assert_eq!(config.domains.dogstatsd.listeners.port, 9125);
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

        let system = ConfigurationSystem::load(raw_map.clone()).expect("system builds");
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

        let system = ConfigurationSystem::load(raw_map.clone()).expect("system builds");

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

    #[test]
    fn translate_small_map_through_witness_and_seed() {
        // A small raw Datadog source map exercising a scalar conversion, an enum parse, a
        // seconds->Duration conversion, and the raw endpoint inputs.
        let datadog: DatadogConfiguration = serde_json::from_value(json!({
            "api_key": "abc",
            "dd_url": "https://custom.example.com",
            "dogstatsd_port": 9125,
            "dogstatsd_tag_cardinality": "high",
            "expected_tags_duration": 15.0,
            "telemetry": { "dogstatsd_origin": true },
        }))
        .expect("datadog source deserializes");

        // A small Saluki-only source setting one seeded field.
        let saluki_only: SalukiOnly = serde_json::from_value(json!({
            "dogstatsd": { "tcp_port": 8126 },
        }))
        .expect("saluki-only source deserializes");

        let (config, error) = translate(&datadog, &saluki_only);
        assert!(error.is_none(), "translation of a valid map records no error");

        // Driven scalar conversion: i64 -> u16.
        assert_eq!(config.domains.dogstatsd.listeners.port, 9125);
        // Driven enum parse.
        assert_eq!(
            config.domains.dogstatsd.origin.tag_cardinality,
            OriginTagCardinality::High
        );
        // Driven seconds(f64) -> Duration.
        assert_eq!(config.shared.tags.expected_tags_duration, Duration::from_secs_f64(15.0));
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
