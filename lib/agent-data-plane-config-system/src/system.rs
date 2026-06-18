//! The staged, no-escape configuration-system lifecycle.
//!
//! This module is the design's `system.rs`: it orchestrates the `load -> Loaded -> start_runtime ->
//! Started` lifecycle. Local sources load once (in [`bootstrap`](crate::bootstrap)); bootstrap reads
//! typed slices from that one read; and [`LoadedConfigurationSystem::start_runtime`] consumes the
//! loaded object **by value** so the raw bootstrap snapshot cannot become an ambient runtime map.
//!
//! # Lifecycle
//!
//! ```text
//! ConfigurationSystem::load(inputs)            -> LoadedConfigurationSystem
//!     loaded.bootstrap() / loaded.authority()   (typed, pre-runtime, diagnostic only)
//! loaded.start_runtime(registration).await     -> StartedConfigurationSystem   (consumes loaded)
//!     started.saluki() / dynamic_handles() / attachments() / config_views() / control()
//! ```
//!
//! # Runtime authority
//!
//! The authority is decided once, at [`load`](ConfigurationSystem::load) time, from the local
//! Datadog `data_plane.*` keys (see `load_local_sources`). It drives two
//! distinct `start_runtime` paths:
//!
//! - [`RuntimeAuthority::LocalSnapshot`]: the retained local Datadog snapshot is the runtime
//!   authority. No Agent connection is made, no inbound stream task runs, and all dynamic handles
//!   are `Fixed`. [`StartedConfigurationSystem::attachments`] returns `None`.
//! - [`RuntimeAuthority::AgentStream`]: the config-system connects to the Agent, awaits the first
//!   `ConfigUpdate::Snapshot`, and uses that as the initial runtime Datadog snapshot (local Datadog
//!   sources are bootstrap-only and are **not** merged into runtime config). The inbound stream task
//!   ingests subsequent updates; dynamic handles are `Live`; `attachments()` returns `Some`.

use std::path::PathBuf;
use std::sync::Arc;

use agent_data_plane_config::{
    BootstrapConfiguration, ConfigViews, ControlConfiguration, InternalConfigView, RuntimeAuthority,
    SalukiConfiguration, SalukiOnlyConfiguration, SourceConfigView,
};
use datadog_agent_config::DatadogConfiguration;
use saluki_common::scrubber::default_scrubber;
use saluki_config_tools::dynamic::ConfigUpdate;
use saluki_error::{generic_error, GenericError};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing::info;

use crate::bootstrap::{load_local_sources, LoadedSources};
use crate::datadog_agent::{
    connect, Attachments, DatadogAgentConnection, RemoteAgentClientConfiguration, RemoteAgentRegistration,
};
use crate::dynamic::{ConfigUpdateRouter, DynamicConfigHandles, ViewSources};
use crate::env_config::EnvConfig;
use crate::translate::translate;

/// How long to wait for the first config snapshot from the Agent stream before giving up.
///
/// The stream sends a full snapshot immediately on connect; this bound exists only to surface a
/// clear error instead of hanging forever if the Agent never sends one.
const FIRST_SNAPSHOT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// The inputs the lifecycle reads local sources from.
///
/// Env prefixes are fixed by convention and are not configurable here: the Datadog source reads
/// `DD_*`, the Saluki source reads `SALUKI_*`. Only the on-disk file paths are inputs; either may be
/// `None` (the source is then configured entirely from its environment prefix).
#[derive(Clone, Debug, Default)]
pub struct ConfigurationSystemInputs {
    /// Path to the Datadog Agent configuration file (`datadog.yaml`). Optional.
    pub datadog_config_path: Option<PathBuf>,

    /// Path to the Saluki configuration file (`saluki.yaml`). Optional.
    pub saluki_config_path: Option<PathBuf>,
}

/// The entry point to the configuration-system lifecycle.
///
/// This is a zero-field type whose only role is to host [`load`](Self::load). It exists so the
/// lifecycle reads as `ConfigurationSystem::load(...)` per the design's `main.rs` sketch.
pub struct ConfigurationSystem;

impl ConfigurationSystem {
    /// Loads the local sources once and returns the staged [`LoadedConfigurationSystem`].
    ///
    /// This reads both source languages (`datadog.yaml`/`DD_*` and `saluki.yaml`/`SALUKI_*`),
    /// parses the typed bootstrap and Saluki-only slices, snapshots the local Datadog source, and
    /// resolves the runtime authority. The raw map is reduced to typed slices here and never
    /// escapes.
    ///
    /// # Errors
    ///
    /// Returns an error if a loader cannot apply its environment provider or a required typed slice
    /// cannot be parsed.
    pub fn load(inputs: ConfigurationSystemInputs) -> Result<LoadedConfigurationSystem, GenericError> {
        let sources = load_local_sources(inputs.datadog_config_path, inputs.saluki_config_path)?;
        Ok(LoadedConfigurationSystem { sources })
    }
}

/// The loaded-but-not-running stage of the lifecycle.
///
/// Holds the one-time local source read as typed slices plus the raw local Datadog snapshot. It
/// exposes only typed bootstrap and diagnostic accessors; the raw snapshot is private. Calling
/// [`start_runtime`](Self::start_runtime) consumes this object by value -- the `Loaded -> Started`
/// type-state move is the compile-time statement that the bootstrap snapshot cannot become an
/// ambient runtime map.
pub struct LoadedConfigurationSystem {
    sources: LoadedSources,
}

impl LoadedConfigurationSystem {
    /// Returns the typed bootstrap allowlist view.
    ///
    /// Used to initialize logging and early telemetry before runtime authority exists.
    pub fn bootstrap(&self) -> &BootstrapConfiguration {
        &self.sources.bootstrap
    }

    /// Returns the resolved runtime authority.
    ///
    /// Decided at load time from the local Datadog `data_plane.*` keys. The binary may use this to
    /// decide whether to construct a [`RemoteAgentRegistration`] at all.
    pub fn authority(&self) -> RuntimeAuthority {
        self.sources.authority
    }

    /// Starts the runtime, consuming the loaded object by value.
    ///
    /// In [`RuntimeAuthority::LocalSnapshot`] mode this parses the retained local Datadog snapshot,
    /// translates it into the initial [`SalukiConfiguration`], builds fixed handles, and makes no
    /// Agent connection. In [`RuntimeAuthority::AgentStream`] mode it connects to the Agent, awaits
    /// the first `ConfigUpdate::Snapshot` (the initial runtime Datadog snapshot -- local Datadog
    /// sources are bootstrap-only and are not merged), translates it, builds live handles, and
    /// spawns the inbound-update task over the remaining stream.
    ///
    /// `registration` carries the identity (pid/display name/flavor/API endpoint) and the
    /// gRPC service names the design's `start_runtime(service_names)` implies; the binary fills it.
    /// In `LocalSnapshot` mode it is unused.
    ///
    /// # Errors
    ///
    /// Returns an error if the Datadog snapshot cannot be parsed or translated, or (in stream mode)
    /// if the Agent connection fails or no first snapshot arrives within
    /// `FIRST_SNAPSHOT_TIMEOUT`.
    pub async fn start_runtime(
        self, registration: RemoteAgentRegistration,
    ) -> Result<StartedConfigurationSystem, GenericError> {
        let LoadedSources {
            bootstrap,
            saluki_only,
            datadog_snapshot,
            authority,
        } = self.sources;

        match authority {
            RuntimeAuthority::LocalSnapshot => start_local(saluki_only, datadog_snapshot).await,
            RuntimeAuthority::AgentStream => start_stream(bootstrap, saluki_only, registration).await,
        }
    }
}

/// Builds a router and the started system from an initial Datadog snapshot and translated model.
///
/// Shared by both authority paths: it builds the handles (before any `spawn`), seeds the view
/// receiver, and assembles the `StartedConfigurationSystem`. The caller decides whether to spawn the
/// inbound task and attach a connection.
fn assemble(
    saluki_only: SalukiOnlyConfiguration, snapshot: serde_json::Value, initial: SalukiConfiguration,
    authority: RuntimeAuthority,
) -> (
    ConfigUpdateRouter,
    DynamicConfigHandles,
    watch::Receiver<ViewSources>,
    SalukiConfiguration,
) {
    let router = ConfigUpdateRouter::new(saluki_only, snapshot, initial.clone(), authority);
    let handles = router.handles(&initial);
    let views_rx = router.views_rx();
    (router, handles, views_rx, initial)
}

/// Starts the runtime in local-snapshot mode (no Agent connection).
async fn start_local(
    saluki_only: SalukiOnlyConfiguration, datadog_snapshot: serde_json::Value,
) -> Result<StartedConfigurationSystem, GenericError> {
    let datadog: DatadogConfiguration = serde_json::from_value(datadog_snapshot.clone())
        .map_err(|e| generic_error!("Failed to parse the local Datadog snapshot at startup: {}", e))?;
    let initial = translate(&saluki_only, &datadog)
        .map_err(|e| generic_error!("Failed to translate the local Datadog snapshot at startup: {}", e))?;

    // The `saluki-env` provider layer reads the runtime Datadog source map; in local mode that is
    // the retained local snapshot.
    let env_config = EnvConfig::from_snapshot(datadog_snapshot.clone()).await?;

    let (router, handles, views_rx, saluki) =
        assemble(saluki_only, datadog_snapshot, initial, RuntimeAuthority::LocalSnapshot);

    // No inbound stream in local mode. Spawning is a no-op (the task returns immediately) but it
    // gives the router somewhere to be owned and keeps the view sender alive for the receiver.
    let (_tx, rx) = tokio::sync::mpsc::channel(1);
    let router_task = router.spawn(rx);

    Ok(StartedConfigurationSystem {
        saluki,
        handles: Some(handles),
        attachments: None,
        env_config,
        views_rx,
        connection: None,
        router_task,
    })
}

/// Starts the runtime in stream mode (connected to the Agent).
async fn start_stream(
    bootstrap: BootstrapConfiguration, saluki_only: SalukiOnlyConfiguration, registration: RemoteAgentRegistration,
) -> Result<StartedConfigurationSystem, GenericError> {
    let client_config = RemoteAgentClientConfiguration::from_bootstrap(&bootstrap);
    let connection = connect(client_config, registration).await?;

    let mut stream = connection.config_stream();

    // The first snapshot is the initial runtime Datadog authority. Local Datadog sources are
    // bootstrap-only and are NOT merged here. Bounded wait so a silent Agent surfaces a clear error
    // instead of hanging.
    let snapshot = match tokio::time::timeout(FIRST_SNAPSHOT_TIMEOUT, next_snapshot(&mut stream)).await {
        Ok(Some(value)) => value,
        Ok(None) => {
            return Err(generic_error!(
                "Agent config stream closed before sending an initial snapshot."
            ))
        }
        Err(_) => {
            return Err(generic_error!(
                "Timed out after {:?} waiting for the initial Agent config snapshot.",
                FIRST_SNAPSHOT_TIMEOUT
            ))
        }
    };

    info!("Received initial configuration snapshot from the Datadog Agent.");

    let datadog: DatadogConfiguration = serde_json::from_value(snapshot.clone())
        .map_err(|e| generic_error!("Failed to parse the initial Agent config snapshot: {}", e))?;
    let initial = translate(&saluki_only, &datadog)
        .map_err(|e| generic_error!("Failed to translate the initial Agent config snapshot: {}", e))?;

    // The `saluki-env` provider layer reads the runtime Datadog source map; in stream mode that is
    // the first stream snapshot (the runtime authority).
    let env_config = EnvConfig::from_snapshot(snapshot.clone()).await?;

    let (router, handles, views_rx, saluki) = assemble(saluki_only, snapshot, initial, RuntimeAuthority::AgentStream);

    let attachments = connection.attachments();

    // Spawn the router over the REMAINING stream receiver; it ingests subsequent updates.
    let router_task = router.spawn(stream);

    Ok(StartedConfigurationSystem {
        saluki,
        handles: Some(handles),
        attachments: Some(attachments),
        env_config,
        views_rx,
        connection: Some(connection),
        router_task,
    })
}

/// Awaits the next `Snapshot` update from the stream, skipping any leading `Partial` updates.
///
/// The Agent sends a full snapshot first, but be defensive: drain any partial updates that arrive
/// before the first snapshot rather than treating them as the initial authority.
async fn next_snapshot(stream: &mut tokio::sync::mpsc::Receiver<ConfigUpdate>) -> Option<serde_json::Value> {
    while let Some(update) = stream.recv().await {
        match update {
            ConfigUpdate::Snapshot(value) => return Some(value),
            ConfigUpdate::Partial { .. } => {
                // Ignore partials that precede the first snapshot; the snapshot is the authority.
                continue;
            }
        }
    }
    None
}

/// The running stage of the lifecycle.
///
/// Owns the initial translated [`SalukiConfiguration`] (the caller's copy; the router holds its own
/// diff baseline), the dynamic handle bundle, the typed attachments (stream mode only), the live
/// views receiver, the shared connection (stream mode only), and the spawned router task.
pub struct StartedConfigurationSystem {
    /// The owned initial translated configuration. Returned by clone from [`saluki`](Self::saluki).
    saluki: SalukiConfiguration,

    /// The dynamic handle bundle. Taken once by [`dynamic_handles`](Self::dynamic_handles).
    handles: Option<DynamicConfigHandles>,

    /// The typed per-consumer attachments. `Some` only in stream mode.
    attachments: Option<Attachments>,

    /// The runtime Datadog source map, materialized for the `saluki-env` provider layer.
    env_config: EnvConfig,

    /// The live "current views" receiver. `/config` views are produced from its latest value.
    views_rx: watch::Receiver<ViewSources>,

    /// The shared Agent connection. `Some` only in stream mode; retained so it (and its background
    /// tasks) outlive the started system.
    #[allow(dead_code)]
    connection: Option<DatadogAgentConnection>,

    /// The spawned router task. Retained so it is not dropped (which would abort it).
    #[allow(dead_code)]
    router_task: JoinHandle<()>,
}

impl StartedConfigurationSystem {
    /// Returns an owned clone of the initial translated configuration.
    ///
    /// Topology construction and CLI setup consume this copy; the system retains its own copy and
    /// the router holds the diff baseline.
    pub fn saluki(&self) -> SalukiConfiguration {
        self.saluki.clone()
    }

    /// Returns the per-slice dynamic handle bundle.
    ///
    /// Call-once: the bundle is moved out via [`Option::take`]. `ScopedConfig<T>` is not `Clone`, so
    /// the bundle cannot be handed out twice. A second call returns `None`.
    pub fn dynamic_handles(&mut self) -> Option<DynamicConfigHandles> {
        self.handles.take()
    }

    /// Returns the runtime environment configuration for the `saluki-env` provider layer.
    ///
    /// This is a confined pass-through of the runtime Datadog source map (see [`EnvConfig`]). The
    /// `saluki-env` crate is out of scope for typed config and consumes a raw map; the binary passes
    /// `env_config.raw()` straight into its constructors without naming the raw-map type.
    pub fn env_config(&self) -> EnvConfig {
        self.env_config.clone()
    }

    /// Returns the typed attachment bundle, or `None` in local-snapshot mode.
    ///
    /// In `LocalSnapshot` mode there is no Agent connection, so there is nothing to attach to; the
    /// binary builds its Agent-backed services only when this is `Some`. This is `Option`-based
    /// rather than a fabricated empty connection.
    pub fn attachments(&self) -> Option<Attachments> {
        self.attachments.clone()
    }

    /// Returns the current `/config` views, produced live-on-request and scrubbed.
    ///
    /// Each call reads the latest fully applied [`ViewSources`] pair from the router's watch channel
    /// (never a torn mid-update state), serializes each to YAML, and scrubs the serialized string
    /// with the shared [`default_scrubber`]. `/config/raw` is the Datadog source snapshot;
    /// `/config/internal` is the translated [`SalukiConfiguration`]. This never exposes
    /// `GenericConfiguration`.
    pub fn config_views(&self) -> ConfigViews {
        let sources = self.views_rx.borrow();
        let raw = SourceConfigView::new(scrub_to_yaml(sources.raw.as_ref()));
        let internal = InternalConfigView::new(scrub_to_yaml(sources.model.as_ref()));
        ConfigViews::new(raw, internal)
    }

    /// Returns the control slice, a convenience for the topology builder.
    pub fn control(&self) -> &ControlConfiguration {
        &self.saluki.control
    }

    /// Returns a clone-able producer of the `/config` views, live-on-request.
    ///
    /// Each invocation reads the latest fully applied snapshot from the router's watch channel and
    /// produces a freshly scrubbed [`ConfigViews`]. The internal supervisor holds this producer for
    /// the lifetime of the process; it never holds a raw configuration map.
    pub fn view_producer(&self) -> ConfigViewProducer {
        let views_rx = self.views_rx.clone();
        Arc::new(move || {
            let sources = views_rx.borrow();
            let raw = SourceConfigView::new(scrub_to_yaml(sources.raw.as_ref()));
            let internal = InternalConfigView::new(scrub_to_yaml(sources.model.as_ref()));
            ConfigViews::new(raw, internal)
        })
    }
}

/// A clone-able producer of [`ConfigViews`], live-on-request from the router's retained snapshot.
pub type ConfigViewProducer = Arc<dyn Fn() -> ConfigViews + Send + Sync>;

/// Serializes a value to YAML and scrubs sensitive data from the serialized string.
///
/// Uses the shared [`default_scrubber`] (the same secret-redaction policy as the rest of the
/// system: api/app keys, tokens, URI passwords). On a serialization failure -- which should not
/// happen for these always-serializable types -- returns a short diagnostic string rather than
/// leaking anything.
fn scrub_to_yaml<T: serde::Serialize>(value: &T) -> String {
    let yaml = match serde_yaml::to_string(value) {
        Ok(s) => s,
        Err(e) => return format!("# failed to serialize config view: {e}"),
    };
    let scrubbed = default_scrubber().scrub_bytes(yaml.as_bytes());
    String::from_utf8_lossy(&scrubbed).into_owned()
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;

    use super::*;

    // A 32-hex-char dummy API key. The scrubber's non-hinted YAML replacer matches a 32-hex value
    // following a `:` separator, so this must not survive into the serialized views.
    const FAKE_API_KEY: &str = "0123456789abcdef0123456789abcdef";

    /// Writes `contents` to a temp file and returns the handle (kept alive for the file's lifetime).
    fn temp_yaml(contents: &str) -> tempfile::NamedTempFile {
        let mut f = tempfile::Builder::new()
            .suffix(".yaml")
            .tempfile()
            .expect("create temp yaml");
        f.write_all(contents.as_bytes()).expect("write temp yaml");
        f.flush().expect("flush temp yaml");
        f
    }

    fn local_inputs(datadog_yaml: &str) -> (ConfigurationSystemInputs, tempfile::NamedTempFile) {
        let file = temp_yaml(datadog_yaml);
        let inputs = ConfigurationSystemInputs {
            datadog_config_path: Some(file.path().to_path_buf()),
            saluki_config_path: None,
        };
        (inputs, file)
    }

    fn dummy_registration() -> RemoteAgentRegistration {
        RemoteAgentRegistration {
            pid: 1,
            display_name: "test".to_string(),
            flavor: "test".to_string(),
            api_endpoint: "127.0.0.1:0".to_string(),
            service_names: vec![],
        }
    }

    // (a) `load` in standalone mode yields a LoadedConfigurationSystem whose authority is
    // LocalSnapshot and whose bootstrap reflects the datadog.yaml.
    #[test]
    fn load_standalone_mode_reflects_sources() {
        let yaml = "data_plane:\n  standalone_mode: true\nlog_level: debug\n";
        let (inputs, _file) = local_inputs(yaml);

        let loaded = ConfigurationSystem::load(inputs).expect("load succeeds");

        assert_eq!(loaded.authority(), RuntimeAuthority::LocalSnapshot);
        assert_eq!(
            loaded.bootstrap().datadog.logging().log_level.as_deref(),
            Some("debug"),
            "bootstrap should reflect the datadog.yaml log_level"
        );
    }

    // remote_agent_enabled: false also forces LocalSnapshot.
    #[test]
    fn load_remote_agent_disabled_is_local() {
        let yaml = "data_plane:\n  remote_agent_enabled: false\n";
        let (inputs, _file) = local_inputs(yaml);
        let loaded = ConfigurationSystem::load(inputs).expect("load succeeds");
        assert_eq!(loaded.authority(), RuntimeAuthority::LocalSnapshot);
    }

    // Default (no flags) resolves to AgentStream.
    #[test]
    fn load_default_is_agent_stream() {
        let (inputs, _file) = local_inputs("log_level: info\n");
        let loaded = ConfigurationSystem::load(inputs).expect("load succeeds");
        assert_eq!(loaded.authority(), RuntimeAuthority::AgentStream);
    }

    // (b) start_runtime in LocalSnapshot mode yields a StartedConfigurationSystem whose saluki()
    // reflects translated values and whose config_views() are non-empty, scrubbed, and secret-free.
    #[tokio::test]
    async fn start_runtime_local_translates_and_views_are_scrubbed() {
        let yaml = format!("data_plane:\n  standalone_mode: true\nsite: us3.datadoghq.com\napi_key: {FAKE_API_KEY}\n");
        let (inputs, _file) = local_inputs(&yaml);

        let loaded = ConfigurationSystem::load(inputs).expect("load succeeds");
        assert_eq!(loaded.authority(), RuntimeAuthority::LocalSnapshot);

        let mut started = loaded
            .start_runtime(dummy_registration())
            .await
            .expect("start_runtime succeeds in local mode");

        // saluki() reflects the translated Datadog `site` value.
        let saluki = started.saluki();
        assert_eq!(
            saluki.components.forwarder.datadog.endpoint.site, "us3.datadoghq.com",
            "translated config should reflect the datadog.yaml site"
        );

        // control() is the same control slice.
        assert_eq!(started.control(), &saluki.control);

        // In local mode there is no Agent connection.
        assert!(started.attachments().is_none(), "local mode has no attachments");

        // dynamic_handles is call-once.
        assert!(started.dynamic_handles().is_some(), "first call returns the bundle");
        assert!(started.dynamic_handles().is_none(), "second call returns None");

        // config_views are non-empty, and the raw view does NOT contain the secret API key.
        let views = started.config_views();
        assert!(!views.raw.as_str().is_empty(), "raw view should be non-empty");
        assert!(!views.internal.as_str().is_empty(), "internal view should be non-empty");
        assert!(
            !views.raw.as_str().contains(FAKE_API_KEY),
            "the raw view must have the api_key scrubbed, got:\n{}",
            views.raw.as_str()
        );
        assert!(
            !views.internal.as_str().contains(FAKE_API_KEY),
            "the internal view must have the api_key scrubbed, got:\n{}",
            views.internal.as_str()
        );
    }
}
