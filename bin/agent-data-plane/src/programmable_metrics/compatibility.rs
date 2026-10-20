//! Telemetry compatibility as an analysis: the nodes the remapper reads native series from, and whether a plan has
//! them.
//!
//! The remapper's rules (`state::metrics::rules`) are shared with native mode, so they cannot change: they are the
//! contract. The [contract](contract::contract) is what the default program meets: every rule scoped to a component or
//! cache that some node of the default program reports for, through its operator's series or its [role](roles), with
//! that node's name and operator. A program that lacks one of those nodes, or runs it as another operator, loses the
//! series the remapper reads from it, and validation warns about it (`telemetry_contract`).
//!
//! Lowering builds the contract from the default program it lowers, and the [catalog](super::catalog) carries it, so
//! that every program is validated against it, here and wherever the catalog's document goes.

use std::collections::BTreeSet;

use saluki_core::observability::metrics::RemapperRule;
use saluki_metric_plan::{export::eliminate, ir::Plan, operators::OperatorRegistry, validate::TelemetryContract};

use crate::state::metrics::{get_compat_remappings, get_datadog_agent_remappings};

mod contract;
pub use self::contract::{contract, report};

pub mod roles;

#[cfg(test)]
mod tests;

/// Returns the remapper's rules: those producing the Datadog Agent's metrics, and the compatibility ones.
pub fn remapper_rules() -> Vec<RemapperRule> {
    let mut rules = get_datadog_agent_remappings();
    rules.extend(get_compat_remappings());
    rules
}

/// Returns the telemetry contract of the default program `default_program`: what it meets, against the native
/// operators and the remapper's rules, but for the nodes that do nothing as configured.
///
/// The detached export leaves those out, such as a filterlist with nothing in it, so a program written from it would
/// otherwise be warned about series that are zero, and that the installer keeps reporting, at zero, without the node.
/// Once the configuration gives such a node something to do, the contract names it again.
pub fn of_default(default_program: &Plan) -> TelemetryContract {
    let registry = OperatorRegistry::native();
    let mut contract = contract(default_program, &registry, &remapper_rules());
    let idle = eliminate(default_program, &registry)
        .omitted
        .into_iter()
        .map(|(node, _)| node)
        .collect::<BTreeSet<_>>();
    contract.entries.retain(|entry| !idle.contains(&entry.node));
    contract
}
