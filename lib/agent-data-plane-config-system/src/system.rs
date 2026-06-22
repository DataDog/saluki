//! The configuration system lifecycle.
//!
//! [`ConfigurationSystem`] loads and starts through two types in a consume-by-value transition:
//!
//! ```text
//! let loader = ConfigurationSystem::load(config);   // synchronous
//! let system = Arc::new(loader.start().await?);      // async, consumes the loader
//! ```
//!
//! - [`ConfigurationSystem::load`] takes ownership of the already-final [`GenericConfiguration`]
//!   that `run.rs` builds (after remote-agent / config-stream resolution) and returns a
//!   [`ConfigurationSystemLoader`].
//! - [`ConfigurationSystemLoader::start`] consumes the loader, runs translation once, and returns
//!   the running [`ConfigurationSystem`].
//!
//! The running system exposes two views:
//!
//! - [`ConfigurationSystem::raw_map`] -- a `&GenericConfiguration` escape hatch for components that
//!   have not yet been flipped onto typed config. It stays live (the Agent config stream still feeds
//!   it), so un-flipped components see updates exactly as before.
//! - [`ConfigurationSystem::saluki`] -- an owned clone of the translated master
//!   [`SalukiConfiguration`]. The master is translated once at `start()` and frozen for the process
//!   lifetime; only the DogStatsD source is real in this stage.

use agent_data_plane_config::{SalukiConfiguration, SalukiOnlyConfiguration};
use bytesize::ByteSize;
use datadog_agent_config::DatadogConfiguration;
use saluki_config_tools::GenericConfiguration;
use saluki_error::{ErrorContext as _, GenericError};

use crate::translate::translate;

/// The running configuration system.
///
/// Holds the retained source map and the translated master [`SalukiConfiguration`]. The master is a
/// plain, immutable field: translated once at [`ConfigurationSystemLoader::start`] and never
/// mutated. A later dynamic flip swaps this for a swappable container without changing
/// [`saluki`](Self::saluki) or [`raw_map`](Self::raw_map).
pub struct ConfigurationSystem {
    /// The retained source map. The single source of raw config for un-flipped components.
    config: GenericConfiguration,

    /// The translated master. Frozen at `start()`; only the DogStatsD source is real in this stage.
    saluki: SalukiConfiguration,
}

impl ConfigurationSystem {
    /// Takes ownership of the final [`GenericConfiguration`] and returns a loader.
    ///
    /// This is the synchronous first half of the lifecycle. The returned
    /// [`ConfigurationSystemLoader`] holds the loaded snapshot and transitions to a running
    /// [`ConfigurationSystem`] in one [`start`](ConfigurationSystemLoader::start) step.
    // TODO: eliminate GenericConfiguration arg; config-system should own loading
    pub fn load(config: GenericConfiguration) -> ConfigurationSystemLoader {
        ConfigurationSystemLoader { config }
    }

    /// Returns the retained source map.
    ///
    /// This is the escape hatch every un-flipped component reads through during the cutover. It is
    /// removed when the last `raw_map()` caller is flipped onto typed config.
    pub fn raw_map(&self) -> &GenericConfiguration {
        &self.config
    }

    /// Returns an owned clone of the translated master [`SalukiConfiguration`].
    ///
    /// The clone is cheap relative to startup cost and lets a flipped helper take a fully owned
    /// slice. In this stage only `components.dogstatsd.source` carries real translated values.
    pub fn saluki(&self) -> SalukiConfiguration {
        self.saluki.clone()
    }
}

/// A loaded-but-not-started configuration system.
///
/// Not a builder: it holds a loaded snapshot and transitions to a running [`ConfigurationSystem`]
/// in a single [`start`](Self::start) call that consumes it by value.
pub struct ConfigurationSystemLoader {
    config: GenericConfiguration,
}

impl ConfigurationSystemLoader {
    /// Starts the runtime, consuming the loader by value.
    ///
    /// Parses the retained map as a [`DatadogConfiguration`], translates it once into the master
    /// [`SalukiConfiguration`], and returns the running system. This is `async` to lock the
    /// end-state signature (it will later await the Agent's initial config once config-system owns
    /// loading); it currently awaits nothing.
    ///
    /// # Errors
    ///
    /// Returns an error if the source map cannot be parsed as a [`DatadogConfiguration`] or if
    /// translation fails. At startup either is fatal: the caller bails and the process exits.
    pub async fn start(self) -> Result<ConfigurationSystem, GenericError> {
        let datadog: DatadogConfiguration = self
            .config
            .as_typed()
            .error_context("Failed to parse Datadog configuration.")?;

        let mut saluki_only = SalukiOnlyConfiguration::default();
        bridge_dd_fallbacks(&mut saluki_only, &self.config);

        let saluki = translate(&saluki_only, &datadog).error_context("Failed to translate configuration.")?;

        Ok(ConfigurationSystem {
            config: self.config,
            saluki,
        })
    }
}

/// Bridges Saluki-schema-only DogStatsD keys from the Datadog source (`DD_*`) into the Saluki-only
/// struct.
///
/// Saluki-schema-only keys are owned by the Saluki source (`SALUKI_*` / `saluki.yaml`) and the clean
/// end state reads them only from there. Existing deployments and tests set these keys via `DD_*`,
/// so until a `SALUKI_*` source is wired this bridges each value from the retained map. This is
/// temporary transition scaffolding that is removed when Saluki-only loading lands.
// TODO: delete once SALUKI_* loading is wired; keeps DD_* parity for DogStatsD Saluki-only knobs.
fn bridge_dd_fallbacks(saluki_only: &mut SalukiOnlyConfiguration, dd: &GenericConfiguration) {
    let dsd = &mut saluki_only.dogstatsd;

    if let Ok(Some(v)) = dd.try_get_typed::<bool>("dogstatsd_allow_context_heap_allocs") {
        dsd.allow_context_heap_allocs = Some(v);
    }
    if let Ok(Some(v)) = dd.try_get_typed::<bool>("dogstatsd_autoscale_udp_listeners") {
        dsd.autoscale_udp_listeners = Some(v);
    }
    if let Ok(Some(v)) = dd.try_get_typed::<usize>("dogstatsd_buffer_count") {
        dsd.buffer_count = Some(v);
    }
    if let Ok(Some(v)) = dd.try_get_typed::<usize>("dogstatsd_cached_contexts_limit") {
        dsd.cached_contexts_limit = Some(v);
    }
    if let Ok(Some(v)) = dd.try_get_typed::<usize>("dogstatsd_cached_tagsets_limit") {
        dsd.cached_tagsets_limit = Some(v);
    }
    if let Ok(Some(v)) = dd.try_get_typed::<f64>("dogstatsd_minimum_sample_rate") {
        dsd.minimum_sample_rate = Some(v);
    }
    if let Ok(Some(v)) = dd.try_get_typed::<bool>("dogstatsd_permissive_decoding") {
        dsd.permissive_decoding = Some(v);
    }
    if let Ok(Some(v)) = dd.try_get_typed::<ByteSize>("dogstatsd_string_interner_size_bytes") {
        dsd.string_interner_size_bytes = Some(v.0);
    }
    if let Ok(Some(v)) = dd.try_get_typed::<u16>("dogstatsd_tcp_port") {
        dsd.tcp_port = Some(v);
    }
}

#[cfg(test)]
mod tests {
    use saluki_config_tools::ConfigurationLoader;
    use serde_json::json;

    use super::*;

    async fn generic_from(value: serde_json::Value) -> GenericConfiguration {
        ConfigurationLoader::default()
            .add_providers([figment::providers::Serialized::defaults(value)])
            .into_generic()
            .await
            .expect("build generic configuration")
    }

    #[tokio::test]
    async fn load_start_saluki_translates_dogstatsd_source() {
        let config = generic_from(json!({ "dogstatsd_port": 7000, "dogstatsd_non_local_traffic": true })).await;

        let system = ConfigurationSystem::load(config).start().await.expect("start succeeds");
        let saluki = system.saluki();

        assert_eq!(saluki.components.dogstatsd.source.port, 7000);
        assert!(saluki.components.dogstatsd.source.non_local_traffic);
    }

    #[tokio::test]
    async fn raw_map_returns_retained_source_map() {
        let config = generic_from(json!({ "dogstatsd_port": 9123 })).await;

        let system = ConfigurationSystem::load(config).start().await.expect("start succeeds");

        let port: u16 = system
            .raw_map()
            .get_typed("dogstatsd_port")
            .expect("dogstatsd_port present in raw map");
        assert_eq!(port, 9123);
    }

    #[tokio::test]
    async fn bridge_carries_saluki_only_knob_from_dd_keys() {
        // `dogstatsd_tcp_port` is a Saluki-schema-only knob: it is not witnessed, so it reaches the
        // native source only through the seed. The bridge must carry the DD_* value into the seed.
        let config = generic_from(json!({ "dogstatsd_tcp_port": 8200 })).await;

        let system = ConfigurationSystem::load(config).start().await.expect("start succeeds");

        assert_eq!(system.saluki().components.dogstatsd.source.tcp_port, 8200);
    }
}
