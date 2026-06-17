//! Component-native configuration for the workload domain.
//!
//! Most of the workload domain in `saluki-components` is expressed through injected runtime state --
//! `workload_provider` handles (`Arc<dyn WorkloadProvider + Send + Sync>`) and capture-entity
//! resolvers -- which the design explicitly classifies as non-config component-internal state that
//! does not move into the leaf crate.
//!
//! What does move here is the genuine configuration that tunes workload/environment collection: the
//! remote-agent string interner budget, host-tag collection timing, an explicit hostname override,
//! and the CRI client timeouts. These are plain resolved values, with no injected clients.

use std::time::Duration;

/// Component-native configuration for the workload/environment domain.
///
/// Carries the configurable knobs that tune workload metadata and host-tag collection. Injected
/// `workload_provider` handles and capture-entity resolvers are runtime-injected component state and
/// are intentionally excluded.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct WorkloadConfig {
    /// Size, in bytes, of the string interner used by the remote-agent workload provider.
    ///
    /// Bounds the memory pre-allocated for interning workload entity strings (tags, entity IDs).
    /// Defaults to 0 (the component applies its own default when unset).
    pub remote_agent_string_interner_size_bytes: usize,

    /// The expected duration of a host-tags collection cycle.
    ///
    /// Used to size and pace periodic host-tag refresh. Defaults to 15 minutes.
    pub host_tags_expected_duration: Duration,

    /// An explicit hostname override for the workload/environment provider.
    ///
    /// When set, this hostname is used in place of any detected hostname. Defaults to unset, which
    /// triggers hostname detection.
    pub hostname: Option<String>,

    /// The connection timeout, in seconds, for the Container Runtime Interface (CRI) client.
    ///
    /// Corresponds to the Datadog `cri_connection_timeout` key. Defaults to 1 second.
    pub cri_connection_timeout_secs: u64,

    /// The query timeout, in seconds, for the Container Runtime Interface (CRI) client.
    ///
    /// Corresponds to the Datadog `cri_query_timeout` key. Defaults to 5 seconds.
    pub cri_query_timeout_secs: u64,
}

impl Default for WorkloadConfig {
    fn default() -> Self {
        Self {
            remote_agent_string_interner_size_bytes: 0,
            host_tags_expected_duration: Duration::from_secs(15 * 60),
            hostname: None,
            cri_connection_timeout_secs: 1,
            cri_query_timeout_secs: 5,
        }
    }
}
