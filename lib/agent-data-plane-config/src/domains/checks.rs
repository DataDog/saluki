//! Checks domain. Carries the checks IPC endpoint; the checks metrics-encoding settings live in
//! `shared.metrics_encoding`.
// TODO: add the rest of the checks pipeline configuration as the checks pipeline is migrated.

use saluki_io::net::ListenAddress;
use serde::Serialize;

use crate::defaults::FAKE_LISTEN_ADDRESS;

// TODO: better name than Domain? Pipeline? Topology? BlueprintConfig?
/// Resolved checks configuration.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Domain {
    /// Address the checks pipeline exposes for IPC with the core Agent. This is a Saluki-only field,
    /// seeded from the Saluki-only source; it is absent from the Datadog Agent config schema.
    pub ipc_endpoint: ListenAddress,
}

impl Default for Domain {
    // Saluki-only seeding overwrites this placeholder before the configuration is published.
    fn default() -> Self {
        Self {
            ipc_endpoint: FAKE_LISTEN_ADDRESS,
        }
    }
}
