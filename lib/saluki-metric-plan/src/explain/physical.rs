//! The physical sections of explain documents: the frame, its slots, fences, exits, and direct edges, rewrite R1's
//! fusion decisions, segments, and output bindings. Isolation slots, the admission points that feed their state owners,
//! and the exits that carry those points' progress are marked; documents without them read as they always did.

use std::collections::{BTreeMap, BTreeSet};

use saluki_core::execution::AdmissionMode;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::boundary_name;
use crate::{
    compile::{Frame, FrameSource, FrameTarget, FusionVerdict, Placement, Site, SlotId, SlotKind},
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

    /// Whether the point feeds a state owner: it sends heartbeats toward it, and a cut is armed over it.
    #[serde(default, skip_serializing_if = "is_false")]
    pub feeds_state: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// What an isolation slot is: the region of one state owner, which owns its state, watermark, and flush ticks.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct IsolationEntry {
    /// The ID that stamps the slot's time-driven output.
    pub output_point: u32,

    /// The admission points whose latest admission times make up its watermark.
    pub upstream: Vec<u32>,
}

/// One slot.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct SlotEntry {
    /// The region host component's ID.
    pub name: String,

    /// The estimated cost per event of the slot's boot nodes, in nanoseconds, which R1's cost model kept within its
    /// budget. Documents written before it was added have none, read as zero. An isolation slot, which R1 never fuses,
    /// is not priced: its cost is zero, whatever its state owner's work.
    #[serde(default)]
    pub cost: u32,

    /// The explained plan's nodes in the slot, in plan order.
    pub nodes: Vec<NodeId>,

    /// The slot's entries in the explained plan.
    pub entries: Vec<SlotInputEntry>,

    /// The slot's exits, in order.
    pub exits: Vec<ExitEntry>,

    /// What makes the slot an isolation slot, if it is one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub isolation: Option<IsolationEntry>,
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

    /// The admission points whose progress the exit forwards toward a state owner, when a batch leaves the slot
    /// without output on it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub progress_points: Vec<u32>,
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

    /// `fused`, `fence`, `isolation`, `separated`, `pipelined`, or `direct`.
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

    /// `admit` or `readmit`, or `time-driven` for an isolation slot's output point, which stamps what a state owner
    /// emits at its flush ticks.
    pub mode: String,

    /// The nodes the segment's batches reach, in plan order.
    pub nodes: Vec<NodeId>,

    /// The slots those nodes run in, in slot order.
    pub slots: Vec<String>,

    /// Where the segment ends: the fixed components it feeds, which re-admit, the state owners it feeds, whose
    /// time-driven output is stamped anew, and the outputs it delivers to.
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
                        feeds_state: point.feeds_state,
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
                            progress_points: exit.progress_points.iter().map(|point| point.get()).collect(),
                        })
                        .collect(),
                    isolation: match &slot.kind {
                        SlotKind::Isolation { output_point, upstream } => Some(IsolationEntry {
                            output_point: output_point.get(),
                            upstream: upstream.iter().map(|point| point.get()).collect(),
                        }),
                        SlotKind::Fused => None,
                    },
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
                    FusionVerdict::Isolation { slot } => (
                        "isolation",
                        format!(
                            "the edge enters or leaves the isolation slot `{}`, which runs its state owner alone",
                            slot_name(*slot)
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

/// Returns the segment of every admission point of the frame whose fixed component the plan has, then the segment of
/// every state owner the isolation slots run, which starts at its slot's output point: its time-driven output is
/// stamped anew there.
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

    let segment = |admission_point: u32, from: PortRef, mode: &str| {
        let mut reached = BTreeSet::new();
        let mut ends = Vec::new();
        let mut pending = leaving.get(&from).copied().into_iter().collect::<Vec<_>>();
        while let Some(node) = pending.pop() {
            let isolated = match placement.site(node) {
                Some(Site::Slot(slot)) => frame.slot(slot).is_some_and(|slot| slot.is_isolation()),
                _ => false,
            };
            if placement.site(node) == Some(Site::Fixed) || isolated {
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
            admission_point,
            from,
            mode: mode.to_string(),
            nodes,
            slots,
            ends,
        }
    };

    let mut segments = frame
        .admission_points()
        .iter()
        .filter(|point| index.contains_key(&point.node))
        .map(|point| {
            let from = PortRef::new(point.node.clone(), point.port.clone());
            segment(point.id.get(), from, mode_name(point.mode))
        })
        .collect::<Vec<_>>();
    for (slot, placed) in frame.slots().iter().zip(placement.slots()) {
        let SlotKind::Isolation { output_point, .. } = &slot.kind else {
            continue;
        };
        for node in &placed.nodes {
            // A state owner's time-driven output leaves by its output port; every edge from it starts the segment.
            let ports = plan
                .edges
                .iter()
                .filter(|edge| &edge.from.node == node)
                .map(|edge| edge.from.clone())
                .collect::<BTreeSet<_>>();
            for from in ports {
                segments.push(segment(output_point.get(), from, "time-driven"));
            }
        }
    }
    segments
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        compile::compile,
        explain::Explain,
        ir::NativeComponentId,
        operators::OperatorRegistry,
        testing::{default_program_catalog, DEFAULT_PROGRAM_JSON},
        validate::Validator,
    };

    fn namer(nodes: &[NodeId]) -> Result<NativeComponentId, String> {
        crate::testing::default_program_slot_name(nodes)
    }

    #[test]
    fn an_isolation_slot_its_feeding_point_and_its_progress_exits_are_marked_and_end_the_segment() {
        let plan = Plan::from_json(DEFAULT_PROGRAM_JSON).unwrap();
        let registry = OperatorRegistry::native();
        let report = Validator::new(&registry, &default_program_catalog()).validate(&plan);
        let frame = Frame::compute(&plan, &registry, &namer).unwrap();
        let compiled = compile(&plan, &report, &registry, &frame).unwrap();
        let physical = PhysicalPlan::of(&compiled.plan, &frame, &compiled.placement);

        let agg = physical.slots.iter().find(|slot| slot.name == "dsd_agg").unwrap();
        assert_eq!(
            agg.isolation,
            Some(IsolationEntry {
                output_point: 5,
                upstream: vec![0],
            })
        );
        assert!(physical
            .slots
            .iter()
            .filter(|slot| slot.name != "dsd_agg")
            .all(|slot| slot.isolation.is_none()));
        let progress = physical
            .slots
            .iter()
            .flat_map(|slot| slot.exits.iter().map(move |exit| (slot.name.as_str(), exit)))
            .filter(|(_, exit)| !exit.progress_points.is_empty())
            .map(|(slot, exit)| (slot.to_string(), exit.name.clone(), exit.progress_points.clone()))
            .collect::<Vec<_>>();
        assert_eq!(
            progress,
            vec![
                (
                    "dsd_pre_aggregation".to_string(),
                    "dsd_prefix_filter_region".to_string(),
                    vec![0]
                ),
                (
                    "dsd_prefix_filter_region".to_string(),
                    "dsd_no_agg_split_region".to_string(),
                    vec![0]
                ),
                ("dsd_no_agg_split_region".to_string(), "dsd_agg".to_string(), vec![0]),
            ]
        );
        let feeding = physical
            .fixed
            .iter()
            .flat_map(|fixed| {
                fixed
                    .admission_points
                    .iter()
                    .map(move |point| (fixed.node.to_string(), point))
            })
            .filter(|(_, point)| point.feeds_state)
            .map(|(node, _)| node)
            .collect::<Vec<_>>();
        assert_eq!(feeding, vec!["dsd_in".to_string()]);
        // DogStatsD's segment ends at aggregation, whose time-driven output is stamped anew, and at the outputs its
        // passthrough delivers to.
        let dogstatsd = physical
            .segments
            .iter()
            .find(|segment| segment.admission_point == 0)
            .unwrap();
        assert!(dogstatsd.ends.contains(&"dsd_agg".to_string()), "{:?}", dogstatsd.ends);
        assert!(dogstatsd.nodes.iter().all(|node| node.to_string() != "dsd_agg"));

        // The text form says the same.
        let text = Explain::of(&plan, &registry, &report)
            .with_physical(&compiled.plan, &frame, &compiled.placement)
            .to_text();
        for line in [
            "      admission point 0 metrics (admit) -> dsd_pre_aggregation, feeds state",
            "      isolation slot, output point 5, watermark of admission points 0",
            "      exit 0 dsd_agg -> dsd_agg, progress of admission points 0",
        ] {
            assert!(
                text.lines().any(|candidate| candidate == line),
                "no line {:?} in:\n{}",
                line,
                text
            );
        }

        // A document written without the new fields reads as one without isolation slots.
        let mut value = serde_json::to_value(&physical).unwrap();
        for slot in value["slots"].as_array_mut().unwrap() {
            slot.as_object_mut().unwrap().remove("isolation");
        }
        let read: PhysicalPlan = serde_json::from_value(value).unwrap();
        assert!(read.slots.iter().all(|slot| slot.isolation.is_none()));
    }
}
