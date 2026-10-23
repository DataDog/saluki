//! Loads local configuration before selecting its runtime authority.
//!
//! [`LoadedConfiguration::load`] prepares a typed snapshot of the local file and environment, which
//! [`LoadedConfiguration::local`] exposes before values from the Datadog Agent stream are applied.
//! [`LoadedConfiguration::run`] layers the Agent's configuration stream over the local sources,
//! while [`LoadedConfiguration::standalone`] keeps the local sources authoritative. Both methods
//! consume the loaded sources and return a [`ConfigurationSystem`].

use std::path::Path;

use agent_data_plane_config::SalukiConfiguration;
use datadog_agent_config::apply_datadog_env_vars;
use saluki_config::dynamic::ConfigUpdate;
use serde_json::Value;
use tokio::sync::mpsc;

use crate::saluki_env_overlay;
use crate::source::SourceTree;
use crate::system::{translate_strict, validate, ConfigurationSystem, ConfigurationUpdates, Error};

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
/// A null-valued key is dropped and an empty file is an empty object. The environment is then
/// overlaid via the generated Datadog reader, the Saluki-only reader, and the canonical proxy
/// variables. `env` sets whether the environment overwrites the file (`AfterFile`) or only fills
/// absent keys (`BeforeFile`); `Disabled` skips the environment entirely.
fn build_base(path: &Path, env: EnvPrecedence) -> Result<Value, Error> {
    let text = std::fs::read_to_string(path).map_err(|e| Error::Base {
        message: format!("read `{}`: {e}", path.display()),
    })?;
    // `Disabled` never reads the process environment, so it is not captured either.
    let vars: Vec<(String, String)> = match env {
        EnvPrecedence::Disabled => Vec::new(),
        EnvPrecedence::AfterFile | EnvPrecedence::BeforeFile => std::env::vars().collect(),
    };
    build_base_from(&text, path, &vars, env)
}

/// Builds the typed base from the configuration file's text and explicit environment variables.
///
/// The same composition as [`build_base`], without reading the filesystem or the process
/// environment: `text` is the file's content, `origin` is the path named in a parse error, and
/// `vars` are the environment variable name/value pairs. `vars` is ignored when `env` is
/// `Disabled`.
///
/// # Errors
///
/// Returns [`Error::Base`] when `text` is not valid YAML or an environment value is malformed for
/// its key's shape.
pub(crate) fn build_base_from(
    text: &str, origin: &Path, vars: &[(String, String)], env: EnvPrecedence,
) -> Result<Value, Error> {
    let mut base: Value = saluki_config::yaml::from_str(text).map_err(|e| Error::Base {
        message: format!("parse `{}`: {e}", origin.display()),
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
    apply_datadog_env_vars(&mut base, vars.iter().cloned(), overwrite).map_err(|message| Error::Base { message })?;
    saluki_env_overlay::apply_env_vars(&mut base, vars, overwrite).map_err(|message| Error::Base { message })?;
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
    use saluki_config::dynamic::ConfigSetting;
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
            .enable_time()
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
    fn trace_env_environment_variables_reach_the_model() {
        // The trace environment picks its source by whether a key was configured. An empty variable is
        // unset, as in the Agent, so it must not block a lower-precedence source. Global tags reach
        // their lists unchanged; nothing here derives an environment from them.
        let _guard = test_env_lock();
        let path = std::env::temp_dir().join(format!("adp_trace_env_{}.yaml", std::process::id()));
        std::fs::write(&path, "{}\n").unwrap();
        std::env::set_var("DD_TAGS", "a:b env:tagged");
        std::env::set_var("DD_EXTRA_TAGS", "env:extra");

        std::env::set_var("DD_APM_ENV", "apm");
        std::env::set_var("DD_ENV", "prod");
        let set = block_on(LoadedConfiguration::load(&path, EnvPrecedence::AfterFile)).expect("local sources load");

        std::env::set_var("DD_APM_ENV", "");
        std::env::set_var("DD_ENV", "");
        let empty = block_on(LoadedConfiguration::load(&path, EnvPrecedence::AfterFile)).expect("local sources load");

        for var in ["DD_APM_ENV", "DD_ENV", "DD_TAGS", "DD_EXTRA_TAGS"] {
            std::env::remove_var(var);
        }
        std::fs::remove_file(&path).ok();

        let traces = &set.local().domains.traces;
        assert_eq!(traces.apm_env.as_deref(), Some("apm"));
        assert_eq!(traces.env.as_deref(), Some("prod"));

        let traces = &empty.local().domains.traces;
        assert_eq!(traces.apm_env, None);
        assert_eq!(traces.env, None);
        let tags = &empty.local().shared.tags;
        assert_eq!(tags.tags, ["a:b", "env:tagged"]);
        assert_eq!(tags.extra_tags, ["env:extra"]);
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

    /// Sets process environment variables for the duration of a scope, removing them when it drops.
    ///
    /// The caller holds [`test_env_lock`] for at least as long.
    struct ScopedEnv(Vec<&'static str>);

    impl ScopedEnv {
        fn set(vars: &[(&'static str, &str)]) -> Self {
            for (name, value) in vars {
                std::env::set_var(name, value);
            }
            Self(vars.iter().map(|(name, _)| *name).collect())
        }
    }

    impl Drop for ScopedEnv {
        fn drop(&mut self) {
            for name in &self.0 {
                std::env::remove_var(name);
            }
        }
    }

    /// Loads a temporary configuration file with the environment applied after it.
    fn load_with_environment(
        test: &str, contents: &str, vars: &[(&'static str, &str)],
    ) -> Result<LoadedConfiguration, Error> {
        let _guard = test_env_lock();
        let path = std::env::temp_dir().join(format!("adp_{test}_{}.yaml", std::process::id()));
        std::fs::write(&path, contents).unwrap();
        let _env = ScopedEnv::set(vars);

        let loaded = block_on(LoadedConfiguration::load(&path, EnvPrecedence::AfterFile));

        std::fs::remove_file(&path).ok();
        loaded
    }

    #[test]
    fn agent_spellings_of_valid_local_inputs_load_and_survive_the_stream() {
        // Each input is one the Agent accepts and ADP once rejected or misread: base-prefixed integers
        // and a JSON list in the environment, and a quoted zero-padded integer in the file.
        let loaded = load_with_environment(
            "agent_spellings",
            "api_key: test-api-key\ndogstatsd_buffer_size: \"010\"\n",
            &[
                ("DD_DOGSTATSD_PORT", "0x10"),
                ("DD_AGENT_IPC_GRPC_MAX_MESSAGE_SIZE", "0x10"),
                ("DD_CMD_PORT", "0x1F90"),
                ("DD_APM_OBFUSCATION_ELASTICSEARCH_KEEP_VALUES", r#"["cr-a","cr-b"]"#),
            ],
        )
        .expect("the Agent's spellings load");

        let local = loaded.local();
        assert_eq!(local.domains.dogstatsd.listeners.port, 16);
        assert_eq!(local.control.ipc.grpc_max_message_size, 16);
        assert_eq!(local.control.ipc.cmd_port, 8080);
        assert_eq!(
            local.domains.traces.obfuscation.elasticsearch.keep_values,
            ["cr-a", "cr-b"]
        );
        assert_eq!(local.domains.dogstatsd.listeners.buffer_size, 8);

        // The Agent's stream then connects and replaces a local value with its own typed one.
        block_on(async {
            let (agent_tx, agent_rx) = mpsc::channel(8);
            agent_tx
                .send(ConfigUpdate::snapshot([ConfigSetting::explicit(
                    "dogstatsd_port",
                    json!(9125),
                )]))
                .await
                .unwrap();
            let (system, _updates) = loaded.run(agent_rx).await.expect("the initial snapshot is accepted");

            let config = system.config();
            assert_eq!(config.domains.dogstatsd.listeners.port, 9125);
            assert_eq!(config.control.ipc.cmd_port, 8080);
            assert_eq!(config.domains.dogstatsd.listeners.buffer_size, 8);
        });
    }

    /// Waits until `system` reports `port` as the DogStatsD port.
    async fn await_dogstatsd_port(system: &ConfigurationSystem, port: u16) {
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while system.config().domains.dogstatsd.listeners.port != port {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("the configuration should report the DogStatsD port {port}"));
    }

    #[test]
    fn an_invalid_octal_integer_reads_as_zero_from_every_source() {
        // The Agent starts with these values and its integer getters read them as 0, so ADP must
        // neither stop at startup nor reject the stream's raw string.
        let loaded = load_with_environment(
            "invalid_octal",
            "api_key: test-api-key\ndogstatsd_buffer_size: \"0128\"\n",
            &[
                ("DD_DOGSTATSD_PORT", "08"),
                ("DD_CMD_PORT", "08.0"),
                ("DD_AGENT_IPC_GRPC_MAX_MESSAGE_SIZE", "-09"),
            ],
        )
        .expect("an invalid octal integer loads");

        let local = loaded.local();
        assert_eq!(local.domains.dogstatsd.listeners.port, 0);
        assert_eq!(local.control.ipc.cmd_port, 0);
        assert_eq!(local.control.ipc.grpc_max_message_size, 0);
        assert_eq!(local.domains.dogstatsd.listeners.buffer_size, 0);

        block_on(async {
            // The stream forwards the Agent's raw strings, which connected startup accepts.
            let (agent_tx, agent_rx) = mpsc::channel(8);
            agent_tx
                .send(ConfigUpdate::snapshot([
                    ConfigSetting::explicit("dogstatsd_port", json!("010")),
                    ConfigSetting::explicit("dogstatsd_buffer_size", json!("09")),
                ]))
                .await
                .unwrap();
            let (system, updates) = loaded.run(agent_rx).await.expect("the initial snapshot is accepted");
            assert_eq!(system.config().domains.dogstatsd.listeners.port, 8);
            assert_eq!(system.config().domains.dogstatsd.listeners.buffer_size, 0);

            // A later update applies too. A rejected update would leave the last value in place.
            tokio::spawn(updates.run());
            agent_tx
                .send(ConfigUpdate::Partial(ConfigSetting::explicit(
                    "dogstatsd_port",
                    json!("0x10"),
                )))
                .await
                .unwrap();
            await_dogstatsd_port(&system, 16).await;
            agent_tx
                .send(ConfigUpdate::Partial(ConfigSetting::explicit(
                    "dogstatsd_port",
                    json!("-08"),
                )))
                .await
                .unwrap();
            await_dogstatsd_port(&system, 0).await;
        });
    }

    #[test]
    fn a_malformed_integer_outside_the_invalid_octal_family_still_stops_startup() {
        let result = load_with_environment("invalid_octal_boundary", "{}\n", &[("DD_DOGSTATSD_PORT", "08.")]);
        match result {
            Err(Error::Base { message }) => assert_eq!(
                message,
                "environment variable `DD_DOGSTATSD_PORT` (dogstatsd_port): invalid integer `08.`"
            ),
            Err(error) => panic!("expected a base error, got {error}"),
            Ok(_) => panic!("`08.` should be rejected"),
        }
    }

    #[test]
    fn a_saluki_only_integer_keeps_its_decimal_grammar() {
        // `dogstatsd_tcp_port` has no Agent counterpart, so a leading zero stays decimal.
        let loaded = load_with_environment("saluki_integer", "{}\n", &[("DD_DOGSTATSD_TCP_PORT", "010")])
            .expect("a zero-padded decimal loads");
        assert_eq!(loaded.local().domains.dogstatsd.listeners.tcp_port, 10);

        let loaded = load_with_environment("saluki_invalid_octal", "{}\n", &[("DD_DOGSTATSD_TCP_PORT", "08")])
            .expect("a zero-padded decimal loads");
        assert_eq!(loaded.local().domains.dogstatsd.listeners.tcp_port, 8);

        let result = load_with_environment("saluki_hex", "{}\n", &[("DD_DOGSTATSD_TCP_PORT", "0x10")]);
        match result {
            Err(Error::Base { message }) => assert_eq!(
                message,
                "environment variable `DD_DOGSTATSD_TCP_PORT` (dogstatsd_tcp_port): invalid integer `0x10`"
            ),
            Err(error) => panic!("expected a base error, got {error}"),
            Ok(_) => panic!("a hexadecimal Saluki-only integer should be rejected"),
        }
    }

    #[test]
    fn a_saluki_only_float_rejects_go_only_spellings() {
        // `memory_slop_factor` has no Agent counterpart, so Go's hexadecimal float form stays invalid.
        let result = load_with_environment("saluki_float", "{}\n", &[("DD_MEMORY_SLOP_FACTOR", "0x1p-2")]);
        match result {
            Err(Error::Base { message }) => assert_eq!(
                message,
                "environment variable `DD_MEMORY_SLOP_FACTOR` (memory_slop_factor): invalid number `0x1p-2`"
            ),
            Err(error) => panic!("expected a base error, got {error}"),
            Ok(_) => panic!("a hexadecimal Saluki-only float should be rejected"),
        }
    }

    #[test]
    fn a_saluki_only_sequence_still_splits_on_whitespace() {
        let loaded = load_with_environment(
            "saluki_sequence",
            "{}\n",
            &[("DD_OTTL_FILTER_CONFIG_TRACES_SPAN", r#"["a","b"] c"#)],
        )
        .expect("a whitespace-separated sequence loads");
        assert_eq!(
            loaded.local().domains.traces.ottl_filter.span_conditions,
            [r#"["a","b"]"#, "c"]
        );
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
    fn build_base_from_reads_the_yaml_alone() {
        let vars = [("DD_DOGSTATSD_PORT".to_string(), "9125".to_string())];
        let yaml = "dogstatsd_port: 8125\nempty_key:\n";

        let base =
            build_base_from(yaml, Path::new("datadog.yaml"), &[], EnvPrecedence::AfterFile).expect("base builds");
        assert_eq!(base.get("dogstatsd_port"), Some(&json!(8125)));
        assert!(base.get("empty_key").is_none());

        // `Disabled` ignores the given variables.
        let base =
            build_base_from(yaml, Path::new("datadog.yaml"), &vars, EnvPrecedence::Disabled).expect("base builds");
        assert_eq!(base.get("dogstatsd_port"), Some(&json!(8125)));

        // An empty file is an empty object.
        let base = build_base_from("", Path::new("datadog.yaml"), &[], EnvPrecedence::AfterFile).expect("base builds");
        assert_eq!(base, json!({}));
    }

    #[test]
    fn build_base_from_lets_the_given_environment_override_the_yaml_after_the_file() {
        let vars = [
            ("DD_DOGSTATSD_PORT".to_string(), "9125".to_string()),
            ("DD_OTTL_FILTER_CONFIG_ERROR_MODE".to_string(), "silent".to_string()),
        ];
        let yaml =
            "dogstatsd_port: 8125\ndogstatsd_non_local_traffic: false\nottl_filter_config:\n  error_mode: strict\n";

        let base =
            build_base_from(yaml, Path::new("datadog.yaml"), &vars, EnvPrecedence::AfterFile).expect("base builds");
        assert_eq!(base.get("dogstatsd_port"), Some(&json!(9125)));
        assert_eq!(base.get("dogstatsd_non_local_traffic"), Some(&json!(false)));
        // The Saluki-only reader takes the given variables too.
        assert_eq!(base.pointer("/ottl_filter_config/error_mode"), Some(&json!("silent")));

        let base =
            build_base_from(yaml, Path::new("datadog.yaml"), &vars, EnvPrecedence::BeforeFile).expect("base builds");
        assert_eq!(base.get("dogstatsd_port"), Some(&json!(8125)));
    }

    #[test]
    fn build_base_from_rejects_a_malformed_environment_value_as_build_base_does() {
        let _guard = test_env_lock();
        let path = std::env::temp_dir().join(format!("adp_build_base_from_bad_{}.yaml", std::process::id()));
        std::fs::write(&path, "dogstatsd_port: 8125\n").unwrap();
        std::env::set_var("DD_DOGSTATSD_PORT", "not-a-number");
        let from_process = build_base(&path, EnvPrecedence::AfterFile);
        std::env::remove_var("DD_DOGSTATSD_PORT");
        std::fs::remove_file(&path).ok();

        let vars = [("DD_DOGSTATSD_PORT".to_string(), "not-a-number".to_string())];
        let from_vars = build_base_from("dogstatsd_port: 8125\n", &path, &vars, EnvPrecedence::AfterFile);

        let (Err(Error::Base { message: expected }), Err(Error::Base { message: actual })) = (from_process, from_vars)
        else {
            panic!("both builds must fail with a base error");
        };
        assert!(actual.contains("DD_DOGSTATSD_PORT"), "{actual}");
        assert_eq!(actual, expected);
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
}
