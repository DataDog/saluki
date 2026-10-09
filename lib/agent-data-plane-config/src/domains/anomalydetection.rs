//! Anomaly detection domain. Carries the FIT endpoints of the isolated anomaly detection
//! process: the one ADP forwards scalar metrics to, and the one it subscribes to for
//! anomaly events. Both are off by default.

use serde::Serialize;
use std::net::SocketAddr;
use std::path::Path;

use crate::defaults::{
    DEFAULT_ANOMALY_DETECTION_EVENTS_ENABLED, DEFAULT_ANOMALY_DETECTION_EVENTS_ENDPOINT,
    DEFAULT_ANOMALY_DETECTION_FORWARDING_ENABLED, DEFAULT_ANOMALY_DETECTION_IPC_ENDPOINT,
};

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
    /// Whether ADP subscribes to the anomaly events the isolated process publishes.
    ///
    /// This is a Saluki-only field, seeded from the Saluki-only source. It is absent from the
    /// Datadog Agent config schema and defaults to `false`.
    pub events_enabled: bool,
    /// FIT broadcast address the isolated anomaly detection process publishes events on.
    ///
    /// The anomaly detection process owns this endpoint as the publisher and keeps it open
    /// for late subscribers; ADP connects as a subscriber, and can start before the process
    /// exists because it keeps retrying. Defaults to `unix:/tmp/aad-isolated/events.sock`.
    pub events_endpoint: String,
}

impl Default for Domain {
    fn default() -> Self {
        Self {
            forwarding_enabled: DEFAULT_ANOMALY_DETECTION_FORWARDING_ENABLED,
            ipc_endpoint: DEFAULT_ANOMALY_DETECTION_IPC_ENDPOINT.to_string(),
            events_enabled: DEFAULT_ANOMALY_DETECTION_EVENTS_ENABLED,
            events_endpoint: DEFAULT_ANOMALY_DETECTION_EVENTS_ENDPOINT.to_string(),
        }
    }
}

impl Domain {
    /// Validates the FIT setup endpoints at the configuration boundary, mirroring the
    /// transport's own endpoint rules. Only enabled features are validated, so a feature
    /// can stay off without constraining its endpoint.
    pub fn validate(&self) -> Result<(), String> {
        if self.forwarding_enabled {
            validate_endpoint("anomaly_detection_ipc_endpoint", &self.ipc_endpoint)?;
        }
        if self.events_enabled {
            validate_endpoint("anomaly_detection_events_endpoint", &self.events_endpoint)?;
        }
        Ok(())
    }
}

/// Validates one FIT address in either supported form.
fn validate_endpoint(key: &str, endpoint: &str) -> Result<(), String> {
    if endpoint.starts_with("tcp://") || endpoint.starts_with("unix://") {
        return Err(format!(
            "{key} `{endpoint}` uses an unsupported address syntax; use `unix:/absolute/path` or `tcp:127.0.0.1:5102`"
        ));
    }
    if let Some(address) = endpoint.strip_prefix("tcp:") {
        let address = address
            .parse::<SocketAddr>()
            .map_err(|error| format!("{key} `{endpoint}` is invalid: {error}; use `tcp:127.0.0.1:5102`"))?;
        if !address.ip().is_loopback() || address.port() == 0 {
            return Err(format!("{key} must use a loopback TCP address with a nonzero port"));
        }
    } else if let Some(path) = endpoint.strip_prefix("unix:") {
        if !Path::new(path).is_absolute() {
            return Err(format!("{key} Unix socket path must be absolute"));
        }
    } else {
        return Err(format!(
            "{key} `{endpoint}` must use `unix:/absolute/path` or `tcp:127.0.0.1:5102`"
        ));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_forwarding_never_validates_the_endpoint() {
        let domain = Domain {
            ipc_endpoint: "not an endpoint".to_string(),
            ..Domain::default()
        };
        domain
            .validate()
            .expect("forwarding is off, so the endpoint is not validated");
    }

    #[test]
    fn disabled_events_never_validate_the_endpoint() {
        let domain = Domain {
            events_endpoint: "not an endpoint".to_string(),
            ..Domain::default()
        };
        domain
            .validate()
            .expect("events are off, so the endpoint is not validated");
    }

    #[test]
    fn valid_endpoints_are_accepted() {
        for endpoint in ["unix:/tmp/aad-isolated.sock", "tcp:127.0.0.1:5102"] {
            let domain = Domain {
                forwarding_enabled: true,
                ipc_endpoint: endpoint.to_string(),
                ..Domain::default()
            };
            domain.validate().unwrap_or_else(|error| panic!("{endpoint}: {error}"));
        }
    }

    #[test]
    fn valid_event_endpoints_are_accepted() {
        for endpoint in ["unix:/tmp/aad-isolated/events.sock", "tcp:127.0.0.1:5103"] {
            let domain = Domain {
                events_enabled: true,
                events_endpoint: endpoint.to_string(),
                ..Domain::default()
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
                ..Domain::default()
            };
            let error = domain.validate().expect_err("endpoint must be rejected");
            assert!(error.contains("anomaly_detection_ipc_endpoint"), "{endpoint}: {error}");
        }
    }

    #[test]
    fn invalid_event_endpoints_are_rejected_with_the_events_key_name() {
        for endpoint in [
            "tcp://0.0.0.0:5103",
            "unix://tmp/events.sock",
            "unix:events.sock",
            "events.sock",
        ] {
            let domain = Domain {
                events_enabled: true,
                events_endpoint: endpoint.to_string(),
                ..Domain::default()
            };
            let error = domain.validate().expect_err("endpoint must be rejected");
            assert!(
                error.contains("anomaly_detection_events_endpoint"),
                "{endpoint}: {error}"
            );
        }
    }

    #[test]
    fn both_endpoints_are_validated_independently() {
        let domain = Domain {
            forwarding_enabled: true,
            ipc_endpoint: "unix:/tmp/aad.sock".to_string(),
            events_enabled: true,
            events_endpoint: "unix:events.sock".to_string(),
        };
        let error = domain.validate().expect_err("the events endpoint must be rejected");
        assert!(error.contains("anomaly_detection_events_endpoint"), "{error}");
    }
}
