//! A mock Datadog intake for the Antithesis harness.
//!
//! Simulates the real `/api/v2/series` intake and, on every payload the Agent
//! Data Plane delivers, fires Antithesis SDK assertions for the Pyld01-Pyld22
//! properties catalogued in the crate `README.md`. These observe whether the
//! Agent honoured its wire contract.

#![deny(clippy::all)]
#![deny(clippy::pedantic)]
#![deny(clippy::perf)]
#![deny(clippy::suspicious)]
#![deny(clippy::complexity)]
#![deny(clippy::cargo)]
#![allow(
    clippy::cargo_common_metadata,
    reason = "workspace crates do not set publish metadata"
)]
#![deny(clippy::unwrap_used)]
#![deny(clippy::dbg_macro)]
#![deny(clippy::print_stdout)]
#![deny(clippy::print_stderr)]
#![deny(clippy::redundant_allocation)]
#![deny(clippy::rc_buffer)]
#![deny(clippy::large_futures)]
#![deny(clippy::large_stack_arrays)]
#![deny(clippy::float_cmp)]
#![deny(clippy::manual_memcpy)]
#![deny(clippy::unnecessary_to_owned)]
#![deny(clippy::disallowed_types)]
#![allow(clippy::multiple_crate_versions, reason = "shared workspace dependency graph")]
#![deny(unused_extern_crates)]
#![deny(unreachable_pub)]
#![deny(missing_copy_implementations)]
#![deny(missing_debug_implementations)]
#![deny(missing_docs)]
#![deny(warnings)]

pub mod http;

mod properties;
mod series_observation;
