//! Resource bounds on artifacts, and the authority of lossy and host-owned nodes.

use std::collections::BTreeMap;

use saluki_core::{accounting::ComponentRegistry, support::SubsystemIdentifier};

use super::{graph::Graph, BoundMeasure, BoundScope, Finding, Location, Rule, ValidationReport};
use crate::{
    ir::{ArtifactRevision, ParamValue, Plan, Provenance},
    limits::{values, LimitSpec, ARTIFACT_NODES, PREDICATE_LEAVES, TEE_FAN_OUT, WORK_PER_INPUT},
    operators::{Cardinality, ConfiguredOperator, Loss, PortRelation},
};

/// Returns the number of predicate leaves in a parameter value.
pub(crate) fn predicate_leaves(value: &ParamValue) -> u64 {
    match value {
        ParamValue::Predicate(predicate) => predicate.leaf_count() as u64,
        ParamValue::List(values) | ParamValue::Set(values) => values.iter().map(predicate_leaves).sum(),
        ParamValue::StringMap(members) => members.values().map(predicate_leaves).sum(),
        ParamValue::Record(members) => members.iter().map(|(_, value)| predicate_leaves(value)).sum(),
        ParamValue::Config { resolved, .. } => resolved.as_deref().map_or(0, predicate_leaves),
        _ => 0,
    }
}

fn artifact_revision(provenance: &Option<Provenance>) -> Option<&ArtifactRevision> {
    match provenance {
        Some(Provenance::Artifact(revision)) => Some(revision),
        _ => None,
    }
}

pub(super) fn check(graph: &Graph<'_>, plan: &Plan, report: &mut ValidationReport) {
    check_authority(graph, report);
    check_provenance_consistency(graph, plan, report);

    // The whole plan is measured for `explain`; the limits are enforced only on each artifact's nodes, so that large
    // native configurations stay valid.
    let everything = vec![true; graph.nodes.len()];
    measure(graph, &everything, BoundScope::Plan, report);

    let mut artifacts: BTreeMap<(u64, String), (ArtifactRevision, Vec<bool>)> = BTreeMap::new();
    for (node, entry) in graph.nodes.iter().enumerate() {
        if let Some(revision) = artifact_revision(&entry.node.provenance) {
            let (_, members) = artifacts
                .entry((revision.revision, revision.hash.to_string()))
                .or_insert_with(|| (revision.clone(), vec![false; graph.nodes.len()]));
            members[node] = true;
        }
    }
    for (revision, members) in artifacts.into_values() {
        let artifact = format!("artifact revision {} ({})", revision.revision, revision.hash);
        let first = report.bounds.len();
        measure(graph, &members, BoundScope::Artifact(revision), report);
        let measured = report.bounds[first..].to_vec();
        for bound in measured {
            if bound.is_within() {
                continue;
            }
            let value = bound
                .value
                .map_or_else(|| "an unbounded number of".to_string(), |v| v.to_string());
            let limit = bound.maximum;
            if bound.limit == ARTIFACT_NODES.name {
                report.push(Finding::new(
                    Rule::ArtifactNodes,
                    Location::Plan,
                    format!(
                        "{} has {} nodes, more than the limit of {}; split it, or simplify it",
                        artifact, value, limit
                    ),
                ));
            } else if bound.limit == PREDICATE_LEAVES.name {
                report.push(Finding::new(
                    Rule::PredicateLeaves,
                    Location::Plan,
                    format!(
                        "the predicates of {} have {} leaves, more than the limit of {}; use `matches` with a list \
                         of values instead of many separate comparisons",
                        artifact, value, limit
                    ),
                ));
            }
        }
        check_fan_out(graph, &members, report);
        check_work_per_input(graph, &members, report);
        check_memory(graph, &members, &artifact, report);
    }
}

/// Measures the bounds of the nodes in `members`, recording one [`BoundMeasure`] per bound in `scope`.
fn measure(graph: &Graph<'_>, members: &[bool], scope: BoundScope, report: &mut ValidationReport) {
    let nodes = members.iter().filter(|member| **member).count() as u64;
    let leaves = (0..graph.nodes.len())
        .filter(|&node| members[node])
        .filter_map(|node| graph.op(node))
        .flat_map(|op| op.params().iter().map(|(_, value)| predicate_leaves(value)))
        .sum::<u64>();
    let fan_out = (0..graph.nodes.len())
        .filter(|&node| members[node])
        .filter_map(|node| graph.op(node))
        .filter(|op| op.contract().ports == PortRelation::Copies)
        .map(|op| op.ports().outputs.len() as u64)
        .max()
        .unwrap_or(0);

    let mut push = |limit: &LimitSpec, maximum: u64, value: Option<u64>| {
        report.bounds.push(BoundMeasure {
            limit: limit.name,
            maximum,
            scope: scope.clone(),
            value,
        });
    };
    push(&ARTIFACT_NODES, values::ARTIFACT_NODES, Some(nodes));
    push(&PREDICATE_LEAVES, values::PREDICATE_LEAVES, Some(leaves));
    push(&TEE_FAN_OUT, values::TEE_FAN_OUT, Some(fan_out));
    if let Some(visits) = visits(graph, members) {
        let worst = entry_nodes(graph, members).map(|node| visits[node]).max().unwrap_or(0);
        push(
            &WORK_PER_INPUT,
            values::WORK_PER_INPUT,
            (worst != u64::MAX).then_some(worst),
        );
    }
}

/// Returns the nodes where records enter the set `members`: members none of whose inputs come from another member.
/// Sources are always entry nodes.
fn entry_nodes<'a>(graph: &'a Graph<'_>, members: &'a [bool]) -> impl Iterator<Item = usize> + 'a {
    graph.order.iter().copied().filter(move |&node| {
        members[node]
            && graph.op(node).is_some_and(|op| {
                (0..op.ports().inputs.len())
                    .all(|port| graph.source(node, port).is_none_or(|(previous, _)| !members[previous]))
            })
    })
}

fn check_fan_out(graph: &Graph<'_>, members: &[bool], report: &mut ValidationReport) {
    let fan_out_limit = values::TEE_FAN_OUT;
    for (node, member) in members.iter().enumerate() {
        let Some(op) = graph.op(node).filter(|_| *member) else {
            continue;
        };
        let outputs = op.ports().outputs.len() as u64;
        if op.contract().ports == PortRelation::Copies && outputs > fan_out_limit {
            report.push(Finding::new(
                Rule::TeeFanOut,
                graph.location(node),
                format!(
                    "`{}` copies each record to {} ports, more than the limit of {}",
                    op.name(),
                    outputs,
                    fan_out_limit
                ),
            ));
        }
    }
}

/// Returns the memory a node's operator declares in its memory bounds, in bytes: what its kernel allocates up front and
/// at most, together.
fn declared_memory(op: &ConfiguredOperator) -> u64 {
    let registry = ComponentRegistry::default();
    op.specify_bounds(&mut registry.bounds_builder(&SubsystemIdentifier::from_segments(["node"])));
    u64::try_from(registry.as_bounds().total_firm_limit_bytes()).unwrap_or(u64::MAX)
}

/// Reports an artifact whose nodes declare more memory than the limit, naming the node that declares the most.
///
/// A kernel's memory is allocated when it is built, before the artifact runs: a size beyond what the host can allocate
/// would end the process, which no panic budget or rollback can recover from, and any size beyond the host's memory
/// bounds escapes them, since those cover the boot program.
fn check_memory(graph: &Graph<'_>, members: &[bool], artifact: &str, report: &mut ValidationReport) {
    let limit = values::ARTIFACT_MEMORY;
    let declared = (0..graph.nodes.len())
        .filter(|&node| members[node])
        .filter_map(|node| graph.op(node).map(|op| (node, declared_memory(op))))
        .collect::<Vec<_>>();
    let total = declared
        .iter()
        .fold(0u64, |total, (_, bytes)| total.saturating_add(*bytes));
    if total <= limit {
        return;
    }
    let Some(&(largest, bytes)) = declared.iter().max_by_key(|(_, bytes)| *bytes) else {
        return;
    };
    report.push(Finding::new(
        Rule::ArtifactMemory,
        graph.location(largest),
        format!(
            "the nodes of {} declare {} bytes of memory, more than the limit of {}, and this node declares {} of \
             them; reduce its cache and interner sizes",
            artifact, total, limit, bytes
        ),
    ));
}

/// Returns how many visits to artifact nodes one record arriving at a node can cause, given the visits caused by a
/// record arriving at each of its output ports' targets.
fn combine(op: &ConfiguredOperator, downstream: &[u64]) -> u64 {
    let contract = op.contract();
    let spread = match contract.ports {
        PortRelation::Copies => downstream
            .iter()
            .fold(0u64, |total, visits| total.saturating_add(*visits)),
        PortRelation::PointDisjoint => downstream.iter().copied().max().unwrap_or(0),
    };
    match contract.cardinality {
        Cardinality::Sink => 0,
        Cardinality::Filter | Cardinality::Map | Cardinality::Source => spread,
        Cardinality::Expand { max: Some(max) } => {
            // Expanded records may all take the costliest port, or each take a different copying port.
            spread.saturating_mul(u64::from(max))
        }
        Cardinality::Expand { max: None } => {
            if spread == 0 {
                0
            } else {
                u64::MAX
            }
        }
    }
}

/// Returns, for every node, how many visits to nodes in `members` one record arriving at it can cause, saturating at
/// `u64::MAX` for unbounded expansion; or `None` if the graph has a cycle.
fn visits(graph: &Graph<'_>, members: &[bool]) -> Option<Vec<u64>> {
    if graph.cyclic {
        return None;
    }
    let mut visits = vec![0u64; graph.nodes.len()];
    for &node in graph.order.iter().rev() {
        let Some(op) = graph.op(node) else { continue };
        let downstream = (0..op.ports().outputs.len())
            .map(|port| graph.target(node, port).map_or(0, |(next, _)| visits[next]))
            .collect::<Vec<_>>();
        visits[node] = u64::from(members[node]).saturating_add(combine(op, &downstream));
    }
    Some(visits)
}

fn check_work_per_input(graph: &Graph<'_>, members: &[bool], report: &mut ValidationReport) {
    let Some(visits) = visits(graph, members) else {
        return;
    };
    let limit = values::WORK_PER_INPUT;
    for node in entry_nodes(graph, members) {
        if visits[node] > limit {
            let visits = if visits[node] == u64::MAX {
                "an unbounded number of".to_string()
            } else {
                visits[node].to_string()
            };
            report.push(Finding::new(
                Rule::WorkPerInput,
                graph.location(node),
                format!(
                    "one record entering the artifact here can cause {} node visits, more than the limit of {}; \
                     reduce how often records are copied",
                    visits, limit
                ),
            ));
        }
    }
}

/// Reports every node whose artifact provenance names a revision that the plan's provenance does not list.
///
/// Composition lists every artifact revision it composes into a plan, so a node that names another one claims an
/// authority nothing granted.
///
/// # Missing
///
/// Whether a listed revision was approved is not checked here. Composition writes artifact provenance, and the
/// installer composes only the artifacts it accepted, under their revision, so on that path the list is approved by
/// construction. A plan whose artifact provenance comes from elsewhere would need the catalog to declare the approved
/// revisions, and validation to check the plan's list against them.
fn check_provenance_consistency(graph: &Graph<'_>, plan: &Plan, report: &mut ValidationReport) {
    let listed = plan
        .provenance
        .as_ref()
        .map_or(&[][..], |provenance| provenance.artifacts.as_slice());
    for (node, entry) in graph.nodes.iter().enumerate() {
        let Some(revision) = artifact_revision(&entry.node.provenance) else {
            continue;
        };
        if !listed.contains(revision) {
            report.push(Finding::new(
                Rule::ProvenanceConsistency,
                graph.location(node),
                format!(
                    "the node's provenance is artifact revision {} ({}), which the plan's provenance does not list; \
                     list every composed artifact revision in the plan's `provenance.artifacts`",
                    revision.revision, revision.hash
                ),
            ));
        }
    }
}

fn check_authority(graph: &Graph<'_>, report: &mut ValidationReport) {
    for (node, entry) in graph.nodes.iter().enumerate() {
        let Some(op) = &entry.op else { continue };
        let contract = op.contract();
        if let (Loss::Lossy(losses), None) = (&contract.loss, &entry.node.provenance) {
            let how = losses
                .iter()
                .map(|loss| loss.description.trim_end_matches('.'))
                .collect::<Vec<_>>()
                .join("; ");
            report.push(Finding::new(
                Rule::LossAuthority,
                graph.location(node),
                format!(
                    "`{}` loses contributions ({}), but the node has no provenance; every lossy node needs the \
                     authority of native configuration or an approved artifact",
                    op.name(),
                    how
                ),
            ));
        }
        if contract.boundary.is_fence() && artifact_revision(&entry.node.provenance).is_some() {
            report.push(Finding::new(
                Rule::TrustedHost,
                graph.location(node),
                format!(
                    "`{}` runs in a host component, which only native configuration can place; artifacts cannot add \
                     or change host components",
                    op.name()
                ),
            ));
        }
    }
}
