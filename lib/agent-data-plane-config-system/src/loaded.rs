//! Loads local configuration before selecting its runtime authority.
//!
//! [`LoadedConfiguration::load`] prepares a typed snapshot of the local file and environment, which
//! [`LoadedConfiguration::local`] exposes before values from the Datadog Agent stream are applied.
//! [`LoadedConfiguration::run`] layers the Agent's configuration stream over the local sources,
//! while [`LoadedConfiguration::standalone`] keeps the local sources authoritative. Both methods
//! consume the loaded sources and return a [`ConfigurationSystem`].

use std::path::Path;

use agent_data_plane_config::SalukiConfiguration;
use datadog_agent_config::apply_datadog_env;
use saluki_config::dynamic::ConfigUpdate;
use serde_json::Value;
use tokio::sync::mpsc;

use crate::source::SourceTree;
use crate::system::{translate_strict, validate, ConfigurationSystem, ConfigurationUpdates, Error};
use crate::{datadog_yaml, saluki_env_overlay};

/// Where environment variables sit relative to the configuration file.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EnvPrecedence {
    /// Environment variables are read below the file: the file wins.
    ///
    /// The file takes precedence over environment variables.
    BeforeFile,
    /// Environment variables are read after the file: the environment wins. Matches the Datadog
    /// Agent's precedence and is the value ADP uses.
    AfterFile,
    /// Environment variables are not read.
    Disabled,
}

/// Local configuration prepared before a runtime authority is selected.
pub struct LoadedConfiguration {
    // Nested local base used for typed translation and Agent-layer merges.
    base: SourceTree,
    // Strictly translated local snapshot exposed before authority selection and used by standalone
    // mode.
    local: SalukiConfiguration,
}

impl LoadedConfiguration {
    /// Loads and strictly translates the local file and environment using the requested precedence.
    ///
    /// The snapshot is translated but not validated: it is not yet authoritative. Under the Datadog
    /// Agent the stream still has settings to contribute, so a local snapshot that could not run on
    /// its own is normal here. Whichever authority [`run`](Self::run) or
    /// [`standalone`](Self::standalone) selects applies validation.
    ///
    /// # Errors
    ///
    /// Returns an error if a local source cannot be read, decoded, deserialized, or translated.
    pub async fn load(path: impl AsRef<Path>, env: EnvPrecedence) -> Result<Self, Error> {
        // Every value the local sources supply was set explicitly: the file set it or an environment
        // variable supplied it.
        let base = SourceTree::all_explicit(build_base(path.as_ref(), env)?);
        let local = translate_strict(&base)?;
        Ok(Self { base, local })
    }

    /// Returns the typed snapshot of the local file and environment.
    ///
    /// Values from the Datadog Agent stream have not been applied to this snapshot.
    pub fn local(&self) -> &SalukiConfiguration {
        &self.local
    }

    /// Uses the Datadog Agent's configuration stream as the runtime authority.
    ///
    /// Waits for the initial Agent snapshot, layers it over the local sources, and strictly
    /// translates the result. Returns the system together with the [`ConfigurationUpdates`] that
    /// apply each later update. The caller must run them.
    ///
    /// # Errors
    ///
    /// Returns an error if the stream closes before its initial snapshot, or the merged configuration
    /// cannot be deserialized or translated.
    pub async fn run(
        self, config_stream: mpsc::Receiver<ConfigUpdate>,
    ) -> Result<(ConfigurationSystem, ConfigurationUpdates), Error> {
        ConfigurationSystem::connected(config_stream, self.base).await
    }

    /// Uses the translated local configuration as the runtime authority.
    ///
    /// No configuration stream or update task is created. Because nothing further will be layered on,
    /// the local snapshot is validated here, where [`run`](Self::run) instead validates the merged
    /// result of the Agent's initial snapshot.
    ///
    /// # Errors
    ///
    /// Returns an error if the local configuration fails validation.
    pub async fn standalone(self) -> Result<ConfigurationSystem, Error> {
        validate(&self.local)?;
        Ok(ConfigurationSystem::standalone(self.local, self.base))
    }
}

/// Builds the typed base: the configuration file parsed to its nested shape, with environment
/// variables read directly and decoded into the schema's shapes on top.
///
/// Datadog keys are resolved as the Datadog Agent resolves them, so dotted and differently cased
/// spellings land at their canonical paths and the map keys of a Datadog setting are rendered as the
/// Agent renders them (see [`datadog_yaml`]). A null-valued key is dropped and an empty file is an
/// empty object. The environment is then overlaid via the generated Datadog reader, the Saluki-only
/// reader, and the canonical proxy variables. `env` sets whether the environment overwrites the file
/// (`AfterFile`) or only fills absent keys (`BeforeFile`); `Disabled` skips the environment entirely.
fn build_base(path: &Path, env: EnvPrecedence) -> Result<Value, Error> {
    let text = std::fs::read_to_string(path).map_err(|e| Error::Base {
        message: format!("read `{}`: {e}", path.display()),
    })?;
    let mut base = datadog_yaml::from_str(&text).map_err(|e| Error::Base {
        message: format!("parse `{}`: {e}", path.display()),
    })?;
    drop_nulls(&mut base);
    if base.is_null() {
        base = Value::Object(serde_json::Map::new());
    }

    let overwrite = match env {
        EnvPrecedence::Disabled => return Ok(base),
        EnvPrecedence::AfterFile => true,
        EnvPrecedence::BeforeFile => false,
    };
    apply_datadog_env(&mut base, overwrite).map_err(|message| Error::Base { message })?;
    saluki_env_overlay::apply_env(&mut base, overwrite).map_err(|message| Error::Base { message })?;
    Ok(base)
}

/// Recursively removes object entries whose value is JSON null: an explicitly null YAML key must not
/// override a model default with null.
fn drop_nulls(value: &mut Value) {
    if let Value::Object(map) = value {
        map.retain(|_, v| !v.is_null());
        for v in map.values_mut() {
            drop_nulls(v);
        }
    }
}

#[cfg(test)]
mod tests {
    use bytesize::ByteSize;
    use saluki_config::test_env_lock;
    use serde_json::json;

    use super::*;

    #[test]
    fn build_base_composes_file_and_environment_by_precedence() {
        let _guard = test_env_lock();
        let path = std::env::temp_dir().join(format!("adp_build_base_{}.yaml", std::process::id()));
        std::fs::write(
            &path,
            "dogstatsd_port: 8125\ndogstatsd_non_local_traffic: false\nempty_key:\n",
        )
        .unwrap();
        std::env::set_var("DD_DOGSTATSD_PORT", "9125");

        // AfterFile: the environment wins over the file, the decoded value is a real number, an
        // explicitly null key is dropped, and an unrelated file key is preserved.
        let base = build_base(&path, EnvPrecedence::AfterFile).expect("base builds");
        assert_eq!(base.get("dogstatsd_port"), Some(&json!(9125)));
        assert_eq!(base.get("dogstatsd_non_local_traffic"), Some(&json!(false)));
        assert!(base.get("empty_key").is_none());

        // BeforeFile: the file wins over the environment.
        let base = build_base(&path, EnvPrecedence::BeforeFile).expect("base builds");
        assert_eq!(base.get("dogstatsd_port"), Some(&json!(8125)));

        // Disabled: the environment is ignored entirely.
        let base = build_base(&path, EnvPrecedence::Disabled).expect("base builds");
        assert_eq!(base.get("dogstatsd_port"), Some(&json!(8125)));

        std::env::remove_var("DD_DOGSTATSD_PORT");
        std::fs::remove_file(&path).ok();
    }

    #[tokio::test]
    async fn local_exposes_translated_configuration() {
        // Disable environment reads so this test does not need `ENV_MUTEX`.
        let path = std::env::temp_dir().join(format!("adp_local_{}.yaml", std::process::id()));
        std::fs::write(&path, "log_level: warn\ndogstatsd_port: 9125\n").unwrap();

        let loaded = LoadedConfiguration::load(&path, EnvPrecedence::Disabled)
            .await
            .expect("local sources load");
        let config = loaded.local();

        assert_eq!(config.control.logging.level, "warn");
        assert_eq!(config.domains.dogstatsd.listeners.port, 9125);

        std::fs::remove_file(&path).ok();
    }

    #[tokio::test]
    async fn load_rejects_translation_invalid_local_sources() {
        let path = std::env::temp_dir().join(format!("adp_local_bad_{}.yaml", std::process::id()));
        // The file parses, but typed translation rejects the value.
        std::fs::write(&path, "dogstatsd_tag_cardinality: bogus\n").unwrap();

        let result = LoadedConfiguration::load(&path, EnvPrecedence::Disabled).await;

        std::fs::remove_file(&path).ok();
        assert!(matches!(result, Err(Error::Translate { .. })));
    }

    #[tokio::test]
    async fn load_leaves_an_incomplete_local_snapshot_to_the_selected_authority() {
        // A local snapshot with no API key is normal: under the Datadog Agent the key arrives over the
        // configuration stream, and the CLI subcommands read this snapshot without submitting
        // anything. Validation therefore belongs to whichever authority the caller then selects.
        let path = std::env::temp_dir().join(format!("adp_no_api_key_{}.yaml", std::process::id()));
        std::fs::write(&path, "log_level: warn\n").unwrap();

        let loaded = LoadedConfiguration::load(&path, EnvPrecedence::Disabled)
            .await
            .expect("a local snapshot without an API key loads");
        assert_eq!("", loaded.local().shared.endpoints.api_key);

        // Standalone mode makes that same snapshot authoritative, so the missing key is fatal there.
        let result = loaded.standalone().await;

        std::fs::remove_file(&path).ok();
        assert!(matches!(result, Err(Error::MissingApiKey)));
    }

    /// Loads the local sources from a temporary configuration file with the given contents.
    ///
    /// `test` names the file, so that tests which run at the same time do not share one.
    async fn load_from_file(test: &str, contents: &str) -> LoadedConfiguration {
        let path = std::env::temp_dir().join(format!("adp_{test}_{}.yaml", std::process::id()));
        std::fs::write(&path, contents).unwrap();
        let loaded = LoadedConfiguration::load(&path, EnvPrecedence::Disabled)
            .await
            .expect("local sources load");
        std::fs::remove_file(&path).ok();
        loaded
    }

    /// Waits until `system` reports `level` as the log level.
    async fn await_log_level(system: &ConfigurationSystem, level: &str) {
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while system.config().control.logging.level != level {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("the configuration should report the log level `{level}`"));
    }

    fn log_level_update(level: &str) -> ConfigUpdate {
        use saluki_config::dynamic::ConfigSetting;

        ConfigUpdate::Partial(ConfigSetting::explicit("log_level", json!(level)))
    }

    #[tokio::test]
    async fn run_applies_later_updates_only_once_the_updates_run() {
        let loaded = load_from_file("run_updates", "api_key: test-api-key\nlog_level: info\n").await;

        // `run` itself applies the initial snapshot. This is the startup gate.
        let (agent_tx, agent_rx) = mpsc::channel(8);
        agent_tx.send(ConfigUpdate::snapshot([])).await.unwrap();
        let (system, updates) = loaded.run(agent_rx).await.expect("the initial snapshot is accepted");
        assert_eq!(system.config().control.logging.level, "info");

        // A later update waits until the updates run. Nothing applies it in the background before
        // then.
        agent_tx.send(log_level_update("warn")).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(system.config().control.logging.level, "info");

        tokio::spawn(updates.run());
        await_log_level(&system, "warn").await;
    }

    #[tokio::test]
    async fn updates_continue_where_a_stopped_run_left_off() {
        let loaded = load_from_file("run_restart", "api_key: test-api-key\nlog_level: info\n").await;

        let (agent_tx, agent_rx) = mpsc::channel(8);
        agent_tx.send(ConfigUpdate::snapshot([])).await.unwrap();
        let (system, updates) = loaded.run(agent_rx).await.expect("the initial snapshot is accepted");

        let first_run = tokio::spawn(updates.run());
        agent_tx.send(log_level_update("warn")).await.unwrap();
        await_log_level(&system, "warn").await;

        // Stop the run, as a supervisor does when it restarts a worker. The next run reads the same
        // stream, so an update sent in between is not lost.
        first_run.abort();
        let _ = first_run.await;
        agent_tx.send(log_level_update("error")).await.unwrap();

        tokio::spawn(updates.run());
        await_log_level(&system, "error").await;
    }

    #[tokio::test]
    async fn updates_fail_once_the_stream_closes() {
        let loaded = load_from_file("run_ended", "api_key: test-api-key\n").await;

        let (agent_tx, agent_rx) = mpsc::channel(8);
        agent_tx.send(ConfigUpdate::snapshot([])).await.unwrap();
        let (_system, updates) = loaded.run(agent_rx).await.expect("the initial snapshot is accepted");
        drop(agent_tx);

        // The stream does not reopen, so running the updates again fails in the same way.
        assert!(matches!(updates.run().await, Err(Error::UpdateStreamClosed)));
        assert!(matches!(updates.run().await, Err(Error::UpdateStreamClosed)));
    }

    #[tokio::test]
    async fn run_fails_if_the_stream_closes_before_the_initial_snapshot() {
        let loaded = load_from_file("run_closed", "api_key: test-api-key\n").await;

        let (agent_tx, agent_rx) = mpsc::channel::<ConfigUpdate>(1);
        drop(agent_tx);

        let result = loaded.run(agent_rx).await;
        assert!(matches!(result, Err(Error::StreamClosed)));
    }

    // `LoadedConfiguration::load` is `async` only for symmetry with the rest of the API; it awaits
    // nothing. The environment tests below drive it on a local runtime rather than with
    // `#[tokio::test]`, so the blocking environment guard is never held across an await point.
    fn block_on<F: std::future::Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime builds")
            .block_on(future)
    }

    #[test]
    fn a_saluki_only_environment_variable_reaches_the_model() {
        // End to end for a key the Datadog schema does not declare: `DD_DATA_PLANE_STANDALONE_MODE`
        // is read at its canonical path by the Saluki-only reader and seeds `control.standalone_mode`.
        let _guard = test_env_lock();
        let path = std::env::temp_dir().join(format!("adp_saluki_env_{}.yaml", std::process::id()));
        std::fs::write(&path, "{}\n").unwrap();
        std::env::set_var("DD_DATA_PLANE_STANDALONE_MODE", "true");

        let loaded = block_on(LoadedConfiguration::load(&path, EnvPrecedence::AfterFile)).expect("local sources load");

        std::env::remove_var("DD_DATA_PLANE_STANDALONE_MODE");
        std::fs::remove_file(&path).ok();
        assert!(loaded.local().control.standalone_mode);
    }

    #[test]
    fn ottl_filter_error_mode_environment_variable_reaches_the_model() {
        // `DD_OTTL_FILTER_CONFIG_ERROR_MODE` exercises the enum leaf: the environment value is a
        // plain string, decoded as a Saluki-only leaf, then validated during configuration
        // deserialization.
        use agent_data_plane_config::domains::traces::OttlErrorMode;

        let _guard = test_env_lock();
        let path = std::env::temp_dir().join(format!("adp_ottl_env_{}.yaml", std::process::id()));
        std::fs::write(&path, "{}\n").unwrap();
        std::env::set_var("DD_OTTL_FILTER_CONFIG_ERROR_MODE", "silent");

        let loaded = block_on(LoadedConfiguration::load(&path, EnvPrecedence::AfterFile)).expect("local sources load");

        std::env::remove_var("DD_OTTL_FILTER_CONFIG_ERROR_MODE");
        std::fs::remove_file(&path).ok();
        assert_eq!(
            loaded.local().domains.traces.ottl_filter.error_mode,
            OttlErrorMode::Silent
        );
    }

    #[test]
    fn a_structured_saluki_only_environment_variable_reaches_the_model() {
        let _guard = test_env_lock();
        let path = std::env::temp_dir().join(format!("adp_saluki_structured_env_{}.yaml", std::process::id()));
        std::fs::write(&path, "{}\n").unwrap();
        std::env::set_var(
            "DD_METRIC_TAG_VALUE_ALLOWLIST",
            r#"[{"metric_prefix":"requests.","tag_name":"customer_id","values":["customer-1"],"on_miss":"replace","replacement":"other"}]"#,
        );

        let loaded = block_on(LoadedConfiguration::load(&path, EnvPrecedence::AfterFile)).expect("local sources load");

        std::env::remove_var("DD_METRIC_TAG_VALUE_ALLOWLIST");
        std::fs::remove_file(&path).ok();
        let entries = &loaded.local().domains.dogstatsd.tag_value_allowlist;
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].metric_prefix, "requests.");
        assert_eq!(entries[0].tag_name, "customer_id");
        assert_eq!(entries[0].values, ["customer-1"]);
        assert_eq!(
            entries[0].on_miss,
            agent_data_plane_config::domains::dogstatsd::TagValueMismatchAction::Replace
        );
        assert_eq!(entries[0].replacement, "other");
    }

    #[test]
    fn metric_tag_value_allowlist_from_file_preserves_whitespace() {
        let path = std::env::temp_dir().join(format!("adp_tag_value_allowlist_{}.yaml", std::process::id()));
        std::fs::write(
            &path,
            "metric_tag_value_allowlist:\n  - metric_prefix: ' requests. '\n    tag_name: ' customer_id '\n    values: [' customer-1 ']\n    on_miss: replace\n    replacement: ' other '\n",
        )
        .unwrap();

        let loaded = block_on(LoadedConfiguration::load(&path, EnvPrecedence::Disabled))
            .expect("allow-list strings containing whitespace should load");

        std::fs::remove_file(&path).ok();
        let entries = &loaded.local().domains.dogstatsd.tag_value_allowlist;
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].metric_prefix, " requests. ");
        assert_eq!(entries[0].tag_name, " customer_id ");
        assert_eq!(entries[0].values, [" customer-1 "]);
        assert_eq!(entries[0].replacement, " other ");
    }

    #[test]
    fn invalid_metric_tag_value_allowlist_from_environment_fails_load() {
        let _guard = test_env_lock();
        let path = std::env::temp_dir().join(format!("adp_bad_tag_value_allowlist_env_{}.yaml", std::process::id()));
        std::fs::write(&path, "{}\n").unwrap();
        std::env::set_var(
            "DD_METRIC_TAG_VALUE_ALLOWLIST",
            r#"[{"metric_prefix":"requests.","tag_name":"customer_id"},{"metric_prefix":"requests.api.","tag_name":"customer_id"}]"#,
        );

        let result = block_on(LoadedConfiguration::load(&path, EnvPrecedence::AfterFile));

        std::env::remove_var("DD_METRIC_TAG_VALUE_ALLOWLIST");
        std::fs::remove_file(&path).ok();
        let error = match result {
            Err(error) => error,
            Ok(_) => panic!("overlapping environment allow-list should fail loading"),
        };
        assert!(matches!(error, Error::Deserialize { .. }));
        assert!(error.to_string().contains("overlapping metric prefixes"));
    }

    #[test]
    fn the_adp_zstd_override_reaches_the_model_from_the_environment() {
        // The documented environment variable for a Saluki-only key must reach the model, since no
        // Datadog schema key covers it.
        let _guard = test_env_lock();
        let path = std::env::temp_dir().join(format!("adp_zstd_env_{}.yaml", std::process::id()));
        std::fs::write(&path, "{}\n").unwrap();
        std::env::set_var("DD_DATA_PLANE_SERIALIZER_ZSTD_COMPRESSOR_LEVEL", "7");

        let loaded = block_on(LoadedConfiguration::load(&path, EnvPrecedence::AfterFile)).expect("local sources load");
        let level = loaded.local().shared.endpoints.compression.effective_zstd_level();

        std::env::remove_var("DD_DATA_PLANE_SERIALIZER_ZSTD_COMPRESSOR_LEVEL");
        std::fs::remove_file(&path).ok();
        assert_eq!(level, 7);
    }

    #[test]
    fn the_adp_stop_timeout_override_reaches_the_model_from_the_environment() {
        let _guard = test_env_lock();
        let path = std::env::temp_dir().join(format!("adp_stop_timeout_env_{}.yaml", std::process::id()));
        std::fs::write(&path, "{}\n").unwrap();
        std::env::set_var("DD_DATA_PLANE_STOP_TIMEOUT", "45");

        let loaded = block_on(LoadedConfiguration::load(&path, EnvPrecedence::AfterFile)).expect("local sources load");
        let stop_timeout = loaded.local().control.stop_timeout;

        std::env::remove_var("DD_DATA_PLANE_STOP_TIMEOUT");
        std::fs::remove_file(&path).ok();
        assert_eq!(stop_timeout, Some(std::time::Duration::from_secs(45)));
    }

    #[test]
    fn build_base_rejects_a_malformed_environment_value() {
        let _guard = test_env_lock();
        let path = std::env::temp_dir().join(format!("adp_build_base_bad_{}.yaml", std::process::id()));
        std::fs::write(&path, "dogstatsd_port: 8125\n").unwrap();
        std::env::set_var("DD_DOGSTATSD_PORT", "not-a-number");

        let result = build_base(&path, EnvPrecedence::AfterFile);

        std::env::remove_var("DD_DOGSTATSD_PORT");
        std::fs::remove_file(&path).ok();
        assert!(matches!(result, Err(Error::Base { .. })));
    }

    #[test]
    fn build_base_accepts_a_human_readable_dogstatsd_interner_size() {
        let _guard = test_env_lock();
        let path = std::env::temp_dir().join(format!("adp_build_base_interner_{}.yaml", std::process::id()));
        std::fs::write(&path, "{}\n").unwrap();
        std::env::set_var("DD_DOGSTATSD_STRING_INTERNER_SIZE_BYTES", "12MiB");

        let base = SourceTree::all_explicit(build_base(&path, EnvPrecedence::AfterFile).expect("base builds"));
        let config = translate_strict(&base).expect("human-readable byte size translates");

        std::env::remove_var("DD_DOGSTATSD_STRING_INTERNER_SIZE_BYTES");
        std::fs::remove_file(&path).ok();
        assert_eq!(
            config.domains.dogstatsd.contexts.string_interner_size_bytes,
            Some(ByteSize::mib(12).as_u64())
        );
    }

    /// Writes `contents` to a temporary configuration file named for `test` and returns the base built from it,
    /// without reading the environment.
    fn base_from_file(test: &str, contents: &str) -> Result<Value, Error> {
        let path = std::env::temp_dir().join(format!("adp_{test}_{}.yaml", std::process::id()));
        std::fs::write(&path, contents).unwrap();
        let base = build_base(&path, EnvPrecedence::Disabled);
        std::fs::remove_file(&path).ok();
        base
    }

    /// Returns the message of the error that building a base from `contents` fails with.
    fn base_error_message(test: &str, contents: &str) -> String {
        match base_from_file(test, contents) {
            Err(Error::Base { message }) => message,
            other => panic!("expected the base to fail, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn yaml_1_1_scalars_in_datadog_keys_reach_the_model_as_the_agent_reads_them() {
        let loaded = load_from_file("agent_scalars", "env: yes\ndogstatsd_buffer_size: 010\n").await;
        assert_eq!(loaded.local().domains.traces.env, "true");
        assert_eq!(loaded.local().domains.dogstatsd.listeners.buffer_size, 8);

        let loaded = load_from_file("agent_octal_env", "env: 0755\n").await;
        assert_eq!(loaded.local().domains.traces.env, "493");
    }

    #[tokio::test]
    async fn additional_endpoints_keys_reach_the_model_as_the_agent_renders_them() {
        let loaded = load_from_file(
            "agent_endpoint_keys",
            "additional_endpoints:\n  true: [k]\n  010: [j]\n  'https://x.test': [i]\n  '010 ': [h]\n",
        )
        .await;

        let endpoints = &loaded.local().shared.endpoints.additional_endpoints;
        let mut keys: Vec<_> = endpoints.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(keys, ["010 ", "8", "https://x.test", "true"]);
        assert_eq!(endpoints["8"], ["j"]);
        assert_eq!(endpoints["true"], ["k"]);
    }

    #[tokio::test]
    async fn dotted_and_cased_datadog_keys_reach_the_model() {
        let loaded = load_from_file(
            "dotted_keys",
            "data_plane.api_listen_address: 127.0.0.1:9111\nLog_Level: debug\ndata_plane:\n  otlp.enabled: true\n",
        )
        .await;

        let control = &loaded.local().control;
        assert_eq!(control.api_listen_address, "127.0.0.1:9111");
        assert_eq!(control.logging.level, "debug");
        assert!(control.otlp);
    }

    #[test]
    fn datadog_key_spellings_resolve_to_one_canonical_base() {
        let cases = [
            // The root and a section each accept a dotted key.
            (
                "data_plane.api_listen_address: a\n",
                json!({"data_plane": {"api_listen_address": "a"}}),
            ),
            (
                "data_plane: {otlp.enabled: true}\n",
                json!({"data_plane": {"otlp": {"enabled": true}}}),
            ),
            (
                "data_plane.otlp: {enabled: true, proxy.enabled: false}\n",
                json!({"data_plane": {"otlp": {"enabled": true, "proxy": {"enabled": false}}}}),
            ),
            // Case folds at every level, including inside a dotted key.
            ("Log_Level: debug\n", json!({"log_level": "debug"})),
            (
                "DATA_PLANE.OTLP.Enabled: true\n",
                json!({"data_plane": {"otlp": {"enabled": true}}}),
            ),
            (
                "Data_Plane: {Api_Listen_Address: a}\n",
                json!({"data_plane": {"api_listen_address": "a"}}),
            ),
            // Spellings of different leaves in one section merge by key.
            (
                "data_plane.api_listen_address: a\ndata_plane: {otlp: {enabled: true}}\nDATA_PLANE.otlp.proxy.enabled: false\n",
                json!({"data_plane": {
                    "api_listen_address": "a",
                    "otlp": {"enabled": true, "proxy": {"enabled": false}},
                }}),
            ),
            // The keys of a leaf's value are rendered, and the leaf itself moves.
            (
                "Additional_Endpoints: {010: [k]}\n",
                json!({"additional_endpoints": {"8": ["k"]}}),
            ),
            // A null counts as absent.
            (
                "log_level: debug\nLOG_LEVEL:\ndata_plane: ~\ndata_plane.otlp.enabled: true\n",
                json!({"log_level": "debug", "data_plane": {"otlp": {"enabled": true}}}),
            ),
            // A canonical document is unchanged.
            (
                "log_level: info\ndata_plane:\n  otlp:\n    enabled: true\n",
                json!({"log_level": "info", "data_plane": {"otlp": {"enabled": true}}}),
            ),
        ];
        for (index, (yaml, expected)) in cases.into_iter().enumerate() {
            let base = base_from_file(&format!("spellings_{index}"), yaml).expect(yaml);
            assert_eq!(base, expected, "{yaml:?}");
        }
    }

    #[test]
    fn keys_outside_the_datadog_schema_keep_their_position_and_spelling() {
        // `host.name` is inside an unknown map, at the root and inside a Datadog section; `Mixed.Case`,
        // `x.y` and `Other` are unknown keys at the root and inside a Datadog section. A dotted path
        // through a leaf stays a literal key too.
        let base = base_from_file(
            "unknown_keys",
            concat!(
                "unknown_labels_xyz: {host.name: a, 010: b}\n",
                "Mixed.Case: 1\n",
                "data_plane: {x.y: 2, Other: 3, unknown_map: {host.name: c, 010: d}}\n",
                "log_level.extra: 4\n",
                "additional_endpoints.https://x.test: 5\n",
            ),
        )
        .unwrap();

        assert_eq!(
            base,
            json!({
                "unknown_labels_xyz": {"host.name": "a", "010": "b"},
                "Mixed.Case": 1,
                "data_plane": {"x.y": 2, "Other": 3, "unknown_map": {"host.name": "c", "010": "d"}},
                "log_level.extra": 4,
                "additional_endpoints.https://x.test": 5,
            })
        );
    }

    #[test]
    fn conflicting_spellings_of_a_datadog_path_fail_the_load() {
        let cases = [
            (
                "data_plane.api_listen_address: a\ndata_plane: {api_listen_address: b}\n",
                "`data_plane.api_listen_address` is set more than once, as `data_plane > api_listen_address` and as `data_plane.api_listen_address`",
            ),
            (
                "Log_Level: debug\nlog_level: info\n",
                "`log_level` is set more than once, as `Log_Level` and as `log_level`",
            ),
            (
                "data_plane: 5\ndata_plane.otlp.enabled: true\n",
                "`data_plane` is set more than once, as `data_plane` and as `data_plane.otlp.enabled`",
            ),
            (
                "data_plane: {otlp: 5}\nData_Plane: {otlp: {enabled: true}}\n",
                "`data_plane.otlp` is set more than once, as `Data_Plane > otlp` and as `data_plane > otlp`",
            ),
            (
                "additional_endpoints: {a: [k]}\nAdditional_Endpoints: {b: [j]}\n",
                "`additional_endpoints` is set more than once, as `Additional_Endpoints` and as `additional_endpoints`",
            ),
            (
                "data_plane: {Extra: 1}\nDATA_PLANE: {Extra: 2}\n",
                "`data_plane.Extra` is set more than once, as `DATA_PLANE > Extra` and as `data_plane > Extra`",
            ),
        ];
        for (index, (yaml, expected)) in cases.into_iter().enumerate() {
            let message = base_error_message(&format!("conflict_{index}"), yaml);
            assert!(message.starts_with("parse `"), "{message}");
            assert!(
                message.ends_with(&format!("{expected}; remove one of them")),
                "{yaml:?}: {message}"
            );
        }
    }

    #[test]
    fn a_conflict_does_not_reveal_the_values() {
        let message = base_error_message(
            "conflict_redaction",
            "api_key: sk-first-secret\nAPI_KEY: sk-second-secret\n",
        );

        assert!(
            message.ends_with("`api_key` is set more than once, as `API_KEY` and as `api_key`; remove one of them"),
            "{message}"
        );
        assert!(!message.contains("sk-"), "{message}");
    }

    #[test]
    fn a_leaf_map_with_keys_rendering_the_same_fails_the_load() {
        let message = base_error_message("leaf_duplicate", "additional_endpoints:\n  yes: [a]\n  true: [b]\n");

        assert!(message.starts_with("parse `"), "{message}");
        assert!(
            message.contains(": duplicate mapping key `true` (unquoted keys are read as YAML values; remove one, or quote it to keep its spelling) at line"),
            "{message}"
        );
    }

    #[tokio::test]
    async fn saluki_only_keys_keep_their_spelling() {
        // A dotted Saluki-only key is a flat key the typed reader does not see, as before, and the keys
        // of a Saluki-only map are not rendered.
        let yaml = concat!(
            "data_plane.standalone_mode: true\n",
            "experimental:\n",
            "  metrics_endpoint_routing:\n",
            "    metric_allowlist: {'https://x.test': [a], 010: [b], yes: [c]}\n",
        );
        let base = base_from_file("saluki_only_base", yaml).unwrap();
        assert_eq!(base["data_plane.standalone_mode"], json!(true));
        assert!(base.get("data_plane").is_none());

        let loaded = load_from_file("saluki_only", yaml).await;

        assert!(!loaded.local().control.standalone_mode);
        assert_eq!(
            serde_json::to_value(&loaded.local().domains.metrics_endpoint_routing.metric_allowlists).unwrap(),
            json!({"https://x.test": ["a"], "010": ["b"], "yes": ["c"]})
        );
    }

    #[tokio::test]
    async fn a_saluki_only_key_moves_with_a_respelled_datadog_section() {
        // `data_plane` is a Datadog section, so its other spellings move to it along with the
        // Saluki-only keys they hold.
        for (index, yaml) in [
            "DATA_PLANE: {standalone_mode: true}\n",
            "data_plane.otlp: {enabled: true}\nData_Plane: {standalone_mode: true}\n",
        ]
        .into_iter()
        .enumerate()
        {
            let loaded = load_from_file(&format!("respelled_section_{index}"), yaml).await;
            assert!(loaded.local().control.standalone_mode, "{yaml:?}");
        }
    }

    #[test]
    fn the_file_wins_over_the_environment_for_a_dotted_key_before_the_file() {
        let _guard = test_env_lock();
        let path = std::env::temp_dir().join(format!("adp_dotted_precedence_{}.yaml", std::process::id()));
        std::fs::write(&path, "data_plane.api_listen_address: 127.0.0.1:1111\n").unwrap();
        std::env::set_var("DD_DATA_PLANE_API_LISTEN_ADDRESS", "127.0.0.1:2222");

        let before = block_on(LoadedConfiguration::load(&path, EnvPrecedence::BeforeFile));
        let after = block_on(LoadedConfiguration::load(&path, EnvPrecedence::AfterFile));

        std::env::remove_var("DD_DATA_PLANE_API_LISTEN_ADDRESS");
        std::fs::remove_file(&path).ok();
        assert_eq!(
            before.expect("loads").local().control.api_listen_address,
            "127.0.0.1:1111"
        );
        assert_eq!(
            after.expect("loads").local().control.api_listen_address,
            "127.0.0.1:2222"
        );
    }

    #[test]
    fn non_finite_numbers_in_a_numeric_leaf_still_fail_the_load() {
        for (index, spelling) in [".nan", ".inf", "-.inf", ".NaN"].into_iter().enumerate() {
            let yaml = format!("forwarder_backoff_factor: {spelling}\n");
            let path = std::env::temp_dir().join(format!("adp_non_finite_{index}_{}.yaml", std::process::id()));
            std::fs::write(&path, yaml).unwrap();

            let result = block_on(LoadedConfiguration::load(&path, EnvPrecedence::Disabled));

            std::fs::remove_file(&path).ok();
            let error = match result {
                Err(error) => error,
                Ok(_) => panic!("`{spelling}` should fail the load"),
            };
            assert!(matches!(error, Error::Deserialize { .. }), "`{spelling}`: {error}");
            assert!(
                error
                    .to_string()
                    .ends_with("expected a number, a numeric string, or a boolean"),
                "`{spelling}`: {error}"
            );
        }
    }
}
