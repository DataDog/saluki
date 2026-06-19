//! Local source loading for the staged configuration lifecycle.
//!
//! This module performs the one-time read of the two local source languages and parses the typed
//! slices the lifecycle needs before runtime authority exists. It is the single place
//! `ConfigurationLoader` and the Datadog source-normalization mechanics (`KEY_ALIASES`,
//! `DatadogRemapper`) are wired together; the raw map produced here never escapes the crate.
//!
//! # Two source languages, two loaders
//!
//! The two trust domains are loaded by separate loaders with fixed env-prefix conventions:
//!
//! - The **Datadog** source (`datadog.yaml` / `DD_*`) is loaded with `KEY_ALIASES` and the
//!   `DatadogRemapper` provider applied -- those are Datadog source-language mechanics. Its env
//!   prefix is always `DD`.
//! - The **Saluki** source (`saluki.yaml` / `SALUKI_*`) is loaded plain: no aliases, no remapper
//!   (those are Datadog concerns). Its env prefix is always `SALUKI`. Saluki keys are always local
//!   in both runtime-authority modes.
//!
//! Both loaders tolerate a missing file (`try_from_yaml`): a deployment may configure entirely via
//! environment variables, and the Saluki source in particular is frequently absent.

use std::path::PathBuf;

use agent_data_plane_config::{BootstrapConfiguration, LocalApiBootstrap, RuntimeAuthority, SalukiOnlyConfiguration};
use datadog_agent_config::{DatadogRemapper, KEY_ALIASES};
use saluki_config_tools::{ConfigurationLoader, GenericConfiguration};
use saluki_error::{generic_error, GenericError};

/// The fixed env prefix for the Datadog source language (`DD_*`).
const DATADOG_ENV_PREFIX: &str = "DD";

/// The fixed env prefix for the Saluki source language (`SALUKI_*`).
const SALUKI_ENV_PREFIX: &str = "SALUKI";

/// The typed slices parsed from the one-time local source read.
///
/// This is an internal carrier returned by [`load_local_sources`]; the public lifecycle wraps it.
/// It deliberately holds no `GenericConfiguration`: the raw map is read here and reduced to typed
/// slices plus a single raw `serde_json::Value` snapshot of the local Datadog source.
pub(crate) struct LoadedSources {
    /// The typed bootstrap allowlist view (Datadog + Saluki sub-slices).
    pub bootstrap: BootstrapConfiguration,

    /// The parsed Saluki-schema-only input. Seeds the translator, both at startup and on every
    /// dynamic retranslation.
    pub saluki_only: SalukiOnlyConfiguration,

    /// The full local Datadog source as a raw JSON value.
    ///
    /// In [`RuntimeAuthority::LocalSnapshot`] mode this is the runtime authority. In
    /// [`RuntimeAuthority::AgentStream`] mode it is bootstrap-only and is not merged into the
    /// runtime Datadog config (the stream snapshot replaces it).
    pub datadog_snapshot: serde_json::Value,

    /// The resolved runtime authority for this process.
    pub authority: RuntimeAuthority,

    /// The raw `data_plane.standalone_mode` value read at bootstrap time.
    ///
    /// `standalone_mode` is not in the Datadog Agent core schema, so it cannot go through the
    /// Datadog witness. It is threaded through here so that both `start_local` and `start_stream`
    /// can apply it to `control.standalone_mode` after translation.
    pub standalone_mode: bool,

    /// The raw `data_plane.otlp.enabled` value read at bootstrap time.
    ///
    /// `data_plane.otlp.enabled` is not in the Datadog Agent core schema (excluded from the
    /// schema overlay), so it cannot go through the Datadog witness. It is threaded through here
    /// so that both `start_local` and `start_stream` can apply it to `control.otlp.enabled`.
    pub otlp_enabled: bool,
}

/// Reads both local sources once and parses the typed slices the lifecycle needs.
///
/// # Authority decision
///
/// The runtime authority is derived from two Datadog `data_plane.*` keys read here:
///
/// - [`RuntimeAuthority::LocalSnapshot`] when `data_plane.standalone_mode == true` **or**
///   `data_plane.remote_agent_enabled == false`. The local Datadog snapshot is the runtime
///   authority; there is no Agent connection.
/// - [`RuntimeAuthority::AgentStream`] otherwise. The Agent config stream is the sole runtime
///   authority for Datadog-schema config.
///
/// `remote_agent_enabled` defaults to `true` and `standalone_mode` defaults to `false`, matching the
/// binary's historical defaults, so the default authority is `AgentStream`.
///
/// # Errors
///
/// Returns an error if either loader cannot apply its environment provider, or if a required typed
/// slice cannot be parsed from the loaded sources.
pub(crate) fn load_local_sources(
    datadog_config_path: Option<PathBuf>, saluki_config_path: Option<PathBuf>,
) -> Result<LoadedSources, GenericError> {
    // --- Datadog source: datadog.yaml / DD_*, with aliases + remapper (Datadog mechanics). ---
    let mut datadog_loader = ConfigurationLoader::default().with_key_aliases(KEY_ALIASES);
    if let Some(path) = datadog_config_path.as_ref() {
        datadog_loader = datadog_loader.try_from_yaml(path);
    }
    let datadog_loader = datadog_loader
        .add_providers([DatadogRemapper::new()])
        .from_environment(DATADOG_ENV_PREFIX)
        .map_err(|e| generic_error!("Failed to apply the Datadog environment provider (prefix DD): {}", e))?;

    // A static snapshot of the local Datadog source is sufficient: local Datadog sources are read
    // once and never reread (runtime updates arrive on the stream in AgentStream mode, and the
    // snapshot is fixed in LocalSnapshot mode).
    let datadog_generic = datadog_loader.bootstrap_generic();

    let mut datadog_bootstrap: agent_data_plane_config::DatadogBootstrap = datadog_generic
        .as_typed()
        .map_err(|e| generic_error!("Failed to parse the Datadog bootstrap slice: {}", e))?;
    let mut datadog_snapshot: serde_json::Value = datadog_generic
        .as_typed()
        .map_err(|e| generic_error!("Failed to snapshot the local Datadog source: {}", e))?;

    // Normalize flat DD_ env var keys to their nested JSON form for pipeline-gate flags.
    // DD_DATA_PLANE_ENABLED produces the figment flat key `data_plane_enabled`; `DatadogConfiguration`
    // needs the nested path `data_plane.enabled`. try_get_typed resolves the authoritative value
    // (with flat-key fallback), and we inject it into the snapshot's nested object so that
    // serde_json::from_value::<DatadogConfiguration> can find and witness it.
    patch_snapshot_pipeline_gates(&mut datadog_snapshot, &datadog_generic)
        .map_err(|e| generic_error!("Failed to normalize data_plane snapshot keys: {}", e))?;

    let standalone_mode = datadog_generic
        .try_get_typed::<bool>("data_plane.standalone_mode")
        .map_err(|e| generic_error!("Failed to read data_plane.standalone_mode: {}", e))?
        .unwrap_or(false);
    let remote_agent_enabled = datadog_generic
        .try_get_typed::<bool>("data_plane.remote_agent_enabled")
        .map_err(|e| generic_error!("Failed to read data_plane.remote_agent_enabled: {}", e))?
        .unwrap_or(true);

    let authority = if standalone_mode || !remote_agent_enabled {
        RuntimeAuthority::LocalSnapshot
    } else {
        RuntimeAuthority::AgentStream
    };

    // Local API/CLI decisions read from nested `data_plane.*` keys (and a top-level key). These are
    // read with the dotted accessor because the typed flatten path does not resolve nested keys.
    let api_listen_address = datadog_generic
        .try_get_typed::<String>("data_plane.api_listen_address")
        .map_err(|e| generic_error!("Failed to read data_plane.api_listen_address: {}", e))?;
    let secure_api_listen_address = datadog_generic
        .try_get_typed::<String>("data_plane.secure_api_listen_address")
        .map_err(|e| generic_error!("Failed to read data_plane.secure_api_listen_address: {}", e))?;
    let dogstatsd_socket = datadog_generic
        .try_get_typed::<String>("dogstatsd_socket")
        .map_err(|e| generic_error!("Failed to read dogstatsd_socket: {}", e))?;
    let local_api = LocalApiBootstrap {
        api_listen_address,
        secure_api_listen_address,
        dogstatsd_socket,
    };

    let otlp_enabled = datadog_generic
        .try_get_typed::<bool>("data_plane.otlp.enabled")
        .map_err(|e| generic_error!("Failed to read data_plane.otlp.enabled: {}", e))?
        .unwrap_or(false);

    // --- Saluki source: saluki.yaml / SALUKI_*, plain (no Datadog aliases/remapper). ---
    let mut saluki_loader = ConfigurationLoader::default();
    if let Some(path) = saluki_config_path.as_ref() {
        saluki_loader = saluki_loader.try_from_yaml(path);
    }
    let saluki_loader = saluki_loader
        .from_environment(SALUKI_ENV_PREFIX)
        .map_err(|e| generic_error!("Failed to apply the Saluki environment provider (prefix SALUKI): {}", e))?;
    let saluki_generic = saluki_loader.bootstrap_generic();

    let mut saluki_only: SalukiOnlyConfiguration = saluki_generic
        .as_typed()
        .map_err(|e| generic_error!("Failed to parse the Saluki-schema-only configuration: {}", e))?;

    // `dogstatsd_tcp_port` is not in the Datadog core schema, so it cannot go through the overlay
    // witness. The Saluki source owns this key, but existing deployments set it via `DD_*` env
    // vars. Bridge the Datadog source value into the Saluki-only struct when not already set.
    if saluki_only.dogstatsd.tcp_port.is_none() {
        if let Ok(Some(v)) = datadog_generic.try_get_typed::<u16>("dogstatsd_tcp_port") {
            saluki_only.dogstatsd.tcp_port = Some(v);
        }
    }
    let saluki_bootstrap = saluki_generic
        .as_typed()
        .map_err(|e| generic_error!("Failed to parse the Saluki bootstrap slice: {}", e))?;

    datadog_bootstrap.local_api = local_api;

    let bootstrap = BootstrapConfiguration {
        datadog: datadog_bootstrap,
        saluki: saluki_bootstrap,
    };

    Ok(LoadedSources {
        bootstrap,
        saluki_only,
        datadog_snapshot,
        authority,
        standalone_mode,
        otlp_enabled,
    })
}

/// Normalizes pipeline-gate flags that arrive as flat figment keys from `DD_*` env vars into their
/// nested JSON form so that `serde_json::from_value::<DatadogConfiguration>` can find them.
///
/// `DD_DATA_PLANE_ENABLED` produces the flat figment key `data_plane_enabled` and JSON
/// `{ "data_plane_enabled": "true" }`.
/// The `DatadogConfiguration` struct expects the nested path `data_plane.enabled`. `try_get_typed`
/// resolves the authoritative value with its flat-key fallback, and we inject it into the nested
/// snapshot object.
fn patch_snapshot_pipeline_gates(
    snapshot: &mut serde_json::Value, generic: &GenericConfiguration,
) -> Result<(), GenericError> {
    let obj = snapshot
        .as_object_mut()
        .ok_or_else(|| generic_error!("Datadog snapshot is not a JSON object"))?;

    // data_plane.enabled (schema default: false)
    if let Ok(Some(v)) = generic.try_get_typed::<bool>("data_plane.enabled") {
        let dp = obj
            .entry("data_plane")
            .or_insert_with(|| serde_json::Value::Object(Default::default()));
        if let Some(dp_obj) = dp.as_object_mut() {
            dp_obj.insert("enabled".to_string(), serde_json::Value::Bool(v));
        }
    }

    // data_plane.dogstatsd.enabled (schema default: true)
    if let Ok(Some(v)) = generic.try_get_typed::<bool>("data_plane.dogstatsd.enabled") {
        let dp = obj
            .entry("data_plane")
            .or_insert_with(|| serde_json::Value::Object(Default::default()));
        if let Some(dp_obj) = dp.as_object_mut() {
            let dsd = dp_obj
                .entry("dogstatsd")
                .or_insert_with(|| serde_json::Value::Object(Default::default()));
            if let Some(dsd_obj) = dsd.as_object_mut() {
                dsd_obj.insert("enabled".to_string(), serde_json::Value::Bool(v));
            }
        }
    }

    Ok(())
}
