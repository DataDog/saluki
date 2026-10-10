//! R6: hoist a drop that every path below a `tee` applies.
//!
//! # Obligations
//!
//! - **The predicate is equivalent on all paths.** Every output port of the `tee` leads directly to a filter (an
//!   operator with `filter` cardinality, or a `split` whose one port leads directly to `drop`), and the filters remove
//!   the same records: `split` filters whose drop conditions (the predicate, or its negation when the `false` port is
//!   the one that drops) are equal in canonical form, resolved operands included, or filter operators of one operator
//!   with equal parameters. Every record the filter removes is then removed on every path, so removing it once, above
//!   the `tee`, removes it from every path as before, and every record the `tee` copies passes each path's filter.
//! - **Nothing between.** The filters are the `tee`'s direct targets, so nothing between the `tee` and a filter
//!   changes what the filter reads. R5a, which runs before R6 in every pass, moves a filter up to the `tee` across the
//!   operators it commutes with.
//! - **Native filters stay.** A host reports native series under a native node's ID; hoisting replaces the per-path
//!   filters by a new node, so no filter with native provenance is hoisted.
//! - **Scopes.** No filter crosses a scope's input or exit, and no port of a removed filter or `drop` is one, which
//!   the scope would then name although the plan no longer has it.
//! - **The frame is fixed.** The hoisted filter runs where the `tee`'s input runs; the pipeline checks that the
//!   rewritten plan places into the boot frame.
//! - **Dynamic operators** are never moved, and no drop moves across one: a dynamic filter or `tee` is left alone.
//!
//! The per-path filters would pass everything once the hoisted copy runs above the `tee`, so they are removed, and
//! the record lists their series as removed and the hoisted filter's as added: the hoisted `drop` counts each removed
//! record once, where each path's `drop` counted it before. Every output receives the same records, in the same order,
//! so R6 does not rely on the capacity rule.

use std::fmt::Write as _;

use super::{
    filters::{input_ref, output_ref, Edit, Filter, FilterForm, FilterPorts, ScopeBoundaries},
    Rewrite, RewriteContext, RewriteId, Rewritten, TelemetryChange, TelemetryEffect,
};
use crate::{
    ir::Plan,
    operators::names,
    validate::{graph::Graph, ValidationReport},
};

/// R6: hoists a drop that every path below a `tee` applies above it.
#[derive(Clone, Copy, Debug, Default)]
pub struct R6;

impl Rewrite for R6 {
    fn id(&self) -> RewriteId {
        RewriteId::R6
    }

    fn apply(&self, ctx: &RewriteContext<'_>, plan: &Plan) -> Option<Rewritten> {
        let mut findings = ValidationReport::default();
        let graph = Graph::build(plan, ctx.registry(), &mut findings);
        if !findings.is_valid() || graph.cyclic {
            return None;
        }
        let scopes = ScopeBoundaries::of(plan);
        (0..graph.nodes.len())
            .filter_map(|tee| guarded(&graph, &scopes, tee).map(|filters| (tee, filters)))
            .filter_map(|(tee, filters)| hoist(plan, &graph, tee, &filters))
            .find(|rewritten| !ctx.is_rejected(RewriteId::R6, &rewritten.nodes))
    }
}

/// Returns the filter on every port of the `tee` at `tee`, in port order, if R6's obligations hold for them.
fn guarded(graph: &Graph<'_>, scopes: &ScopeBoundaries, tee: usize) -> Option<Vec<Filter>> {
    let op = graph.op(tee)?;
    if op.name() != names::TEE || op.contract().is_dynamic() {
        return None;
    }
    // The hoisted filter must not join a scope the filters are not in: the tee's input is no scope's exit. It may be a
    // scope's input: the hoisted filter is then first in that scope, as the tee was.
    if scopes.is_exit(&input_ref(graph, (tee, 0))) {
        return None;
    }
    let filters = (0..op.ports().outputs.len())
        .map(|port| {
            let (target, _) = graph.target(tee, port)?;
            if scopes.crossed_by(&output_ref(graph, (tee, port)), &input_ref(graph, (target, 0))) {
                return None;
            }
            // No port of the filter's own nodes is a boundary: the rewrite removes them.
            Filter::at(graph, target).filter(|filter| {
                !filter.is_native(graph) && !filter.is_dynamic(graph) && !scopes.bound_at(graph, filter)
            })
        })
        .collect::<Option<Vec<_>>>()?;
    let first = filters.first()?;
    filters
        .iter()
        .all(|filter| filter.removes_what(first, graph))
        .then_some(filters)
}

/// Returns the plan with the filters below `tee` replaced by one copy above it.
fn hoist(plan: &Plan, graph: &Graph<'_>, tee: usize, filters: &[Filter]) -> Option<Rewritten> {
    let mut edit = Edit::of(plan);
    let first = filters.first()?;
    let ports = FilterPorts::of(graph, first);
    let t = graph.id(tee);
    let t_in = input_ref(graph, (tee, 0));
    let from = output_ref(graph, graph.source(tee, 0)?);

    // Each port of the tee leads where its filter kept records.
    let mut removed = Vec::new();
    for (port, filter) in filters.iter().enumerate() {
        let t_port = output_ref(graph, (tee, port));
        let kept = input_ref(graph, graph.target(filter.node, filter.keep_port())?);
        let position = edit.edge_from(&t_port)?;
        edit.set_edge(position, t_port, kept);
        removed.extend(filter.nodes().into_iter().map(|node| graph.id(node).clone()));
    }

    // One copy of the first filter above the tee.
    let f = graph.id(first.node);
    let d = first.drop().map(|drop| graph.id(drop));
    let hoisted = edit.fresh_id(f, "r6h")?;
    let hoisted_drop = match d {
        Some(d) => Some(edit.fresh_id(d, "r6h")?),
        None => None,
    };
    let mut copies = vec![edit.copy_node(f, hoisted.clone())?];
    if let (Some(d), Some(drop)) = (d, &hoisted_drop) {
        copies.push(edit.copy_node(d, drop.clone())?);
    }
    let into_tee = edit.edge_into(&t_in)?;
    edit.set_edge(into_tee, from, ports.input(&hoisted));
    let mut edges = vec![(ports.keep(&hoisted), t_in)];
    edges.extend(ports.drop_edge(&hoisted, hoisted_drop.as_ref()));
    edit.insert_edges(into_tee, edges);
    edit.place_before(t, copies);
    edit.remove_nodes(&removed);

    let mut sites = filters.iter().flat_map(Filter::nodes).collect::<Vec<_>>();
    sites.push(tee);
    sites.sort_unstable();
    let mut nodes = sites.into_iter().map(|node| graph.id(node).clone()).collect::<Vec<_>>();

    let mut telemetry = vec![TelemetryEffect {
        node: t.clone(),
        change: TelemetryChange::Changed,
        detail: "`plan_node_events_total`: every port counts only the records the hoisted filter keeps".to_string(),
    }];
    for filter in filters {
        telemetry.push(TelemetryEffect {
            node: graph.id(filter.node).clone(),
            change: TelemetryChange::Removed,
            detail: format!(
                "`plan_node_events_total`: the hoisted filter `{}` reports its counts",
                hoisted
            ),
        });
        if let Some(drop) = filter.drop() {
            telemetry.push(TelemetryEffect {
                node: graph.id(drop).clone(),
                change: TelemetryChange::Removed,
                detail: "`plan_loss_total`: the hoisted drop counts each removed record once".to_string(),
            });
        }
    }
    telemetry.push(TelemetryEffect {
        node: hoisted.clone(),
        change: TelemetryChange::Added,
        detail: "`plan_node_events_total`: the per-port counts of the hoisted filter".to_string(),
    });
    nodes.push(hoisted.clone());
    if let Some(drop) = hoisted_drop {
        telemetry.push(TelemetryEffect {
            node: drop.clone(),
            change: TelemetryChange::Added,
            detail: "`plan_loss_total`: the policy loss of the hoisted filter".to_string(),
        });
        nodes.push(drop);
    }

    Some(Rewritten {
        plan: edit.finish(),
        nodes,
        justification: justification(graph, tee, filters, &hoisted.to_string()),
        telemetry,
        capacity_rule: false,
    })
}

/// Returns the record's justification: each obligation and how it holds, then what the rewrite did.
fn justification(graph: &Graph<'_>, tee: usize, filters: &[Filter], hoisted: &str) -> String {
    let t = graph.id(tee);
    let described = filters
        .iter()
        .map(|filter| filter.describe(graph))
        .collect::<Vec<_>>()
        .join(", ");
    let mut text = format!(
        "equivalent on all paths: each of the {} ports of `{}` leads directly to a filter, {}, and they remove the \
         same records",
        filters.len(),
        t,
        described
    );
    match filters.first().map(|filter| (&filter.form, filter.drop_condition())) {
        Some((FilterForm::Split { .. }, Some(condition))) => {
            let condition = serde_json::to_string(&condition).unwrap_or_default();
            let _ = write!(text, " (drop condition {} in canonical form)", condition);
        }
        _ => {
            let _ = write!(text, " (one filter operator with equal parameters)");
        }
    }
    let _ = write!(
        text,
        "; nothing between: the filters are the `tee`'s direct targets; hoisted one copy, `{}`, above `{}`, and \
         removed the per-port filters, which would pass everything below it",
        hoisted, t
    );
    text
}

#[cfg(test)]
mod tests;
