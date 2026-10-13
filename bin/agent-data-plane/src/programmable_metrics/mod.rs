//! Programmable metric processing: ADP's side of the plan crate's contracts.
//!
//! The metrics path of the topology runs the default program, native configuration lowered into a plan, or a policy
//! artifact composed into it. This module holds what the plan crate leaves to the host:
//!
//! - [`host_facts`]: what lowering needs to know about the process, captured once at boot;
//! - [`bindings`]: the closed registry of configuration bindings that plan parameters may name;
//! - [`resources`]: the outputs, their bindings, the recipients, and the delivery hosts' settings, resolved from the
//!   current configuration and the accepted resources artifact, with the parameter codec of output bindings, host
//!   settings, and recipient protocols;
//! - [`credentials`]: the credentials recipients send with, resolved from their references;
//! - [`catalog`]: the plan crate's catalog over the bindings, the outputs, and the delivery hosts;
//! - [`telemetry_profiles`]: the native telemetry identity of each reserved node and each lease, outside the IR;
//! - [`lowering`]: native configuration into the default program;
//! - [`lanes`]: how many lanes the slots of the metrics path run on, fixed at boot;
//! - [`topology`]: the metrics path of the topology, built from the default program's physical frame;
//! - [`installer`]: booting the metrics path, and installing a new generation of the plan whenever a configuration
//!   change, a policy artifact, or a resources artifact changes it, with the resources its delivery hosts run;
//! - [`telemetry`]: what regions and the installer publish, under the native identities of the nodes;
//! - [`snapshot_bridge`]: the worker that answers DogStatsD aggregation's context snapshot requests from the region
//!   that runs it, when aggregation runs as a stateful kernel;
//! - [`channel_stub`]: the local development stub of the artifact channel, which polls a stub server for policy and
//!   resources snapshots;
//! - [`api`]: the control surface on the privileged API: status, resources, export, plan, and explain.

pub mod api;
pub mod bindings;
pub mod catalog;
pub mod channel_stub;
pub mod credentials;
pub mod host_facts;
pub mod installer;
pub mod lanes;
pub mod lowering;
pub mod resources;
pub mod snapshot_bridge;
pub mod telemetry;
pub mod telemetry_profiles;
pub mod topology;

#[cfg(test)]
mod delivery_tests;
#[cfg(test)]
mod test_hosts;
#[cfg(test)]
mod test_intake;
#[cfg(test)]
mod test_support;
#[cfg(test)]
mod test_topology;
