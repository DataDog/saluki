//! Telemetry compatibility as an analysis: the nodes the remapper reads native series from, and whether a plan has
//! them.
//!
//! The remapper's rules (`state::metrics::rules`) are shared with native mode, so they cannot change: they are the
//! contract. The [contract](contract::contract) is what the default program meets: every rule scoped to a component or
//! cache that some node of the default program reports for, through its operator's series or its [role](roles), with
//! that node's name and operator. A program that lacks one of those nodes, or runs it as another operator, loses the
//! series the remapper reads from it, and validation warns about it (`telemetry_contract`).
//!
//! This module is built beside [`telemetry_profiles`](super::telemetry_profiles), which it replaces once nodes report
//! under their names.

mod contract;
pub use self::contract::{contract, report};

pub mod roles;

#[cfg(test)]
mod tests;
