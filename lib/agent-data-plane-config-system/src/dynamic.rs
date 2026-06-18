//! The typed dynamic update router.
//!
//! This module is the single place inbound source configuration updates are received and turned into
//! typed, per-slice configuration updates. Components never watch string keys and never reread the
//! raw map; they hold [`ScopedConfig<T>`] handles that this router feeds.
//!
//! # Mechanism
//!
//! ```text
//! inbound source update (ConfigUpdate)
//!     -> ingest: merge into the retained source snapshot (serde_json::Value)
//!     -> apply: parse the snapshot as DatadogConfiguration
//!     -> reuse translate() (the same seed-then-drive as startup; fallible)
//!         -> on error: warn, reject the whole update, retain last-good, emit nothing
//!     -> diff each old native slice against the new native slice
//!     -> send the changed slices on their watch channels (ScopedConfig<T> wakes)
//! ```
//!
//! Diff granularity is coarse: the unit is the whole component-consumed type `T`. The router compares
//! the previous slice against the freshly translated slice and sends only if any field changed.
//!
//! # Live vs fixed
//!
//! Whether a handle is [`ScopedConfig::Live`] or [`ScopedConfig::Fixed`] is decided by the
//! [`RuntimeAuthority`] the router was constructed with, never by the component:
//!
//! - [`RuntimeAuthority::AgentStream`]: inbound updates exist, so each handle is `Live` backed by a
//!   watch receiver. An async task (see [`ConfigUpdateRouter::spawn`]) owns the senders and applies
//!   inbound updates.
//! - [`RuntimeAuthority::LocalSnapshot`]: there is no inbound stream, so each handle is `Fixed` and
//!   no task runs.
//!
//! # Sender/receiver ownership
//!
//! The watch senders must outlive the handles. [`ConfigUpdateRouter::handles`] (and the per-slice
//! `scoped_*` accessors) hand out *receivers* derived from the router's senders. The router itself
//! retains the senders. [`ConfigUpdateRouter::spawn`] then *consumes* the router by value, moving the
//! senders into the spawned task; the receivers handed out earlier stay alive as long as the task
//! lives. So handles must be built (via `handles`) before `spawn` is called.

use std::sync::Arc;

use agent_data_plane_config::{RuntimeAuthority, SalukiConfiguration, SalukiOnlyConfiguration};
use datadog_agent_config::DatadogConfiguration;
use saluki_component_config::dogstatsd::{
    DogStatsDDebugLogConfig, DogStatsDPostAggregateFilterConfig, DogStatsDPrefixFilterConfig, TagFilterlistConfig,
};
use saluki_component_config::forwarder::{DatadogForwarderConfig, MrfConfig};
use saluki_component_config::ScopedConfig;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tracing::warn;

use crate::translate::translate;

/// The pair of snapshots `/config` views are produced from.
///
/// This is the router's shared "current views" state: the latest fully applied raw Datadog source
/// snapshot and the matching translated [`SalukiConfiguration`]. The router updates this atomically
/// on every accepted update via a [`watch`] channel, so a view producer always observes a
/// consistent (never torn mid-update) pair. Both are wrapped in [`Arc`] so a reader can cheaply
/// clone the latest pair out of the channel without cloning the whole snapshot.
///
/// In [`RuntimeAuthority::LocalSnapshot`] mode no task runs, so this stays at the initial pair for
/// the process lifetime -- correct, since there are no runtime updates.
#[derive(Clone)]
pub struct ViewSources {
    /// The retained raw Datadog source snapshot. Serialized + scrubbed for `/config/raw`.
    pub raw: Arc<serde_json::Value>,

    /// The current translated model. Serialized + scrubbed for `/config/internal`.
    pub model: Arc<SalukiConfiguration>,
}

/// The bundle of per-slice dynamic configuration handles.
///
/// Returned by `started.dynamic_handles()`. Each field is a [`ScopedConfig<T>`] that topology passes
/// to the one component that needs that dynamic slice. Whether a given handle is live or fixed is a
/// deployment detail (see the module docs); the component sees only the typed handle.
pub struct DynamicConfigHandles {
    /// The Datadog forwarder slice. Consumed latest-read for API-key refresh.
    pub forwarder: ScopedConfig<DatadogForwarderConfig>,

    /// The multi-region failover slice.
    pub multi_region_failover: ScopedConfig<MrfConfig>,

    /// The DogStatsD prefix/name filter slice.
    pub prefix_filter: ScopedConfig<DogStatsDPrefixFilterConfig>,

    /// The DogStatsD metric-tag filterlist slice.
    pub tag_filterlist: ScopedConfig<TagFilterlistConfig>,

    /// The DogStatsD post-aggregation metric filter slice.
    pub post_aggregate_filter: ScopedConfig<DogStatsDPostAggregateFilterConfig>,

    /// The DogStatsD debug-log slice.
    pub debug_log: ScopedConfig<DogStatsDDebugLogConfig>,

    /// The dynamic log level (the Datadog `log_level` string).
    pub log_level: ScopedConfig<String>,
}

/// The watch senders the router owns, one per dynamic slice.
///
/// Held by the router and moved into the spawned task. The receivers handed out via the `scoped_*`
/// accessors keep these channels alive.
struct Senders {
    forwarder: watch::Sender<DatadogForwarderConfig>,
    multi_region_failover: watch::Sender<MrfConfig>,
    prefix_filter: watch::Sender<DogStatsDPrefixFilterConfig>,
    tag_filterlist: watch::Sender<TagFilterlistConfig>,
    post_aggregate_filter: watch::Sender<DogStatsDPostAggregateFilterConfig>,
    debug_log: watch::Sender<DogStatsDDebugLogConfig>,
    log_level: watch::Sender<String>,
}

/// Routes inbound source updates into typed per-slice [`ScopedConfig<T>`] updates.
///
/// The router owns one [`watch::Sender`] per dynamic slice, the retained raw source snapshot, the
/// Saluki-only seed source, and the last-good [`SalukiConfiguration`] used as the diff baseline.
///
/// See the module docs for the full lifecycle and sender/receiver ownership rules.
pub struct ConfigUpdateRouter {
    /// The Saluki-only seed source. Re-seeds the translator on every retranslation, exactly as at
    /// startup.
    saluki_only: SalukiOnlyConfiguration,

    /// The retained raw Datadog source snapshot. Inbound updates are merged into this before
    /// retranslation; it is also what `/config/raw` would serialize.
    snapshot: serde_json::Value,

    /// The last successfully translated configuration. The diff baseline: each accepted update is
    /// diffed against this, and a rejected update leaves it untouched.
    last_good: SalukiConfiguration,

    /// Whether inbound updates exist. `AgentStream` produces live handles and runs a task;
    /// `LocalSnapshot` produces fixed handles and runs no task.
    authority: RuntimeAuthority,

    /// The per-slice watch senders.
    senders: Senders,

    /// The shared "current views" state. Seeded in [`new`](Self::new) and updated on every accepted
    /// [`apply`](Self::apply). `/config` views are produced live-on-request from the latest value.
    views_tx: watch::Sender<ViewSources>,
}

/// Replaces `cur` with `next` if they differ, reporting whether a change occurred.
///
/// Used as the `send_if_modified` closure body: `watch::Sender::send_if_modified` notifies receivers
/// only when this returns `true`, giving the coarse per-slice diff (notify only when the whole slice
/// changed).
fn replace_if_changed<T: PartialEq>(cur: &mut T, next: T) -> bool {
    if *cur != next {
        *cur = next;
        true
    } else {
        false
    }
}

impl ConfigUpdateRouter {
    /// Creates a router seeded with the initial translated configuration.
    ///
    /// `saluki_only` is the seed source reused on every retranslation. `initial_snapshot` is the raw
    /// Datadog source snapshot the first translation was produced from (retained for merging inbound
    /// updates). `initial` is the first translated [`SalukiConfiguration`], stored as the diff
    /// baseline and used to seed each watch channel. `authority` decides live-vs-fixed handles.
    pub fn new(
        saluki_only: SalukiOnlyConfiguration, initial_snapshot: serde_json::Value, initial: SalukiConfiguration,
        authority: RuntimeAuthority,
    ) -> Self {
        let log_level = initial_log_level(&initial_snapshot);
        let views_tx = watch::Sender::new(ViewSources {
            raw: Arc::new(initial_snapshot.clone()),
            model: Arc::new(initial.clone()),
        });
        let senders = Senders {
            forwarder: watch::Sender::new(initial.components.forwarder.datadog.clone()),
            multi_region_failover: watch::Sender::new(initial.components.metrics.multi_region_failover.clone()),
            prefix_filter: watch::Sender::new(initial.components.dogstatsd.prefix_filter.clone()),
            tag_filterlist: watch::Sender::new(initial.components.dogstatsd.tag_filterlist.clone()),
            post_aggregate_filter: watch::Sender::new(initial.components.dogstatsd.post_aggregate_filter.clone()),
            debug_log: watch::Sender::new(initial.components.dogstatsd.debug_log.clone()),
            log_level: watch::Sender::new(log_level),
        };

        Self {
            saluki_only,
            snapshot: initial_snapshot,
            last_good: initial,
            authority,
            senders,
            views_tx,
        }
    }

    /// Returns a receiver of the shared "current views" state.
    ///
    /// `start_runtime` keeps this receiver in `StartedConfigurationSystem` and produces `/config`
    /// views from its latest value. The receiver outlives the router (the sender is moved into the
    /// spawned task in stream mode, and dropped harmlessly in local mode where no task runs).
    pub fn views_rx(&self) -> watch::Receiver<ViewSources> {
        self.views_tx.subscribe()
    }

    /// Whether this router produces live handles (it received an inbound stream).
    fn is_live(&self) -> bool {
        matches!(self.authority, RuntimeAuthority::AgentStream)
    }

    /// Ingests one inbound source update and applies it.
    ///
    /// Merges the update into the retained source snapshot, then calls [`apply`](Self::apply):
    ///
    /// - [`ConfigUpdate::Snapshot`] replaces the retained snapshot wholesale.
    /// - [`ConfigUpdate::Partial`] is merged into the retained snapshot by dotted-path upsert (the
    ///   same merge the raw loader uses), creating intermediate objects as needed.
    ///
    /// The merge mutates the snapshot regardless of whether translation later succeeds; a rejected
    /// translation leaves `last_good` and the watch channels untouched, but the retained snapshot
    /// reflects the merged inbound bytes (matching the design: merge into the retained source
    /// snapshot, then parse).
    ///
    /// [`ConfigUpdate::Snapshot`]: saluki_config_tools::dynamic::ConfigUpdate::Snapshot
    /// [`ConfigUpdate::Partial`]: saluki_config_tools::dynamic::ConfigUpdate::Partial
    pub fn ingest(&mut self, update: saluki_config_tools::dynamic::ConfigUpdate) {
        use saluki_config_tools::dynamic::ConfigUpdate;

        match update {
            ConfigUpdate::Snapshot(new_state) => {
                self.snapshot = new_state;
            }
            ConfigUpdate::Partial { key, value } => {
                saluki_config_tools::upsert(&mut self.snapshot, &key, value);
            }
        }

        let snapshot = std::mem::take(&mut self.snapshot);
        self.apply(&snapshot);
        self.snapshot = snapshot;
    }

    /// Retranslates from a raw snapshot and routes any changed slices.
    ///
    /// Parses `raw_snapshot` as a [`DatadogConfiguration`], reuses [`translate`] (the same
    /// seed-then-drive as startup), diffs each dynamic slice against the last-good configuration, and
    /// sends the changed slices on their watch channels. On any parse or translation error, it logs a
    /// warning, rejects the whole update, and retains the last-good configuration (no slice is sent).
    pub fn apply(&mut self, raw_snapshot: &serde_json::Value) {
        let datadog: DatadogConfiguration = match serde_json::from_value(raw_snapshot.clone()) {
            Ok(c) => c,
            Err(e) => {
                warn!(error = %e, "rejecting update; keeping last-good config");
                return;
            }
        };

        let next = match translate(&self.saluki_only, &datadog) {
            Ok(next) => next,
            Err(e) => {
                warn!(error = %e, "rejecting update; keeping last-good config");
                return;
            }
        };

        // One coarse per-slice diff per dynamic slice: notify only when the whole slice changed.
        self.senders
            .forwarder
            .send_if_modified(|cur| replace_if_changed(cur, next.components.forwarder.datadog.clone()));
        self.senders
            .multi_region_failover
            .send_if_modified(|cur| replace_if_changed(cur, next.components.metrics.multi_region_failover.clone()));
        self.senders
            .prefix_filter
            .send_if_modified(|cur| replace_if_changed(cur, next.components.dogstatsd.prefix_filter.clone()));
        self.senders
            .tag_filterlist
            .send_if_modified(|cur| replace_if_changed(cur, next.components.dogstatsd.tag_filterlist.clone()));
        self.senders
            .post_aggregate_filter
            .send_if_modified(|cur| replace_if_changed(cur, next.components.dogstatsd.post_aggregate_filter.clone()));
        self.senders
            .debug_log
            .send_if_modified(|cur| replace_if_changed(cur, next.components.dogstatsd.debug_log.clone()));

        // `log_level` has no native-slice home (it is consumed by the logging layer, not a component
        // group), so it is routed as a bare `String` extracted from the parsed Datadog source rather
        // than from the translated model.
        self.senders
            .log_level
            .send_if_modified(|cur| replace_if_changed(cur, datadog.log_level.clone()));

        // Publish the new (raw, model) pair atomically so `/config` views regenerate from the
        // latest fully applied snapshot. This is sent only on an accepted update, so a rejected
        // update leaves the views at the last-good pair.
        let _ = self.views_tx.send(ViewSources {
            raw: Arc::new(raw_snapshot.clone()),
            model: Arc::new(next.clone()),
        });

        self.last_good = next;
    }

    /// Returns the forwarder handle, live if the router is wired to a stream, else fixed.
    pub fn scoped_forwarder(&self, initial: DatadogForwarderConfig) -> ScopedConfig<DatadogForwarderConfig> {
        if self.is_live() {
            ScopedConfig::live(initial, self.senders.forwarder.subscribe())
        } else {
            ScopedConfig::fixed(initial)
        }
    }

    /// Returns the multi-region failover handle, live if wired to a stream, else fixed.
    pub fn scoped_multi_region_failover(&self, initial: MrfConfig) -> ScopedConfig<MrfConfig> {
        if self.is_live() {
            ScopedConfig::live(initial, self.senders.multi_region_failover.subscribe())
        } else {
            ScopedConfig::fixed(initial)
        }
    }

    /// Returns the DogStatsD prefix-filter handle, live if wired to a stream, else fixed.
    pub fn scoped_prefix_filter(
        &self, initial: DogStatsDPrefixFilterConfig,
    ) -> ScopedConfig<DogStatsDPrefixFilterConfig> {
        if self.is_live() {
            ScopedConfig::live(initial, self.senders.prefix_filter.subscribe())
        } else {
            ScopedConfig::fixed(initial)
        }
    }

    /// Returns the DogStatsD tag-filterlist handle, live if wired to a stream, else fixed.
    pub fn scoped_tag_filterlist(&self, initial: TagFilterlistConfig) -> ScopedConfig<TagFilterlistConfig> {
        if self.is_live() {
            ScopedConfig::live(initial, self.senders.tag_filterlist.subscribe())
        } else {
            ScopedConfig::fixed(initial)
        }
    }

    /// Returns the DogStatsD post-aggregate-filter handle, live if wired to a stream, else fixed.
    pub fn scoped_post_aggregate_filter(
        &self, initial: DogStatsDPostAggregateFilterConfig,
    ) -> ScopedConfig<DogStatsDPostAggregateFilterConfig> {
        if self.is_live() {
            ScopedConfig::live(initial, self.senders.post_aggregate_filter.subscribe())
        } else {
            ScopedConfig::fixed(initial)
        }
    }

    /// Returns the DogStatsD debug-log handle, live if wired to a stream, else fixed.
    pub fn scoped_debug_log(&self, initial: DogStatsDDebugLogConfig) -> ScopedConfig<DogStatsDDebugLogConfig> {
        if self.is_live() {
            ScopedConfig::live(initial, self.senders.debug_log.subscribe())
        } else {
            ScopedConfig::fixed(initial)
        }
    }

    /// Returns the dynamic log-level handle, live if wired to a stream, else fixed.
    pub fn scoped_log_level(&self, initial: String) -> ScopedConfig<String> {
        if self.is_live() {
            ScopedConfig::live(initial, self.senders.log_level.subscribe())
        } else {
            ScopedConfig::fixed(initial)
        }
    }

    /// Builds the full [`DynamicConfigHandles`] bundle from a translated configuration's slices.
    ///
    /// The `initial` value for each handle is taken from `source` (the freshly translated
    /// [`SalukiConfiguration`]); the router only adds the receiver when live. `start_runtime` calls
    /// this once, before [`spawn`](Self::spawn), so the receivers are created while the senders still
    /// live in the router.
    ///
    /// The `log_level` initial is read from the retained source snapshot (it has no native-slice
    /// home), so `source` does not carry it.
    pub fn handles(&self, source: &SalukiConfiguration) -> DynamicConfigHandles {
        DynamicConfigHandles {
            forwarder: self.scoped_forwarder(source.components.forwarder.datadog.clone()),
            multi_region_failover: self
                .scoped_multi_region_failover(source.components.metrics.multi_region_failover.clone()),
            prefix_filter: self.scoped_prefix_filter(source.components.dogstatsd.prefix_filter.clone()),
            tag_filterlist: self.scoped_tag_filterlist(source.components.dogstatsd.tag_filterlist.clone()),
            post_aggregate_filter: self
                .scoped_post_aggregate_filter(source.components.dogstatsd.post_aggregate_filter.clone()),
            debug_log: self.scoped_debug_log(source.components.dogstatsd.debug_log.clone()),
            log_level: self.scoped_log_level(self.senders.log_level.borrow().clone()),
        }
    }

    /// Spawns the inbound update task, consuming the router.
    ///
    /// In stream mode ([`RuntimeAuthority::AgentStream`]) the task loops receiving [`ConfigUpdate`]s,
    /// calling [`ingest`](Self::ingest) on each, until the channel closes. Consuming the router moves
    /// the watch senders into the task, so they outlive every handle previously handed out via
    /// [`handles`](Self::handles).
    ///
    /// In local mode ([`RuntimeAuthority::LocalSnapshot`]) there is no inbound stream: the handles are
    /// all `Fixed`, so this task simply drops the receiver and returns. (If local-mode file watching
    /// is added later, the same task shape applies with a file-change trigger.) `start_runtime` need
    /// not spawn at all in local mode, but `spawn` tolerates it.
    ///
    /// Handles MUST be built (via [`handles`](Self::handles)) before calling `spawn`, because `spawn`
    /// takes the router by value.
    ///
    /// [`ConfigUpdate`]: saluki_config_tools::dynamic::ConfigUpdate
    pub fn spawn(mut self, mut updates: mpsc::Receiver<saluki_config_tools::dynamic::ConfigUpdate>) -> JoinHandle<()> {
        tokio::spawn(async move {
            if !self.is_live() {
                return;
            }
            while let Some(update) = updates.recv().await {
                self.ingest(update);
            }
        })
    }
}

/// Extracts the `log_level` string from a raw Datadog source snapshot.
///
/// Reads the top-level `log_level` key directly. Falls back to the Datadog default (`"info"`) when
/// the key is missing or not a string, mirroring the `DatadogConfiguration` default.
fn initial_log_level(snapshot: &serde_json::Value) -> String {
    snapshot
        .get("log_level")
        .and_then(|v| v.as_str())
        .unwrap_or("info")
        .to_string()
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use saluki_config_tools::dynamic::ConfigUpdate;
    use tokio::time::timeout;

    use super::*;

    fn router(authority: RuntimeAuthority, snapshot: serde_json::Value) -> ConfigUpdateRouter {
        let saluki_only = SalukiOnlyConfiguration::default();
        let datadog: DatadogConfiguration = serde_json::from_value(snapshot.clone()).expect("valid snapshot");
        let initial = translate(&saluki_only, &datadog).expect("initial translation succeeds");
        ConfigUpdateRouter::new(saluki_only, snapshot, initial, authority)
    }

    // A changed slice triggers a send, and `ScopedConfig::current()` reflects the new value.
    #[tokio::test]
    async fn changed_slice_notifies_handle() {
        let mut r = router(RuntimeAuthority::AgentStream, serde_json::json!({}));
        let mut handle = r.scoped_log_level("info".to_string());
        assert!(handle.is_live());
        assert_eq!(handle.current(), "info");

        r.apply(&serde_json::json!({ "log_level": "debug" }));

        handle.changed().await;
        assert_eq!(handle.current(), "debug");
    }

    // An unchanged slice does NOT notify its handle.
    #[tokio::test]
    async fn unchanged_slice_does_not_notify() {
        let mut r = router(
            RuntimeAuthority::AgentStream,
            serde_json::json!({ "log_level": "info" }),
        );
        let mut handle = r.scoped_prefix_filter(r.last_good.components.dogstatsd.prefix_filter.clone());

        // A snapshot that changes only `log_level` must not wake the prefix-filter handle.
        r.apply(&serde_json::json!({ "log_level": "warn" }));

        let result = timeout(Duration::from_millis(20), handle.changed()).await;
        assert!(result.is_err(), "an unchanged slice must not notify its handle");
    }

    // A malformed snapshot is rejected and the last-good value is retained.
    #[tokio::test]
    async fn malformed_snapshot_is_rejected() {
        let mut r = router(
            RuntimeAuthority::AgentStream,
            serde_json::json!({ "log_level": "info" }),
        );
        let mut handle = r.scoped_log_level("info".to_string());

        // `log_level` must deserialize as a string; an array is malformed for `DatadogConfiguration`.
        r.apply(&serde_json::json!({ "log_level": [1, 2, 3] }));

        // The handle still shows the old value, and `changed()` never resolves (nothing was sent).
        assert_eq!(handle.current(), "info");
        let result = timeout(Duration::from_millis(20), handle.changed()).await;
        assert!(result.is_err(), "a rejected update must not notify any handle");
    }

    // Fixed handles in local mode never resolve `changed()`.
    #[tokio::test]
    async fn fixed_handle_never_resolves_in_local_mode() {
        let mut r = router(
            RuntimeAuthority::LocalSnapshot,
            serde_json::json!({ "log_level": "info" }),
        );
        let mut handle = r.scoped_log_level("info".to_string());
        assert!(!handle.is_live());

        // Even an accepted, changing update cannot wake a fixed handle.
        r.apply(&serde_json::json!({ "log_level": "debug" }));

        let result = timeout(Duration::from_millis(20), handle.changed()).await;
        assert!(result.is_err(), "a Fixed handle must never resolve changed()");
    }

    // `ingest` merges a `Partial` into the retained snapshot by dotted-path upsert.
    #[tokio::test]
    async fn ingest_partial_merges_into_snapshot() {
        let mut r = router(
            RuntimeAuthority::AgentStream,
            serde_json::json!({ "log_level": "info" }),
        );
        let mut handle = r.scoped_log_level("info".to_string());

        r.ingest(ConfigUpdate::Partial {
            key: "log_level".to_string(),
            value: serde_json::json!("error"),
        });

        handle.changed().await;
        assert_eq!(handle.current(), "error");
        assert_eq!(r.snapshot.get("log_level").and_then(|v| v.as_str()), Some("error"));
    }
}
