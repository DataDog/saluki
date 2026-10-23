//! Experimental stateful metrics delivery.

use std::{
    num::{NonZeroU64, NonZeroUsize},
    time::Duration,
};

use serde::Serialize;

use crate::defaults::{
    DEFAULT_STATEFUL_METRICS_DICTIONARY_MAX_BYTES, DEFAULT_STATEFUL_METRICS_DICTIONARY_MAX_ENTRIES,
    DEFAULT_STATEFUL_METRICS_RETRY_MAX_AGE, DEFAULT_STATEFUL_METRICS_RETRY_MAX_ATTEMPTS,
    DEFAULT_STATEFUL_METRICS_RETRY_MAX_BYTES, DEFAULT_STATEFUL_METRICS_WORKERS,
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
    /// Dictionary entry cap for each sender worker. Defaults to `60000`; zero is invalid.
    ///
    /// The total across the process is this value times `workers`. Eviction starts near the cap and
    /// removes the lowest-scoring entries; entries younger than the eviction grace period are
    /// protected, so the dictionary can exceed the cap briefly. A cap below the live series working
    /// set re-sends many definitions on every flush. The default covers the working set of most nodes,
    /// and dictionary memory grows only with the series a worker actually sees, so lowering it mainly
    /// suits memory-constrained hosts. Requires a restart.
    pub dictionary_max_entries: NonZeroUsize,
    /// Estimated dictionary byte cap for each sender worker. Defaults to 256 MiB; zero is invalid.
    ///
    /// The total across the process is this value times `workers`. The estimate covers retained
    /// definitions and lookup keys, not allocator slack or buffered and inflight metrics, so actual
    /// memory use is higher. Whichever of this and `dictionary_max_entries` is reached first triggers
    /// eviction. Requires a restart.
    pub dictionary_max_bytes: NonZeroU64,
    /// Byte budget for unacknowledged payloads Foldspace retains, compressed, to retry itself, per sender worker.
    /// Defaults to 8 MiB.
    ///
    /// Covers the compressed payloads plus dictionary definitions kept only for them. Past it, the oldest payloads
    /// still waiting for an endpoint move to the retry queue as logical batches. Zero moves every waiting payload to
    /// the retry queue at once. Requires a restart.
    pub retry_max_bytes: u64,
    /// How long a payload may wait inside Foldspace for an endpoint before it moves to the retry queue. Defaults to two
    /// minutes; zero disables the limit. Requires a restart.
    pub retry_max_age: Duration,
    /// Sends per payload before a stream that ends without acknowledging it moves it to the retry queue instead of
    /// replaying it. Defaults to `8`; zero is treated as one. Requires a restart.
    pub retry_max_attempts: u32,
}

impl Default for Domain {
    fn default() -> Self {
        Self {
            endpoint: None,
            additional_endpoints: Vec::new(),
            workers: DEFAULT_STATEFUL_METRICS_WORKERS,
            dictionary_max_entries: DEFAULT_STATEFUL_METRICS_DICTIONARY_MAX_ENTRIES,
            dictionary_max_bytes: DEFAULT_STATEFUL_METRICS_DICTIONARY_MAX_BYTES,
            retry_max_bytes: DEFAULT_STATEFUL_METRICS_RETRY_MAX_BYTES,
            retry_max_age: DEFAULT_STATEFUL_METRICS_RETRY_MAX_AGE,
            retry_max_attempts: DEFAULT_STATEFUL_METRICS_RETRY_MAX_ATTEMPTS,
        }
    }
}
