//! Programmable metric processing: ADP's side of the plan crate's contracts.
//!
//! The metrics path of the topology runs the default program: native configuration lowered into a plan. This module
//! holds what the plan crate leaves to the host:
//!
//! - [`host_facts`]: what lowering needs to know about the process, captured once at boot;
//! - [`bindings`]: the closed registry of configuration bindings that plan parameters may name;
//! - [`outputs`]: the native metrics outputs, their recipients, and their fingerprints;
//! - [`catalog`]: the plan crate's catalog over the bindings and the outputs;
//! - [`telemetry_profiles`]: the native telemetry identity of each reserved node, outside the IR;
//! - [`lowering`]: native configuration into the default program;
//! - [`topology`]: the metrics path of the topology, built from the default program's physical frame;
//! - [`installer`]: booting the metrics path, and installing a new generation of the plan whenever a configuration
//!   change changes it;
//! - [`telemetry`]: what regions and the installer publish, under the native identities of the nodes.

pub mod bindings;
pub mod catalog;
pub mod host_facts;
pub mod installer;
pub mod lowering;
pub mod outputs;
pub mod telemetry;
pub mod telemetry_profiles;
pub mod topology;

#[cfg(test)]
mod test_support;
#[cfg(test)]
mod test_topology;
