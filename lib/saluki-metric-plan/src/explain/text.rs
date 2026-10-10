//! The text rendering of explain documents.

use std::fmt::Write as _;

use serde_json::Value;

use super::{BoundEntry, BoundScopeEntry, Explain, PhysicalPlan, RewriteEntry};
use crate::{
    canonical::{to_canonical_string, PlanHash},
    ir::Provenance,
    model::PortType,
    validate::describe_port_type,
};

/// Renders a document as text: one section per part of the document, each item on its own lines, in document order.
pub(super) fn render(explain: &Explain) -> String {
    let mut out = String::new();
    // Writing to a `String` cannot fail.
    let _ = write_document(explain, &mut out);
    out
}

fn write_document(explain: &Explain, out: &mut String) -> std::fmt::Result {
    writeln!(out, "explain_version {}", explain.explain_version)?;

    let plan = &explain.plan;
    writeln!(out)?;
    writeln!(out, "plan")?;
    writeln!(out, "  ir_version {}", plan.ir_version)?;
    writeln!(out, "  hash {}", hash(&plan.hash))?;
    writeln!(out, "  structural_hash {}", hash(&plan.structural_hash))?;
    if let Some(provenance) = &plan.provenance {
        if let Some(change) = provenance.config_change {
            writeln!(out, "  config_change {}", change)?;
        }
        for artifact in &provenance.artifacts {
            writeln!(out, "  artifact revision {} {}", artifact.revision, artifact.hash)?;
        }
    }
    let errors = explain.findings.iter().filter(|f| f.severity == "error").count();
    let warnings = explain.findings.len() - errors;
    writeln!(
        out,
        "  {}: {}, {}",
        if plan.valid { "valid" } else { "invalid" },
        count(errors, "error"),
        count(warnings, "warning")
    )?;

    let graph = &explain.graph;
    writeln!(out)?;
    writeln!(out, "nodes ({})", graph.nodes.len())?;
    for node in &graph.nodes {
        write!(out, "  {}: {}", node.id, node.op)?;
        match (&node.boundary, &node.state) {
            (Some(boundary), Some(state)) => write!(out, " [{}, state {}]", boundary, state)?,
            _ => write!(out, " [not configured]")?,
        }
        match &node.provenance {
            Some(Provenance::Native(component)) => write!(out, " native {}", component)?,
            Some(Provenance::Artifact(artifact)) => {
                write!(out, " artifact revision {} {}", artifact.revision, artifact.hash)?
            }
            None => write!(out, " no provenance")?,
        }
        writeln!(out)?;
        if !node.loss.is_empty() {
            writeln!(out, "    loss {}", node.loss.join(", "))?;
        }
        for (name, value) in &node.params {
            writeln!(out, "    param {} = {}", name, json(value))?;
        }
        for binding in &node.bindings {
            let resolved = binding.resolved.as_ref().map_or_else(|| "unresolved".to_string(), json);
            writeln!(
                out,
                "    binding {} <- {} = {}",
                binding.param, binding.binding, resolved
            )?;
        }
        for input in &node.inputs {
            let from = input
                .from
                .as_ref()
                .map_or_else(|| "nothing".to_string(), ToString::to_string);
            writeln!(
                out,
                "    in {} <- {}, accepts {}",
                input.name,
                from,
                describe_port_type(&input.accepts)
            )?;
        }
        for output in &node.outputs {
            let to = output
                .to
                .as_ref()
                .map_or_else(|| "nothing".to_string(), ToString::to_string);
            writeln!(
                out,
                "    out {} -> {}, carries {}",
                output.name,
                to,
                port_type(&output.port_type)
            )?;
        }
    }

    writeln!(out)?;
    writeln!(out, "edges ({})", graph.edges.len())?;
    for edge in &graph.edges {
        writeln!(out, "  {} -> {}: {}", edge.from, edge.to, port_type(&edge.port_type))?;
    }

    writeln!(out)?;
    writeln!(out, "outputs ({})", graph.outputs.len())?;
    for output in &graph.outputs {
        writeln!(out, "  {}", output.id)?;
        writeln!(out, "    accepts {}", describe_port_type(&output.accepts))?;
        writeln!(out, "    fingerprint {}", output.fingerprint)?;
        let recipients = output.recipients.iter().map(ToString::to_string).collect::<Vec<_>>();
        writeln!(out, "    recipients {}", list(&recipients))?;
        let delivery = serde_json::to_value(output.delivery).unwrap_or(Value::Null);
        writeln!(
            out,
            "    delivery {}",
            delivery.as_str().map_or_else(|| json(&delivery), str::to_string)
        )?;
        let emitters = output.emitted_by.iter().map(ToString::to_string).collect::<Vec<_>>();
        writeln!(out, "    emitted by {}", list(&emitters))?;
    }

    writeln!(out)?;
    writeln!(out, "scopes ({})", graph.scopes.len())?;
    for scope in &graph.scopes {
        let inputs = scope
            .inputs
            .iter()
            .map(|(input, port)| format!("{} = {}", input, port))
            .collect::<Vec<_>>();
        if scope.exits.is_empty() {
            writeln!(out, "  {}: {}", scope.name, list(&inputs))?;
        } else {
            let exits = scope
                .exits
                .iter()
                .map(|(exit, port)| format!("{} = {}", exit, port))
                .collect::<Vec<_>>();
            writeln!(out, "  {}: {}; exits {}", scope.name, list(&inputs), list(&exits))?;
        }
    }

    writeln!(out)?;
    writeln!(out, "bounds ({})", explain.bounds.len())?;
    for bound in &explain.bounds {
        writeln!(out, "  {}", bound_line(bound))?;
    }

    writeln!(out)?;
    writeln!(out, "findings ({})", explain.findings.len())?;
    for finding in &explain.findings {
        writeln!(
            out,
            "  {}[{}] {}: {}",
            finding.severity, finding.rule, finding.location, finding.message
        )?;
    }
    if let Some(rewrites) = &explain.rewrites {
        write_rewrites(rewrites, out)?;
    }
    if let Some(physical) = &explain.physical {
        write_physical(physical, out)?;
    }
    Ok(())
}

fn write_rewrites(rewrites: &[RewriteEntry], out: &mut String) -> std::fmt::Result {
    writeln!(out)?;
    writeln!(out, "rewrites ({})", rewrites.len())?;
    for rewrite in rewrites {
        let nodes = rewrite.nodes.iter().map(ToString::to_string).collect::<Vec<_>>();
        write!(out, "  {} {}: {}", rewrite.id, rewrite.outcome, list(&nodes))?;
        if rewrite.capacity_rule {
            write!(out, " [capacity rule]")?;
        }
        writeln!(out)?;
        if let Some(reason) = &rewrite.reason {
            writeln!(out, "    reason {}", reason)?;
        }
        writeln!(out, "    justification {}", rewrite.justification)?;
        for effect in &rewrite.telemetry {
            writeln!(
                out,
                "    telemetry {} {}: {}",
                effect.change, effect.node, effect.detail
            )?;
        }
    }
    Ok(())
}

fn write_physical(physical: &PhysicalPlan, out: &mut String) -> std::fmt::Result {
    writeln!(out)?;
    writeln!(out, "frame")?;
    writeln!(out, "  fixed ({})", physical.fixed.len())?;
    for fixed in &physical.fixed {
        write!(out, "    {}: {} [{}]", fixed.node, fixed.op, fixed.boundary)?;
        if let Some(input) = &fixed.input {
            write!(out, ", fed by {}", input)?;
        }
        if fixed.idle {
            write!(out, ", idle")?;
        }
        writeln!(out)?;
        for point in &fixed.admission_points {
            writeln!(
                out,
                "      admission point {} {} ({}) -> {}{}",
                point.id,
                point.port,
                point.mode,
                point.to.as_deref().unwrap_or("nothing"),
                if point.feeds_state { ", feeds state" } else { "" }
            )?;
        }
    }
    writeln!(out, "  slots ({})", physical.slots.len())?;
    for slot in &physical.slots {
        // R1 never fuses an isolation slot, so its cost model does not price one.
        match &slot.isolation {
            Some(_) => writeln!(out, "    {} (not priced: R1 never fuses an isolation slot)", slot.name)?,
            None => writeln!(out, "    {} (estimated {} ns per event)", slot.name, slot.cost)?,
        }
        if let Some(isolation) = &slot.isolation {
            let upstream = isolation.upstream.iter().map(ToString::to_string).collect::<Vec<_>>();
            writeln!(
                out,
                "      isolation slot, output point {}, watermark of admission points {}",
                isolation.output_point,
                list(&upstream)
            )?;
        }
        let nodes = slot.nodes.iter().map(ToString::to_string).collect::<Vec<_>>();
        writeln!(out, "      nodes {}", list(&nodes))?;
        for entry in &slot.entries {
            writeln!(out, "      {}: {} -> {}", entry.key, entry.from, entry.input)?;
        }
        for exit in &slot.exits {
            let progress = if exit.progress_points.is_empty() {
                String::new()
            } else {
                let points = exit.progress_points.iter().map(ToString::to_string).collect::<Vec<_>>();
                format!(", progress of admission points {}", list(&points))
            };
            writeln!(
                out,
                "      exit {} {} -> {}{}{}",
                exit.index,
                exit.name,
                exit.to,
                if exit.idle { ", idle" } else { "" },
                progress
            )?;
        }
    }
    writeln!(out, "  edges ({})", physical.edges.len())?;
    for edge in &physical.edges {
        writeln!(out, "    {} -> {} ({})", edge.from, edge.to, edge.kind)?;
    }

    writeln!(out)?;
    writeln!(out, "fusion ({})", physical.fusion.len())?;
    for decision in &physical.fusion {
        writeln!(
            out,
            "  {} -> {}: {} ({})",
            decision.from, decision.to, decision.decision, decision.reason
        )?;
    }

    writeln!(out)?;
    writeln!(out, "segments ({})", physical.segments.len())?;
    for segment in &physical.segments {
        // An isolation slot's output point stamps time-driven output; it admits nothing.
        let point = if segment.mode == "time-driven" {
            "output point"
        } else {
            "admission point"
        };
        writeln!(
            out,
            "  {} {} {} ({})",
            point, segment.admission_point, segment.from, segment.mode
        )?;
        writeln!(out, "    slots {}", list(&segment.slots))?;
        let nodes = segment.nodes.iter().map(ToString::to_string).collect::<Vec<_>>();
        writeln!(out, "    nodes {}", list(&nodes))?;
        writeln!(out, "    ends {}", list(&segment.ends))?;
    }

    writeln!(out)?;
    writeln!(out, "output bindings ({})", physical.output_bindings.len())?;
    for binding in &physical.output_bindings {
        writeln!(out, "  {}", binding.output)?;
        let recipients = binding.recipients.iter().map(ToString::to_string).collect::<Vec<_>>();
        writeln!(out, "    recipients {}", list(&recipients))?;
        let delivery = serde_json::to_value(binding.delivery).unwrap_or(Value::Null);
        writeln!(
            out,
            "    delivery {} (overflow {}, closure {})",
            delivery.as_str().map_or_else(|| json(&delivery), str::to_string),
            binding.overflow,
            binding.closure
        )?;
        writeln!(out, "    fed by {}", list(&binding.fed_by))?;
        writeln!(out, "    idle {}", list(&binding.idle))?;
    }
    Ok(())
}

fn bound_line(bound: &BoundEntry) -> String {
    let scope = match &bound.scope {
        BoundScopeEntry::Plan => "plan".to_string(),
        BoundScopeEntry::Artifact(artifact) => format!("artifact revision {}", artifact.revision),
    };
    let value = bound
        .value
        .map_or_else(|| "unbounded".to_string(), |value| value.to_string());
    let verdict = match (bound.enforced, bound.within) {
        (true, true) => "within the limit",
        (true, false) => "exceeds the limit",
        (false, true) => "within the limit, not enforced",
        (false, false) => "beyond the limit, not enforced",
    };
    format!(
        "{} {}: {} of {} ({})",
        scope, bound.limit, value, bound.maximum, verdict
    )
}

fn hash(hash: &Option<PlanHash>) -> String {
    hash.map_or_else(
        || "none (the plan has no canonical form)".to_string(),
        |hash| hash.to_string(),
    )
}

fn port_type(port_type: &Option<PortType>) -> String {
    port_type
        .as_ref()
        .map_or_else(|| "unknown".to_string(), describe_port_type)
}

/// Renders a JSON value compactly, in canonical form when it has one.
fn json(value: &Value) -> String {
    to_canonical_string(value).unwrap_or_else(|_| value.to_string())
}

/// Returns `count` and `noun`, plural unless `count` is one.
fn count(count: usize, noun: &str) -> String {
    if count == 1 {
        format!("1 {}", noun)
    } else {
        format!("{} {}s", count, noun)
    }
}

fn list(items: &[String]) -> String {
    if items.is_empty() {
        "none".to_string()
    } else {
        items.join(", ")
    }
}
