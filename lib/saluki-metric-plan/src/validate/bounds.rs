//! Resource bounds on the programs artifacts compose, and what a program may hold.

use std::collections::{BTreeMap, BTreeSet};

use saluki_core::{accounting::ComponentRegistry, support::SubsystemIdentifier};

use super::{graph::Graph, BoundMeasure, BoundScope, Finding, Location, Rule, ValidationReport};
use crate::{
    ir::{ComposedArtifact, NodeId, ParamValue, Plan, ScopeName},
    limits::{values, LimitSpec, ARTIFACT_NODES, PREDICATE_LEAVES, TEE_FAN_OUT, WORK_PER_INPUT},
    operators::{Cardinality, ConfiguredOperator, OperatorRegistry, PortRelation},
};

/// What the default program has in a scope a program replaces: a program's limit rises to it where it is more than
/// the limit, and never above it.
///
/// The limits bound what an artifact makes the host do beyond what native configuration sized it for. The default
/// program's own measures are native configuration's, so a program may keep them, unmodified or not, but not grow
/// past them; where the default program is within a limit, the program is held to the limit.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct Baseline {
    nodes: u64,
    leaves: u64,
    memory: u64,
    /// The ports of each copying node, by node.
    fan_out: BTreeMap<NodeId, u64>,
    /// The node visits one record entering the scope at each entry node causes, by node.
    work: BTreeMap<NodeId, u64>,
}

impl Baseline {
    /// Returns the largest fan-out of the default program's copying nodes.
    fn widest(&self) -> u64 {
        self.fan_out.values().copied().max().unwrap_or(0)
    }

    /// Returns the most work per input of any of the default program's entry nodes.
    fn heaviest(&self) -> u64 {
        self.work.values().copied().max().unwrap_or(0)
    }
}

/// Measures the default program `default` in each of `scopes`.
pub(super) fn baselines(
    default: &Plan, registry: &OperatorRegistry, scopes: &[ScopeName],
) -> BTreeMap<ScopeName, Baseline> {
    // The default program's own findings are not the program's: they are reported when it is validated.
    let mut scratch = ValidationReport::default();
    let graph = Graph::build(default, registry, &mut scratch);
    scopes
        .iter()
        .map(|scope| {
            let nodes = crate::export::scope_members(default, scope);
            let members = (0..graph.nodes.len())
                .map(|node| nodes.contains(graph.id(node)))
                .collect::<Vec<_>>();
            let member_ops = || {
                (0..graph.nodes.len())
                    .filter(|&node| members[node])
                    .filter_map(|node| graph.op(node).map(|op| (node, op)))
            };
            let fan_out = member_ops()
                .filter(|(_, op)| op.contract().ports == PortRelation::Copies)
                .map(|(node, op)| (graph.id(node).clone(), op.ports().outputs.len() as u64))
                .collect();
            let work = visits(&graph, &members)
                .map(|visits| {
                    entry_nodes(&graph, &members)
                        .map(|node| (graph.id(node).clone(), visits[node]))
                        .collect()
                })
                .unwrap_or_default();
            let baseline = Baseline {
                nodes: members.iter().filter(|member| **member).count() as u64,
                leaves: member_ops()
                    .flat_map(|(_, op)| op.params().iter().map(|(_, value)| predicate_leaves(value)))
                    .sum(),
                memory: member_ops().fold(0u64, |total, (_, op)| total.saturating_add(declared_memory(op))),
                fan_out,
                work,
            };
            (scope.clone(), baseline)
        })
        .collect()
}

/// Returns the limit `limit`, raised to the default program's `baseline` if that is more, and how to name it.
fn raised(limit: u64, baseline: Option<u64>) -> (u64, String) {
    match baseline {
        Some(baseline) if baseline > limit => (
            baseline,
            format!(
                "{}, which the default program has here, above the limit of {}",
                baseline, limit
            ),
        ),
        _ => (limit, format!("the limit of {}", limit)),
    }
}

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

pub(super) fn check(
    graph: &Graph<'_>, programs: &[(ComposedArtifact, BTreeSet<NodeId>)], baselines: &BTreeMap<ScopeName, Baseline>,
    report: &mut ValidationReport,
) {
    check_programs(graph, programs, report);

    // The whole plan is measured for `explain`; the limits are enforced only on the nodes of each program an artifact
    // composes, so that large native configurations stay valid.
    let everything = vec![true; graph.nodes.len()];
    measure(graph, &everything, BoundScope::Plan, None, report);

    for (artifact, nodes) in programs {
        let baseline = baselines.get(&artifact.scope);
        let revision = artifact.revision();
        let members = (0..graph.nodes.len())
            .map(|node| nodes.contains(graph.id(node)))
            .collect::<Vec<_>>();
        let artifact = format!("artifact revision {} ({})", revision.revision, revision.hash);
        let first = report.bounds.len();
        measure(graph, &members, BoundScope::Artifact(revision), baseline, report);
        let measured = report.bounds[first..].to_vec();
        for bound in measured {
            if bound.is_within() {
                continue;
            }
            let value = bound
                .value
                .map_or_else(|| "an unbounded number of".to_string(), |v| v.to_string());
            if bound.limit == ARTIFACT_NODES.name {
                let (_, limit) = raised(values::ARTIFACT_NODES, baseline.map(|baseline| baseline.nodes));
                report.push(Finding::new(
                    Rule::ArtifactNodes,
                    Location::Plan,
                    format!(
                        "{} has {} nodes, more than {}; split it, or simplify it",
                        artifact, value, limit
                    ),
                ));
            } else if bound.limit == PREDICATE_LEAVES.name {
                let (_, limit) = raised(values::PREDICATE_LEAVES, baseline.map(|baseline| baseline.leaves));
                report.push(Finding::new(
                    Rule::PredicateLeaves,
                    Location::Plan,
                    format!(
                        "the predicates of {} have {} leaves, more than {}; use `matches` with a list of values \
                         instead of many separate comparisons",
                        artifact, value, limit
                    ),
                ));
            }
        }
        check_fan_out(graph, &members, baseline, report);
        check_work_per_input(graph, &members, baseline, report);
        check_memory(graph, &members, &artifact, baseline, report);
    }
}

/// Measures the bounds of the nodes in `members`, recording one [`BoundMeasure`] per bound in `scope`, each with its
/// limit raised to `baseline`'s measure where that is more.
fn measure(
    graph: &Graph<'_>, members: &[bool], scope: BoundScope, baseline: Option<&Baseline>, report: &mut ValidationReport,
) {
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
    let maximum = |limit: u64, measure: fn(&Baseline) -> u64| raised(limit, baseline.map(measure)).0;
    push(
        &ARTIFACT_NODES,
        maximum(values::ARTIFACT_NODES, |baseline| baseline.nodes),
        Some(nodes),
    );
    push(
        &PREDICATE_LEAVES,
        maximum(values::PREDICATE_LEAVES, |baseline| baseline.leaves),
        Some(leaves),
    );
    push(
        &TEE_FAN_OUT,
        maximum(values::TEE_FAN_OUT, Baseline::widest),
        Some(fan_out),
    );
    if let Some(visits) = visits(graph, members) {
        let worst = entry_nodes(graph, members).map(|node| visits[node]).max().unwrap_or(0);
        push(
            &WORK_PER_INPUT,
            maximum(values::WORK_PER_INPUT, Baseline::heaviest),
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

fn check_fan_out(graph: &Graph<'_>, members: &[bool], baseline: Option<&Baseline>, report: &mut ValidationReport) {
    for (node, member) in members.iter().enumerate() {
        let Some(op) = graph.op(node).filter(|_| *member) else {
            continue;
        };
        // The default program's node of the same ID, if it copies, may already copy to more ports.
        let (fan_out_limit, limit) = raised(
            values::TEE_FAN_OUT,
            baseline.and_then(|baseline| baseline.fan_out.get(graph.id(node)).copied()),
        );
        let outputs = op.ports().outputs.len() as u64;
        if op.contract().ports == PortRelation::Copies && outputs > fan_out_limit {
            report.push(Finding::new(
                Rule::TeeFanOut,
                graph.location(node),
                format!(
                    "`{}` copies each record to {} ports, more than {}",
                    op.name(),
                    outputs,
                    limit
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
fn check_memory(
    graph: &Graph<'_>, members: &[bool], artifact: &str, baseline: Option<&Baseline>, report: &mut ValidationReport,
) {
    let (limit, named) = raised(values::ARTIFACT_MEMORY, baseline.map(|baseline| baseline.memory));
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
            "the nodes of {} declare {} bytes of memory, more than {}, and this node declares {} of them; reduce \
             its cache and interner sizes",
            artifact, total, named, bytes
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

fn check_work_per_input(
    graph: &Graph<'_>, members: &[bool], baseline: Option<&Baseline>, report: &mut ValidationReport,
) {
    let Some(visits) = visits(graph, members) else {
        return;
    };
    for node in entry_nodes(graph, members) {
        // The default program's work from the same entry node may already be more.
        let (limit, named) = raised(
            values::WORK_PER_INPUT,
            baseline.and_then(|baseline| baseline.work.get(graph.id(node)).copied()),
        );
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
                    "one record entering the artifact here can cause {} node visits, more than {}; reduce how often \
                     records are copied",
                    visits, named
                ),
            ));
        }
    }
}

/// Reports every node of a program whose operator a program may not hold (`trusted_host`).
///
/// A program is every node of the scope its artifact replaces, kept as the default program has it or not: what a node
/// may be does not depend on whether the author changed it. So an artifact cannot hold a source or an opaque host even
/// unmodified, and a scope that holds one cannot be replaced, which no host's default program does.
fn check_programs(graph: &Graph<'_>, programs: &[(ComposedArtifact, BTreeSet<NodeId>)], report: &mut ValidationReport) {
    let program = programs.iter().flat_map(|(_, nodes)| nodes).collect::<BTreeSet<_>>();
    for (node, entry) in graph.nodes.iter().enumerate() {
        let Some(op) = &entry.op else { continue };
        if program.contains(graph.id(node)) && !op.def().authorable() {
            report.push(Finding::new(
                Rule::TrustedHost,
                graph.location(node),
                format!(
                    "`{}` runs in a host component, which only native configuration can place; a program cannot add, \
                     change, or hold host components",
                    op.name()
                ),
            ));
        }
    }
}
