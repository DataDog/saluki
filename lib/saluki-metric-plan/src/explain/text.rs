//! The text rendering of explain documents.

use std::{collections::BTreeMap, fmt::Write as _};

use serde_json::Value;

use super::{BoundEntry, BoundScopeEntry, Explain, PhysicalPlan, ResourcesEntry, RewriteEntry};
use crate::{
    canonical::{to_canonical_string, PlanHash},
    ir::{ParamName, PayloadClasses, Provenance, ResourceProvenance, Selector},
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

    if let Some(resources) = &explain.resources {
        write_resources(resources, out)?;
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

/// Says who declared a resource, and, once compared with the active plan, whether it is reused or new.
fn resource_origin(provenance: &ResourceProvenance, change: &Option<String>) -> String {
    let declared = match provenance {
        ResourceProvenance::Native => "native".to_string(),
        ResourceProvenance::Declared(revision) => {
            format!("resources revision {} {}", revision.revision, revision.hash)
        }
    };
    match change {
        Some(change) => format!("{}, {}", change, declared),
        None => declared,
    }
}

/// Writes parameters on one line as a JSON object, or `none`.
fn params_line(params: &BTreeMap<ParamName, Value>) -> String {
    if params.is_empty() {
        return "none".to_string();
    }
    let object = params
        .iter()
        .map(|(name, value)| (name.to_string(), value.clone()))
        .collect::<serde_json::Map<_, _>>();
    json(&Value::Object(object))
}

fn write_resources(resources: &ResourcesEntry, out: &mut String) -> std::fmt::Result {
    writeln!(out)?;
    writeln!(out, "resources")?;
    writeln!(out, "  bound outputs ({})", resources.outputs.len())?;
    for output in &resources.outputs {
        writeln!(
            out,
            "    {} ({})",
            output.id,
            resource_origin(&output.provenance, &output.change)
        )?;
        writeln!(out, "      host {}", output.host)?;
        let selector = match &output.selector {
            Selector::Role(role) => format!("role {}", role),
            Selector::Group(key) => format!("group {}", key),
            Selector::Declared(recipients) => {
                let recipients = recipients.iter().map(ToString::to_string).collect::<Vec<_>>();
                format!("declared {}", list(&recipients))
            }
        };
        writeln!(out, "      selector {}", selector)?;
        writeln!(out, "      encoder {}", params_line(&output.encoder))?;
        writeln!(out, "      telemetry {}", output.telemetry)?;
    }
    writeln!(out, "  hosts ({})", resources.hosts.len())?;
    for host in &resources.hosts {
        match &host.change {
            Some(change) => writeln!(out, "    {} ({})", host.host, change)?,
            None => writeln!(out, "    {}", host.host)?,
        }
        writeln!(out, "      settings {}", params_line(&host.settings))?;
    }
    writeln!(out, "  recipients ({})", resources.recipients.len())?;
    for recipient in &resources.recipients {
        writeln!(
            out,
            "    {} ({})",
            recipient.id,
            resource_origin(&recipient.provenance, &recipient.change)
        )?;
        writeln!(out, "      host {}, endpoint {}", recipient.host, recipient.endpoint)?;
        let classes = match recipient.classes {
            PayloadClasses::All => "all",
            PayloadClasses::MetricsOnly => "metrics only",
        };
        writeln!(out, "      credential {}, classes {}", recipient.credential, classes)?;
        writeln!(out, "      protocol {}", params_line(&recipient.protocol))?;
        let delivery = serde_json::to_value(recipient.delivery).unwrap_or(Value::Null);
        writeln!(
            out,
            "      delivery {}",
            delivery.as_str().map_or_else(|| json(&delivery), str::to_string)
        )?;
        let outputs = recipient.outputs.iter().map(ToString::to_string).collect::<Vec<_>>();
        writeln!(out, "      outputs {}", list(&outputs))?;
    }
    let removed = &resources.removed;
    if !removed.is_empty() {
        let names = |items: Vec<String>| list(&items);
        writeln!(out, "  removed")?;
        writeln!(
            out,
            "    outputs {}",
            names(removed.outputs.iter().map(ToString::to_string).collect())
        )?;
        writeln!(
            out,
            "    hosts {}",
            names(removed.hosts.iter().map(ToString::to_string).collect())
        )?;
        writeln!(
            out,
            "    recipients {}",
            names(removed.recipients.iter().map(ToString::to_string).collect())
        )?;
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

/// Renders the physical sections of a document alone as text, as [`render`] renders them after the logical ones.
pub(super) fn render_physical(physical: &PhysicalPlan) -> String {
    let mut out = String::new();
    // Writing to a `String` cannot fail.
    let _ = write_physical(physical, &mut out);
    // The sections start with the blank line that separates them from the logical ones in a whole document.
    match out.strip_prefix('\n') {
        Some(alone) => alone.to_string(),
        None => out,
    }
}

fn write_physical(physical: &PhysicalPlan, out: &mut String) -> std::fmt::Result {
    writeln!(out)?;
    writeln!(out, "frame")?;
    if let Some(layout) = &physical.layout {
        writeln!(
            out,
            "  laid out on {}, {}, and {}, {} R1's pipelining",
            count(usize::from(layout.dogstatsd_lanes), "DogStatsD producer lane"),
            count(usize::from(layout.state_lanes), "state lane"),
            count(usize::from(layout.post_lanes), "post-aggregation lane"),
            if layout.pipelining { "with" } else { "without" }
        )?;
    }
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
                "      {} {} ({}{}) -> {}{}{}",
                points("admission", point.id, point.lanes),
                point.port,
                point.mode,
                lanes_suffix(point.lanes),
                point.to.as_deref().unwrap_or("nothing"),
                routing_suffix(&point.routing),
                if point.feeds_state { ", feeds state" } else { "" }
            )?;
        }
    }
    writeln!(out, "  slots ({})", physical.slots.len())?;
    for slot in &physical.slots {
        // R1 never fuses an isolation slot, so its cost model does not price one.
        match &slot.isolation {
            Some(_) => writeln!(
                out,
                "    {} (not priced: R1 never fuses an isolation slot){}",
                slot.name,
                lanes_suffix(slot.lanes)
            )?,
            None => writeln!(
                out,
                "    {} (estimated {} ns per event){}",
                slot.name,
                slot.cost,
                lanes_suffix(slot.lanes)
            )?,
        }
        if let Some(isolation) = &slot.isolation {
            let upstream = isolation.upstream.iter().map(ToString::to_string).collect::<Vec<_>>();
            let output = if isolation.output_points.is_empty() {
                format!("output point {}", isolation.output_point)
            } else {
                let lanes = isolation
                    .output_points
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>();
                format!("output points {}", list(&lanes))
            };
            writeln!(
                out,
                "      isolation slot, {}, watermark of admission points {}",
                output,
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
            let routing = if exit.key_groups.is_empty() {
                routing_suffix(&exit.routing).to_string()
            } else {
                let groups = exit
                    .key_groups
                    .iter()
                    .map(|owned| format!("{} to {} on lane {}", owned.first, owned.last, owned.lane))
                    .collect::<Vec<_>>();
                format!("{} (key groups {})", routing_suffix(&exit.routing), list(&groups))
            };
            writeln!(
                out,
                "      exit {} {} -> {}{}{}{}",
                exit.index,
                exit.name,
                exit.to,
                routing,
                if exit.idle { ", idle" } else { "" },
                progress
            )?;
        }
    }
    writeln!(out, "  edges ({})", physical.edges.len())?;
    for edge in &physical.edges {
        writeln!(
            out,
            "    {} -> {} ({}{})",
            edge.from,
            edge.to,
            edge.kind,
            routing_suffix(&edge.routing)
        )?;
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
        let kind = if segment.mode == "time-driven" {
            "output"
        } else {
            "admission"
        };
        writeln!(
            out,
            "  {} {} ({})",
            points(kind, segment.admission_point, segment.lanes),
            segment.from,
            segment.mode
        )?;
        writeln!(out, "    slots {}", list(&segment.slots))?;
        let nodes = segment.nodes.iter().map(ToString::to_string).collect::<Vec<_>>();
        writeln!(out, "    nodes {}", list(&nodes))?;
        writeln!(out, "    ends {}", list(&segment.ends))?;
    }

    writeln!(out)?;
    writeln!(out, "output bindings ({})", physical.output_bindings.len())?;
    for binding in &physical.output_bindings {
        match &binding.encode_host {
            Some(host) => writeln!(out, "  {}: a lease in {}", binding.output, host)?,
            None if binding.removed => writeln!(
                out,
                "  {}: removed, the plan no longer declares it; below as the boot plan declares it",
                binding.output
            )?,
            None => writeln!(out, "  {}", binding.output)?,
        }
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

/// Names the points of one output or slot, of `kind` (`admission` or `output`): `<kind> point <id>` for one lane, and
/// `<kind> points <first> to <last>` for its consecutive lanes' points.
fn points(kind: &str, first: u32, lanes: u16) -> String {
    match lanes {
        0 | 1 => format!("{} point {}", kind, first),
        lanes => format!("{} points {} to {}", kind, first, first + u32::from(lanes) - 1),
    }
}

/// Says how many lanes something runs on, after what it describes: nothing for one lane.
fn lanes_suffix(lanes: u16) -> String {
    match lanes {
        0 | 1 => String::new(),
        lanes => format!(", {} lanes", lanes),
    }
}

/// Says how an edge reaches the lanes of its target, after the target: nothing when it reaches one lane.
fn routing_suffix(routing: &str) -> &'static str {
    match routing {
        "fixed" => ", lane by lane",
        "keyed" => ", keyed",
        "affinity" => ", affinity",
        "spread" => ", spread",
        _ => "",
    }
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
