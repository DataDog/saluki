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
use saluki_config::{ConfigurationLoader, GenericConfiguration};
use serde_json::Value;
use tokio::sync::mpsc;

use crate::env_provider::EnvironmentProvider;
use crate::saluki_env_overlay;
use crate::source::SourceTree;
use crate::system::{translate_strict, ConfigurationSystem, Error};

// The environment-variable prefix ADP reads (`DD_`). Mirrors
// `PlatformSettings::get_env_var_prefix()`; hardcoded so the configuration system need not depend on
// `datadog-agent-commons` for a single constant.
const ENV_VAR_PREFIX: &str = "DD";

// Bound on the internal channel that forwards Agent updates into the compatibility map. Matches the
// Agent stream's own channel depth (`RemoteAgentBootstrap::create_config_stream`).
const COMPAT_FORWARD_CHANNEL_SIZE: usize = 100;

/// Where environment variables sit relative to the configuration file.
///
/// One setting, applied identically to the Figment provider order for the by-key view and to the
/// order the typed base is composed in.
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
///
/// Retains a nested base for the typed path and a loader for the legacy by-key path. Both use the
/// same source path and precedence but build their representations independently.
pub struct LoadedConfiguration {
    loader: ConfigurationLoader,
    // Nested local base used for typed translation and Agent-layer merges.
    base: SourceTree,
    // Strictly translated local snapshot exposed before authority selection and used by standalone
    // mode.
    local: SalukiConfiguration,
}

impl LoadedConfiguration {
    /// Loads and strictly translates the local file and environment using the requested precedence.
    ///
    /// # Errors
    ///
    /// Returns an error if a local source cannot be read, decoded, deserialized, or translated.
    pub async fn load(path: impl AsRef<Path>, env: EnvPrecedence) -> Result<Self, Error> {
        let loader = build_loader(path.as_ref(), env)?;
        // Every value the local sources supply was set explicitly: the file set it or an environment
        // variable supplied it.
        let base = SourceTree::all_explicit(build_base(path.as_ref(), env)?);
        let local = translate_strict(&base)?;
        Ok(Self { loader, base, local })
    }

    /// Returns the typed snapshot of the local file and environment.
    ///
    /// Values from the Datadog Agent stream have not been applied to this snapshot.
    pub fn local(&self) -> &SalukiConfiguration {
        &self.local
    }

    /// Returns the local file and environment through the legacy by-key configuration API.
    // TODO: Remove this compatibility view once bootstrap consumers use `local`.
    pub fn raw_config(&self) -> GenericConfiguration {
        self.loader.bootstrap_generic()
    }

    /// Uses the Datadog Agent's configuration stream as the runtime authority.
    ///
    /// Waits for the initial Agent snapshot, layers it over the local sources, strictly translates
    /// the result, and then starts the update task.
    ///
    /// # Errors
    ///
    /// Returns an error if the compatibility map cannot be built, the stream closes before its
    /// initial snapshot, or the merged configuration cannot be deserialized or translated.
    pub async fn run(self, config_stream: mpsc::Receiver<ConfigUpdate>) -> Result<ConfigurationSystem, Error> {
        // The configuration system owns the Agent stream and forwards each update into this
        // compatibility map, so `raw_map()` keeps serving un-migrated components.
        let (compat_tx, compat_rx) = mpsc::channel(COMPAT_FORWARD_CHANNEL_SIZE);
        let compat_map = self.loader.with_dynamic_configuration(compat_rx).into_generic().await?;

        ConfigurationSystem::connected(config_stream, compat_tx, compat_map, self.base).await
    }

    /// Uses the translated local configuration as the runtime authority.
    ///
    /// No configuration stream or update task is created.
    ///
    /// # Errors
    ///
    /// Returns an error if the compatibility map cannot be built from the local sources.
    pub async fn standalone(self) -> Result<ConfigurationSystem, Error> {
        let compat_map = self.loader.into_generic().await?;
        Ok(ConfigurationSystem::standalone(compat_map, self.local))
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

/// Builds the by-key configuration view at the given precedence, with later sources overriding
/// earlier ones.
///
/// Two environment providers are used together. [`ConfigurationLoader::from_environment`] scans the
/// `DD_` prefix and contributes every variable as a flat key, which is what a key not covered by
/// either source model needs. [`EnvironmentProvider`] then contributes the modeled keys at their
/// canonical paths, so a nested key such as `proxy.http` is reachable in the shape the Datadog Agent
/// itself uses. It sits at the higher precedence of the two because it knows a key's real shape,
/// while the scanning provider can only guess from the variable's name.
fn build_loader(path: &Path, env: EnvPrecedence) -> Result<ConfigurationLoader, Error> {
    let loader = ConfigurationLoader::default();
    let loader = match env {
        EnvPrecedence::AfterFile => loader
            .from_yaml(path)?
            .from_environment(ENV_VAR_PREFIX)?
            .add_providers([schema_env_provider()?]),
        EnvPrecedence::BeforeFile => loader
            .from_environment(ENV_VAR_PREFIX)?
            .add_providers([schema_env_provider()?])
            .from_yaml(path)?,
        EnvPrecedence::Disabled => loader.from_yaml(path)?,
    };
    Ok(loader)
}

/// Builds the schema-driven environment provider, reporting a malformed value the same way the typed
/// base does.
fn schema_env_provider() -> Result<EnvironmentProvider, Error> {
    EnvironmentProvider::new().map_err(|message| Error::Base { message })
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
        // The compatibility loader accepts this value, but typed translation rejects it.
        std::fs::write(&path, "dogstatsd_tag_cardinality: bogus\n").unwrap();

        let result = LoadedConfiguration::load(&path, EnvPrecedence::Disabled).await;

        std::fs::remove_file(&path).ok();
        assert!(matches!(result, Err(Error::Translate { .. })));
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
    fn a_nested_datadog_environment_variable_reaches_the_by_key_view() {
        // `DD_PROXY_HTTP` names a nested key, which Figment's prefix scan cannot place. The
        // schema-driven provider resolves it, so the by-key view serves it at `proxy.http`.
        let _guard = test_env_lock();
        let path = std::env::temp_dir().join(format!("adp_bykey_env_{}.yaml", std::process::id()));
        std::fs::write(&path, "{}\n").unwrap();
        std::env::set_var("DD_PROXY_HTTP", "http://proxy.example.com");

        let loaded = block_on(LoadedConfiguration::load(&path, EnvPrecedence::AfterFile)).expect("local sources load");
        let raw = loaded.raw_config();

        std::env::remove_var("DD_PROXY_HTTP");
        std::fs::remove_file(&path).ok();
        assert_eq!(
            raw.try_get_typed::<String>("proxy.http").expect("key reads"),
            Some("http://proxy.example.com".to_string())
        );
    }

    #[test]
    fn the_adp_zstd_override_reaches_both_views_from_the_environment() {
        // `DD_DATA_PLANE_SERIALIZER_ZSTD_COMPRESSOR_LEVEL` names a nested Saluki-only key that the
        // prefix scan flattens to `data_plane_serializer_zstd_compressor_level`. The five payload
        // encoders read it from the by-key view, so cover that view alongside the typed model.
        let _guard = test_env_lock();
        let path = std::env::temp_dir().join(format!("adp_zstd_env_{}.yaml", std::process::id()));
        std::fs::write(&path, "{}\n").unwrap();
        std::env::set_var("DD_DATA_PLANE_SERIALIZER_ZSTD_COMPRESSOR_LEVEL", "7");

        let loaded = block_on(LoadedConfiguration::load(&path, EnvPrecedence::AfterFile)).expect("local sources load");
        let from_by_key = loaded
            .raw_config()
            .try_get_typed::<i32>("data_plane.serializer_zstd_compressor_level")
            .expect("key reads");
        let from_typed = loaded
            .local()
            .shared
            .endpoints
            .compression
            .zstd_compressor_level_override;

        std::env::remove_var("DD_DATA_PLANE_SERIALIZER_ZSTD_COMPRESSOR_LEVEL");
        std::fs::remove_file(&path).ok();
        assert_eq!(from_by_key, Some(7));
        assert_eq!(from_typed, Some(7));
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
