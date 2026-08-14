//! A mock Datadog intake for the Antithesis harness.
//!
//! Simulates the real `/api/v2/series` intake, fires payload-shape assertions,
//! and exposes the raw metric context lists used by the differential scenario.

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
// The comparison machinery behind the two differential oracles is reachable only from their routes, which
// only a differential build links. Without the feature it is all dead code, and the crate denies warnings,
// so the whole oracle tree would fail to compile for the general scenario. See the `differential` feature
// in Cargo.toml for why the gate cannot be a runtime switch.
#![cfg_attr(not(feature = "differential"), allow(dead_code))]

pub mod capture;
mod context_diff;
pub mod context_pool;
pub mod http;

mod lenient_decode;
mod oracle;
mod properties;
mod series;
mod series_observation;
mod sketch;
mod sut_config;
