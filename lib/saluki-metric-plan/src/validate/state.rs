//! The rules on state owners: bucket widths that share a boundary often enough for a cut, a timestamp split that gives
//! rates the interval of the aggregation it feeds, and context limits within the host's state budget.
//!
//! Which widths a state owner may use at all is its operator's parameter check (an allowed bucket width); these rules
//! relate the state owners of one plan to each other and to the host.

use std::{collections::BTreeMap, time::Duration};

use super::{graph::Graph, Catalog, Finding, Rule, ValidationReport};
use crate::{
    ir::{ParamValue, Provenance},
    operators::{names, ConfiguredOperator, KeyedState, StateContract, StateTime, DEFAULT_PORT},
};

/// The widest bucket that any number of a plan's state owners may share: a minute, which every allowed width up to it
/// divides.
const SHARED_WIDTH_LIMIT: Duration = Duration::from_secs(60);

pub(super) fn check(graph: &Graph<'_>, catalog: Option<&dyn Catalog>, report: &mut ValidationReport) {
    check_widths_above_sixty(graph, report);
    check_split_windows(graph, report);
    if let Some(capacity) = catalog.and_then(|catalog| catalog.state_budget()) {
        check_state_budget(graph, capacity, report);
    }
}

/// Returns the keyed state of a node's operator, if it keeps state by key.
fn keyed(op: &ConfiguredOperator) -> Option<&KeyedState> {
    match &op.contract().state {
        StateContract::Keyed(state) => Some(state),
        _ => None,
    }
}

/// Returns the bucket width of a state owner, if it buckets by time and the width is known.
fn bucket_width(op: &ConfiguredOperator) -> Option<Duration> {
    match keyed(op)?.time {
        StateTime::Buckets { width } => width,
    }
}

/// `widths_above_sixty`: a plan's state owners use at most one bucket width above 60 seconds, so that the least
/// common multiple of its widths is the larger of 60 seconds and its largest width, and a cut between its widths waits
/// at most that long.
fn check_widths_above_sixty(graph: &Graph<'_>, report: &mut ValidationReport) {
    // The first node, in plan order, of each width above 60 seconds.
    let mut first = BTreeMap::new();
    for node in 0..graph.nodes.len() {
        let Some(width) = graph.op(node).and_then(bucket_width) else {
            continue;
        };
        if width > SHARED_WIDTH_LIMIT {
            first.entry(width).or_insert(node);
        }
    }
    let mut widths = first.into_iter().collect::<Vec<_>>();
    widths.sort_by_key(|&(_, node)| node);
    let Some(&(kept, kept_node)) = widths.first() else {
        return;
    };
    for &(width, node) in &widths[1..] {
        report.push(Finding::new(
            Rule::WidthsAboveSixty,
            graph.location(node),
            format!(
                "the node buckets by {} s, and `{}` by {} s: a plan uses at most one bucket width above 60 seconds, so \
                 that its widths line up at least every max(60 s, largest width); use {} s here too, or a width of at \
                 most 60 seconds",
                width.as_secs(),
                graph.id(kept_node),
                kept.as_secs(),
                kept.as_secs()
            ),
        ));
    }
}

/// `split_window`: a `timestamp_split` gives timestamped counters the rate interval of the aggregation its
/// untimestamped points go to, so that a series leaves as rates of one interval whether or not its points were
/// timestamped.
fn check_split_windows(graph: &Graph<'_>, report: &mut ValidationReport) {
    for node in 0..graph.nodes.len() {
        let Some(op) = graph.op(node).filter(|op| op.name() == names::TIMESTAMP_SPLIT) else {
            continue;
        };
        let Some(default) = op
            .ports()
            .outputs
            .iter()
            .position(|port| port.name.as_str() == DEFAULT_PORT)
        else {
            continue;
        };
        for owner in state_owners_below(graph, node, default) {
            let Some(owner_op) = graph.op(owner) else { continue };
            if same_window(op.params().get("window"), owner_op.params().get("window"))
                || owner_op.name() != names::AGGREGATE
            {
                continue;
            }
            let (Some(window), Some(width)) = (op.params().duration("window"), bucket_width(owner_op)) else {
                continue;
            };
            report.push(Finding::new(
                Rule::SplitWindow,
                graph.location(node),
                format!(
                    "the split gives timestamped counters a rate interval of {} s, but `{}`, which aggregates its \
                     untimestamped points, buckets by {} s: set the split's `window` to the aggregation's width, so \
                     that every rate of a series leaves with one interval",
                    window.as_secs(),
                    graph.id(owner),
                    width.as_secs()
                ),
            ));
        }
    }
}

/// Returns `true` if two `window` parameters are known to be equal: equal known values, or bindings to the same
/// configuration, which always have the same value.
fn same_window(split: Option<&ParamValue>, owner: Option<&ParamValue>) -> bool {
    match (split, owner) {
        (Some(ParamValue::Config { binding: a, .. }), Some(ParamValue::Config { binding: b, .. })) if a == b => true,
        (Some(split), Some(owner)) => match (split.known(), owner.known()) {
            (Some(split), Some(owner)) => split == owner,
            // A value not known yet cannot be compared; lowering resolves it, and the plan it lowers is checked.
            _ => true,
        },
        _ => true,
    }
}

/// Returns the state owners that the records leaving one output port of `node` reach first: the nodes that keep state
/// by key on every path from the port, without passing another, in plan order.
fn state_owners_below(graph: &Graph<'_>, node: usize, port: usize) -> Vec<usize> {
    let mut owners = Vec::new();
    let mut visited = vec![false; graph.nodes.len()];
    let mut pending = graph
        .target(node, port)
        .map(|(target, _)| target)
        .into_iter()
        .collect::<Vec<_>>();
    while let Some(next) = pending.pop() {
        if std::mem::replace(&mut visited[next], true) {
            continue;
        }
        let Some(op) = graph.op(next) else { continue };
        if keyed(op).is_some() {
            owners.push(next);
            continue;
        }
        pending.extend((0..op.ports().outputs.len()).filter_map(|port| graph.target(next, port).map(|(to, _)| to)));
    }
    owners.sort_unstable();
    owners
}

/// `state_budget`: an artifact's state owner whose context limit exceeds what the host's state owners hold together
/// never reaches its limit, since every lineage of every state owner draws on one budget of that size.
fn check_state_budget(graph: &Graph<'_>, capacity: u64, report: &mut ValidationReport) {
    for node in 0..graph.nodes.len() {
        if !matches!(graph.nodes[node].node.provenance, Some(Provenance::Artifact(_))) {
            continue;
        }
        let Some(op) = graph.op(node).filter(|op| keyed(op).is_some()) else {
            continue;
        };
        let Some(limit) = op
            .params()
            .integer("context_limit")
            .and_then(|limit| u64::try_from(limit).ok())
        else {
            continue;
        };
        if limit > capacity {
            report.push(Finding::new(
                Rule::StateBudget,
                graph.location(node),
                format!(
                    "the node's context limit is {}, but the host's state owners hold at most {} contexts together, \
                     so it holds at most that many, and fewer while other state owners hold some; lower the limit to \
                     at most {}",
                    limit, capacity, capacity
                ),
            ));
        }
    }
}
