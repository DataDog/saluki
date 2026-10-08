//! Anomaly detection domain. Carries the FIT endpoint of the isolated anomaly detection
//! process that ADP forwards scalar metrics to; forwarding is off by default.

use serde::Serialize;
use std::net::SocketAddr;
use std::path::Path;

use crate::defaults::{DEFAULT_ANOMALY_DETECTION_FORWARDING_ENABLED, DEFAULT_ANOMALY_DETECTION_IPC_ENDPOINT};

/// Resolved anomaly detection forwarding configuration.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Domain {
    /// Whether ADP forwards scalar metrics to the isolated anomaly detection process.
    ///
    /// This is a Saluki-only field, seeded from the Saluki-only source. It is absent from the
    /// Datadog Agent config schema and defaults to `false`.
    pub forwarding_enabled: bool,
    /// FIT setup address of the isolated anomaly detection process.
    ///
    /// The anomaly detection process owns the shared-memory ring and listens for this
    /// connection; ADP connects as the producer, so the process must be started first.
    /// Defaults to `unix:/tmp/aad-isolated/aad.sock`.
    pub ipc_endpoint: String,
}

impl Default for Domain {
    fn default() -> Self {
        Self {
            forwarding_enabled: DEFAULT_ANOMALY_DETECTION_FORWARDING_ENABLED,
            ipc_endpoint: DEFAULT_ANOMALY_DETECTION_IPC_ENDPOINT.to_string(),
        }
    }
}

impl Domain {
    /// Validates the FIT setup endpoint at the configuration boundary, mirroring the
    /// transport's own endpoint rules. Only validated when forwarding is enabled, so the
    /// feature can stay off without constraining the endpoint.
    pub fn validate(&self) -> Result<(), String> {
        if !self.forwarding_enabled {
            return Ok(());
        }

        let endpoint = &self.ipc_endpoint;
        if endpoint.starts_with("tcp://") || endpoint.starts_with("unix://") {
            return Err(format!(
                "anomaly_detection_ipc_endpoint `{endpoint}` uses an unsupported address syntax; use `unix:/absolute/path` or `tcp:127.0.0.1:5102`"
            ));
        }
        if let Some(address) = endpoint.strip_prefix("tcp:") {
            let address = address.parse::<SocketAddr>().map_err(|error| {
                format!("anomaly_detection_ipc_endpoint `{endpoint}` is invalid: {error}; use `tcp:127.0.0.1:5102`")
            })?;
            if !address.ip().is_loopback() || address.port() == 0 {
                return Err(
                    "anomaly_detection_ipc_endpoint must use a loopback TCP address with a nonzero port".to_string(),
                );
            }
        } else if let Some(path) = endpoint.strip_prefix("unix:") {
            if !Path::new(path).is_absolute() {
                return Err("anomaly_detection_ipc_endpoint Unix socket path must be absolute".to_string());
            }
        } else {
            return Err(format!(
                "anomaly_detection_ipc_endpoint `{endpoint}` must use `unix:/absolute/path` or `tcp:127.0.0.1:5102`"
            ));
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_forwarding_never_validates_the_endpoint() {
        let domain = Domain {
            forwarding_enabled: false,
            ipc_endpoint: "not an endpoint".to_string(),
        };
        domain
            .validate()
            .expect("forwarding is off, so the endpoint is not validated");
    }

    #[test]
    fn valid_endpoints_are_accepted() {
        for endpoint in ["unix:/tmp/aad-isolated.sock", "tcp:127.0.0.1:5102"] {
            let domain = Domain {
                forwarding_enabled: true,
                ipc_endpoint: endpoint.to_string(),
            };
            domain.validate().unwrap_or_else(|error| panic!("{endpoint}: {error}"));
        }
    }

    #[test]
    fn invalid_endpoints_are_rejected_with_the_key_name() {
        for endpoint in [
            "tcp://0.0.0.0:5102",
            "unix://tmp/aad.sock",
            "unix:relative/path.sock",
            "tcp:10.0.0.1:5102",
            "tcp:127.0.0.1:0",
            "tcp:127.0.0.1:notaport",
            "grpc://example",
        ] {
            let domain = Domain {
                forwarding_enabled: true,
                ipc_endpoint: endpoint.to_string(),
            };
            let error = domain.validate().expect_err("endpoint must be rejected");
            assert!(error.contains("anomaly_detection_ipc_endpoint"), "{endpoint}: {error}");
        }
    }
}
