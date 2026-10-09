//! Programmable metric processing: ADP's side of the plan crate's contracts.
//!
//! This module describes the native metrics path as a plan. It holds what the plan crate leaves to the host:
//!
//! - [`host_facts`]: what lowering needs to know about the process, captured once at boot;
//! - [`bindings`]: the closed registry of configuration bindings that plan parameters may name;
//! - [`outputs`]: the native metrics outputs, their recipients, and their fingerprints;
//! - [`catalog`]: the plan crate's catalog over the bindings and the outputs;
//! - [`telemetry_profiles`]: the native telemetry identity of each reserved node, outside the IR;
//! - [`lowering`]: native configuration into the default program.
//!
//! # Missing
//!
//! Nothing here runs yet: the legacy topology still builds the metrics path, and these modules are compiled only for
//! their tests, which compare what they describe with the legacy topology. Stage 3 builds the plan-driven topology from
//! the lowered program, captures [`HostFacts`](host_facts::HostFacts) in `run.rs`, and compiles this module into the
//! binary.

pub mod bindings;
pub mod catalog;
pub mod host_facts;
pub mod lowering;
pub mod outputs;
pub mod telemetry_profiles;

#[cfg(test)]
mod test_support;
