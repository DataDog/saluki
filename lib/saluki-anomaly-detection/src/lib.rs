//! Analytical core for agent anomaly detection, ported from the Agent's Go observer.
//!
//! This crate holds the reusable, runtime-agnostic parts of the anomaly detection pipeline: the shared
//! data model ([`model`]), series identity and the Go-compatible hashing used by the historical store
//! ([`identity`]), typed configuration with the Agent's defaults ([`config`]), the bounded bucket store
//! that detectors read from ([`storage`]), the small detector/extractor/scorer interfaces that the
//! engine drives ([`traits`]), and the detector implementations with their shared statistics
//! ([`detectors`]).
//!
//! # Design
//!
//! The crate is deliberately **dependency-free**. It is a pure, synchronous computation core: the store,
//! engine, detectors, extractors, and scorer are all driven by an exclusive owner, so there is no need for
//! async runtimes, HTTP, or columnar file formats here. Keeping the core free of `tokio`, Parquet, and
//! network dependencies lets it run identically in the offline replay testbench and, later, in the live
//! pipeline, and it keeps the analytical math easy to test in isolation.
//!
//! ## Why identity is self-contained instead of reusing Saluki's metric context
//!
//! Saluki already has a metric context type
//! (`saluki_core::data_model::event::metric::context`), but this crate does not reuse it, for two reasons:
//!
//! 1. Depending on `saluki-core` would pull `futures`, `async-trait`, `http`, `papaya`, `quick_cache`, and
//!    the metrics facade into the analytical core, contradicting the design above.
//! 2. More importantly, Saluki's context key is computed with `saluki-common`'s `foldhash`-based hasher, so
//!    it is **not bit-compatible** with the Go observer's context key. This port must reproduce the Go
//!    storage key bit-for-bit (see [`identity::storage_key`]) because series identity is part of the output
//!    we compare against the Go reference.
//!
//! The Go context key is therefore reimplemented in-crate from the source of truth (`pkg/aggregator/ckey`
//! and `observer/impl/storage.go`) and pinned with the Go test vectors.
//!
//! # Host semantics
//!
//! Every metric-backed value carries its host separately from its tags, as `Option<String>`. An unset host
//! (`None`) and an explicitly empty host (`Some(String::new())`) are **distinct identities**, matching the
//! offline replay requirement. The Go-compatible key functions collapse both to `""` because the Go model
//! represents a host as a plain string; this collapse is documented on those functions.
#![deny(warnings)]
#![deny(missing_docs)]

pub mod config;
pub mod detectors;
pub mod identity;
pub mod model;
pub mod storage;
pub mod traits;
