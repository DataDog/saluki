//! Leaving out what does nothing: the nodes of a plan that, as configured, change no record.
//!
//! A default program lowered from native configuration has a node for every native feature, whether or not it is
//! configured: a metric filterlist with nothing in it, a namespace prefix with no namespace, a mapper with no profiles.
//! Such a node runs, and its native telemetry reports, but it does nothing to what the plan delivers. [`eliminate`]
//! finds those nodes, and removes them from a copy of the plan, so that:
//!
//! - the [detached export](super#the-detached-form) is only what actually happens, and lists what it left out
//!   ([`ExportBundle::omitted`](super::ExportBundle::omitted)); and
//! - a tool's diff of what two plans do ([`diff_operations`](crate::install::diff_operations)) and the nodes of the
//!   default program a published program does not run as it writes them ([`node_differences`](super::node_differences))
//!   see a program written from that export, which does not have the nodes, as doing what the default program, which
//!   does, does.
//!
//! The plan that runs is never changed by this: the default program keeps every node, whatever its values, so its
//! structure never depends on a configuration value, and the installer compares plans as written
//! ([`operation_hash`](super::operation_hash)): publishing the shorter export over the default program is a generation
//! that builds no kernel, after which the plan running is the program as published.
//!
//! # What is left out
//!
//! Decisions read **literal** parameters only. A binding, a fact or a signal, is not known, whatever its resolved value,
//! so it never decides anything. Two kinds of node go, until none is left:
//!
//! - **A constant `split`**: one whose predicate its literal operands decide ([`Predicate::literal_value`]), such as a
//!   filterlist with nothing in it (`intake_name in []`), together with everything only its dead port feeds, by rewrite
//!   R3's rules and obligations (no source, fence, stateful or dynamic operator is removed; a `merge` the dead
//!   subgraph feeds keeps its other inputs, renumbered in order; an `emit` that goes leaves its output declared).
//!   A `flag` or `is_empty` of a bound fact or signal never decides one.
//! - **An identity node**: a node with one input and one output whose operator says that, with these literal
//!   parameters, it passes every record through unchanged ([`OperatorDef::is_identity`]); it is never a source, a
//!   fence, a state owner, or a dynamic operator. The edge into it is spliced to where its output leads.
//!
//! Splits go first, in plan order, then identity nodes, in plan order, and the search starts again after each, since a
//! fold can leave a `merge` with one input. A plan that does not validate structurally loses nothing.
//!
//! [`OperatorDef::is_identity`]: crate::operators::OperatorDef::is_identity

use serde::{Deserialize, Serialize};

use crate::{
    canonical::strip_resolved_values,
    compile::rewrite::r3::fold_split,
    ir::{Edge, NodeId, Plan, PortRef},
    normalize::normalize_params,
    operators::{names, Cardinality, OperatorRegistry, StateContract},
    validate::{graph::Graph, ValidationReport},
};

/// Why [`eliminate`] left a node out.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Omission {
    /// It passes every record through unchanged as configured, such as a mapper with no profiles.
    Identity,

    /// It is a `split` whose literal operands decide its predicate, such as a filterlist with nothing in it: every record
    /// leaves by one port, and the edge into it now leads where that port does.
    ConstantSplit,

    /// Only the dead port of a constant `split` reaches it, such as the `drop` of an empty filterlist, so nothing ever
    /// does.
    Unreachable,

    /// A reason a later version gives that this reader does not know.
    #[serde(other)]
    Unknown,
}

/// A plan with what does nothing left out, and what was.
#[derive(Clone, Debug)]
pub struct Eliminated {
    /// The plan without the nodes left out: their parameters as the plan wrote them, with the `inputs` of a `merge` that
    /// lost some renumbered.
    pub plan: Plan,

    /// Each node left out, with why, in the order they were left out.
    pub omitted: Vec<(NodeId, Omission)>,
}

/// Returns `plan` without the nodes that do nothing as configured (see [the module documentation](self)), deciding by
/// the literal parameters only.
pub fn eliminate(plan: &Plan, registry: &OperatorRegistry) -> Eliminated {
    let mut decided = literal_form(plan);
    let mut result = plan.clone();
    let mut omitted = Vec::new();
    loop {
        let mut findings = ValidationReport::default();
        let graph = Graph::build(&decided, registry, &mut findings);
        if !findings.is_valid() || graph.cyclic {
            break;
        }
        if let Some((split, folded)) = constant_split(&graph, &decided, &result) {
            // Folding gives back one plan for each it was given.
            let Ok([next_decided, next_result]) = <[Plan; 2]>::try_from(folded.plans) else {
                break;
            };
            omitted.push((split, Omission::ConstantSplit));
            omitted.extend(folded.unreachable.into_iter().map(|node| (node, Omission::Unreachable)));
            (decided, result) = (next_decided, next_result);
            continue;
        }
        if let Some((node, inbound, outbound)) = identity_node(&graph) {
            decided = splice(&decided, &node, &inbound, &outbound);
            result = splice(&result, &node, &inbound, &outbound);
            omitted.push((node, Omission::Identity));
            continue;
        }
        break;
    }
    Eliminated { plan: result, omitted }
}

/// Returns `plan` as the decisions read it: every binding without its resolved value, so not known, and every node's
/// parameters in normal form ([`normalize_params`]), with every predicate's literal `flag` and `is_empty` folded.
fn literal_form(plan: &Plan) -> Plan {
    let mut plan = plan.clone();
    for node in &mut plan.nodes {
        let params = node
            .params
            .iter()
            .map(|(name, value)| {
                let mut value = value.clone();
                strip_resolved_values(&mut value);
                (name.clone(), value)
            })
            .collect();
        node.params = normalize_params(&node.op, &params);
    }
    plan
}

/// Returns the first `split`, in plan order, whose literal operands decide its predicate and which R3's obligations
/// let fold, with `decided` and `result` folded at it.
fn constant_split(
    graph: &Graph<'_>, decided: &Plan, result: &Plan,
) -> Option<(NodeId, crate::compile::rewrite::r3::Folded)> {
    (0..graph.nodes.len()).find_map(|split| {
        let op = graph.op(split).filter(|op| op.name() == names::SPLIT)?;
        let value = op.params().predicate("predicate")?.literal_value()?;
        let folded = fold_split(&[decided, result], graph, split, value)?;
        Some((graph.id(split).clone(), folded))
    })
}

/// Returns the first identity node, in plan order, that can be left out, with the port that feeds it and the port its
/// output leads to.
fn identity_node(graph: &Graph<'_>) -> Option<(NodeId, PortRef, PortRef)> {
    (0..graph.nodes.len()).find_map(|node| {
        let op = graph.op(node)?;
        let contract = op.contract();
        let ports = op.ports();
        let removable = op.is_identity()
            && ports.inputs.len() == 1
            && ports.outputs.len() == 1
            && contract.state == StateContract::None
            && !contract.is_dynamic()
            && !contract.boundary.is_fence()
            && contract.cardinality != Cardinality::Source;
        if !removable {
            return None;
        }
        let (from, from_port) = graph.source(node, 0)?;
        let (to, to_port) = graph.target(node, 0)?;
        let inbound = PortRef::new(
            graph.id(from).clone(),
            graph.output_name((from, from_port)).parse().ok()?,
        );
        let outbound = PortRef::new(graph.id(to).clone(), graph.input_name((to, to_port)).parse().ok()?);
        Some((graph.id(node).clone(), inbound, outbound))
    })
}

/// Returns `plan` without `node`, the edge from `inbound` into it leading to `outbound`, where its output led, in the
/// place of the edge into it.
fn splice(plan: &Plan, node: &NodeId, inbound: &PortRef, outbound: &PortRef) -> Plan {
    let mut plan = plan.clone();
    plan.nodes.retain(|kept| &kept.id != node);
    plan.edges = plan
        .edges
        .iter()
        .filter(|edge| &edge.from.node != node)
        .map(|edge| {
            if &edge.to.node == node {
                Edge {
                    from: inbound.clone(),
                    to: outbound.clone(),
                }
            } else {
                edge.clone()
            }
        })
        .collect();
    plan
}

#[cfg(test)]
mod tests;
