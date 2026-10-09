//! Experimental stateful metrics delivery.

use std::num::{NonZeroU64, NonZeroUsize};

use serde::Serialize;

use crate::defaults::{
    DEFAULT_STATEFUL_METRICS_DICTIONARY_MAX_BYTES, DEFAULT_STATEFUL_METRICS_DICTIONARY_MAX_ENTRIES,
    DEFAULT_STATEFUL_METRICS_WORKERS,
};

/// Configuration for the experimental stateful series client.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Domain {
    /// Plaintext gRPC intake endpoint. Unset by default, preserving HTTP delivery.
    ///
    /// Set an `http://host:port` endpoint only for integration testing. An empty value is invalid.
    /// Changing this startup-only setting requires a restart; sketches still use the HTTP intake.
    pub endpoint: Option<String>,
    /// Further plaintext gRPC intake endpoints that receive every stateful payload. Empty by default.
    ///
    /// Each must be a distinct `http://host:port` origin and requires `endpoint`. All endpoints use
    /// the primary API key and share each worker's dictionary. Requires a restart. Drain an
    /// endpoint's persisted retries before removing it; startup rejects the removal otherwise.
    pub additional_endpoints: Vec<String>,
    /// Independent sender workers. Defaults to `3`; zero is invalid.
    ///
    /// High-throughput workloads may increase this at the cost of per-worker queues, dictionaries,
    /// and inflight memory. Requires a restart. Drain persisted retries with the previous count
    /// before changing it; startup rejects a count change while retry files remain.
    pub workers: NonZeroUsize,
    /// Dictionary entry cap for each sender worker. Defaults to `20000`; zero is invalid.
    ///
    /// The total across the process is this value times `workers`. Eviction starts near the cap and
    /// removes the lowest-scoring entries; entries younger than the eviction grace period are
    /// protected, so the dictionary can exceed the cap briefly. A cap below the live series working
    /// set re-sends many definitions on every flush; high-cardinality workloads should raise it, at
    /// the cost of dictionary memory. Requires a restart.
    pub dictionary_max_entries: NonZeroUsize,
    /// Estimated dictionary byte cap for each sender worker. Defaults to 16 MiB; zero is invalid.
    ///
    /// The total across the process is this value times `workers`. The estimate covers retained
    /// definitions and lookup keys, not allocator slack or buffered and inflight metrics, so actual
    /// memory use is higher. Whichever of this and `dictionary_max_entries` is reached first triggers
    /// eviction. Requires a restart.
    pub dictionary_max_bytes: NonZeroU64,
}

impl Default for Domain {
    fn default() -> Self {
        Self {
            endpoint: None,
            additional_endpoints: Vec::new(),
            workers: DEFAULT_STATEFUL_METRICS_WORKERS,
            dictionary_max_entries: DEFAULT_STATEFUL_METRICS_DICTIONARY_MAX_ENTRIES,
            dictionary_max_bytes: DEFAULT_STATEFUL_METRICS_DICTIONARY_MAX_BYTES,
        }
    }
}
