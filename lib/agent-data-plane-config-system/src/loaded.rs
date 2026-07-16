//! [`LoadedConfiguration`]: the local configuration sources loaded once, ready to be bound to a
//! runtime authority.
//!
//! The configuration system owns loading. [`LoadedConfiguration::load`] reads the local file and
//! environment using the requested precedence, then one of the two terminals binds those sources to
//! an authority: [`LoadedConfiguration::run`] uses the Datadog Agent's config stream, while
//! [`LoadedConfiguration::standalone`] treats the local sources as authoritative.

use std::path::Path;

use datadog_agent_config::apply_datadog_env;
// TODO: remove after migration to typed config; these support the legacy flat-key loader.
use datadog_agent_config::{DatadogRemapper, EnvOverlayMode, KEY_ALIASES};
use saluki_config::dynamic::ConfigUpdate;
use saluki_config::{ConfigurationLoader, GenericConfiguration};
use serde_json::Value;
use tokio::sync::mpsc;

use crate::saluki_env_overlay;
use crate::system::{ConfigurationSystem, Error};

// The environment-variable prefix ADP reads (`DD_`). Mirrors
// `PlatformSettings::get_env_var_prefix()`; hardcoded so the configuration system need not depend on
// `datadog-agent-commons` for a single constant.
const ENV_VAR_PREFIX: &str = "DD";

// Bound on the internal channel that forwards Agent updates into the compatibility map. Matches the
// Agent stream's own channel depth (`RemoteAgentBootstrap::create_config_stream`).
const COMPAT_FORWARD_CHANNEL_SIZE: usize = 100;

/// Where environment variables sit relative to the configuration file.
///
/// One setting, applied identically to the Figment provider order (which flat key wins) and to the
/// typed env-key overlay. Replaces the raw `EnvOverlayMode` at the configuration system's boundary.
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

impl EnvPrecedence {
    /// The overlay flag the typed deserializer consumes for the same precedence.
    //
    // The remote Agent layer takes precedence over local environment values, so the environment
    // fills only a nested slot that no higher-authority source supplied. Both file precedence modes
    // use this rule; `Disabled` suppresses environment relocation entirely.
    fn overlay_mode(self) -> EnvOverlayMode {
        match self {
            EnvPrecedence::Disabled => EnvOverlayMode::Disabled,
            EnvPrecedence::AfterFile | EnvPrecedence::BeforeFile => EnvOverlayMode::Fallback,
        }
    }
}

/// Local configuration sources loaded once, not yet bound to a runtime authority.
///
/// Holds both the nested typed value and the by-key view so they resolve the local sources
/// identically.
pub struct LoadedConfiguration {
    loader: ConfigurationLoader,
    // The complete configuration read from the file and environment at startup. It is retained so
    // the typed path and the by-key compatibility view use the same local sources.
    base: Value,
    env: EnvPrecedence,
}

impl LoadedConfiguration {
    /// Loads the local sources once from `path` and the environment, at the given precedence.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be read or is not valid YAML, or if the environment
    /// prefix is invalid.
    pub async fn load(path: impl AsRef<Path>, env: EnvPrecedence) -> Result<Self, Error> {
        let loader = build_loader(path.as_ref(), env)?;
        let base = build_base(path.as_ref(), env)?;
        Ok(Self { loader, base, env })
    }

    /// A static snapshot of the local sources for startup phase configuration.
    // TODO: remove this backdoor and use typed configuration for the bootstrap phase
    pub fn raw_config(&self) -> GenericConfiguration {
        self.loader.bootstrap_generic()
    }

    /// Connected authority: attach the Datadog Agent's config stream, block for the first
    /// authoritative snapshot, deserialize and translate it, and start the update task. This is the
    /// strict startup gate: a snapshot that cannot be received, deserialized, or translated fails
    /// the boot.
    ///
    /// # Errors
    ///
    /// Returns an error if the initial configuration cannot be built.
    pub async fn run(self, config_stream: mpsc::Receiver<ConfigUpdate>) -> Result<ConfigurationSystem, Error> {
        // The configuration system owns the Agent stream and forwards each update into this
        // compatibility map, so `raw_map()` keeps serving un-migrated components.
        let (compat_tx, compat_rx) = mpsc::channel(COMPAT_FORWARD_CHANNEL_SIZE);
        let compat_map = self.loader.with_dynamic_configuration(compat_rx).into_generic().await?;

        ConfigurationSystem::connected(config_stream, compat_tx, compat_map, self.base, self.env.overlay_mode()).await
    }

    /// Standalone authority: the local sources are authoritative, with no stream and no update task.
    /// Also a strict startup gate.
    ///
    /// # Errors
    ///
    /// Returns an error if the configuration cannot be built from the local sources.
    pub async fn standalone(self) -> Result<ConfigurationSystem, Error> {
        let compat_map = self.loader.into_generic().await?;
        ConfigurationSystem::standalone(compat_map, self.base, self.env.overlay_mode())
    }
}

/// Builds the typed base: the configuration file parsed to its nested shape, with environment
/// variables read directly and decoded into the schema's shapes on top.
///
/// It reads the same file as the by-key compatibility view, normalizes it the same way (drop
/// null-valued keys; an empty file is an empty object), and then overlays the environment via the
/// generated Datadog reader, the Saluki-only reader, and the
/// canonical proxy variables. `env` sets whether the environment overwrites the file (`AfterFile`)
/// or only fills absent keys (`BeforeFile`); `Disabled` skips the environment entirely.
fn build_base(path: &Path, env: EnvPrecedence) -> Result<Value, Error> {
    let text = std::fs::read_to_string(path).map_err(|e| Error::Base {
        message: format!("read `{}`: {e}", path.display()),
    })?;
    let mut base: Value = serde_yaml::from_str(&text).map_err(|e| Error::Base {
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

/// Recursively removes object entries whose value is JSON null, mirroring the compatibility loader's
/// file normalization: an explicitly null YAML key must not override a model default with null.
fn drop_nulls(value: &mut Value) {
    if let Value::Object(map) = value {
        map.retain(|_, v| !v.is_null());
        for v in map.values_mut() {
            drop_nulls(v);
        }
    }
}

/// Builds the by-key configuration view at the given precedence. File keys are normalized to the
/// names used by environment variables, and later sources override earlier ones.
fn build_loader(path: &Path, env: EnvPrecedence) -> Result<ConfigurationLoader, Error> {
    let loader = ConfigurationLoader::default().with_key_aliases(KEY_ALIASES);
    let loader = match env {
        EnvPrecedence::AfterFile => loader
            .from_yaml(path)?
            .add_providers([DatadogRemapper::new()])
            .from_environment(ENV_VAR_PREFIX)?,
        EnvPrecedence::BeforeFile => loader
            .add_providers([DatadogRemapper::new()])
            .from_environment(ENV_VAR_PREFIX)?
            .from_yaml(path)?,
        EnvPrecedence::Disabled => loader.from_yaml(path)?,
    };
    Ok(loader)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    // The environment is process-global; serialize the tests that mutate it.
    static ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn build_base_composes_file_and_environment_by_precedence() {
        let _guard = ENV_MUTEX.lock().unwrap();
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

    #[test]
    fn build_base_rejects_a_malformed_environment_value() {
        let _guard = ENV_MUTEX.lock().unwrap();
        let path = std::env::temp_dir().join(format!("adp_build_base_bad_{}.yaml", std::process::id()));
        std::fs::write(&path, "dogstatsd_port: 8125\n").unwrap();
        std::env::set_var("DD_DOGSTATSD_PORT", "not-a-number");

        let result = build_base(&path, EnvPrecedence::AfterFile);

        std::env::remove_var("DD_DOGSTATSD_PORT");
        std::fs::remove_file(&path).ok();
        assert!(matches!(result, Err(Error::Base { .. })));
    }
}
