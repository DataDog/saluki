//! The bounded, bucketed time-series store and its configuration resolution.
//!
//! This module is the Rust port of the Go observer's `observer/impl/storage.go`. It is the single source
//! of truth that detectors, correlators, and output enrichment read from, and it keeps only the summary
//! statistics needed to compute every aggregate at read time.
//!
//! # Representation
//!
//! Each stored sample lands in a **one-second bucket** keyed by the integer Unix-second timestamp. A
//! bucket retains only `sum` and `count`; the raw samples are discarded. A bucket that received exactly
//! one sample carries an implicit count of `1`, and the parallel count vector is allocated lazily on the
//! first same-second merge, so the common payload stays small. Same-second values *add* into the bucket.
//!
//! Buckets are kept in timestamp order, so every range query is a binary search and out-of-order or
//! duplicate-timestamp writes insert at the correct index. Aggregation happens at read time (`avg =
//! sum/count`, `sum`, `count`), so one stored series serves every aggregate view.
//!
//! # Determinism
//!
//! Unlike the Go implementation, which iterates Go maps (random order) and guards shared state with a
//! mutex, this store is a **single-owner synchronous** structure with no interior locking: the engine owns
//! it exclusively and calls it from one thread. Series are stored in a [`std::collections::BTreeMap`]
//! keyed by [`SeriesRef`], so iteration is deterministic ascending-ref order. The Go code sorts by ref
//! where order is observable (listing, eviction tie-breaks); the remaining difference is that unordered Go
//! map iteration becomes a stable order here, which is a strict improvement and never changes the *set* of
//! series affected.
//!
//! # Host semantics
//!
//! The host is stored as `Option<String>`, so an unset host and an explicitly empty one remain distinct
//! metadata values. The Go-compatible storage key collapses both to the empty string (the Go model has a
//! single host field), so `None` and `Some("")` produce the same key and therefore merge into one series;
//! the first writer's metadata is retained. See [`crate::identity`] for the key derivation.
//!
//! # Deliberate omissions
//!
//! The Go store interns tag sets through a bounded pool to share canonical tag views across series. This
//! port stores each series' tags as its own `Vec<String>`: interning is a memory optimisation with no
//! observable effect on identity, aggregation, or eviction, and it would add bookkeeping to every removal
//! path. It can be added behind the same API if profiling ever calls for it.

mod resolve;
mod store;

/// The namespace reserved for observer-internal telemetry, such as testbench charts.
///
/// Detectors must not treat series in this namespace as workload data, and inactivity eviction never
/// removes them. Capacity eviction is the one path that does *not* protect this namespace.
pub const TELEMETRY_NAMESPACE: &str = "telemetry";

/// The namespace used for internal agent telemetry while `datadog.*` metrics are normalised.
pub const AGENT_NAMESPACE: &str = "agent";

pub use resolve::{resolve_storage_config, ResolvedStorageConfig, StorageConfigOverrides, StorageConfigWarning};
pub use store::{AddResult, SeriesFilter, TimeSeriesStorage};
