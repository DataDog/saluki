//! Human-readable `explain` renderer for [`MetricPlan`].
//!
//! Renders topological execution order, operator summaries, effect contracts, wiring edges, validation results, and
//! telemetry-contract warnings so operators and maintainers can inspect any lowered or custom program before running
//! it.

use std::{collections::BTreeMap, fmt::Write as _};

use crate::{
    effects::operator_spec,
    ir::{MetricPlan, Node, NodeId, Op, RouteFilterKind},
    telemetry_check::{analyze_telemetry_contracts, TelemetryContract},
    validate::validate_plan,
};

/// Produces a human-readable explanation of `plan`, including validation status, topological stages, effect
/// contracts, edges, and telemetry-contract warnings.
pub fn explain_plan(plan: &MetricPlan, contracts: &[TelemetryContract]) -> String {
    let mut out = String::new();

    let title = plan.description.as_deref().unwrap_or("unnamed-plan");
    let _ = writeln!(out, "MetricPlan v{} ({title})", plan.version);

    let validation = validate_plan(plan);
    match &validation {
        Ok(validated) => {
            let sources: Vec<&str> = plan
                .nodes
                .iter()
                .filter_map(|n| match &n.op {
                    Op::Source { stream } => Some(stream.as_str()),
                    _ => None,
                })
                .collect();
            let emits: Vec<&str> = plan
                .nodes
                .iter()
                .filter_map(|n| match &n.op {
                    Op::Emit { target } => Some(target.as_str()),
                    _ => None,
                })
                .collect();
            let _ = writeln!(
                out,
                "Status: VALID ({} nodes, {} edges, {} stateful)",
                plan.nodes.len(),
                plan.edges.len(),
                validated.stateful_node_count()
            );
            let _ = writeln!(out, "Sources: {}", sources.join(", "));
            let _ = writeln!(out, "Targets: {}", emits.join(", "));
        }
        Err(errors) => {
            let _ = writeln!(out, "Status: INVALID ({} error(s))", errors.len());
            for err in errors {
                let _ = writeln!(out, "  - error: {err}");
            }
        }
    }

    let ordered_nodes: Vec<&Node> = match &validation {
        Ok(validated) => validated.nodes_in_topo_order(),
        Err(_) => plan.nodes.iter().collect(),
    };

    let mut outgoing: BTreeMap<&NodeId, Vec<String>> = BTreeMap::new();
    for edge in &plan.edges {
        let label = if edge.from_port == crate::ir::DEFAULT_OUT_PORT {
            edge.to.to_string()
        } else {
            format!("{}->{}", edge.from_port, edge.to)
        };
        outgoing.entry(&edge.from).or_default().push(label);
    }

    let _ = writeln!(out, "\nNodes (topological order):");
    for (idx, node) in ordered_nodes.iter().enumerate() {
        let spec = operator_spec(&node.op);
        let detail = summarize_op(&node.op);
        let next = outgoing
            .get(&node.id)
            .map(|list| list.join(", "))
            .unwrap_or_else(|| "-".to_string());

        let mut flags = Vec::new();
        if spec.stateful {
            flags.push("stateful");
        }
        if spec.may_drop {
            flags.push("may_drop");
        }
        let flag_str = if flags.is_empty() {
            "pure".to_string()
        } else {
            flags.join(",")
        };

        let _ = writeln!(
            out,
            "  {:>2}. {:<26} {:<22} [{flag_str}] reads=({}) writes=({}) {detail} => [{next}]",
            idx + 1,
            node.id,
            spec.name,
            spec.reads,
            spec.writes
        );
    }

    let warnings = analyze_telemetry_contracts(plan, contracts);
    if warnings.is_empty() {
        let _ = writeln!(out, "\nTelemetry contracts: OK (0 warnings)");
    } else {
        let _ = writeln!(out, "\nTelemetry contract warnings ({}):", warnings.len());
        for warning in warnings {
            let _ = writeln!(out, "  - warning: {warning}");
        }
    }

    out
}

fn summarize_op(op: &Op) -> String {
    match op {
        Op::Source { stream } => format!("stream={stream}"),
        Op::Map { profiles } => format!("profiles={}", profiles.len()),
        Op::PrefixFilter {
            metric_prefix,
            metric_prefix_blocklist,
            filter,
        } => format!(
            "prefix={metric_prefix:?} blocklist={} filter_values={} prefix_rules={}",
            metric_prefix_blocklist.len(),
            filter.values.len(),
            filter.prefix_rules.len()
        ),
        Op::TimestampSplit => "ports=(aggregate,passthrough)".to_string(),
        Op::TagFilter {
            entries,
            value_allowlists,
        } => format!("key_rules={} value_rules={}", entries.len(), value_allowlists.len()),
        Op::Aggregate {
            window_duration_secs,
            context_limit,
            counter_expiry_secs,
            flush_open_windows,
            histogram,
        } => format!(
            "window={}s limit={context_limit} expiry={counter_expiry_secs}s flush_open={flush_open_windows} hist_stats={}",
            window_duration_secs.get(),
            histogram.statistics().len()
        ),
        Op::PostAggregateFilter {
            filter,
            histogram_aggregates,
            histogram_percentiles,
        } => format!(
            "filter_values={} agg_suffixes={} pct_suffixes={}",
            filter.values.len(),
            histogram_aggregates.len(),
            histogram_percentiles.len()
        ),
        Op::EnrichHostTags { binding } => format!("binding={binding}"),
        Op::RouteFilter { kind } => match kind {
            RouteFilterKind::Allowlist { names, prefixes } => {
                format!("allowlist(names={},prefixes={})", names.len(), prefixes.len())
            }
            RouteFilterKind::Mirroring {
                enabled,
                policy,
                signal_binding,
            } => format!(
                "mirroring(enabled={enabled},active={},allowlist={},binding={signal_binding:?})",
                policy.enabled,
                policy.allowlist.len()
            ),
            RouteFilterKind::SeriesAllowlist { enabled, metrics } => {
                format!("series_allowlist(enabled={enabled},metrics={})", metrics.len())
            }
        },
        Op::Merge => "fan-in".to_string(),
        Op::Tee => "fan-out".to_string(),
        Op::Emit { target } => format!("target={target}"),
        Op::Drop => "discard".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lowering::{lower_native_pipeline, NativePipelineConfig};

    #[test]
    fn explain_renders_readable_plan_summary() {
        let plan = lower_native_pipeline(&NativePipelineConfig::default());
        let explanation = explain_plan(&plan, &[TelemetryContract::default_internal_telemetry()]);

        assert!(explanation.contains("Status: VALID"));
        assert!(explanation.contains("dsd_agg"));
        assert!(explanation.contains("stateful"));
        assert!(explanation.contains("Telemetry contracts: OK"));
    }
}
