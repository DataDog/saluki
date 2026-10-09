//! The physical sections of explain documents: the frame, its slots, fences, exits, and direct edges, rewrite R1's
//! fusion decisions, segments, and output bindings.

use std::collections::{BTreeMap, BTreeSet};

use saluki_core::execution::AdmissionMode;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::boundary_name;
use crate::{
    compile::{Frame, FrameSource, FrameTarget, FusionVerdict, Placement, Site, SlotId},
    extension::DeliveryPolicy,
    ir::{NodeId, OperatorName, OutputId, Plan, PortName, PortRef, RecipientId},
    operators::names,
};

/// How a plan runs: the frame built at boot, and where the plan's nodes run in it.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct PhysicalPlan {
    /// The fixed components: the fences of the boot plan, each run by a host component of its own.
    pub fixed: Vec<FixedEntry>,

    /// The slots, each run by one region host component.
    pub slots: Vec<SlotEntry>,

    /// The physical edges, in frame order: each admission point's, then each slot's exits.
    pub edges: Vec<PhysicalEdgeEntry>,

    /// Rewrite R1's decision for every edge of the boot plan: why it is or is not inside a slot.
    pub fusion: Vec<FusionEntry>,

    /// The segments: what each admission point's batches reach before another admission point re-admits them.
    pub segments: Vec<SegmentEntry>,

    /// Every output's binding: its recipients, delivery policy, and what feeds its encoder.
    pub output_bindings: Vec<OutputBindingEntry>,
}

/// One fixed component.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct FixedEntry {
    /// The component's node.
    pub node: NodeId,

    /// The node's operator.
    pub op: OperatorName,

    /// `io_host` or `opaque_host`.
    pub boundary: String,

    /// What feeds the component's input, if it has one, as `<component>.<output>`.
    pub input: Option<String>,

    /// Whether the explained plan leaves the component out, so that it is idle.
    pub idle: bool,

    /// The component's admission points, one per output port.
    pub admission_points: Vec<AdmissionPointEntry>,
}

/// One admission point.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct AdmissionPointEntry {
    /// The point's ID.
    pub id: u32,

    /// The output port.
    pub port: PortName,

    /// `admit` (a source stamps what is not stamped yet) or `readmit` (an opaque host stamps everything again).
    pub mode: String,

    /// Where the port's physical edge leads, or `None` if the port has none.
    pub to: Option<String>,
}

/// One slot.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct SlotEntry {
    /// The region host component's ID.
    pub name: String,

    /// The estimated cost per event of the slot's boot nodes, in nanoseconds, which R1's cost model kept within its
    /// budget. Documents written before it was added have none, read as zero.
    #[serde(default)]
    pub cost: u32,

    /// The explained plan's nodes in the slot, in plan order.
    pub nodes: Vec<NodeId>,

    /// The slot's entries in the explained plan.
    pub entries: Vec<SlotInputEntry>,

    /// The slot's exits, in order.
    pub exits: Vec<ExitEntry>,
}

/// One entry of a slot: a logical edge entering it.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct SlotInputEntry {
    /// How the slot recognizes the entry's batches: `admission point <id>` or `entry <id>`.
    pub key: String,

    /// The output port outside the slot that the edge leaves.
    pub from: PortRef,

    /// The input port inside the slot that the edge enters.
    pub input: PortRef,
}

/// One exit of a slot.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ExitEntry {
    /// The exit's index.
    pub index: u16,

    /// The exit's name: the region host's output.
    pub name: String,

    /// Where it leads: a fixed component's node, a slot, or `output <id>`.
    pub to: String,

    /// Whether the explained plan leaves it idle.
    pub idle: bool,
}

/// One physical edge.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct PhysicalEdgeEntry {
    /// Where it starts, as `<component>.<output>`.
    pub from: String,

    /// Where it leads.
    pub to: String,

    /// `exit` for a slot's exit, `admission` for an admission point's edge to a slot, and `direct` for an admission
    /// point's edge straight to a fixed component or an output.
    pub kind: String,
}

/// One fusion decision.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct FusionEntry {
    /// The output port the edge leaves.
    pub from: PortRef,

    /// The input port the edge enters.
    pub to: PortRef,

    /// `fused`, `fence`, `separated`, or `direct`.
    pub decision: String,

    /// Why.
    pub reason: String,
}

/// One segment: the part of the plan between one admission point and the next.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct SegmentEntry {
    /// The admission point's ID.
    pub admission_point: u32,

    /// The admission point's port.
    pub from: PortRef,

    /// `admit` or `readmit`.
    pub mode: String,

    /// The nodes the segment's batches reach, in plan order.
    pub nodes: Vec<NodeId>,

    /// The slots those nodes run in, in slot order.
    pub slots: Vec<String>,

    /// Where the segment ends: the fixed components it feeds, which re-admit, and the outputs it delivers to.
    pub ends: Vec<String>,
}

/// One output's binding.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct OutputBindingEntry {
    /// The output.
    pub output: OutputId,

    /// The physical recipients it delivers to.
    pub recipients: Vec<RecipientId>,

    /// Its delivery policy.
    pub delivery: DeliveryPolicy,

    /// What it does when its downstream cannot accept more.
    pub overflow: String,

    /// What it does with queued work when it closes.
    pub closure: String,

    /// The physical edges that feed its encoder in the explained plan.
    pub fed_by: Vec<String>,

    /// The exits to it that the explained plan leaves idle.
    pub idle: Vec<String>,
}

fn mode_name(mode: AdmissionMode) -> &'static str {
    match mode {
        AdmissionMode::Admit => "admit",
        AdmissionMode::Readmit => "readmit",
    }
}

impl PhysicalPlan {
    /// Describes how `plan`, placed into `frame`, runs.
    pub fn of(plan: &Plan, frame: &Frame, placement: &Placement) -> Self {
        let slot_name = |slot: SlotId| frame.describe_target(&FrameTarget::Slot(slot));
        let present = plan.nodes.iter().map(|node| &node.id).collect::<BTreeSet<_>>();

        let fixed = frame
            .fixed()
            .iter()
            .map(|fixed| FixedEntry {
                node: fixed.node.clone(),
                op: fixed.operator.clone(),
                boundary: boundary_name(fixed.boundary).to_string(),
                input: fixed.input.as_ref().map(|source| frame.describe_source(source)),
                idle: !present.contains(&fixed.node),
                admission_points: fixed
                    .admission_points
                    .iter()
                    .filter_map(|&id| frame.admission_point(id))
                    .map(|point| AdmissionPointEntry {
                        id: point.id.get(),
                        port: point.port.clone(),
                        mode: mode_name(point.mode).to_string(),
                        to: point.route.as_ref().map(|target| frame.describe_target(target)),
                    })
                    .collect(),
            })
            .collect();

        let slots = frame
            .slots()
            .iter()
            .map(|slot| {
                let placed = placement.slot(slot.id);
                SlotEntry {
                    name: slot.name.to_string(),
                    cost: slot.cost,
                    nodes: placed.map(|placed| placed.nodes.clone()).unwrap_or_default(),
                    entries: placed
                        .map(|placed| {
                            placed
                                .entries
                                .iter()
                                .map(|entry| SlotInputEntry {
                                    key: entry.key.to_string(),
                                    from: entry.from.clone(),
                                    input: entry.input.clone(),
                                })
                                .collect()
                        })
                        .unwrap_or_default(),
                    exits: slot
                        .exits
                        .iter()
                        .map(|exit| ExitEntry {
                            index: exit.index.get() as u16,
                            name: exit.name.to_string(),
                            to: frame.describe_target(&exit.target),
                            idle: placement.is_idle(slot.id, exit.index),
                        })
                        .collect(),
                }
            })
            .collect();

        let edges = frame
            .edges()
            .iter()
            .map(|edge| PhysicalEdgeEntry {
                from: frame.describe_source(&edge.from),
                to: frame.describe_target(&edge.to),
                kind: match (&edge.from, &edge.to) {
                    (FrameSource::Exit { .. }, _) => "exit",
                    (FrameSource::Admission(_), FrameTarget::Slot(_)) => "admission",
                    (FrameSource::Admission(_), _) => "direct",
                }
                .to_string(),
            })
            .collect();

        let fusion = frame
            .fusion()
            .iter()
            .map(|decision| {
                let (name, reason) = match &decision.verdict {
                    FusionVerdict::Fused { slot } => (
                        "fused",
                        format!(
                            "both nodes are synchronous and nothing separates them: slot `{}`",
                            slot_name(*slot)
                        ),
                    ),
                    FusionVerdict::Fence { node, boundary } => (
                        "fence",
                        format!(
                            "`{}` is an {}, a fence that fusion never crosses",
                            node,
                            boundary_name(*boundary)
                        ),
                    ),
                    FusionVerdict::Separated { from, to, through } => (
                        "separated",
                        format!(
                            "fusing would form a cycle{}: slots `{}` and `{}` stay apart",
                            through
                                .as_ref()
                                .map_or_else(String::new, |node| format!(" through `{}`", node)),
                            slot_name(*from),
                            slot_name(*to)
                        ),
                    ),
                    FusionVerdict::Pipelined { from, to, budget } => (
                        "pipelined",
                        format!(
                            "fusing would make one region cost more than {} ns per event, the cost model's budget: \
                             slots `{}` and `{}` pipeline",
                            budget,
                            slot_name(*from),
                            slot_name(*to)
                        ),
                    ),
                    FusionVerdict::Direct { fence } => (
                        "direct",
                        format!(
                            "an `emit` or `drop` right after `{}` runs in no slot: a direct edge",
                            fence
                        ),
                    ),
                };
                FusionEntry {
                    from: decision.from.clone(),
                    to: decision.to.clone(),
                    decision: name.to_string(),
                    reason,
                }
            })
            .collect();

        let segments = segments(plan, frame, placement);

        let output_bindings = frame
            .outputs()
            .iter()
            .map(|boot| {
                let decl = plan.output(&boot.id).unwrap_or(boot);
                let target = FrameTarget::Output(boot.id.clone());
                let mut fed_by = Vec::new();
                let mut idle = Vec::new();
                for edge in frame.edges().iter().filter(|edge| edge.to == target) {
                    let used = match edge.from {
                        FrameSource::Exit { slot, exit } => !placement.is_idle(slot, exit),
                        FrameSource::Admission(point) => frame
                            .admission_point(point)
                            .is_some_and(|point| present.contains(&point.node)),
                    };
                    let name = frame.describe_source(&edge.from);
                    if used {
                        fed_by.push(name);
                    } else {
                        idle.push(name);
                    }
                }
                OutputBindingEntry {
                    output: boot.id.clone(),
                    recipients: decl.recipients.clone(),
                    delivery: decl.delivery,
                    overflow: decl.delivery.overflow().as_str().to_string(),
                    closure: decl.delivery.closure().as_str().to_string(),
                    fed_by,
                    idle,
                }
            })
            .collect();

        Self {
            fixed,
            slots,
            edges,
            fusion,
            segments,
            output_bindings,
        }
    }
}

/// Returns the segment of every admission point of the frame whose fixed component the plan has.
fn segments(plan: &Plan, frame: &Frame, placement: &Placement) -> Vec<SegmentEntry> {
    let index = plan
        .nodes
        .iter()
        .enumerate()
        .map(|(position, node)| (&node.id, position))
        .collect::<BTreeMap<_, _>>();
    let mut targets: BTreeMap<&NodeId, Vec<&NodeId>> = BTreeMap::new();
    let mut leaving: BTreeMap<PortRef, &NodeId> = BTreeMap::new();
    for edge in &plan.edges {
        targets.entry(&edge.from.node).or_default().push(&edge.to.node);
        leaving.insert(edge.from.clone(), &edge.to.node);
    }

    frame
        .admission_points()
        .iter()
        .filter(|point| index.contains_key(&point.node))
        .map(|point| {
            let from = PortRef::new(point.node.clone(), point.port.clone());
            let mut reached = BTreeSet::new();
            let mut ends = Vec::new();
            let mut pending = leaving.get(&from).copied().into_iter().collect::<Vec<_>>();
            while let Some(node) = pending.pop() {
                if placement.site(node) == Some(Site::Fixed) {
                    let end = node.to_string();
                    if !ends.contains(&end) {
                        ends.push(end);
                    }
                    continue;
                }
                if !reached.insert(node) {
                    continue;
                }
                let emitted = plan
                    .node(node)
                    .filter(|written| written.op.as_str() == names::EMIT)
                    .and_then(|written| written.params.get("output"))
                    .and_then(Value::as_str);
                if let Some(output) = emitted {
                    let end = format!("output {}", output);
                    if !ends.contains(&end) {
                        ends.push(end);
                    }
                }
                pending.extend(targets.get(node).into_iter().flatten().copied());
            }
            let mut nodes = reached.into_iter().cloned().collect::<Vec<_>>();
            nodes.sort_by_key(|node| index.get(node).copied().unwrap_or(usize::MAX));
            let slots = nodes
                .iter()
                .filter_map(|node| match placement.site(node) {
                    Some(Site::Slot(slot)) => Some(slot),
                    _ => None,
                })
                .collect::<BTreeSet<_>>()
                .into_iter()
                .map(|slot| frame.describe_target(&FrameTarget::Slot(slot)))
                .collect();
            ends.sort();
            SegmentEntry {
                admission_point: point.id.get(),
                from,
                mode: mode_name(point.mode).to_string(),
                nodes,
                slots,
                ends,
            }
        })
        .collect()
}
