//! Experimental stateful metrics delivery.

use std::num::{NonZeroU64, NonZeroUsize};

use serde::Serialize;

use crate::defaults::{DEFAULT_STATEFUL_METRICS_DICTIONARY_MAX_BYTES, DEFAULT_STATEFUL_METRICS_DICTIONARY_MAX_ENTRIES};

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
    /// the primary API key and share the sender's dictionary. Requires a restart. Removing an
    /// additional endpoint discards its persisted retries at startup with a warning.
    pub additional_endpoints: Vec<String>,
    /// Dictionary entry cap for the sender. Defaults to `60000`; zero is invalid.
    ///
    /// Eviction starts near the cap and removes the lowest-scoring entries; entries younger than the eviction grace period are
    /// protected, so the dictionary can exceed the cap briefly. A cap below the live series working
    /// set re-sends many definitions on every flush. The default covers the working set of most nodes,
    /// and dictionary memory grows only with the series the sender actually sees, so lowering it mainly
    /// suits memory-constrained hosts. Requires a restart.
    pub dictionary_max_entries: NonZeroUsize,
    /// Estimated dictionary byte cap for the sender. Defaults to 256 MiB; zero is invalid.
    ///
    /// The estimate covers retained definitions and lookup keys, not allocator slack or buffered and inflight metrics, so actual
    /// memory use is higher. Whichever of this and `dictionary_max_entries` is reached first triggers
    /// eviction. Requires a restart.
    pub dictionary_max_bytes: NonZeroU64,
}

impl Default for Domain {
    fn default() -> Self {
        Self {
            endpoint: None,
            additional_endpoints: Vec::new(),
            dictionary_max_entries: DEFAULT_STATEFUL_METRICS_DICTIONARY_MAX_ENTRIES,
            dictionary_max_bytes: DEFAULT_STATEFUL_METRICS_DICTIONARY_MAX_BYTES,
        }
    }
}
