//! Checks domain. Carries the checks IPC endpoint; the checks metrics-encoding settings live in
//! `shared.metrics_encoding`.
// TODO: add the rest of the checks pipeline configuration as the checks pipeline is migrated.

use serde::Serialize;
use std::net::SocketAddr;
use std::path::Path;

use crate::defaults::{DEFAULT_CHECKS_IPC_ENDPOINT, DEFAULT_CHECKS_IPC_RING_CAPACITY_BYTES};

// TODO: better name than Domain? Pipeline? Topology? BlueprintConfig?
/// Resolved checks configuration.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Domain {
    /// FIT setup address for check telemetry from the Agent Check Runner.
    ///
    /// This is a Saluki-only field, seeded from the Saluki-only source. It is absent from the
    /// Datadog Agent config schema. Defaults to `tcp:127.0.0.1:5101`.
    pub ipc_endpoint: String,
    /// FIT ring capacity in bytes. Defaults to 1 MiB. Must be a multiple of eight between
    /// 16 bytes and 1 GiB. Increase it when bursts exceed the consumer's throughput;
    /// doing so reserves more shared memory but reduces producer drops.
    pub ipc_ring_capacity_bytes: usize,
}

impl Default for Domain {
    fn default() -> Self {
        Self {
            ipc_endpoint: DEFAULT_CHECKS_IPC_ENDPOINT.to_string(),
            ipc_ring_capacity_bytes: DEFAULT_CHECKS_IPC_RING_CAPACITY_BYTES,
        }
    }
}

impl Domain {
    /// Validates the FIT setup endpoint and shared-memory capacity at the configuration boundary.
    pub fn validate(&self) -> Result<(), String> {
        let endpoint = &self.ipc_endpoint;
        if endpoint.starts_with("tcp://") || endpoint.starts_with("unix://") {
            return Err(format!(
                "checks_ipc_endpoint `{endpoint}` uses the old gRPC address syntax; use `tcp:127.0.0.1:5101` or `unix:/absolute/path` for FIT"
            ));
        }
        if let Some(address) = endpoint.strip_prefix("tcp:") {
            let address = address.parse::<SocketAddr>().map_err(|error| {
                format!("checks_ipc_endpoint `{endpoint}` is invalid: {error}; use `tcp:127.0.0.1:5101`")
            })?;
            if !address.ip().is_loopback() || address.port() == 0 {
                return Err("checks_ipc_endpoint must use a loopback TCP address with a nonzero port".to_string());
            }
        } else if let Some(path) = endpoint.strip_prefix("unix:") {
            if !Path::new(path).is_absolute() {
                return Err("checks_ipc_endpoint Unix socket path must be absolute".to_string());
            }
        } else {
            return Err(format!(
                "checks_ipc_endpoint `{endpoint}` must use `tcp:127.0.0.1:5101` or `unix:/absolute/path`; gRPC addresses such as `tcp://...` are no longer supported"
            ));
        }

        let capacity = self.ipc_ring_capacity_bytes;
        if !(16..=(1 << 30)).contains(&capacity) || !capacity.is_multiple_of(8) {
            return Err(
                "checks_ipc_ring_capacity_bytes must be a multiple of 8 from 16 bytes through 1 GiB".to_string(),
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_configuration_rejects_legacy_address_and_invalid_capacity() {
        let mut config = Domain::default();
        config.validate().unwrap();
        config.ipc_endpoint = "tcp://0.0.0.0:5105".to_string();
        assert!(config.validate().unwrap_err().contains("gRPC"));
        config.ipc_endpoint = "unix:/tmp/checks-fit.sock".to_string();
        config.validate().unwrap();
        config.ipc_ring_capacity_bytes = 17;
        assert!(config.validate().unwrap_err().contains("capacity"));
    }
}
