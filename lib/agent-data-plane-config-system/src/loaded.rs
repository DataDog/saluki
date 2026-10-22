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
    /// The file's Datadog keys are read as the Datadog Agent reads them (see [`datadog_yaml`]). If
    /// that reading cannot be built or translated but the file read with every key as written can,
    /// the second reading is used: a respelled key whose value ADP cannot use is ignored rather than
    /// failing the load.
    ///
    /// # Errors
    ///
    /// Returns an error if a local source cannot be read, decoded, deserialized, or translated.
    pub async fn load(path: impl AsRef<Path>, env: EnvPrecedence) -> Result<Self, Error> {
        let path = path.as_ref();
        // Every value the local sources supply was set explicitly: the file set it or an environment
        // variable supplied it.
        let (error, base) = match build_base(path, env, KeyReading::Datadog).map(SourceTree::all_explicit) {
            Ok(base) => match translate_strict(&base) {
                Ok(local) => return Ok(Self { base, local }),
                Err(error) => (error, Some(base)),
            },
            Err(error) => (error, None),
        };

        let Ok(plain) = build_base(path, env, KeyReading::AsWritten).map(SourceTree::all_explicit) else {
            return Err(error);
        };
        if base.as_ref() == Some(&plain) {
            return Err(error);
        }
        match translate_strict(&plain) {
            Ok(local) => Ok(Self { base: plain, local }),
            Err(_) => Err(error),
        }
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

/// How [`build_base`] reads the configuration file's keys.
#[derive(Clone, Copy, Debug)]
enum KeyReading {
    /// Datadog keys are resolved as the Datadog Agent resolves them: dotted and differently cased
    /// spellings land at their canonical paths, and the map keys of a Datadog setting are rendered as
    /// the Agent renders them.
    Datadog,
    /// Every key is read as written.
    AsWritten,
}

/// Builds the typed base: the configuration file parsed to its nested shape, with environment
/// variables read directly and decoded into the schema's shapes on top.
///
/// `reading` sets how the file's keys are read. A null-valued key is dropped and an empty file is an
/// empty object. The environment is then overlaid via the generated Datadog reader, the Saluki-only
/// reader, and the canonical proxy variables. `env` sets whether the environment overwrites the file
/// (`AfterFile`) or only fills absent keys (`BeforeFile`); `Disabled` skips the environment entirely.
fn build_base(path: &Path, env: EnvPrecedence, reading: KeyReading) -> Result<Value, Error> {
    let text = std::fs::read_to_string(path).map_err(|e| Error::Base {
        message: format!("read `{}`: {e}", path.display()),
    })?;
    let parsed = match reading {
        KeyReading::Datadog => datadog_yaml::from_str(&text),
        KeyReading::AsWritten => saluki_config::yaml::from_str(&text).map_err(|e| e.to_string()),
    };
    let mut base = parsed.map_err(|e| Error::Base {
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
        let base = build_base(&path, EnvPrecedence::AfterFile, KeyReading::Datadog).expect("base builds");
        assert_eq!(base.get("dogstatsd_port"), Some(&json!(9125)));
        assert_eq!(base.get("dogstatsd_non_local_traffic"), Some(&json!(false)));
        assert!(base.get("empty_key").is_none());

        // BeforeFile: the file wins over the environment.
        let base = build_base(&path, EnvPrecedence::BeforeFile, KeyReading::Datadog).expect("base builds");
        assert_eq!(base.get("dogstatsd_port"), Some(&json!(8125)));

        // Disabled: the environment is ignored entirely.
        let base = build_base(&path, EnvPrecedence::Disabled, KeyReading::Datadog).expect("base builds");
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

        let result = build_base(&path, EnvPrecedence::AfterFile, KeyReading::Datadog);

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

        let base = SourceTree::all_explicit(
            build_base(&path, EnvPrecedence::AfterFile, KeyReading::Datadog).expect("base builds"),
        );
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
        let base = build_base(&path, EnvPrecedence::Disabled, KeyReading::Datadog);
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

    /// Returns whether `yaml` translates when every key is read as written, without the environment.
    fn translates_with_keys_as_written(test: &str, yaml: &str) -> bool {
        translates_with(test, yaml, KeyReading::AsWritten)
    }

    /// Returns whether `yaml` translates when its keys are read with `reading`, without the environment.
    fn translates_with(test: &str, yaml: &str, reading: KeyReading) -> bool {
        let path = std::env::temp_dir().join(format!("adp_{test}_{reading:?}_{}.yaml", std::process::id()));
        std::fs::write(&path, yaml).unwrap();
        let base = build_base(&path, EnvPrecedence::Disabled, reading);
        std::fs::remove_file(&path).ok();
        base.is_ok_and(|base| translate_strict(&SourceTree::all_explicit(base)).is_ok())
    }

    /// Loads `yaml` from a temporary file named for `test`, without the environment.
    fn try_load(test: &str, yaml: &str) -> Result<LoadedConfiguration, Error> {
        let path = std::env::temp_dir().join(format!("adp_{test}_load_{}.yaml", std::process::id()));
        std::fs::write(&path, yaml).unwrap();
        let loaded = block_on(LoadedConfiguration::load(&path, EnvPrecedence::Disabled));
        std::fs::remove_file(&path).ok();
        loaded
    }

    #[test]
    fn colliding_spellings_that_loaded_before_still_load_with_a_stable_winner() {
        // Each pair is one file written in both orders. A key in its canonical place wins; among
        // respelled keys the one written last wins; a respelled non-map stays where it was written.
        let cases = [
            // A dotted key and its nested spelling.
            (
                "data_plane.api_listen_address: a\ndata_plane: {api_listen_address: b}\n",
                "data_plane: {api_listen_address: b}\ndata_plane.api_listen_address: a\n",
                json!({"data_plane": {"api_listen_address": "b"}}),
                json!({"data_plane": {"api_listen_address": "b"}}),
            ),
            // A case alias and the canonical key.
            (
                "Log_Level: debug\nlog_level: info\n",
                "log_level: info\nLog_Level: debug\n",
                json!({"log_level": "info"}),
                json!({"log_level": "info"}),
            ),
            // Two respellings and no canonical key: the later one wins.
            (
                "Log_Level: debug\nLOG_LEVEL: warn\n",
                "LOG_LEVEL: warn\nLog_Level: debug\n",
                json!({"log_level": "warn"}),
                json!({"log_level": "debug"}),
            ),
            (
                "data_plane.api_listen_address: a\nDATA_PLANE: {api_listen_address: b}\n",
                "DATA_PLANE: {api_listen_address: b}\ndata_plane.api_listen_address: a\n",
                json!({"data_plane": {"api_listen_address": "b"}}),
                json!({"data_plane": {"api_listen_address": "a"}}),
            ),
            // A respelled map-valued leaf does not merge with the canonical one.
            (
                "additional_endpoints: {a: [k]}\nAdditional_Endpoints: {b: [j]}\n",
                "Additional_Endpoints: {b: [j]}\nadditional_endpoints: {a: [k]}\n",
                json!({"additional_endpoints": {"a": ["k"]}}),
                json!({"additional_endpoints": {"a": ["k"]}}),
            ),
            // A respelled scalar where a section belongs stays put; the section still forms.
            (
                "data_plane.otlp.enabled: true\nData_Plane: 5\n",
                "Data_Plane: 5\ndata_plane.otlp.enabled: true\n",
                json!({"data_plane": {"otlp": {"enabled": true}}, "Data_Plane": 5}),
                json!({"data_plane": {"otlp": {"enabled": true}}, "Data_Plane": 5}),
            ),
            (
                "Data_Plane: {otlp: 5}\ndata_plane.otlp.enabled: true\n",
                "data_plane.otlp.enabled: true\nData_Plane: {otlp: 5}\n",
                json!({"data_plane": {"otlp": {"enabled": true}}, "Data_Plane": {"otlp": 5}}),
                json!({"data_plane": {"otlp": {"enabled": true}}, "Data_Plane": {"otlp": 5}}),
            ),
            // Unknown and Saluki-only keys under a respelled section yield to the canonical section.
            (
                "data_plane: {Extra: 1, standalone_mode: false}\nDATA_PLANE: {Extra: 2, standalone_mode: true}\n",
                "DATA_PLANE: {Extra: 2, standalone_mode: true}\ndata_plane: {Extra: 1, standalone_mode: false}\n",
                json!({"data_plane": {"Extra": 1, "standalone_mode": false}}),
                json!({"data_plane": {"Extra": 1, "standalone_mode": false}}),
            ),
            // A null counts as absent, wherever it is written.
            (
                "log_level: ~\nLog_Level: debug\n",
                "Log_Level: debug\nlog_level: ~\n",
                json!({"log_level": "debug"}),
                json!({"log_level": "debug"}),
            ),
            (
                "Log_Level: ~\nlog_level: info\nDATA_PLANE: ~\ndata_plane.otlp.enabled: true\n",
                "data_plane.otlp.enabled: true\nDATA_PLANE: ~\nlog_level: info\nLog_Level: ~\n",
                json!({"log_level": "info", "data_plane": {"otlp": {"enabled": true}}}),
                json!({"log_level": "info", "data_plane": {"otlp": {"enabled": true}}}),
            ),
            // Leaf map keys that render the same keep one entry: the key written as its rendered text
            // wins, and otherwise the later key, including inside a list of maps.
            (
                "additional_endpoints: {010: [a], '8': [b], yes: [c], on: [d]}\n",
                "additional_endpoints: {on: [d], yes: [c], '8': [b], 010: [a]}\n",
                json!({"additional_endpoints": {"8": ["b"], "true": ["d"]}}),
                json!({"additional_endpoints": {"8": ["b"], "true": ["c"]}}),
            ),
            (
                "dogstatsd_mapper_profiles: [{name: p, prefix: 'p.', mappings: [{match: 'p.*', name: m, tags: {yes: a, 'true': b}}]}]\n",
                "dogstatsd_mapper_profiles: [{name: p, prefix: 'p.', mappings: [{match: 'p.*', name: m, tags: {'true': b, yes: a}}]}]\n",
                json!({"dogstatsd_mapper_profiles": [
                    {"name": "p", "prefix": "p.", "mappings": [{"match": "p.*", "name": "m", "tags": {"true": "b"}}]},
                ]}),
                json!({"dogstatsd_mapper_profiles": [
                    {"name": "p", "prefix": "p.", "mappings": [{"match": "p.*", "name": "m", "tags": {"true": "b"}}]},
                ]}),
            ),
        ];
        for (index, (first, second, first_base, second_base)) in cases.into_iter().enumerate() {
            for (order, yaml, expected) in [("first", first, first_base), ("second", second, second_base)] {
                let test = format!("collision_{index}_{order}");
                assert!(
                    translates_with_keys_as_written(&test, yaml),
                    "{yaml:?} did not load before"
                );
                assert_eq!(base_from_file(&test, yaml).expect(yaml), expected, "{yaml:?}");
                let loaded = try_load(&test, yaml);
                assert!(loaded.is_ok(), "{yaml:?}: {:?}", loaded.err());
            }
        }
    }

    #[tokio::test]
    async fn colliding_spellings_reach_the_model_with_the_canonical_value() {
        let loaded = load_from_file(
            "collision_model",
            concat!(
                "data_plane.api_listen_address: 127.0.0.1:1111\n",
                "data_plane: {api_listen_address: '127.0.0.1:2222', standalone_mode: false}\n",
                "DATA_PLANE: {standalone_mode: true}\n",
                "Log_Level: debug\n",
                "log_level: warn\n",
                "additional_endpoints: {010: [a], '8': [b]}\n",
            ),
        )
        .await;

        let config = loaded.local();
        assert_eq!(config.control.api_listen_address, "127.0.0.1:2222");
        assert!(!config.control.standalone_mode);
        assert_eq!(config.control.logging.level, "warn");
        let endpoints = &config.shared.endpoints.additional_endpoints;
        assert_eq!(endpoints.keys().collect::<Vec<_>>(), ["8"]);
        assert_eq!(endpoints["8"], ["b"]);
    }

    #[test]
    fn errors_the_plain_yaml_reading_raised_are_still_raised() {
        // Duplicate keys, including ones the parser compares by value, and null keys inside a
        // Datadog setting's map failed before this module resolved spellings, and still fail.
        let cases = [
            "log_level: info\nlog_level: debug\n",
            "data_plane:\n  api_listen_address: a\n  api_listen_address: b\n",
            "additional_endpoints: {010: [a], 8: [b]}\n",
            "additional_endpoints: {~: [a]}\n",
        ];
        for (index, yaml) in cases.into_iter().enumerate() {
            assert!(
                saluki_config::yaml::from_str::<Value>(yaml).is_err(),
                "{yaml:?} parsed before"
            );
            let message = base_error_message(&format!("still_fails_{index}"), yaml);
            assert!(message.starts_with("parse `"), "{yaml:?}: {message}");
        }

        // A scalar where the canonical spelling expects a section failed translation before, and a
        // respelled section does not displace it.
        let yaml = "data_plane: {otlp: 5}\nData_Plane: {otlp: {enabled: true}}\n";
        assert!(!translates_with_keys_as_written("still_fails_section", yaml));
        match try_load("still_fails_section", yaml) {
            Err(error) => assert!(
                error
                    .to_string()
                    .ends_with("invalid type: integer `5`, expected struct DataPlaneOtlp"),
                "{error}"
            ),
            Ok(_) => panic!("{yaml:?} should still fail"),
        }
    }

    #[test]
    fn a_respelled_key_whose_value_cannot_be_used_falls_back_to_keys_as_written() {
        let default_level = try_load("fallback_default", "")
            .expect("an empty file loads")
            .local()
            .control
            .logging
            .level
            .clone();
        // Each file loaded before, when its respelled keys were not read. Read as the Agent reads
        // them they cannot be translated, so every key is read as written, as for the last file,
        // whose valid respelling is ignored along with the invalid one.
        let cases = [
            "Log_Level: [a]\n",
            "DATA_PLANE: {standalone_mode: maybe}\n",
            "data_plane.otlp.enabled: maybe\n",
            "DogStatsD_Port: abc\nLog_Level: debug\n",
        ];
        for (index, yaml) in cases.into_iter().enumerate() {
            let test = format!("fallback_{index}");
            assert!(
                translates_with_keys_as_written(&test, yaml),
                "{yaml:?} did not load before"
            );
            assert!(
                !translates_with(&test, yaml, KeyReading::Datadog),
                "{yaml:?} needs no fallback"
            );
            let loaded = try_load(&test, yaml).unwrap_or_else(|e| panic!("{yaml:?}: {e}"));
            let config = loaded.local();
            assert_eq!(config.control.logging.level, default_level, "{yaml:?}");
            assert!(!config.control.standalone_mode, "{yaml:?}");
            assert!(!config.control.otlp, "{yaml:?}");
            assert_eq!(config.domains.dogstatsd.listeners.port, 8125, "{yaml:?}");
        }

        // A file that cannot be translated either way still fails, with the error its Agent reading
        // raises.
        let yaml = "dogstatsd_tag_cardinality: bogus\nLog_Level: debug\n";
        assert!(!translates_with_keys_as_written("fallback_fails", yaml));
        assert!(matches!(try_load("fallback_fails", yaml), Err(Error::Translate { .. })));
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
    fn the_environment_precedence_applies_to_the_resolved_file_value() {
        // The file value is the dotted key alone, or the canonical spelling when both are written.
        let files = [
            ("data_plane.api_listen_address: 127.0.0.1:1111\n", "127.0.0.1:1111"),
            (
                "data_plane.api_listen_address: 127.0.0.1:1111\ndata_plane: {api_listen_address: '127.0.0.1:3333'}\n",
                "127.0.0.1:3333",
            ),
        ];
        let _guard = test_env_lock();
        std::env::set_var("DD_DATA_PLANE_API_LISTEN_ADDRESS", "127.0.0.1:2222");
        for (index, (yaml, file_value)) in files.into_iter().enumerate() {
            let path = std::env::temp_dir().join(format!("adp_dotted_precedence_{index}_{}.yaml", std::process::id()));
            std::fs::write(&path, yaml).unwrap();

            let before = block_on(LoadedConfiguration::load(&path, EnvPrecedence::BeforeFile));
            let after = block_on(LoadedConfiguration::load(&path, EnvPrecedence::AfterFile));

            std::fs::remove_file(&path).ok();
            assert_eq!(
                before.expect("loads").local().control.api_listen_address,
                file_value,
                "{yaml:?}"
            );
            assert_eq!(
                after.expect("loads").local().control.api_listen_address,
                "127.0.0.1:2222",
                "{yaml:?}"
            );
        }
        std::env::remove_var("DD_DATA_PLANE_API_LISTEN_ADDRESS");
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
