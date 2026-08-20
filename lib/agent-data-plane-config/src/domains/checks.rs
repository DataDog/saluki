//! Checks domain. Carries the checks IPC endpoint; the checks metrics-encoding settings live in
//! `shared.metrics_encoding`.
// TODO: add the rest of the checks pipeline configuration as the checks pipeline is migrated.

use serde::Serialize;

use crate::defaults::DEFAULT_CHECKS_IPC_ENDPOINT;

// TODO: better name than Domain? Pipeline? Topology? BlueprintConfig?
/// Resolved checks configuration.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Domain {
    /// Address the checks pipeline exposes for IPC with the core Agent.
    ///
    /// This is a Saluki-only field, seeded from the Saluki-only source. It is absent from the
    /// Datadog Agent config schema. Defaults to `tcp://0.0.0.0:5105`.
    pub ipc_endpoint: String,
}

impl Default for Domain {
    fn default() -> Self {
        Self {
            ipc_endpoint: DEFAULT_CHECKS_IPC_ENDPOINT.to_string(),
        }
    }
}
