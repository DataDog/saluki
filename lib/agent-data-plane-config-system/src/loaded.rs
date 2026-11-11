//! Loads local configuration before selecting its runtime authority.
//!
//! [`LoadedConfiguration::load`] prepares a typed snapshot of the local file and environment, which
//! [`LoadedConfiguration::local`] exposes before values from the Datadog Agent stream are applied.
//! [`LoadedConfiguration::run`] layers the Agent's configuration stream over the local sources,
//! while [`LoadedConfiguration::standalone`] keeps the local sources authoritative. Both methods
//! consume the loaded sources and return a [`ConfigurationSystem`].

use std::path::Path;

use agent_data_plane_config::SalukiConfiguration;
use datadog_agent_config::{apply_datadog_env_vars, datadog_renamed_keys, RenamedKey};
use saluki_config::dynamic::ConfigUpdate;
use serde_json::Value;
use tokio::sync::mpsc;
use tracing::warn;

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
    /// The error that caused the whole file to be read with every key as written.
    ///
    /// `None` when Datadog-style key resolution succeeded. Callers should report this after logging
    /// is initialized: the fallback also ignores valid dotted and differently cased Datadog keys.
    pub yaml_fallback_error: Option<Error>,
    // Former setting names the local sources used, reported by `warn_deprecated_names`.
    deprecated_names: Vec<DeprecatedName>,
}

/// A former setting name that a local source used.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct DeprecatedName {
    /// The former name as the source wrote it: a dotted file key or an environment variable.
    pub(crate) former: String,
    /// The dotted name of the setting.
    pub(crate) current: String,
}

impl From<&RenamedKey> for DeprecatedName {
    /// The former name as a dotted file key.
    fn from(renamed: &RenamedKey) -> Self {
        DeprecatedName {
            former: renamed.former.join("."),
            current: renamed.path.join("."),
        }
    }
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
    /// the second reading is used: a respelled key whose value cannot be translated is ignored rather
    /// than failing the load. [`yaml_fallback_error`](Self::yaml_fallback_error) retains the cause.
    /// Both readings use the same file contents. A value that translates is kept, even if a component
    /// later rejects it.
    ///
    /// # Errors
    ///
    /// Returns an error if a local source cannot be read, decoded, deserialized, or translated.
    pub async fn load(path: impl AsRef<Path>, env: EnvPrecedence) -> Result<Self, Error> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path).map_err(|e| Error::Base {
            message: format!("read `{}`: {e}", path.display()),
        })?;
        let reading = read_local(&text, path, &env_vars(env), env, datadog_renamed_keys())?;
        Ok(Self {
            local: reading.local?,
            base: reading.base,
            yaml_fallback_error: reading.yaml_fallback_error,
            deprecated_names: reading.deprecated_names,
        })
    }

    /// Logs a warning for each former setting name the local file or environment used.
    ///
    /// Call this once logging is initialized.
    pub fn warn_deprecated_names(&self) {
        for name in &self.deprecated_names {
            warn!(
                "`{}` is a deprecated name for `{}`. Use the current name instead.",
                name.former, name.current
            );
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

/// The local sources as [`LoadedConfiguration::load`] reads them.
pub(crate) struct LocalReading {
    /// The base of the selected key reading.
    pub(crate) base: SourceTree,
    /// The strict translation of `base`, or the Agent-style reading's error when neither reading
    /// translates.
    pub(crate) local: Result<SalukiConfiguration, Error>,
    /// The error that caused every key to be read as written; `None` when the Agent-style reading
    /// is used.
    pub(crate) yaml_fallback_error: Option<Error>,
    /// Former setting names the selected reading used.
    pub(crate) deprecated_names: Vec<DeprecatedName>,
}

/// Reads the file's text and explicit environment variables, selecting the key reading
/// [`LoadedConfiguration::load`] uses.
///
/// `renamed` lists the former setting names to move and report (see [`build_base_from`]).
///
/// The Agent-style reading is used unless only the keys-as-written reading translates. When neither
/// translates, the Agent-style base is kept with its translation error, so a caller that only needs
/// the base can still use it.
///
/// # Errors
///
/// Returns the Agent-style reading's error when its base cannot be built and the keys-as-written
/// reading does not translate.
pub(crate) fn read_local(
    text: &str, origin: &Path, vars: &[(String, String)], env: EnvPrecedence, renamed: &[RenamedKey],
) -> Result<LocalReading, Error> {
    // Every value the local sources supply was set explicitly: the file set it or an environment
    // variable supplied it.
    let build = |reading| {
        build_base_from(text, origin, vars, env, reading, renamed)
            .map(|(base, names)| (SourceTree::all_explicit(base), names))
    };
    let (error, base) = match build(KeyReading::Datadog) {
        Ok((base, deprecated_names)) => match translate_strict(&base) {
            Ok(local) => {
                return Ok(LocalReading {
                    base,
                    local: Ok(local),
                    yaml_fallback_error: None,
                    deprecated_names,
                })
            }
            Err(error) => (error, Some((base, deprecated_names))),
        },
        Err(error) => (error, None),
    };

    let fallback = build(KeyReading::AsWritten)
        .ok()
        .filter(|(plain, _)| base.as_ref().map(|(base, _)| base) != Some(plain))
        .and_then(|(plain, names)| translate_strict(&plain).ok().map(|local| (plain, names, local)));
    match (fallback, base) {
        (Some((plain, deprecated_names, local)), _) => Ok(LocalReading {
            base: plain,
            local: Ok(local),
            yaml_fallback_error: Some(error),
            deprecated_names,
        }),
        (None, Some((base, deprecated_names))) => Ok(LocalReading {
            base,
            local: Err(error),
            yaml_fallback_error: None,
            deprecated_names,
        }),
        (None, None) => Err(error),
    }
}

/// How [`build_base_from`] reads the configuration file's keys.
#[derive(Clone, Copy, Debug)]
enum KeyReading {
    /// Datadog keys are resolved as the Datadog Agent resolves them: dotted and differently cased
    /// spellings land at their canonical paths, and the map keys of a Datadog setting are rendered as
    /// the Agent renders them.
    Datadog,
    /// Every key is read as written.
    AsWritten,
}

/// Captures the process environment variables `env` reads.
fn env_vars(env: EnvPrecedence) -> Vec<(String, String)> {
    // `Disabled` never reads the process environment, so it is not captured either.
    match env {
        EnvPrecedence::Disabled => Vec::new(),
        EnvPrecedence::AfterFile | EnvPrecedence::BeforeFile => std::env::vars().collect(),
    }
}

/// Builds the typed base from the configuration file's text and explicit environment variables, and
/// returns it with the former setting names they used.
///
/// `text` is the file's content, `origin` is the path named in a parse error, and `vars` are the
/// environment variable name/value pairs. `vars` is ignored when `env` is `Disabled`.
///
/// `reading` sets how the file's keys are read. A former setting name in `renamed` then moves to its
/// setting (see [`datadog_yaml`]). `renamed` drives only that move: `KeyReading::Datadog` resolves
/// spellings against the generated table, so a former name that is not in it is matched only as
/// written. The `datadog_yaml` tests cover how former names are resolved. A null-valued key is
/// dropped and an empty file is an empty object.
/// The environment is then overlaid via the generated Datadog reader, the Saluki-only reader, and the
/// canonical proxy variables. `env` sets whether the environment overwrites the file
/// (`AfterFile`) or only fills absent keys (`BeforeFile`); `Disabled` skips the environment entirely.
///
/// # Errors
///
/// Returns [`Error::Base`] when `text` is not valid YAML or an environment value is malformed for
/// its key's shape.
fn build_base_from(
    text: &str, origin: &Path, vars: &[(String, String)], env: EnvPrecedence, reading: KeyReading,
    renamed: &[RenamedKey],
) -> Result<(Value, Vec<DeprecatedName>), Error> {
    let parsed = match reading {
        KeyReading::Datadog => datadog_yaml::from_str(text),
        KeyReading::AsWritten => saluki_config::yaml::from_str(text).map_err(|e| e.to_string()),
    };
    let mut base = parsed.map_err(|e| Error::Base {
        message: format!("parse `{}`: {e}", origin.display()),
    })?;
    let mut deprecated_names: Vec<DeprecatedName> = datadog_yaml::move_renamed_keys(&mut base, renamed)
        .into_iter()
        .map(DeprecatedName::from)
        .collect();
    drop_nulls(&mut base);
    if base.is_null() {
        base = Value::Object(serde_json::Map::new());
    }

    let overwrite = match env {
        EnvPrecedence::Disabled => return Ok((base, deprecated_names)),
        EnvPrecedence::AfterFile => true,
        EnvPrecedence::BeforeFile => false,
    };
    let read = apply_datadog_env_vars(&mut base, vars.iter().cloned(), overwrite)
        .map_err(|message| Error::Base { message })?;
    saluki_env_overlay::apply_env_vars(&mut base, vars, overwrite).map_err(|message| Error::Base { message })?;
    deprecated_names.extend(env_deprecated_names(renamed, &read));
    Ok((base, deprecated_names))
}

/// The former setting names in `renamed` whose environment variable is in `read`, the variables the
/// environment reader read, in `renamed` order. Each is reported under the name it was set with.
fn env_deprecated_names<'a>(
    renamed: &'a [RenamedKey], read: &'a [String],
) -> impl Iterator<Item = DeprecatedName> + 'a {
    renamed.iter().filter_map(|renamed| {
        let env_var = renamed.env_var?.to_lowercase();
        // The reader may have read a case variant of the bound name.
        let name = read.iter().find(|name| name.to_lowercase() == env_var)?;
        Some(DeprecatedName {
            former: name.clone(),
            current: renamed.path.join("."),
        })
    })
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
    use std::collections::HashMap;

    use bytesize::ByteSize;
    use saluki_config::dynamic::ConfigSetting;
    use saluki_config::test_env_lock;
    use serde_json::json;

    use super::*;

    /// Builds the typed base from `text` and the process environment.
    fn build_base(path: &Path, text: &str, env: EnvPrecedence, reading: KeyReading) -> Result<Value, Error> {
        build_base_from(text, path, &env_vars(env), env, reading, datadog_renamed_keys()).map(|(base, _)| base)
    }

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
        let text = std::fs::read_to_string(&path).unwrap();

        // AfterFile: the environment wins over the file, the decoded value is a real number, an
        // explicitly null key is dropped, and an unrelated file key is preserved.
        let base = build_base(&path, &text, EnvPrecedence::AfterFile, KeyReading::Datadog).expect("base builds");
        assert_eq!(base.get("dogstatsd_port"), Some(&json!(9125)));
        assert_eq!(base.get("dogstatsd_non_local_traffic"), Some(&json!(false)));
        assert!(base.get("empty_key").is_none());

        // BeforeFile: the file wins over the environment.
        let base = build_base(&path, &text, EnvPrecedence::BeforeFile, KeyReading::Datadog).expect("base builds");
        assert_eq!(base.get("dogstatsd_port"), Some(&json!(8125)));

        // Disabled: the environment is ignored entirely.
        let base = build_base(&path, &text, EnvPrecedence::Disabled, KeyReading::Datadog).expect("base builds");
        assert_eq!(base.get("dogstatsd_port"), Some(&json!(8125)));

        std::env::remove_var("DD_DOGSTATSD_PORT");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn both_key_readings_use_the_captured_file_contents() {
        // Regression: the fallback reread the file, allowing its base and the first error to describe
        // different contents. Both readings now use the text captured by load.
        let path = std::env::temp_dir().join(format!("adp_captured_yaml_{}.yaml", std::process::id()));
        std::fs::write(&path, "DogStatsD_Port: abc\nLog_Level: debug\n").unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        std::fs::write(&path, "dogstatsd_port: 9999\nlog_level: warn\n").unwrap();

        let datadog = build_base(&path, &text, EnvPrecedence::Disabled, KeyReading::Datadog).unwrap();
        let plain = build_base(&path, &text, EnvPrecedence::Disabled, KeyReading::AsWritten).unwrap();
        std::fs::remove_file(&path).ok();

        assert_eq!(datadog, json!({"dogstatsd_port": "abc", "log_level": "debug"}));
        assert_eq!(plain, json!({"DogStatsD_Port": "abc", "Log_Level": "debug"}));
        assert!(matches!(
            translate_strict(&SourceTree::all_explicit(datadog)),
            Err(Error::Deserialize { .. })
        ));
        let config = translate_strict(&SourceTree::all_explicit(plain)).expect("fallback translates");
        assert_eq!(config.domains.dogstatsd.listeners.port, 8125);
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
        assert!(loaded.yaml_fallback_error.is_none());

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

    /// Flat and nested Saluki-only keys with non-default values to detect missed assignments.
    ///
    /// Each entry contains a YAML path, an environment variable name, and value text valid for both.
    const SALUKI_ONLY_SAMPLE: &[(&str, &str, &str)] = &[
        ("dogstatsd_tcp_port", "DD_DOGSTATSD_TCP_PORT", "9125"),
        (
            "aggregate_window_duration_seconds",
            "DD_AGGREGATE_WINDOW_DURATION_SECONDS",
            "17",
        ),
        ("data_plane.standalone_mode", "DD_DATA_PLANE_STANDALONE_MODE", "true"),
        (
            "apm_config.obfuscation.sql.keep_sql_alias",
            "DD_APM_CONFIG_OBFUSCATION_SQL_KEEP_SQL_ALIAS",
            "true",
        ),
    ];

    fn assert_saluki_only_sample_reached(config: &SalukiConfiguration) {
        let dsd = &config.domains.dogstatsd;
        assert_eq!(dsd.listeners.tcp_port, 9125);
        assert_eq!(dsd.aggregation.window_duration_seconds.get(), 17);
        assert!(config.control.standalone_mode);
        assert!(config.domains.traces.obfuscation.sql.keep_sql_alias);
    }

    #[tokio::test]
    async fn saluki_only_keys_from_the_file_reach_the_model() {
        let mut yaml = String::new();
        for (key, _, value) in SALUKI_ONLY_SAMPLE {
            let segments: Vec<&str> = key.split('.').collect();
            for (depth, segment) in segments.iter().enumerate() {
                let indent = "  ".repeat(depth);
                if depth + 1 == segments.len() {
                    yaml.push_str(&format!("{indent}{segment}: {value}\n"));
                } else {
                    yaml.push_str(&format!("{indent}{segment}:\n"));
                }
            }
        }

        let loaded = load_from_file("saluki_only_sample", &yaml).await;

        assert_saluki_only_sample_reached(loaded.local());
    }

    #[test]
    fn saluki_only_keys_from_the_environment_reach_the_model() {
        let _guard = test_env_lock();
        let path = std::env::temp_dir().join(format!("adp_saluki_only_sample_env_{}.yaml", std::process::id()));
        std::fs::write(&path, "{}\n").unwrap();
        for (_, var, value) in SALUKI_ONLY_SAMPLE {
            std::env::set_var(var, value);
        }

        let loaded = block_on(LoadedConfiguration::load(&path, EnvPrecedence::AfterFile));

        for (_, var, _) in SALUKI_ONLY_SAMPLE {
            std::env::remove_var(var);
        }
        std::fs::remove_file(&path).ok();
        assert_saluki_only_sample_reached(loaded.expect("local sources load").local());
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

        let text = std::fs::read_to_string(&path).unwrap();
        let result = build_base(&path, &text, EnvPrecedence::AfterFile, KeyReading::Datadog);

        std::env::remove_var("DD_DOGSTATSD_PORT");
        std::fs::remove_file(&path).ok();
        assert!(matches!(result, Err(Error::Base { .. })));
    }

    #[test]
    fn build_base_from_reads_the_yaml_alone() {
        let vars = [("DD_DOGSTATSD_PORT".to_string(), "9125".to_string())];
        let yaml = "dogstatsd_port: 8125\nempty_key:\n";

        let (base, _) = build_base_from(
            yaml,
            Path::new("datadog.yaml"),
            &[],
            EnvPrecedence::AfterFile,
            KeyReading::Datadog,
            datadog_renamed_keys(),
        )
        .expect("base builds");
        assert_eq!(base.get("dogstatsd_port"), Some(&json!(8125)));
        assert!(base.get("empty_key").is_none());

        // `Disabled` ignores the given variables.
        let (base, _) = build_base_from(
            yaml,
            Path::new("datadog.yaml"),
            &vars,
            EnvPrecedence::Disabled,
            KeyReading::Datadog,
            datadog_renamed_keys(),
        )
        .expect("base builds");
        assert_eq!(base.get("dogstatsd_port"), Some(&json!(8125)));

        // An empty file is an empty object.
        let (base, _) = build_base_from(
            "",
            Path::new("datadog.yaml"),
            &[],
            EnvPrecedence::AfterFile,
            KeyReading::Datadog,
            datadog_renamed_keys(),
        )
        .expect("base builds");
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

        let (base, _) = build_base_from(
            yaml,
            Path::new("datadog.yaml"),
            &vars,
            EnvPrecedence::AfterFile,
            KeyReading::Datadog,
            datadog_renamed_keys(),
        )
        .expect("base builds");
        assert_eq!(base.get("dogstatsd_port"), Some(&json!(9125)));
        assert_eq!(base.get("dogstatsd_non_local_traffic"), Some(&json!(false)));
        // The Saluki-only reader takes the given variables too.
        assert_eq!(base.pointer("/ottl_filter_config/error_mode"), Some(&json!("silent")));

        let (base, _) = build_base_from(
            yaml,
            Path::new("datadog.yaml"),
            &vars,
            EnvPrecedence::BeforeFile,
            KeyReading::Datadog,
            datadog_renamed_keys(),
        )
        .expect("base builds");
        assert_eq!(base.get("dogstatsd_port"), Some(&json!(8125)));
    }

    #[test]
    fn build_base_from_rejects_a_malformed_environment_value_as_build_base_does() {
        let _guard = test_env_lock();
        let path = std::env::temp_dir().join(format!("adp_build_base_from_bad_{}.yaml", std::process::id()));
        std::fs::write(&path, "dogstatsd_port: 8125\n").unwrap();
        std::env::set_var("DD_DOGSTATSD_PORT", "not-a-number");
        let text = std::fs::read_to_string(&path).unwrap();
        let from_process = build_base(&path, &text, EnvPrecedence::AfterFile, KeyReading::Datadog);
        std::env::remove_var("DD_DOGSTATSD_PORT");
        std::fs::remove_file(&path).ok();

        let vars = [("DD_DOGSTATSD_PORT".to_string(), "not-a-number".to_string())];
        let from_vars = build_base_from(
            "dogstatsd_port: 8125\n",
            &path,
            &vars,
            EnvPrecedence::AfterFile,
            KeyReading::Datadog,
            datadog_renamed_keys(),
        );

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

        let text = std::fs::read_to_string(&path).unwrap();
        let base = SourceTree::all_explicit(
            build_base(&path, &text, EnvPrecedence::AfterFile, KeyReading::Datadog).expect("base builds"),
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
        let text = std::fs::read_to_string(&path).unwrap();
        let base = build_base(&path, &text, EnvPrecedence::Disabled, KeyReading::Datadog);
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
        assert_eq!(loaded.local().domains.traces.env.as_deref(), Some("true"));
        assert_eq!(loaded.local().domains.dogstatsd.listeners.buffer_size, 8);

        let loaded = load_from_file("agent_octal_env", "env: 0755\n").await;
        assert_eq!(loaded.local().domains.traces.env.as_deref(), Some("493"));
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
        assert!(loaded.yaml_fallback_error.is_none());
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
        let text = std::fs::read_to_string(&path).unwrap();
        let base = build_base(&path, &text, EnvPrecedence::Disabled, reading);
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

    #[tokio::test]
    async fn a_null_leaf_map_entry_does_not_hide_a_key_that_renders_the_same() {
        // Regression: the null entry claimed the rendered key `true` and the base dropped it, so the
        // map loaded empty instead of falling back or keeping `yes`.
        for (index, yaml) in [
            "apm_config: {analyzed_rate_by_service: {'true': ~, yes: 0.5}}\n",
            "apm_config: {analyzed_rate_by_service: {yes: 0.5, 'true': ~}}\n",
        ]
        .into_iter()
        .enumerate()
        {
            let loaded = load_from_file(&format!("null_leaf_entry_{index}"), yaml).await;
            let rates = &loaded.local().domains.traces.analyzed_rate_by_service;
            assert_eq!(*rates, HashMap::from([("true".to_owned(), 0.5)]), "{yaml:?}");
        }
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
            "Dogstatsd_Tag_Cardinality: bogus\n",
            "DogStatsD_Port: abc\nLog_Level: debug\n",
        ];
        for (index, yaml) in cases.into_iter().enumerate() {
            let test = format!("fallback_{index}");
            assert!(
                translates_with_keys_as_written(&test, yaml),
                "{yaml:?} did not load before"
            );
            let base = SourceTree::all_explicit(base_from_file(&test, yaml).expect("base builds"));
            let error = translate_strict(&base).expect_err("Agent-style reading cannot be translated");
            let loaded = try_load(&test, yaml).unwrap_or_else(|e| panic!("{yaml:?}: {e}"));
            // Regression: load discarded the fallback cause before logging was initialized.
            assert_eq!(
                loaded
                    .yaml_fallback_error
                    .as_ref()
                    .expect("fallback retains its cause")
                    .to_string(),
                error.to_string(),
                "{yaml:?}"
            );
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
        // A dotted Saluki-only key stays a flat key, which the typed reader does not see.
        let yaml = "data_plane.standalone_mode: true\n";
        let base = base_from_file("saluki_only_base", yaml).unwrap();
        assert_eq!(base["data_plane.standalone_mode"], json!(true));
        assert!(base.get("data_plane").is_none());

        let loaded = load_from_file("saluki_only", yaml).await;

        assert!(!loaded.local().control.standalone_mode);
    }

    #[tokio::test]
    async fn metric_allowlist_keys_reach_the_model_as_the_agent_renders_them() {
        let loaded = load_from_file(
            "metric_allowlist_keys",
            concat!(
                "experimental:\n",
                "  metrics_endpoint_routing:\n",
                "    metric_allowlist: {'https://x.test': [a], 010: [b], yes: [c]}\n",
            ),
        )
        .await;

        assert_eq!(
            serde_json::to_value(&loaded.local().domains.metrics_endpoint_routing.metric_allowlists).unwrap(),
            json!({"https://x.test": ["a"], "8": ["b"], "true": ["c"]})
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

    #[tokio::test]
    async fn former_setting_names_in_the_file_reach_the_current_settings() {
        let loaded = load_from_file("renamed_list", "statsd_metric_blocklist: [a]\n").await;
        let filter = &loaded.local().domains.dogstatsd.metric_filter;
        assert_eq!(filter.values, ["a"]);
        assert!(!filter.match_prefix);
        // The sources that compatibility checks and the raw view read hold only the current name.
        let keys: Vec<String> = loaded.base.flattened_keys().into_iter().map(|(key, ..)| key).collect();
        assert!(keys.iter().any(|key| key == "metric_filterlist"), "{keys:?}");
        assert!(
            !keys.iter().any(|key| key.starts_with("statsd_metric_blocklist")),
            "{keys:?}"
        );

        let loaded = load_from_file("renamed_prefix", "statsd_metric_blocklist_match_prefix: true\n").await;
        let filter = &loaded.local().domains.dogstatsd.metric_filter;
        assert!(filter.values.is_empty());
        assert!(filter.match_prefix);

        let loaded = load_from_file(
            "renamed_both",
            "metric_filterlist: [b]\nmetric_filterlist_match_prefix: false\nstatsd_metric_blocklist: [a]\nstatsd_metric_blocklist_match_prefix: true\n",
        )
        .await;
        let filter = &loaded.local().domains.dogstatsd.metric_filter;
        assert_eq!(filter.values, ["a"]);
        assert!(filter.match_prefix);
    }

    #[test]
    fn a_former_setting_name_still_moves_when_keys_are_read_as_written() {
        let yaml = "Log_Level: [a]\nmetric_filterlist: [b]\nstatsd_metric_blocklist: [a]\n";
        let loaded = try_load("renamed_fallback", yaml).expect("the keys-as-written reading loads");

        assert!(loaded.yaml_fallback_error.is_some());
        assert_eq!(loaded.local().domains.dogstatsd.metric_filter.values, ["a"]);
        assert_eq!(
            loaded.deprecated_names,
            [DeprecatedName {
                former: "statsd_metric_blocklist".to_string(),
                current: "metric_filterlist".to_string(),
            }]
        );
    }

    #[test]
    fn former_setting_names_used_by_the_file_or_environment_are_recorded() {
        let names = |yaml: &str, vars: &[(&str, &str)]| -> Vec<(String, String)> {
            let vars: Vec<_> = vars.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
            let (_, names) = build_base_from(
                yaml,
                Path::new("datadog.yaml"),
                &vars,
                EnvPrecedence::AfterFile,
                KeyReading::Datadog,
                datadog_renamed_keys(),
            )
            .expect("base builds");
            names.into_iter().map(|name| (name.former, name.current)).collect()
        };
        let pair = |former: &str, current: &str| (former.to_string(), current.to_string());

        assert_eq!(
            names(
                "statsd_metric_blocklist: [a]\nSTATSD_METRIC_BLOCKLIST_MATCH_PREFIX: true\n",
                &[]
            ),
            [
                pair("statsd_metric_blocklist", "metric_filterlist"),
                pair("statsd_metric_blocklist_match_prefix", "metric_filterlist_match_prefix"),
            ]
        );
        // A null former name clears its setting, so it is recorded too.
        assert_eq!(
            names("statsd_metric_blocklist:\n", &[]),
            [pair("statsd_metric_blocklist", "metric_filterlist")]
        );
        // An empty variable is not read.
        assert_eq!(
            names(
                "",
                &[
                    ("DD_STATSD_METRIC_BLOCKLIST", "a"),
                    ("DD_STATSD_METRIC_BLOCKLIST_MATCH_PREFIX", "")
                ]
            ),
            [pair("DD_STATSD_METRIC_BLOCKLIST", "metric_filterlist")]
        );
        assert!(names("metric_filterlist: [a]\n", &[("DD_METRIC_FILTERLIST", "b")]).is_empty());
    }

    #[test]
    fn former_setting_names_beat_current_ones_only_within_a_source() {
        let build = |yaml: &str, vars: &[(&str, &str)]| {
            let vars: Vec<_> = vars.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
            let (base, _) = build_base_from(
                yaml,
                Path::new("datadog.yaml"),
                &vars,
                EnvPrecedence::AfterFile,
                KeyReading::Datadog,
                datadog_renamed_keys(),
            )
            .expect("base builds");
            base["metric_filterlist"].clone()
        };

        assert_eq!(
            build("metric_filterlist: [file]\n", &[("DD_STATSD_METRIC_BLOCKLIST", "env")]),
            json!(["env"])
        );
        assert_eq!(
            build("statsd_metric_blocklist: [file]\n", &[("DD_METRIC_FILTERLIST", "env")]),
            json!(["env"])
        );
        assert_eq!(
            build(
                "",
                &[("DD_METRIC_FILTERLIST", "new"), ("DD_STATSD_METRIC_BLOCKLIST", "old")]
            ),
            json!(["old"])
        );
    }

    /// Former names for the deprecation tests, highest priority first for each setting.
    const RENAMED: &[RenamedKey] = &[
        RenamedKey {
            former: &["oldest_list"],
            path: &["new_list"],
            env_var: Some("DD_OLDEST_LIST"),
        },
        RenamedKey {
            former: &["old_list"],
            path: &["new_list"],
            env_var: Some("DD_OLD_LIST"),
        },
        RenamedKey {
            former: &["old_flag"],
            path: &["new_flag"],
            env_var: None,
        },
        RenamedKey {
            former: &["legacy", "old_nested"],
            path: &["section", "new_nested"],
            env_var: Some("DD_LEGACY_OLD_NESTED"),
        },
    ];

    fn deprecated(former: &str, current: &str) -> DeprecatedName {
        DeprecatedName {
            former: former.to_string(),
            current: current.to_string(),
        }
    }

    #[test]
    fn a_former_file_key_is_reported_by_its_dotted_name() {
        assert_eq!(
            DeprecatedName::from(&RENAMED[3]),
            deprecated("legacy.old_nested", "section.new_nested")
        );
    }

    #[test]
    fn only_former_variables_the_environment_reader_read_are_reported() {
        let read = ["dd_old_list", "DD_NEW_LIST", "DD_OLD_FLAG", "DD_LEGACY_OLD_NESTED"].map(String::from);

        let names: Vec<_> = env_deprecated_names(RENAMED, &read).collect();

        // `DD_OLD_FLAG` is not a former variable: the reader does not read `new_flag`.
        assert_eq!(
            names,
            [
                deprecated("dd_old_list", "new_list"),
                deprecated("DD_LEGACY_OLD_NESTED", "section.new_nested"),
            ]
        );
    }

    #[test]
    fn a_former_name_moves_when_keys_are_read_as_written() {
        // Read as written, a former name is a plain key, and only its exact spelling moves.
        let mut base: Value = saluki_config::yaml::from_str("OLD_LIST: [c]\nnew_list: [b]\nold_list: [a]\n").unwrap();

        let names: Vec<_> = datadog_yaml::move_renamed_keys(&mut base, RENAMED)
            .into_iter()
            .map(DeprecatedName::from)
            .collect();

        assert_eq!(base, json!({"OLD_LIST": ["c"], "new_list": ["a"]}));
        assert_eq!(names, [deprecated("old_list", "new_list")]);
    }

    /// Former names of modeled settings, for tests that build and translate a base.
    ///
    /// The Agent-style reading resolves spellings against the generated paths, so these are read in
    /// their exact spelling, or inside a respelled section of the schema.
    const RENAMED_SETTINGS: &[RenamedKey] = &[
        RenamedKey {
            former: &["old_port"],
            path: &["dogstatsd_port"],
            env_var: None,
        },
        RenamedKey {
            former: &["data_plane", "old_standalone_mode"],
            path: &["data_plane", "standalone_mode"],
            env_var: None,
        },
    ];

    fn build_renamed(
        yaml: &str, vars: &[(String, String)], env: EnvPrecedence, reading: KeyReading,
    ) -> (Value, Vec<DeprecatedName>) {
        build_base_from(yaml, Path::new("datadog.yaml"), vars, env, reading, RENAMED_SETTINGS).expect("base builds")
    }

    #[test]
    fn a_null_former_name_clears_the_current_name_before_nulls_are_dropped() {
        for reading in [KeyReading::Datadog, KeyReading::AsWritten] {
            let (base, names) = build_renamed(
                "dogstatsd_port: 9000\nold_port: null\n",
                &[],
                EnvPrecedence::Disabled,
                reading,
            );

            assert_eq!(base, json!({}), "{reading:?}");
            assert_eq!(names, [deprecated("old_port", "dogstatsd_port")], "{reading:?}");
        }
    }

    #[test]
    fn a_current_name_in_the_environment_beats_a_former_name_in_the_file() {
        let vars = [("DD_DOGSTATSD_PORT".to_string(), "9125".to_string())];
        let yaml = "old_port: 9000\n";

        let (after, names) = build_renamed(yaml, &vars, EnvPrecedence::AfterFile, KeyReading::Datadog);
        assert_eq!(after, json!({"dogstatsd_port": 9125}));
        assert_eq!(names, [deprecated("old_port", "dogstatsd_port")]);

        let (before, _) = build_renamed(yaml, &vars, EnvPrecedence::BeforeFile, KeyReading::Datadog);
        assert_eq!(before, json!({"dogstatsd_port": 9000}));
    }

    #[test]
    fn the_selected_key_reading_supplies_the_former_names() {
        let read = |yaml: &str| {
            read_local(
                yaml,
                Path::new("datadog.yaml"),
                &[],
                EnvPrecedence::Disabled,
                RENAMED_SETTINGS,
            )
            .expect("local sources read")
        };
        // Only the Agent-style reading finds the former name inside the respelled section.
        let respelled = "DATA_PLANE: {old_standalone_mode: true}\nold_port: 9000\n";
        let both = [
            deprecated("old_port", "dogstatsd_port"),
            deprecated("data_plane.old_standalone_mode", "data_plane.standalone_mode"),
        ];

        let reading = read(respelled);
        assert!(reading.yaml_fallback_error.is_none());
        assert!(reading.local.expect("translates").control.standalone_mode);
        assert_eq!(reading.deprecated_names, both);

        // Only the keys-as-written reading translates.
        let reading = read(&format!("{respelled}Log_Level: [a]\n"));
        assert!(reading.yaml_fallback_error.is_some());
        assert_eq!(
            reading.local.expect("translates").domains.dogstatsd.listeners.port,
            9000
        );
        assert_eq!(reading.deprecated_names, [deprecated("old_port", "dogstatsd_port")]);

        // Neither reading translates.
        let reading = read(&format!("{respelled}dogstatsd_tag_cardinality: bogus\n"));
        assert!(reading.local.is_err());
        assert_eq!(reading.deprecated_names, both);
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

    #[test]
    fn an_empty_metric_filter_prefix_item_is_the_empty_prefix() {
        // As in the Agent, an empty YAML list item is a null entry, whose empty prefix matches every
        // metric.
        let loaded = try_load("prefix_empty_item", "metric_filterlist_prefix:\n  - prefix: a.\n  -\n").expect("loads");
        let prefixes: Vec<_> = loaded
            .local()
            .domains
            .dogstatsd
            .metric_filter
            .prefix_rules
            .iter()
            .map(|rule| rule.prefix.as_str())
            .collect();
        assert_eq!(prefixes, ["a.", ""]);
    }
}
