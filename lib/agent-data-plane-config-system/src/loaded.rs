//! Loads local configuration before selecting its runtime authority.
//!
//! [`LoadedConfiguration::load`] prepares a typed snapshot of the local file and environment, which
//! [`LoadedConfiguration::local`] exposes before values from the Datadog Agent stream are applied.
//! [`LoadedConfiguration::run`] layers the Agent's configuration stream over the local sources,
//! while [`LoadedConfiguration::standalone`] keeps the local sources authoritative. Both methods
//! consume the loaded sources and return a [`ConfigurationSystem`].

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use agent_data_plane_config::defaults::MIN_STANDALONE_CONFIG_RELOAD_INTERVAL;
use agent_data_plane_config::SalukiConfiguration;
use datadog_agent_config::apply_datadog_env;
use saluki_config::dynamic::ConfigUpdate;
use serde_json::Value;
use tokio::sync::mpsc;
use tracing::{info, warn};

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
    // The file the base was built from, which standalone mode re-reads when the experimental
    // `experimental.standalone_config_reload` setting is enabled.
    file: ConfigFile,
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
        let path = path.as_ref();
        let text = read_file(path).map_err(|message| Error::Base { message })?;
        let base = SourceTree::all_explicit(parse_base(path, &text, env)?);
        let local = translate_strict(&base)?;
        let file = ConfigFile {
            path: path.to_path_buf(),
            env,
            last_read: Ok(text),
            unsettled: None,
            changes: Arc::default(),
        };
        Ok(Self { base, local, file })
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
        if self.local.control.standalone_config_reload.enabled {
            warn!(
                path = %self.file.path.display(),
                "The experimental standalone configuration reload is enabled, but the Datadog Agent's configuration \
                 stream is the authority, so the configuration file is not re-read. The setting applies in standalone \
                 mode only."
            );
        }
        ConfigurationSystem::connected(config_stream, self.base).await
    }

    /// Uses the translated local configuration as the runtime authority.
    ///
    /// No configuration stream is created. Because nothing is layered on, the local snapshot is
    /// validated here, where [`run`](Self::run) instead validates the merged result of the Agent's
    /// initial snapshot.
    ///
    /// By default there is no update task either, and the second element is `None`. When the
    /// experimental `experimental.standalone_config_reload.enabled` setting is on, the second element
    /// is the [`ConfigurationUpdates`] that re-read the configuration file and apply each change, as
    /// [`run`](Self::run)'s apply each update from the Agent. The caller must run them. The setting
    /// exists only for the programmable metrics demo; it is not for production use and may be
    /// removed without notice.
    ///
    /// # Errors
    ///
    /// Returns an error if the local configuration fails validation.
    pub async fn standalone(self) -> Result<(ConfigurationSystem, Option<ConfigurationUpdates>), Error> {
        validate(&self.local)?;
        // Read once, here: the setting is restart-only, so a later edit to it changes nothing.
        let reload = self.local.control.standalone_config_reload.clone();
        if !reload.enabled {
            return Ok((ConfigurationSystem::standalone(self.local, self.base), None));
        }

        // A change is applied once two reads agree, so the file is read at most every 100 milliseconds.
        let interval = reload.interval.max(MIN_STANDALONE_CONFIG_RELOAD_INTERVAL);
        if interval != reload.interval {
            warn!(
                configured = ?reload.interval,
                interval = ?interval,
                "The standalone configuration reload interval is shorter than the shortest allowed; using the shortest."
            );
        }
        info!(
            path = %self.file.path.display(),
            interval = ?interval,
            "Experimental standalone configuration reload is enabled: the configuration file is re-read and changes \
             are applied. This is for the programmable metrics demo only, not for production use."
        );
        let (system, updates) = ConfigurationSystem::standalone_reloading(self.local, self.base, self.file, interval);
        Ok((system, Some(updates)))
    }
}

/// The configuration file the local base was built from, kept so standalone mode can re-read it.
pub(crate) struct ConfigFile {
    path: PathBuf,
    // The precedence the base was built with at startup, which every re-read keeps.
    env: EnvPrecedence,
    // What the last read taken as a change found: the file's text, or why the file could not be read.
    last_read: Result<String, String>,
    // A read that differs from `last_read`, seen once. It is taken as a change only when the next read finds the
    // same, so a file caught while it is being written is never applied half-written.
    unsettled: Option<Result<String, String>>,
    // How many changes were taken, applied or rejected: what a test waits for to know the file was read.
    changes: Arc<AtomicU64>,
}

impl ConfigFile {
    /// Returns the path of the file.
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Takes in what a read of the file found, `read`, and, if the file changed and has settled, rebuilds the base
    /// from it and the environment as [`LoadedConfiguration::load`] did.
    ///
    /// Returns `None` when the file reads as it did when it last changed, whether that read succeeded or not, so an
    /// unchanged file, valid or not, is no change; and when it reads differently for the first time, since a file
    /// being written reads differently on every read until it is whole. A change is taken when two consecutive reads
    /// agree: then this returns the new base, or why it could not be built: the file could not be read or parsed, or
    /// an environment variable could not be decoded.
    pub(crate) fn observe(&mut self, read: Result<String, String>) -> Option<Result<Value, Error>> {
        if read == self.last_read {
            self.unsettled = None;
            return None;
        }
        if self.unsettled.as_ref() != Some(&read) {
            self.unsettled = Some(read);
            return None;
        }
        self.unsettled = None;
        self.last_read = read.clone();
        self.changes.fetch_add(1, Ordering::Relaxed);
        Some(match read {
            Ok(text) => parse_base(&self.path, &text, self.env),
            Err(message) => Err(Error::Base { message }),
        })
    }

    /// Reads the file, as [`observe`](Self::observe) takes it in, on a thread that may block, off the async runtime.
    pub(crate) async fn read(&self) -> Result<String, String> {
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || read_file(&path))
            .await
            .unwrap_or_else(|e| Err(format!("read `{}`: {e}", self.path.display())))
    }

    /// Reads the file and takes in what it found, as the reload does.
    #[cfg(test)]
    fn reread(&mut self) -> Option<Result<Value, Error>> {
        let read = read_file(&self.path);
        self.observe(read)
    }
}

/// Builds the typed base from the configuration file at `path`, as [`parse_base`] does from its text.
#[cfg(test)]
fn build_base(path: &Path, env: EnvPrecedence) -> Result<Value, Error> {
    let text = read_file(path).map_err(|message| Error::Base { message })?;
    parse_base(path, &text, env)
}

/// Reads the configuration file's text, or describes why it cannot be read.
fn read_file(path: &Path) -> Result<String, String> {
    std::fs::read_to_string(path).map_err(|e| format!("read `{}`: {e}", path.display()))
}

/// Builds the typed base: the text of the configuration file at `path` parsed to its nested shape, with environment
/// variables read directly and decoded into the schema's shapes on top.
///
/// A null-valued key is dropped and an empty file is an empty object. The environment is then
/// overlaid via the generated Datadog reader, the Saluki-only reader, and the canonical proxy
/// variables. `env` sets whether the environment overwrites the file (`AfterFile`) or only fills
/// absent keys (`BeforeFile`); `Disabled` skips the environment entirely.
fn parse_base(path: &Path, text: &str, env: EnvPrecedence) -> Result<Value, Error> {
    let mut base: Value = serde_yaml::from_str(text).map_err(|e| Error::Base {
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

    /// A configuration file in the temporary directory that a test rewrites while the process runs.
    ///
    /// `test` names the file, so that tests which run at the same time do not share one. The file is
    /// removed on drop.
    struct EditedFile {
        path: std::path::PathBuf,
    }

    impl EditedFile {
        fn new(test: &str, contents: &str) -> Self {
            let file = Self {
                path: std::env::temp_dir().join(format!("adp_{test}_{}.yaml", std::process::id())),
            };
            file.write(contents);
            file
        }

        /// Replaces the file's contents atomically, by writing a new file and renaming it over the old one, so a
        /// re-read never sees a partly written file.
        fn write(&self, contents: &str) {
            let staged = self.path.with_extension("yaml.staged");
            std::fs::write(&staged, contents).unwrap();
            std::fs::rename(&staged, &self.path).unwrap();
        }
    }

    impl Drop for EditedFile {
        fn drop(&mut self) {
            std::fs::remove_file(&self.path).ok();
        }
    }

    /// Standalone configuration with the reload on, re-reading every 100 milliseconds, the shortest interval, followed
    /// by `rest`.
    fn reloading(rest: &str) -> String {
        format!(
            "api_key: test-api-key\nexperimental:\n  standalone_config_reload:\n    enabled: true\n    interval: 100ms\n{rest}"
        )
    }

    /// Waits until the reload has taken `count` changes of the file, applied or rejected: proof that it read them.
    async fn await_changes(changes: &AtomicU64, count: u64) {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while changes.load(Ordering::Relaxed) < count {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("the reload should take {count} changes"));
    }

    /// Waits until the live view `view` changes to `expected`.
    async fn await_change<T: Clone + PartialEq + std::fmt::Debug + 'static>(
        view: &mut agent_data_plane_config::Live<T>, expected: T,
    ) {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while view.changed().await != expected {}
        })
        .await
        .unwrap_or_else(|_| panic!("the live view should change to {expected:?}"));
    }

    /// Waits long enough for the reload, re-reading every 100 milliseconds, to have read the file several times.
    async fn several_rereads() {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }

    #[tokio::test]
    async fn standalone_config_reload_is_off_by_default() {
        let file = EditedFile::new("reload_off", "api_key: test-api-key\nlog_level: info\n");
        let loaded = LoadedConfiguration::load(&file.path, EnvPrecedence::Disabled)
            .await
            .expect("local sources load");
        assert!(!loaded.local().control.standalone_config_reload.enabled);

        let (system, updates) = loaded.standalone().await.expect("standalone configuration is valid");
        assert!(updates.is_none(), "no update task without the setting");

        // Nothing re-reads the file, so an edit never reaches the configuration.
        file.write("api_key: test-api-key\nlog_level: warn\n");
        several_rereads().await;
        assert_eq!(system.config().control.logging.level, "info");
    }

    #[tokio::test]
    async fn standalone_config_reload_applies_a_file_change_to_live_views() {
        let file = EditedFile::new("reload_applies", &reloading("log_level: info\n"));
        let loaded = LoadedConfiguration::load(&file.path, EnvPrecedence::Disabled)
            .await
            .expect("local sources load");
        let (system, updates) = loaded.standalone().await.expect("standalone configuration is valid");
        let updates = updates.expect("the setting creates an update task");
        let mut level = system.live(|config| &config.control.logging.level);

        // Like a stream update, a file change waits until the updates run.
        file.write(&reloading("log_level: warn\n"));
        several_rereads().await;
        assert_eq!(system.config().control.logging.level, "info");

        tokio::spawn(updates.run());
        await_change(&mut level, "warn".to_string()).await;

        // The raw source view follows the accepted file too.
        assert_eq!(system.raw_snapshot()()["log_level"], json!("warn"));
    }

    #[tokio::test]
    async fn standalone_config_reload_rejects_an_invalid_file_and_applies_a_later_valid_one() {
        let file = EditedFile::new("reload_invalid", &reloading("log_level: info\ndogstatsd_port: 9125\n"));
        let loaded = LoadedConfiguration::load(&file.path, EnvPrecedence::Disabled)
            .await
            .expect("local sources load");
        let changes = Arc::clone(&loaded.file.changes);
        let (system, updates) = loaded.standalone().await.expect("standalone configuration is valid");
        tokio::spawn(updates.expect("the setting creates an update task").run());
        let mut level = system.live(|config| &config.control.logging.level);

        // Each invalid file is rejected as a whole: its other values do not apply either. They are: YAML that does
        // not parse, a value that fails translation, and a configuration that fails validation (no API key). The
        // reload takes each as a change, so it read it.
        for (taken, invalid) in (1..).zip([
            reloading("log_level: error\ndogstatsd_port: [unterminated\n"),
            reloading("log_level: error\ndogstatsd_port: 7000\ndogstatsd_tag_cardinality: bogus\n"),
            "log_level: error\ndogstatsd_port: 7000\nexperimental:\n  standalone_config_reload:\n    enabled: true\n"
                .to_string(),
        ]) {
            file.write(&invalid);
            await_changes(&changes, taken).await;
            let config = system.config();
            assert_eq!(config.control.logging.level, "info", "{invalid}");
            assert_eq!(config.domains.dogstatsd.listeners.port, 9125, "{invalid}");
            assert_eq!(system.raw_snapshot()()["log_level"], json!("info"), "{invalid}");
        }

        // A later valid edit applies.
        file.write(&reloading("log_level: warn\ndogstatsd_port: 7000\n"));
        await_change(&mut level, "warn".to_string()).await;
        assert_eq!(system.config().domains.dogstatsd.listeners.port, 7000);
    }

    #[tokio::test]
    async fn standalone_config_reload_settings_are_read_at_startup() {
        let file = EditedFile::new("reload_restart_only", &reloading("log_level: info\n"));
        let loaded = LoadedConfiguration::load(&file.path, EnvPrecedence::Disabled)
            .await
            .expect("local sources load");
        let (system, updates) = loaded.standalone().await.expect("standalone configuration is valid");
        tokio::spawn(updates.expect("the setting creates an update task").run());
        let mut level = system.live(|config| &config.control.logging.level);

        // Turning the setting off, and slowing it down, in the file is applied as configuration, but the reload keeps
        // running on the settings read at startup.
        file.write(
            "api_key: test-api-key\nlog_level: warn\nexperimental:\n  standalone_config_reload:\n    enabled: false\n    interval: 1h\n",
        );
        await_change(&mut level, "warn".to_string()).await;
        assert!(!system.config().control.standalone_config_reload.enabled);

        file.write("api_key: test-api-key\nlog_level: error\n");
        await_change(&mut level, "error".to_string()).await;
    }

    #[test]
    fn a_reread_reports_only_a_settled_change_and_names_the_file_it_cannot_parse() {
        let file = EditedFile::new("reload_reread", "log_level: info\n");
        let mut config_file = ConfigFile {
            path: file.path.clone(),
            env: EnvPrecedence::Disabled,
            last_read: read_file(&file.path),
            unsettled: None,
            changes: Arc::default(),
        };
        assert!(config_file.reread().is_none(), "an unchanged file is no change");

        // A change is taken once two reads agree.
        file.write("log_level: [unterminated\n");
        assert!(config_file.reread().is_none(), "a change read once has not settled");
        let error = match config_file.reread() {
            Some(Err(error)) => error,
            other => panic!("YAML that does not parse is reported: {other:?}"),
        };
        assert!(matches!(error, Error::Base { .. }));
        assert!(error.to_string().contains(&file.path.display().to_string()), "{error}");
        // The same invalid file read again is no change, so it is reported once.
        assert!(config_file.reread().is_none());
        assert!(config_file.reread().is_none());

        file.write("log_level: warn\n");
        assert!(config_file.reread().is_none());
        let base = config_file.reread().expect("a change").expect("a valid file");
        assert_eq!(base["log_level"], json!("warn"));
        assert_eq!(config_file.changes.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn a_file_caught_while_it_is_written_is_never_applied() {
        let file = EditedFile::new("reload_half_written", "log_level: info\n");
        let mut config_file = ConfigFile {
            path: file.path.clone(),
            env: EnvPrecedence::Disabled,
            last_read: read_file(&file.path),
            unsettled: None,
            changes: Arc::default(),
        };
        // Each read finds more of the file, so none settles; the whole file, read twice, does.
        assert!(config_file
            .observe(Ok("log_level: info\ndogstatsd_po".to_string()))
            .is_none());
        assert!(config_file
            .observe(Ok("log_level: info\ndogstatsd_port: 70".to_string()))
            .is_none());
        let whole = "log_level: info\ndogstatsd_port: 7000\n".to_string();
        assert!(config_file.observe(Ok(whole.clone())).is_none());
        let base = config_file.observe(Ok(whole)).expect("a change").expect("a valid file");
        assert_eq!(base["dogstatsd_port"], json!(7000));
        // A file that reads as it did when it last changed, after a read that differed, is no change.
        assert!(config_file.observe(Ok("partial".to_string())).is_none());
        assert!(config_file
            .observe(Ok("log_level: info\ndogstatsd_port: 7000\n".to_string()))
            .is_none());
        assert!(config_file
            .observe(Ok("log_level: info\ndogstatsd_port: 7000\n".to_string()))
            .is_none());
        assert_eq!(config_file.changes.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn standalone_config_reload_reverts_a_removed_key_to_its_default() {
        let file = EditedFile::new("reload_removed", &reloading("log_level: info\ndogstatsd_port: 7000\n"));
        let loaded = LoadedConfiguration::load(&file.path, EnvPrecedence::Disabled)
            .await
            .expect("local sources load");
        let (system, updates) = loaded.standalone().await.expect("standalone configuration is valid");
        tokio::spawn(updates.expect("the setting creates an update task").run());
        let mut port = system.live(|config| &config.domains.dogstatsd.listeners.port);
        assert_eq!(system.config().domains.dogstatsd.listeners.port, 7000);

        // The file is the whole base: a key it no longer sets takes its default again.
        file.write(&reloading("log_level: info\n"));
        await_change(&mut port, 8125).await;
    }

    #[tokio::test]
    async fn standalone_config_reload_keeps_the_configuration_while_the_file_is_gone_and_applies_it_recreated() {
        let file = EditedFile::new("reload_recreated", &reloading("log_level: info\n"));
        let loaded = LoadedConfiguration::load(&file.path, EnvPrecedence::Disabled)
            .await
            .expect("local sources load");
        let changes = Arc::clone(&loaded.file.changes);
        let (system, updates) = loaded.standalone().await.expect("standalone configuration is valid");
        tokio::spawn(updates.expect("the setting creates an update task").run());
        let mut level = system.live(|config| &config.control.logging.level);

        // A file that cannot be read is a rejected change: the configuration stays as it was.
        std::fs::remove_file(&file.path).unwrap();
        await_changes(&changes, 1).await;
        assert_eq!(system.config().control.logging.level, "info");

        // Recreated, it applies.
        file.write(&reloading("log_level: warn\n"));
        await_change(&mut level, "warn".to_string()).await;
        assert_eq!(changes.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn standalone_config_reload_keeps_the_environment_precedence_of_startup() {
        let _guard = test_env_lock();
        let file = EditedFile::new("reload_env", &reloading("log_level: info\ndogstatsd_port: 8125\n"));
        std::env::set_var("DD_DOGSTATSD_PORT", "9125");

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("runtime builds");
        let port = runtime.block_on(async {
            let loaded = LoadedConfiguration::load(&file.path, EnvPrecedence::AfterFile)
                .await
                .expect("local sources load");
            let (system, updates) = loaded.standalone().await.expect("standalone configuration is valid");
            tokio::spawn(updates.expect("the setting creates an update task").run());
            let mut level = system.live(|config| &config.control.logging.level);
            assert_eq!(system.config().domains.dogstatsd.listeners.port, 9125);

            // The file changes the port, but the environment still wins, as it did at startup.
            file.write(&reloading("log_level: warn\ndogstatsd_port: 7000\n"));
            await_change(&mut level, "warn".to_string()).await;
            system.config().domains.dogstatsd.listeners.port
        });

        std::env::remove_var("DD_DOGSTATSD_PORT");
        assert_eq!(port, 9125);
    }

    #[test]
    fn standalone_config_reload_lets_the_file_win_over_the_environment_when_it_did_at_startup() {
        let _guard = test_env_lock();
        let file = EditedFile::new(
            "reload_env_before",
            &reloading("log_level: info\ndogstatsd_port: 8000\n"),
        );
        std::env::set_var("DD_DOGSTATSD_PORT", "9125");

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("runtime builds");
        let ports = runtime.block_on(async {
            let loaded = LoadedConfiguration::load(&file.path, EnvPrecedence::BeforeFile)
                .await
                .expect("local sources load");
            let (system, updates) = loaded.standalone().await.expect("standalone configuration is valid");
            tokio::spawn(updates.expect("the setting creates an update task").run());
            let mut port = system.live(|config| &config.domains.dogstatsd.listeners.port);
            let mut ports = vec![system.config().domains.dogstatsd.listeners.port];

            // The environment only fills what the file leaves out, at startup and on every re-read: a port the file
            // sets wins, and one it stops setting comes from the environment.
            file.write(&reloading("log_level: info\ndogstatsd_port: 7000\n"));
            await_change(&mut port, 7000).await;
            ports.push(system.config().domains.dogstatsd.listeners.port);
            file.write(&reloading("log_level: info\n"));
            await_change(&mut port, 9125).await;
            ports.push(system.config().domains.dogstatsd.listeners.port);
            ports
        });

        std::env::remove_var("DD_DOGSTATSD_PORT");
        assert_eq!(ports, [8000, 7000, 9125]);
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
}
