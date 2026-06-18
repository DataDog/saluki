//! A confined pass-through of the runtime Datadog source map for the `saluki-env` provider layer.
//!
//! # Why this exists
//!
//! The `saluki-env` crate (host, workload, and autodiscovery providers, feature detection, cgroups
//! readers) is a general-purpose crate whose constructors take a
//! `saluki_config_tools::GenericConfiguration`. It is **out of scope** for this cutover and was not
//! adapted to typed config. The binary, however, must not name `GenericConfiguration`, import
//! `saluki-config-tools`, or load raw config itself.
//!
//! [`EnvConfig`] bridges that gap: the config-system (the only ADP crate permitted raw-map APIs)
//! materializes the runtime Datadog source snapshot into a `GenericConfiguration` once and hands the
//! binary an opaque [`EnvConfig`]. The binary passes `env_config.raw()` straight into the
//! `saluki-env` constructors via type inference, never writing the raw-map type name.
//!
//! This is a deliberate, documented simplification. The values genuinely consumed here -- feature
//! detection, cgroups hierarchy discovery, the fixed-hostname fallback, and CRI/containerd
//! parameters -- are local-environment facts that have no typed home in `SalukiConfiguration` and
//! that `saluki-env` reads through its own raw-map constructors. Confining the raw map to this
//! pass-through keeps "the config-system is the only place that touches the raw map" true.

use saluki_config_tools::{ConfigurationLoader, GenericConfiguration};
use saluki_error::{generic_error, GenericError};

/// An opaque handle to the runtime Datadog source map for the `saluki-env` provider layer.
///
/// Construct it from the runtime Datadog snapshot via [`EnvConfig::from_snapshot`]; read the
/// underlying raw configuration via [`EnvConfig::raw`]. The binary never names the inner type.
#[derive(Clone)]
pub struct EnvConfig {
    raw: GenericConfiguration,
}

impl EnvConfig {
    /// Materializes the runtime Datadog snapshot into an [`EnvConfig`].
    ///
    /// This is the single, confined point where the runtime source map is rebuilt as a
    /// `GenericConfiguration` for the `saluki-env` layer.
    ///
    /// # Errors
    ///
    /// Returns an error if the snapshot cannot be loaded into a `GenericConfiguration`.
    pub async fn from_snapshot(snapshot: serde_json::Value) -> Result<Self, GenericError> {
        let raw = ConfigurationLoader::default()
            .add_providers([figment::providers::Serialized::defaults(snapshot)])
            .into_generic()
            .await
            .map_err(|e| generic_error!("Failed to materialize environment configuration: {}", e))?;
        Ok(Self { raw })
    }

    /// Returns the underlying raw configuration for the `saluki-env` provider constructors.
    ///
    /// The binary passes this straight into `saluki-env` APIs; type inference means the binary never
    /// has to name the raw-map type.
    pub fn raw(&self) -> &GenericConfiguration {
        &self.raw
    }

    /// Deserializes a single optional typed value from the runtime Datadog source map.
    ///
    /// This is the typed escape hatch for binary-local components whose configuration is genuinely
    /// Saluki-schema-only and has no home in `SalukiConfiguration` (the OTTL trace processors, whose
    /// config types live in the binary). The raw key access stays confined to this crate; the binary
    /// receives a fully typed value and never touches the raw map.
    ///
    /// # Errors
    ///
    /// Returns an error if the key is present but cannot be deserialized into `T`.
    pub fn get_typed<T>(&self, key: &str) -> Result<Option<T>, GenericError>
    where
        T: serde::de::DeserializeOwned,
    {
        self.raw
            .try_get_typed::<T>(key)
            .map_err(|e| generic_error!("Failed to read `{}` from environment configuration: {}", key, e))
    }
}
