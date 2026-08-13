//! [`ConfigurationSystem`]: the runtime configuration, translated from the raw sources and kept
//! current as the Datadog Agent streams updates.

use std::sync::Arc;

use agent_data_plane_config::{Live, SalukiConfiguration};
use arc_swap::ArcSwap;
use datadog_agent_config::{DatadogConfiguration, TranslateErrors};
use saluki_config::dynamic::ConfigUpdate;
use saluki_config::{ConfigurationError, GenericConfiguration};
use serde::Deserialize;
use serde_json::Value;
use snafu::Snafu;
use tokio::sync::{mpsc, watch};
use tracing::{debug, warn};

use crate::saluki_only::SalukiOnly;
use crate::source::SourceTree;
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

    /// The fully merged configuration resolved no usable Datadog API key.
    #[snafu(display(
        "no Datadog API key is configured: set `api_key` in the Datadog Agent's configuration, or \
         `DD_API_KEY` in the environment. Every payload is authenticated with this key, so nothing can \
         be submitted without one"
    ))]
    MissingApiKey,
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
        compat_map: GenericConfiguration, base: SourceTree,
    ) -> Result<Self> {
        // The first stream message is the authoritative initial snapshot.
        let first = agent_rx.recv().await.ok_or(Error::StreamClosed)?;

        // Fold it into the accumulating Agent layer and forward it to the compat map, then wait for
        // the compat map to apply it so `raw_map()` is populated before any consumer reads it.
        let mut agent = SourceTree::empty();
        fold(&mut agent, &first);
        forward(&compat_tx, first).await;
        compat_map.ready().await;

        // Startup is the strict gate: this is the first, authoritative Agent snapshot, so any error
        // fails the boot and we never run on bad config. At runtime (see `agent_loop`) the same
        // check instead rejects the offending update and keeps the last-known-good configuration,
        // because a runtime update must never take the system down.
        let config = translate_authoritative(&base.overlay(&agent))?;

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
    mut agent_rx: mpsc::Receiver<ConfigUpdate>, compat_tx: mpsc::Sender<ConfigUpdate>, base: SourceTree,
    mut agent: SourceTree, current: Arc<ArcSwap<SalukiConfiguration>>, tick: Arc<watch::Sender<()>>,
) {
    while let Some(update) = agent_rx.recv().await {
        // Validate-then-commit: fold onto a tentative copy of the Agent layer and drive the typed
        // model from it. Only a fully successful update advances the committed layer, so a rejected
        // value never lingers to re-poison a later merge.
        let mut tentative = agent.clone();
        fold(&mut tentative, &update);
        match translate_authoritative(&base.overlay(&tentative)) {
            Ok(config) => {
                agent = tentative;
                current.store(Arc::new(config));
                tick.send_replace(());
                debug!("Applied configuration update.");
            }
            Err(e) => warn!(
                error = %e,
                "Rejected configuration update; keeping the last-known-good typed configuration. The \
                 compatibility map still receives this update, so an un-migrated component may act on \
                 a value the typed model rejected."
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
/// `Snapshot` replaces the layer; `Partial` applies one (possibly dotted) key, the same handling the
/// `saluki-config` updater uses, so this layer applies Agent updates the same way as the compatibility
/// view.
///
/// Each setting's provenance is retained, which is what lets a later update that demotes a value to
/// an Agent default stop shadowing the local value it had been overriding.
fn fold(agent: &mut SourceTree, update: &ConfigUpdate) {
    match update {
        ConfigUpdate::Snapshot(settings) => *agent = SourceTree::from_settings(settings),
        ConfigUpdate::Partial(setting) => agent.set(setting),
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
pub(crate) fn translate_strict(merged: &SourceTree) -> Result<SalukiConfiguration> {
    let Sources { datadog, saluki } = deserialize_sources(&merged.to_value())?;
    let (config, errors) = translate(&datadog, &saluki, merged);
    if let Some(errors) = errors {
        return Err(Error::Translate { source: errors });
    }
    Ok(config)
}

/// Translates merged sources that are authoritative for the running process, rejecting a
/// configuration ADP cannot run on.
///
/// This is [`translate_strict`] plus [`validate`]. Use it where the merged sources are complete: the
/// Datadog Agent's snapshot layered over the local base, or the local base alone in standalone mode.
///
/// # Errors
///
/// Returns an error if translation fails, or if the translated configuration fails validation.
pub(crate) fn translate_authoritative(merged: &SourceTree) -> Result<SalukiConfiguration> {
    let config = translate_strict(merged)?;
    validate(&config)?;
    Ok(config)
}

/// Checks the invariants a configuration must satisfy for this process to do useful work.
///
/// Translation alone cannot make these checks. It converts one key at a time and every schema key
/// has a default, so a setting the operator never supplied is indistinguishable from one they did
/// until the whole merged configuration is in hand.
///
/// Apply this only to an authoritative configuration. The local snapshot
/// [`LoadedConfiguration::load`][crate::LoadedConfiguration::load] produces is incomplete by design:
/// under the Datadog Agent the API key arrives over the configuration stream, so a local-only
/// snapshot legitimately has none, and CLI subcommands read that snapshot without ever submitting a
/// payload.
///
/// # Errors
///
/// Returns [`Error::MissingApiKey`] if no usable API key resolved. Every payload ADP submits is
/// authenticated with this key, so an empty one turns each flush into a rejected request that the
/// forwarder then retries. Failing here names the cause once instead of leaving an operator to infer
/// it from a stream of authentication failures.
pub(crate) fn validate(config: &SalukiConfiguration) -> Result<()> {
    // A blank key is as unusable as an absent one, and a padded key is a typo we should name rather
    // than send.
    if config.shared.endpoints.api_key.trim().is_empty() {
        return Err(Error::MissingApiKey);
    }

    Ok(())
}

// TODO: A map/array-valued schema leaf is replaced wholesale when any source (file, environment, or
// the Agent config stream) supplies it. Verify this is the intended semantic for the remote Agent
// config stream: ADP is that stream's first consumer, so the correct behavior for a stream update to
// a map-shaped setting may not have been defined yet.

/// The sources deserialized from the merged configuration value, separated by source authority.
struct Sources {
    datadog: DatadogConfiguration,
    saluki: SalukiOnly,
}

/// Deserializes both source models from the merged configuration value.
///
/// The source models use ordinary serde-compatible field types, so deserializing from
/// `serde_json::Value` preserves the values. Both read the canonical nested shape: the local base
/// is built that way by the schema-driven environment readers, and the Datadog Agent's stream
/// delivers dotted keys that are nested on arrival.
fn deserialize_sources(merged: &Value) -> Result<Sources> {
    let saluki = SalukiOnly::deserialize(merged)?;
    let datadog = DatadogConfiguration::deserialize(merged)?;
    Ok(Sources { datadog, saluki })
}

/// Translates the Datadog and Saluki-only sources into one [`SalukiConfiguration`], returning every
/// error recorded while converting an individual Datadog value.
///
/// The Datadog `drive` feeds every supported key to a `DatadogTranslator`; a value that cannot be
/// converted leaves its field at the model default and records an error. The Saluki-only values
/// then seed their disjoint destinations, which cannot fail. The returned configuration is always
/// complete: every valid value is present, and every invalid one holds its default.
///
/// `sources` is the same merged layer the models were deserialized from. The translator consults it
/// for provenance, which a deserialized source model cannot supply: a schema key with a default is
/// always present, so its value alone cannot say whether an input set it explicitly.
fn translate(
    datadog: &DatadogConfiguration, saluki: &SalukiOnly, sources: &SourceTree,
) -> (SalukiConfiguration, Option<TranslateErrors>) {
    let (mut config, errors) = DatadogTranslator::new(datadog, sources).translate();
    saluki.seed(&mut config);
    (config, errors)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use agent_data_plane_config::domains::dogstatsd::OriginTagCardinality;
    use agent_data_plane_config::shared::V3SeriesMode;
    use agent_data_plane_config::Provenance;
    use agent_data_plane_config::{Live, SalukiConfiguration};
    use datadog_agent_config::DatadogConfiguration;
    use saluki_config::dynamic::{ConfigSetting, ConfigUpdate, Provenance as StreamProvenance};
    use saluki_config::ConfigurationLoader;
    use serde_json::{json, Value};
    use tokio::sync::mpsc;

    use super::{
        translate, translate_authoritative, translate_strict, ConfigurationSystem, Error, SalukiOnly, SourceTree,
    };

    /// API key the connected-system fixture puts in the local base.
    ///
    /// An authoritative configuration must resolve one (see [`super::validate`]), and putting it in
    /// the base rather than in a streamed snapshot keeps it in place across the snapshot replacements
    /// these tests exercise.
    const TEST_API_KEY: &str = "test-api-key";

    /// Builds a standalone system whose authority is the local sources (`file` + `env`).
    ///
    /// Translates without validating so a test can state only the setting it is exercising. The
    /// production standalone path validates; `loaded.rs` covers that.
    async fn standalone_system(
        file: Option<Value>, env: Option<&[(String, String)]>,
    ) -> Result<ConfigurationSystem, Error> {
        let (compat_map, _) = ConfigurationLoader::for_tests(file, env, false).await;
        let base = SourceTree::all_explicit(compat_map.as_typed::<Value>().expect("base extracts"));
        let config = translate_strict(&base)?;
        Ok(ConfigurationSystem::standalone(compat_map, config))
    }

    /// Builds a connected system whose base is `base` and whose authority is the returned Agent
    /// stream. The initial (empty) snapshot is queued before the system blocks on it, so the caller
    /// gets back a stream ready for `Partial`/`Snapshot` updates.
    ///
    /// `base` is given an [`api_key`][TEST_API_KEY] unless it states its own, so a caller varying
    /// some unrelated setting need not restate what validation requires.
    async fn connected_system(mut base: Value) -> (ConfigurationSystem, mpsc::Sender<ConfigUpdate>) {
        let (agent_tx, agent_rx) = mpsc::channel(100);
        let (compat_map, compat_tx) = ConfigurationLoader::for_tests(None, None, true).await;
        let compat_tx = compat_tx.expect("dynamic sender exists");
        agent_tx.send(ConfigUpdate::snapshot([])).await.unwrap();
        if let Some(base) = base.as_object_mut() {
            base.entry("api_key").or_insert(json!(TEST_API_KEY));
        }
        let base = SourceTree::all_explicit(base);
        let system = ConfigurationSystem::connected(agent_rx, compat_tx, compat_map, base)
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
        let system = standalone_system(Some(json!({ "log_level": "warn", "dogstatsd_port": 9125 })), None)
            .await
            .expect("system builds");
        let config = system.config();

        assert_eq!(config.control.logging.level, "warn");
        assert_eq!(config.domains.dogstatsd.listeners.port, 9125);
    }

    #[test]
    fn boolean_use_v3_api_series_enabled_is_normalized() {
        let sources = SourceTree::all_explicit(json!({
            "use_v3_api": {
                "series": {
                    "enabled": true
                }
            }
        }));

        let config = translate_strict(&sources).expect("a boolean V3 series mode should translate");

        assert_eq!(config.shared.metrics_encoding.v3_series_mode, V3SeriesMode::Enabled);
    }

    #[tokio::test]
    async fn connected_stream_translates_metrics_v3_routing_configuration() {
        let (system, agent_tx) = connected_system(json!({
            "data_plane": {
                "metrics": {
                    "v3": {
                        "series": {
                            "enabled": true
                        }
                    }
                }
            }
        }))
        .await;

        assert_eq!(
            system.config().shared.metrics_encoding.v3_series_mode,
            V3SeriesMode::DatadogOnly
        );

        agent_tx
            .send(ConfigUpdate::snapshot([
                ConfigSetting::explicit("serializer_compressor_kind", json!("zstd")),
                ConfigSetting::explicit("serializer_experimental_use_v3_api.compression_level", json!(7)),
                ConfigSetting::explicit(
                    "serializer_experimental_use_v3_api.series.endpoints",
                    json!(["https://app.us3.datadoghq.com"]),
                ),
                ConfigSetting::explicit("serializer_experimental_use_v3_api.series.validate", json!(true)),
                ConfigSetting::explicit("serializer_experimental_use_v3_api.series.use_beta", json!(true)),
                ConfigSetting::explicit(
                    "serializer_experimental_use_v3_api.series.beta_route",
                    json!("/api/intake/metrics/custom/series"),
                ),
                ConfigSetting::explicit(
                    "serializer_experimental_use_v3_api.series.shadow_sample_rate",
                    json!(0.25),
                ),
                ConfigSetting::explicit(
                    "serializer_experimental_use_v3_api.series.shadow_sites",
                    json!(["us3.datadoghq.com"]),
                ),
                ConfigSetting::explicit("use_v2_api.series", json!(false)),
                ConfigSetting::explicit("use_v3_api.series.enabled", json!("false")),
                // The Agent sends an object-valued setting whole, and these entry keys contain dots.
                ConfigSetting::explicit(
                    "use_v3_api.series.endpoints",
                    json!({ "https://app.datadoghq.com": "true" }),
                ),
                ConfigSetting::explicit("observability_pipelines_worker.metrics.enabled", json!(true)),
                ConfigSetting::explicit(
                    "observability_pipelines_worker.metrics.url",
                    json!("https://opw.example.com"),
                ),
                ConfigSetting::explicit("observability_pipelines_worker.metrics.use_v3_api.series", json!(true)),
            ]))
            .await
            .unwrap();

        await_config(&system, "the streamed metrics V3 routing configuration", |config| {
            config.shared.metrics_encoding.v3_series_mode == V3SeriesMode::Disabled
                && config
                    .shared
                    .metrics_encoding
                    .v3_series_endpoint_modes
                    .get("https://app.datadoghq.com")
                    == Some(&V3SeriesMode::Enabled)
        })
        .await;

        let config = system.config();
        let metrics = &config.shared.metrics_encoding;
        assert!(!metrics.use_v2_series_api);
        assert_eq!(metrics.v3_api.compression_level, 7);
        assert_eq!(metrics.v3_api.series.endpoints, vec!["https://app.us3.datadoghq.com"]);
        assert!(metrics.v3_api.series.validate);
        assert!(metrics.v3_api.series.use_beta);
        assert_eq!(metrics.v3_api.series.beta_route, "/api/intake/metrics/custom/series");
        assert_eq!(metrics.v3_api.series.shadow_sample_rate, 0.25);
        assert_eq!(metrics.v3_api.series.shadow_sites, vec!["us3.datadoghq.com"]);

        let opw = &config.shared.endpoints.opw_intake;
        assert!(opw.enabled);
        assert_eq!(opw.url, "https://opw.example.com");
        assert!(opw.use_v3_series);
    }

    #[tokio::test]
    async fn nested_datadog_key_reaches_the_model() {
        // Sources deliver the Agent's canonical nested shape, which is what the Datadog
        // deserializer reads. A string list supplied as one space-separated string (the form an
        // environment variable carries) is still split on whitespace at the leaf.
        let system = standalone_system(
            Some(json!({
                "autoscaling": {
                    "failover": {
                        "enabled": true,
                        "metrics": "container.memory.usage container.cpu.usage",
                    }
                }
            })),
            None,
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
    async fn nested_saluki_only_key_seeds_the_model() {
        let system = standalone_system(Some(json!({ "data_plane": { "standalone_mode": true } })), None)
            .await
            .expect("system builds");

        assert!(system.config().control.standalone_mode);
    }

    #[tokio::test]
    async fn a_flattened_spelling_of_a_nested_key_is_not_read() {
        // Nothing translates `autoscaling_failover_enabled` into the nested slot: resolving an
        // environment variable to its canonical path is the environment readers' job, and they do it
        // before a value ever reaches this point. A flattened key arriving from any other source is
        // simply not a key the model knows.
        let system = standalone_system(Some(json!({ "autoscaling_failover_enabled": true })), None)
            .await
            .expect("system builds");

        assert!(!system.config().shared.autoscaling_failover.enabled);
    }

    #[tokio::test]
    async fn load_fails_on_translation_invalid_startup_config() {
        // Startup is the strict gate: a value figment accepts but the model rejects fails the load,
        // so the process never boots on bad config.
        let result = standalone_system(Some(json!({ "dogstatsd_tag_cardinality": "bogus" })), None).await;

        assert!(matches!(result, Err(Error::Translate { .. })));
    }

    #[tokio::test]
    async fn negative_dogstatsd_workers_count_is_rejected_at_startup() {
        let result = standalone_system(Some(json!({ "dogstatsd_workers_count": -1 })), None).await;

        let Err(error) = result else {
            panic!("negative worker count should fail the startup translation gate");
        };
        assert!(matches!(error, Error::Translate { .. }));
        assert!(error.to_string().contains("dogstatsd_workers_count"));
        assert!(error.to_string().contains("greater than or equal to 0"));
    }

    #[test]
    fn zero_otlp_trace_interner_size_is_rejected() {
        // Component builders used to discover this after translation. Reject zero before publishing
        // an invalid typed model.
        let sources = SourceTree::all_explicit(json!({ "otlp_config": { "traces": { "string_interner_size": 0 } } }));
        let error = translate_strict(&sources).expect_err("zero trace interner size should fail translation");

        assert!(matches!(error, Error::Deserialize { .. }));
        assert!(error.to_string().contains("value of bytes must be greater than zero"));
    }

    #[test]
    fn oversized_otlp_trace_interner_size_is_rejected() {
        let sources =
            SourceTree::all_explicit(json!({ "otlp_config": { "traces": { "string_interner_size": "2GiB" } } }));
        let error = translate_strict(&sources).expect_err("oversized trace interner should fail translation");

        assert!(matches!(error, Error::Deserialize { .. }));
        assert!(error.to_string().contains("must not exceed 1073741824 bytes"));
    }

    #[test]
    fn positive_otlp_trace_interner_size_is_accepted() {
        let sources =
            SourceTree::all_explicit(json!({ "otlp_config": { "traces": { "string_interner_size": "512KiB" } } }));
        let config = translate_strict(&sources).expect("positive trace interner size should translate");

        assert_eq!(config.domains.otlp.traces.string_interner_size.get(), 512 * 1024);
    }

    #[tokio::test]
    async fn standalone_loads_numeric_byte_size() {
        // A byte-size setting documented as accepting a bare integer (`10485760`) rather than a
        // string (`"10MB"`) must not abort the strict startup gate. The typed model normalizes it,
        // and the translator resolves it to the same byte count.
        let system = standalone_system(Some(json!({ "dogstatsd_log_file_max_size": 10485760 })), None)
            .await
            .expect("numeric byte size boots");

        assert_eq!(system.config().domains.dogstatsd.debug_log.log_file_max_size, 10485760);
    }

    #[test]
    fn a_configuration_without_an_api_key_is_rejected() {
        // Nothing ADP submits is accepted without a key, so the authoritative gate names the cause
        // rather than letting every flush fail authentication. Translation alone accepts this: the
        // schema default for `api_key` is the empty string, so nothing is missing to translate.
        let sources = SourceTree::all_explicit(json!({}));
        translate_strict(&sources).expect("an absent API key still translates");

        let error = translate_authoritative(&sources).expect_err("an absent API key should fail the gate");

        assert!(matches!(error, Error::MissingApiKey));
        assert!(error.to_string().contains("api_key"));
    }

    #[test]
    fn a_blank_api_key_is_rejected() {
        // An explicitly blank key is as unusable as an absent one, and whitespace is a typo worth
        // reporting rather than submitting.
        for key in ["", "   ", "\t\n"] {
            let sources = SourceTree::all_explicit(json!({ "api_key": key }));

            assert!(
                matches!(translate_authoritative(&sources), Err(Error::MissingApiKey)),
                "{key:?} should not count as an API key"
            );
        }

        let sources = SourceTree::all_explicit(json!({ "api_key": TEST_API_KEY }));
        assert_eq!(
            TEST_API_KEY,
            translate_authoritative(&sources)
                .expect("a real key passes the gate")
                .shared
                .endpoints
                .api_key
        );
    }

    #[tokio::test]
    async fn an_update_that_blanks_the_api_key_is_rejected_keeping_last_known_good() {
        // Validation covers runtime updates too: an update that leaves the process unable to submit
        // anything is rejected like any other invalid one, and the working key stays in place.
        let (system, agent_tx) = connected_system(json!({ "log_level": "warn" })).await;
        assert_eq!(TEST_API_KEY, system.config().shared.endpoints.api_key);

        agent_tx
            .send(ConfigUpdate::Partial(ConfigSetting::explicit("api_key", json!(""))))
            .await
            .unwrap();
        agent_tx
            .send(ConfigUpdate::Partial(ConfigSetting::explicit(
                "log_level",
                json!("error"),
            )))
            .await
            .unwrap();

        await_config(&system, "the later valid update to take effect", |c| {
            c.control.logging.level == "error"
        })
        .await;
        assert_eq!(TEST_API_KEY, system.config().shared.endpoints.api_key);
    }

    #[tokio::test]
    async fn standalone_loads_scalars_written_in_any_form_the_agent_casts() {
        // The Agent reads a setting by casting whatever its configuration holds to the accessor's
        // type, so a boolean written where the schema declares a string, or a quoted integer, is a
        // configuration it accepts. Each must reach the typed model instead of aborting the strict
        // startup gate.
        let system = standalone_system(
            Some(json!({
                "use_v3_api": { "series": { "enabled": true } },
                "dogstatsd_port": "8126",
            })),
            None,
        )
        .await
        .expect("scalars in Agent-castable forms boot");

        assert_eq!(
            system.config().shared.metrics_encoding.v3_series_mode,
            V3SeriesMode::Enabled
        );
        assert_eq!(system.config().domains.dogstatsd.listeners.port, 8126);
    }

    #[tokio::test]
    async fn translation_invalid_update_is_rejected_keeping_last_known_good() {
        let (system, agent_tx) =
            connected_system(json!({ "log_level": "warn", "dogstatsd_tag_cardinality": "high" })).await;
        assert_eq!(
            system.config().domains.dogstatsd.origin.tag_cardinality,
            OriginTagCardinality::High
        );

        // Send a translation-invalid update, then a valid update to a different field. Updates are
        // processed in order, so once the second is observed the first has already been handled.
        agent_tx
            .send(ConfigUpdate::Partial(ConfigSetting::explicit(
                "dogstatsd_tag_cardinality",
                json!("bogus"),
            )))
            .await
            .unwrap();
        agent_tx
            .send(ConfigUpdate::Partial(ConfigSetting::explicit(
                "log_level",
                json!("error"),
            )))
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
        let (system, agent_tx) = connected_system(json!({ "log_level": "info" })).await;

        let burst = [
            "warn", "error", "debug", "trace", "info", "warn", "error", "debug", "trace", "info", "warn", "error",
            "debug", "trace", "info", "warn", "error", "debug", "trace",
        ];
        for (i, level) in burst.iter().enumerate() {
            agent_tx
                .send(ConfigUpdate::Partial(ConfigSetting::explicit(
                    "log_level",
                    json!(level),
                )))
                .await
                .unwrap();
            // Interleave a translation-invalid update mid-burst, then correct it. The invalid value
            // is rejected whole (last-known-good retained) rather than wedging the task, so the
            // baseline keeps converging on the latest valid value regardless of the transient bad one.
            if i == burst.len() / 2 {
                agent_tx
                    .send(ConfigUpdate::Partial(ConfigSetting::explicit(
                        "dogstatsd_tag_cardinality",
                        json!("bogus"),
                    )))
                    .await
                    .unwrap();
                agent_tx
                    .send(ConfigUpdate::Partial(ConfigSetting::explicit(
                        "dogstatsd_tag_cardinality",
                        json!("high"),
                    )))
                    .await
                    .unwrap();
            }
        }
        let final_level = "error";
        agent_tx
            .send(ConfigUpdate::Partial(ConfigSetting::explicit(
                "log_level",
                json!(final_level),
            )))
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

    // Issue #1965: the Core Agent streams every setting it knows about, including the ones nobody
    // configured, so its schema defaults must not overwrite the local file.
    #[tokio::test]
    async fn a_defaulted_agent_value_does_not_erase_a_local_one() {
        let (system, agent_tx) = connected_system(json!({ "dd_url": "https://vector.example.com" })).await;

        agent_tx
            .send(ConfigUpdate::snapshot([ConfigSetting::new(
                "dd_url",
                json!("https://app.datadoghq.com"),
                StreamProvenance::Default,
            )]))
            .await
            .unwrap();
        // A later unrelated update is observable, so once it lands the snapshot above has been handled.
        agent_tx
            .send(ConfigUpdate::Partial(ConfigSetting::explicit(
                "log_level",
                json!("error"),
            )))
            .await
            .unwrap();
        await_config(&system, "the trailing update to take effect", |c| {
            c.control.logging.level == "error"
        })
        .await;

        let dd_url = &system.config().shared.endpoints.dd_url;
        assert!(dd_url.is_explicit());
        assert_eq!(dd_url.value, "https://vector.example.com");
    }

    #[tokio::test]
    async fn demoting_an_agent_value_to_a_default_reveals_the_local_value() {
        let (system, agent_tx) = connected_system(json!({ "dd_url": "https://vector.example.com" })).await;

        // The Agent takes over the setting.
        agent_tx
            .send(ConfigUpdate::Partial(ConfigSetting::explicit(
                "dd_url",
                json!("https://app.datadoghq.eu"),
            )))
            .await
            .unwrap();
        await_config(&system, "the Agent override to take effect", |c| {
            c.shared.endpoints.dd_url.value == "https://app.datadoghq.eu"
        })
        .await;

        // The operator removes it from the Agent's configuration, so the Agent now reports its own
        // default. The Agent layer must stop shadowing the local value rather than pinning the value
        // it last held.
        agent_tx
            .send(ConfigUpdate::Partial(ConfigSetting::new(
                "dd_url",
                json!("https://app.datadoghq.com"),
                StreamProvenance::Default,
            )))
            .await
            .unwrap();

        await_config(&system, "the local value to be revealed again", |c| {
            c.shared.endpoints.dd_url.value == "https://vector.example.com"
        })
        .await;
        assert!(system.config().shared.endpoints.dd_url.is_explicit());
    }

    #[tokio::test]
    async fn a_snapshot_replaces_the_agent_layer() {
        let (system, agent_tx) = connected_system(json!({})).await;

        agent_tx
            .send(ConfigUpdate::snapshot([ConfigSetting::explicit(
                "dogstatsd_port",
                json!(9125),
            )]))
            .await
            .unwrap();
        await_config(&system, "the first snapshot to take effect", |c| {
            c.domains.dogstatsd.listeners.port == 9125
        })
        .await;

        // A snapshot is the producer's complete state, so a setting it omits is no longer set and the
        // port returns to the schema default rather than lingering at 9125.
        agent_tx
            .send(ConfigUpdate::snapshot([ConfigSetting::explicit(
                "log_level",
                json!("error"),
            )]))
            .await
            .unwrap();

        await_config(&system, "the replacing snapshot to take effect", |c| {
            c.control.logging.level == "error"
        })
        .await;
        assert_eq!(system.config().domains.dogstatsd.listeners.port, 8125);
    }

    #[tokio::test]
    async fn a_live_view_wakes_when_only_provenance_changes() {
        let (system, agent_tx) = connected_system(json!({})).await;
        let mut dd_url = system.live(|c| &c.shared.endpoints.dd_url);
        // Nothing has set the URL, so it is the schema default the Agent supplies.
        assert_eq!(dd_url.provenance, Provenance::Default);
        assert_eq!(dd_url.value, "https://app.datadoghq.com");

        // The same URL, now deliberately chosen. The value is unchanged, so only provenance can carry
        // the fact that it became an override.
        agent_tx
            .send(ConfigUpdate::Partial(ConfigSetting::explicit(
                "dd_url",
                json!("https://app.datadoghq.com"),
            )))
            .await
            .unwrap();

        let updated = tokio::time::timeout(Duration::from_secs(2), dd_url.changed())
            .await
            .expect("the view observes a provenance-only change");
        assert_eq!(updated.provenance, Provenance::Explicit);
        assert_eq!(updated.value, "https://app.datadoghq.com");
    }

    #[tokio::test]
    async fn live_view_observes_debug_log_update() {
        let (system, agent_tx) = connected_system(json!({ "dogstatsd_metrics_stats_enable": false })).await;
        let mut view = system.live(|c| &c.domains.dogstatsd.debug_log);
        assert!(!view.metrics_stats_enable);

        agent_tx
            .send(ConfigUpdate::Partial(ConfigSetting::explicit(
                "dogstatsd_metrics_stats_enable",
                json!(true),
            )))
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
        let (system, agent_tx) = connected_system(json!({ "dogstatsd_metrics_stats_enable": false })).await;
        let mut stats = system.live(|c| &c.domains.dogstatsd.debug_log.metrics_stats_enable);
        assert!(!*stats);

        agent_tx
            .send(ConfigUpdate::Partial(ConfigSetting::explicit(
                "dogstatsd_metrics_stats_enable",
                json!(true),
            )))
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
        let system = standalone_system(Some(json!({ "dogstatsd_metrics_stats_enable": true })), None)
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
        // A small raw source map exercising a scalar conversion, an enum parse, a duration parse, the
        // raw endpoint inputs, and one seeded Saluki-only field.
        let sources = SourceTree::all_explicit(json!({
            "api_key": "abc",
            "dd_url": "https://custom.example.com",
            "dogstatsd_port": 9125,
            "dogstatsd_tag_cardinality": "high",
            "expected_tags_duration": "15s",
            "telemetry": { "dogstatsd_origin": true },
            "dogstatsd_tcp_port": 8126,
        }));
        let value = sources.to_value();
        let datadog: DatadogConfiguration = serde_json::from_value(value.clone()).expect("datadog source deserializes");
        let saluki: SalukiOnly = serde_json::from_value(value).expect("saluki-only source deserializes");

        let (config, errors) = translate(&datadog, &saluki, &sources);
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
        // Raw endpoint inputs: carried through without selecting a primary endpoint here.
        assert_eq!(config.shared.endpoints.api_key, "abc");
        assert!(config.shared.endpoints.dd_url.is_explicit());
        assert_eq!(config.shared.endpoints.dd_url.value, "https://custom.example.com");
        // Seeded Saluki-only field.
        assert_eq!(config.domains.dogstatsd.listeners.tcp_port, 8126);
    }
}
