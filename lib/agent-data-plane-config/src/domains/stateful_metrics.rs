//! Experimental stateful metrics delivery.

use serde::Serialize;

/// Configuration for the experimental stateful series client.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Domain {
    /// Plaintext gRPC intake endpoint. Unset by default, preserving HTTP delivery.
    ///
    /// Set an `http://host:port` endpoint only for integration testing. An empty value is invalid.
    /// Changing this startup-only setting requires a restart; sketches still use the HTTP intake.
    pub endpoint: Option<String>,
}
