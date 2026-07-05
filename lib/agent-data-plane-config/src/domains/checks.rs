//! Checks domain. Carries the checks IPC endpoint; the checks metrics-encoding settings live in
//! `shared.metrics_encoding`.
// TODO: add the rest of the checks pipeline configuration as the checks pipeline is migrated.

use serde::Serialize;

use crate::control::ListenAddress;

// TODO: better name than Domain? Pipeline? Topology? BlueprintConfig?
/// Resolved checks configuration.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Domain {
    /// Address the checks pipeline exposes for IPC with the core Agent. This is a Saluki-only field,
    /// seeded from the Saluki-only source; it is absent from the Datadog Agent config schema.
    pub ipc_endpoint: ListenAddress,
}

impl Default for Domain {
    fn default() -> Self {
        Self {
            // Saluki-schema-only knob: absent from the source, the checks IPC source listens on TCP
            // port 5105 across all interfaces (the `tcp://0.0.0.0:5105` form of `any_tcp(5105)`).
            ipc_endpoint: ListenAddress("tcp://0.0.0.0:5105".to_string()),
        }
    }
}
