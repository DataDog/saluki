//! Experimental stateful metrics delivery.

use std::num::NonZeroUsize;

use serde::Serialize;

use crate::defaults::DEFAULT_STATEFUL_METRICS_WORKERS;

/// Configuration for the experimental stateful series client.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Domain {
    /// Plaintext gRPC intake endpoint. Unset by default, preserving HTTP delivery.
    ///
    /// Set an `http://host:port` endpoint only for integration testing. An empty value is invalid.
    /// Changing this startup-only setting requires a restart; sketches still use the HTTP intake.
    pub endpoint: Option<String>,
    /// Independent sender workers. Defaults to `3`; zero is invalid.
    ///
    /// High-throughput workloads may increase this at the cost of per-worker queues, dictionaries,
    /// and inflight memory. Requires a restart. Drain persisted retries with the previous count
    /// before changing it; startup rejects a count change while retry files remain.
    pub workers: NonZeroUsize,
}

impl Default for Domain {
    fn default() -> Self {
        Self {
            endpoint: None,
            workers: DEFAULT_STATEFUL_METRICS_WORKERS,
        }
    }
}
