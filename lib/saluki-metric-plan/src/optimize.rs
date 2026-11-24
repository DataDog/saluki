//! Trivial dead-node elimination and constant folding for [`MetricPlan`].
//!
//! Deliberately stays minimal: eliminates provable no-op operators, folds statically always-dropping route filters,
//! simplifies unary `Merge`/`Tee` nodes, and prunes unreachable subgraphs. Does not reorder operators across
//! merges or stateful boundaries.

use std::collections::{BTreeMap, BTreeSet};

use saluki_metric_kernels::routing_filter::MetricAllowlistKernel;

use crate::{
    ir::{Edge, NodeId, Op, RouteFilterKind},
    validate::{validate_plan, ValidatedPlan},
};

/// Summary of nodes simplified or removed by [`optimize_plan`].
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct OptimizationReport {
    /// Node IDs eliminated because they were no-ops, unary merge/tee, or unreachable.
    pub eliminated_nodes: Vec<NodeId>,

    /// Node IDs folded into [`Op::Drop`] because they statically dropped every metric.
    pub folded_drops: Vec<NodeId>,
}

/// Applies trivial dead-node elimination and constant folding to `validated`, returning a re-validated plan and an
/// [`OptimizationReport`].
pub fn optimize_plan(validated: &ValidatedPlan) -> (ValidatedPlan, OptimizationReport) {
    let mut plan = validated.plan().clone();
    let mut report = OptimizationReport::default();

    // Iterate to a fixed point; each pass removes or folds at least one node, so termination is guaranteed.
    loop {
        let mut changed = false;

        // 1. Fold statically always-dropping RouteFilters into Op::Drop and disconnect their outgoing edges.
        let mut newly_dropped: BTreeSet<NodeId> = BTreeSet::new();
        for node in &mut plan.nodes {
            if is_static_drop_filter(&node.op) {
                node.op = Op::Drop;
                newly_dropped.insert(node.id.clone());
                report.folded_drops.push(node.id.clone());
                changed = true;
            }
        }
        if !newly_dropped.is_empty() {
            plan.edges.retain(|edge| !newly_dropped.contains(&edge.from));
        }

        // Build incoming/outgoing adjacency maps for structural simplifications.
        let mut incoming: BTreeMap<NodeId, Vec<Edge>> = BTreeMap::new();
        let mut outgoing: BTreeMap<NodeId, Vec<Edge>> = BTreeMap::new();
        for edge in &plan.edges {
            incoming.entry(edge.to.clone()).or_default().push(edge.clone());
            outgoing.entry(edge.from.clone()).or_default().push(edge.clone());
        }

        // 2. Prune non-source nodes that have become unreachable (0 incoming edges).
        let unreachable: BTreeSet<NodeId> = plan
            .nodes
            .iter()
            .filter(|node| !matches!(node.op, Op::Source { .. }))
            .filter(|node| incoming.get(&node.id).is_none_or(Vec::is_empty))
            .map(|node| node.id.clone())
            .collect();
        if !unreachable.is_empty() {
            report.eliminated_nodes.extend(unreachable.iter().cloned());
            plan.nodes.retain(|n| !unreachable.contains(&n.id));
            plan.edges
                .retain(|e| !unreachable.contains(&e.from) && !unreachable.contains(&e.to));
            continue;
        }

        // 3. Fold Tee with 0 outgoing edges into Drop.
        for node in &mut plan.nodes {
            if matches!(node.op, Op::Tee) && outgoing.get(&node.id).is_none_or(Vec::is_empty) {
                node.op = Op::Drop;
                report.folded_drops.push(node.id.clone());
                changed = true;
            }
        }
        if changed {
            continue;
        }

        // 4. Bypass a single pass-through no-op node (1 incoming edge -> 1 outgoing edge).
        let bypass_candidate = plan.nodes.iter().find_map(|node| {
            let in_edges = incoming.get(&node.id)?;
            let out_edges = outgoing.get(&node.id)?;
            if in_edges.len() == 1 && out_edges.len() == 1 && is_passthrough_noop(&node.op) {
                Some((node.id.clone(), in_edges[0].clone(), out_edges[0].clone()))
            } else {
                None
            }
        });

        if let Some((node_id, in_edge, out_edge)) = bypass_candidate {
            // Replace `in_edge -> node -> out_edge` with `in_edge.from[in_edge.from_port] -> out_edge.to`.
            plan.nodes.retain(|n| n.id != node_id);
            plan.edges.retain(|e| e.to != node_id && e.from != node_id);
            plan.edges.push(Edge::new(in_edge.from, in_edge.from_port, out_edge.to));
            report.eliminated_nodes.push(node_id);
            continue;
        }

        break;
    }

    let optimized = validate_plan(&plan).expect("optimization preserves plan validity");
    (optimized, report)
}

fn is_static_drop_filter(op: &Op) -> bool {
    let Op::RouteFilter { kind } = op else {
        return false;
    };
    match kind {
        RouteFilterKind::Allowlist { names, prefixes } => {
            MetricAllowlistKernel::for_allowlist(names, prefixes).drops_all()
        }
        RouteFilterKind::Mirroring {
            enabled,
            policy,
            signal_binding: None,
        } => MetricAllowlistKernel::for_mirroring(*enabled, policy).drops_all(),
        RouteFilterKind::Mirroring {
            enabled: false,
            signal_binding: Some(_),
            ..
        } => true,
        RouteFilterKind::SeriesAllowlist { enabled, metrics } => {
            MetricAllowlistKernel::for_series_allowlist(*enabled, metrics).drops_all()
        }
        RouteFilterKind::Mirroring {
            enabled: true,
            signal_binding: Some(_),
            ..
        } => false,
    }
}

fn is_passthrough_noop(op: &Op) -> bool {
    match op {
        Op::Map { profiles } => profiles.is_empty(),
        Op::PrefixFilter {
            metric_prefix, filter, ..
        } => metric_prefix.is_empty() && filter.is_empty(),
        Op::TagFilter {
            entries,
            value_allowlists,
        } => entries.is_empty() && value_allowlists.is_empty(),
        Op::PostAggregateFilter { filter, .. } => filter.is_empty(),
        Op::RouteFilter {
            kind:
                RouteFilterKind::Mirroring {
                    enabled: true,
                    policy,
                    signal_binding: None,
                },
        } => policy.enabled && policy.allowlist.is_empty(),
        Op::Merge | Op::Tee => true,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use saluki_core::data_model::event::metric::Metric;
    use saluki_metric_kernels::routing_filter::MetricMirroring;

    use super::*;
    use crate::{
        interpreter::{ReferenceInterpreter, RuntimeBindings},
        lowering::{lower_native_pipeline, NativePipelineConfig, STREAM_DOGSTATSD, TARGET_PRIMARY},
    };

    #[test]
    fn eliminates_noop_operators_from_default_native_plan() {
        let plan = lower_native_pipeline(&NativePipelineConfig::default());
        let validated = validate_plan(&plan).expect("valid plan");
        let before_count = validated.plan().nodes.len();

        let (optimized, report) = optimize_plan(&validated);
        assert!(optimized.plan().nodes.len() < before_count);
        assert!(report.eliminated_nodes.contains(&NodeId::from("dsd_mapper")));
        assert!(report.eliminated_nodes.contains(&NodeId::from("dsd_prefix_filter")));
        assert!(report.eliminated_nodes.contains(&NodeId::from("dsd_tag_filterlist")));
        assert!(report.eliminated_nodes.contains(&NodeId::from("dsd_post_agg_filter")));

        // Differential check: optimized plan produces identical outputs to unoptimized plan.
        let mut raw_interp = ReferenceInterpreter::new(&validated, RuntimeBindings::default()).unwrap();
        let mut opt_interp = ReferenceInterpreter::new(&optimized, RuntimeBindings::default()).unwrap();

        let batch = vec![
            Metric::counter("req.count", 3.0),
            Metric::gauge("ts.gauge", (1000, 7.0)),
            Metric::histogram("req.latency", [1.0, 5.0]),
        ];

        let raw_imm = raw_interp.ingest(STREAM_DOGSTATSD, 12, batch.clone()).unwrap();
        let opt_imm = opt_interp.ingest(STREAM_DOGSTATSD, 12, batch).unwrap();
        assert_eq!(raw_imm.get(TARGET_PRIMARY), opt_imm.get(TARGET_PRIMARY));

        let mut raw_flush = raw_interp.flush(20, false);
        let mut opt_flush = opt_interp.flush(20, false);
        for list in raw_flush.values_mut().chain(opt_flush.values_mut()) {
            list.sort_by(|a, b| a.context().name().cmp(b.context().name()));
        }
        assert_eq!(raw_flush.get(TARGET_PRIMARY), opt_flush.get(TARGET_PRIMARY));
    }

    #[test]
    fn folds_disabled_mrf_branch_and_prunes_dead_emit() {
        let config = NativePipelineConfig {
            mrf_branch_configured: true,
            mrf_enabled: false,
            mrf_mirroring: MetricMirroring {
                enabled: false,
                allowlist: vec![],
            },
            ..NativePipelineConfig::default()
        };
        let validated = validate_plan(&lower_native_pipeline(&config)).unwrap();
        let (optimized, report) = optimize_plan(&validated);

        assert!(report.folded_drops.contains(&NodeId::from("mrf_metrics_gateway")));
        assert!(report.eliminated_nodes.contains(&NodeId::from("mrf_out")));
        assert!(optimized.plan().node(&NodeId::from("mrf_out")).is_none());
    }
}
