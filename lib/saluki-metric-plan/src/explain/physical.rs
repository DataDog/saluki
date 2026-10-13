//! The physical sections of explain documents: the frame, its slots, fences, exits, and direct edges, rewrite R1's
//! fusion decisions, segments, and output bindings. Isolation slots, the admission points that feed their state owners,
//! and the exits that carry those points' progress are marked; documents without them read as they always did.
//!
//! A frame laid out on lanes ([`LanePolicy`][crate::compile::LanePolicy]) says so: its lane counts and whether R1
//! pipelines its slots, the lanes of every admission point and slot, how every edge into a slot with lanes reaches its
//! lanes (with the key groups each lane owns behind a keyed exit), and the output point of every lane of an isolation
//! slot. A frame on one lane, laid out with R1's pipelining, has none of these members, so its document reads as it
//! did before lanes, and a document without them reads as one on one lane.

use std::collections::{BTreeMap, BTreeSet};

use saluki_core::execution::AdmissionMode;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::boundary_name;
use crate::{
    compile::{
        ExitIndex, ExitRouting, Frame, FrameSource, FrameTarget, FusionVerdict, Placement, PointRouting, Site, SlotId,
        SlotKind,
    },
    extension::DeliveryPolicy,
    ir::{NodeId, OperatorName, OutputId, Plan, PortName, PortRef, RecipientId},
    operators::names,
};

/// How a plan runs: the frame built at boot, and where the plan's nodes run in it.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct PhysicalPlan {
    /// The lanes the frame was laid out on, and whether R1's cost model pipelines its slots; left out for a frame on
    /// one lane laid out with pipelining, as every frame was before lanes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layout: Option<LayoutEntry>,

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

/// The lanes a frame was laid out on, and whether R1's cost model pipelines its slots.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct LayoutEntry {
    /// The admission points of the DogStatsD source's metrics, and the lanes of the slot they feed: the producer
    /// lanes.
    pub dogstatsd_lanes: u16,

    /// The lanes of each isolation slot whose state owner's state is partitioned, fed by other slots' exits only: the
    /// state lanes.
    pub state_lanes: u16,

    /// The lanes of each fused slot with no state owner downstream that the DogStatsD points do not feed: the
    /// post-aggregation lanes.
    pub post_lanes: u16,

    /// Whether R1's cost model cuts chains of synchronous nodes into pipelined slots; without it, fusion is maximal
    /// between fences.
    pub pipelining: bool,
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
    /// The point's ID: with lanes, its first lane's.
    pub id: u32,

    /// The number of lanes of the output, each an admission point of its own, with consecutive IDs from `id`: one,
    /// which documents leave out, for an output without lanes.
    #[serde(default = "one_lane", skip_serializing_if = "is_one_lane")]
    pub lanes: u16,

    /// The output port.
    pub port: PortName,

    /// `admit` (a source stamps what is not stamped yet) or `readmit` (an opaque host stamps everything again).
    pub mode: String,

    /// Where the port's physical edge leads, or `None` if the port has none.
    pub to: Option<String>,

    /// How the edge reaches the lanes of the slot it leads to: `fixed` (lane by lane) or `spread` (each buffer to the
    /// first lane with room); `one`, which documents leave out, when the slot runs one lane or the edge leads to no
    /// slot.
    #[serde(default = "one_routing", skip_serializing_if = "is_one_routing")]
    pub routing: String,

    /// Whether the point feeds a state owner: it sends heartbeats toward it, and a cut is armed over it.
    #[serde(default, skip_serializing_if = "is_false")]
    pub feeds_state: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

fn one_lane() -> u16 {
    1
}

fn is_one_lane(lanes: &u16) -> bool {
    *lanes == 1
}

fn one_routing() -> String {
    ExitRouting::One.name().to_string()
}

fn is_one_routing(routing: &str) -> bool {
    routing == ExitRouting::One.name()
}

/// What an isolation slot is: the region of one state owner, which owns its state, watermark, and flush ticks.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct IsolationEntry {
    /// The ID that stamps the slot's time-driven output: with lanes, its first lane's.
    pub output_point: u32,

    /// The IDs that stamp each lane's time-driven output, in lane order, for a slot on several lanes; left out for a
    /// slot on one lane, whose ID is `output_point`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub output_points: Vec<u32>,

    /// The admission points whose latest admission times make up its watermark: every lane's point of a point with
    /// lanes.
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

    /// The number of lanes the slot runs on, each a task with an input of its own: one, which documents leave out, for
    /// a slot without lanes.
    #[serde(default = "one_lane", skip_serializing_if = "is_one_lane")]
    pub lanes: u16,

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

    /// How the exit reaches the lanes of the slot it leads to: `keyed` (each event by its partition key, through the
    /// key groups), `affinity` (lane `i` to lane `i` modulo the target's lanes), or `spread` (each buffer to the first
    /// lane with room); `one`, which documents leave out, when the target runs one lane or is no slot.
    #[serde(default = "one_routing", skip_serializing_if = "is_one_routing")]
    pub routing: String,

    /// For a keyed exit, the key groups each lane of the target owns, in lane order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub key_groups: Vec<KeyGroupsEntry>,
}

/// The key groups one lane of a state owner's isolation slot owns, behind a keyed exit: a contiguous range.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct KeyGroupsEntry {
    /// The lane.
    pub lane: u16,

    /// The first group the lane owns.
    pub first: u16,

    /// The last group the lane owns.
    pub last: u16,
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

    /// How the edge reaches the lanes of the slot it leads to, as its exit or admission point says; `one`, which
    /// documents leave out, when the target runs one lane or is no slot.
    #[serde(default = "one_routing", skip_serializing_if = "is_one_routing")]
    pub routing: String,
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
    /// The admission point's ID: with lanes, its first lane's.
    pub admission_point: u32,

    /// The number of points the segment starts at, with consecutive IDs from `admission_point`: the lanes of a point
    /// with lanes, or of an isolation slot on lanes, each lane of which stamps its time-driven output at a point of its
    /// own. One, which documents leave out, without lanes.
    #[serde(default = "one_lane", skip_serializing_if = "is_one_lane")]
    pub lanes: u16,

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

    /// The encode host its encoder stream runs in, as a lease, if it is bound to a delivery host: `<encode host> of
    /// <delivery host>`. A fixed destination has none, nor has an output the explained plan no longer declares.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encode_host: Option<String>,

    /// Whether the explained plan no longer declares the output, which the frame's boot plan does: it then runs no
    /// lease, its recipients and delivery policy are those the boot plan declares, and only a fixed destination has
    /// exits of its own, all idle.
    #[serde(default, skip_serializing_if = "is_false")]
    pub removed: bool,

    /// The physical recipients it delivers to.
    pub recipients: Vec<RecipientId>,

    /// Its delivery policy.
    pub delivery: DeliveryPolicy,

    /// What it does when its downstream cannot accept more.
    pub overflow: String,

    /// What it does with queued work when it closes.
    pub closure: String,

    /// The physical edges that feed its encoder in the explained plan: for a bound output, the exits to its encode host
    /// that carry its `emit`s.
    pub fed_by: Vec<String>,

    /// The exits to it that the explained plan leaves idle: for a bound output, the exits to its encode host that carry
    /// none of its `emit`s.
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

        let (lanes, options) = (frame.lane_policy(), frame.options());
        let layout = (!lanes.is_one() || !options.pipelining).then_some(LayoutEntry {
            dogstatsd_lanes: lanes.dogstatsd,
            state_lanes: lanes.state,
            post_lanes: lanes.post,
            pipelining: options.pipelining,
        });

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
                        lanes: point.lanes,
                        port: point.port.clone(),
                        mode: mode_name(point.mode).to_string(),
                        to: point.route.as_ref().map(|target| frame.describe_target(target)),
                        routing: point.routing.name().to_string(),
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
                    lanes: slot.lanes,
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
                            routing: exit.routing.name().to_string(),
                            key_groups: key_groups(&exit.routing),
                        })
                        .collect(),
                    isolation: match &slot.kind {
                        SlotKind::Isolation {
                            output_points,
                            upstream,
                        } => Some(IsolationEntry {
                            // Lane 0's is the slot's output point without lanes.
                            output_point: output_points.first().map_or(0, |point| point.get()),
                            output_points: if output_points.len() > 1 {
                                output_points.iter().map(|point| point.get()).collect()
                            } else {
                                Vec::new()
                            },
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
                routing: match edge.from {
                    FrameSource::Admission(point) => frame
                        .admission_point(point)
                        .map_or(PointRouting::One, |point| point.routing)
                        .name(),
                    FrameSource::Exit { slot, exit } => frame
                        .slot(slot)
                        .and_then(|slot| slot.exits.get(exit.get()))
                        .map_or(ExitRouting::One.name(), |exit| exit.routing.name()),
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

        // The explained plan's outputs, then those of the boot plan it no longer declares.
        let declared = plan
            .outputs
            .iter()
            .chain(frame.outputs().iter().filter(|boot| plan.output(&boot.id).is_none()));
        let output_bindings = declared
            .map(|decl| {
                let target = FrameTarget::of_output(decl);
                let removed = plan.output(&decl.id).is_none();
                // The exits that carry the output's `emit`s, which, to an encode host, carry other outputs' too.
                let emitting = emitting_exits(plan, frame, placement, &decl.id, &target);
                let mut fed_by = Vec::new();
                let mut idle = Vec::new();
                // An encode host's exits are the host's, shared by the outputs bound to it: an output the explained
                // plan no longer declares has no lease there, and so no exit.
                let own_exits = !(removed && matches!(target, FrameTarget::EncodeHost(_)));
                for edge in frame.edges().iter().filter(|edge| own_exits && edge.to == target) {
                    let used = match edge.from {
                        FrameSource::Exit { slot, exit } => match &target {
                            FrameTarget::EncodeHost(_) => emitting.contains(&(slot, exit)),
                            _ => !placement.is_idle(slot, exit),
                        },
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
                    output: decl.id.clone(),
                    encode_host: match &target {
                        FrameTarget::EncodeHost(_) if !removed => Some(frame.describe_target(&target)),
                        _ => None,
                    },
                    removed,
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
            layout,
            fixed,
            slots,
            edges,
            fusion,
            segments,
            output_bindings,
        }
    }

    /// Returns the physical sections alone as human-readable text, as [`Explain::to_text`][super::Explain::to_text]
    /// renders them after the logical ones, with a trailing newline. The output is deterministic.
    pub fn to_text(&self) -> String {
        super::text::render_physical(self)
    }
}

/// Returns the key groups each lane of a keyed exit's target owns, in lane order: nothing for an exit that is not
/// keyed.
fn key_groups(routing: &ExitRouting) -> Vec<KeyGroupsEntry> {
    let ExitRouting::Keyed(groups) = routing else {
        return Vec::new();
    };
    (0..groups.lanes())
        .filter_map(|lane| {
            let owned = groups.groups_of(lane);
            // Groups are below 256, and a lane owns none only when there are more lanes than groups.
            Some(KeyGroupsEntry {
                lane,
                first: owned.start as u16,
                last: owned.end.checked_sub(1).filter(|_| !owned.is_empty())? as u16,
            })
        })
        .collect()
}

/// Returns the exits, as `(slot, exit)`, by which the plan's edges into `emit`s to `output` leave their slots toward
/// `target`, the output's encode host or fixed destination.
fn emitting_exits(
    plan: &Plan, frame: &Frame, placement: &Placement, output: &OutputId, target: &FrameTarget,
) -> BTreeSet<(SlotId, ExitIndex)> {
    let emits = plan
        .nodes
        .iter()
        .filter(|node| {
            node.op.as_str() == names::EMIT
                && node.params.get("output").and_then(Value::as_str) == Some(output.as_str())
        })
        .map(|node| &node.id)
        .collect::<BTreeSet<_>>();
    plan.edges
        .iter()
        .filter(|edge| emits.contains(&edge.to.node))
        .filter_map(|edge| match placement.site(&edge.from.node) {
            Some(Site::Slot(slot)) => frame
                .slot(slot)
                .and_then(|found| found.exit_to(target))
                .map(|exit| (slot, exit.index)),
            _ => None,
        })
        .collect()
}

/// Returns the segment of every admission point of the frame whose fixed component the plan has, then the segment of
/// every state owner the isolation slots run, which starts at its slot's output point: its time-driven output is
/// stamped anew there. The lanes of a point, or of an isolation slot, share one segment, which starts at all of their
/// points.
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

    let segment = |admission_point: u32, lanes: u16, from: PortRef, mode: &str| {
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
            lanes,
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
            segment(point.id.get(), point.lanes, from, mode_name(point.mode))
        })
        .collect::<Vec<_>>();
    for (slot, placed) in frame.slots().iter().zip(placement.slots()) {
        let Some(output_point) = slot.output_point(0) else {
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
                segments.push(segment(output_point.get(), slot.lanes, from, "time-driven"));
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
                output_points: Vec::new(),
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
