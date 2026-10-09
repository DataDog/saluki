//! The physical frame: the structure of the topology, fixed at boot, that every generation runs in.
//!
//! A topology is built once, and a component's outputs are fixed when it is added, so the physical structure cannot
//! follow each candidate plan. [`Frame::compute`] derives it once from the boot plan (the default program):
//!
//! - **Fixed components** are the plan's fences, each run by a component of its own: sources (`io_host`) and opaque
//!   hosts (`opaque_host`). Each of their output ports is an **admission point**, where batches are stamped with their
//!   admission time and generation: sources admit (they stamp what is not stamped yet), and opaque hosts readmit (they
//!   stamp everything again, so nothing crosses them).
//! - The declared **outputs**, each one encoder stream, are fixed too.
//! - **Slots** hold the synchronous operators between fences. A slot is a connected set of synchronous nodes, as large
//!   as fusion allows (rewrite R1): two synchronous neighbors share a slot unless some path leaves one of them through
//!   a fence and comes back to the other, since fusing them would then form a cycle through that fence, or unless
//!   R1's cost model cuts between them, so that a chain too costly for one task pipelines across several (see
//!   [below](#pipelining)). A slot may have several inputs. Each slot is run by one region host component, named by
//!   the host's [slot namer][SlotNamer].
//! - Each slot's **exits** are its outputs: one for every edge of the boot plan that leaves the slot, to a fixed
//!   component or to another slot, and one for every output the boot plan delivers to downstream of the slot, whether
//!   or not the boot plan emits to it from the slot. Exits nothing uses stay connected and idle, so that later
//!   generations can use them without changing the topology. Exits are named after their target: the fixed
//!   component, the slot, or the output.
//! - The **edges** connect admission points and exits to their targets. An `emit` or `drop` whose only input comes
//!   straight from a fence is not run in a slot: it becomes a direct edge from the admission point to the output, or
//!   no edge at all.
//!
//! The frame records every fusion decision ([`FusionDecision`]) so that `explain` can say why each edge of the boot
//! plan is or is not inside a slot.
//!
//! # Slots
//!
//! The slots of a connected set `C` of synchronous nodes are computed from each node's **fence depth**: the largest
//! number of fences on any path from a node of `C` to it. Nodes of `C` that no path from `C` through a fence reaches
//! have depth zero. Neighbors of equal depth share a slot. Every path from one slot to another then either crosses a
//! fence, which increases the depth, or is a direct edge between neighbors of different depths, so the slots and fixed
//! components form an acyclic graph, as a topology must.
//!
//! # Pipelining
//!
//! A region runs its program in one task, one batch after another, so one stream of batches through a region gets at
//! most one core. Natively, each component of a chain runs in a task of its own, and consecutive batches move through
//! the chain in parallel: the chain keeps the throughput of its costliest component. R1 therefore fuses a chain only
//! as far as that throughput is kept, by a cost model:
//!
//! - Each operator estimates its work per event ([`OperatorDef::cost`][crate::operators::OperatorDef::cost]), and each
//!   native component, the nodes with its provenance, costs the sum of its nodes' estimates.
//! - The **budget** of a region is the estimated cost of the costliest native component that runs in any slot, or
//!   [`REGION_COST_FLOOR`] if that is less: a region within the floor is fast enough for one stream whatever it fuses.
//! - Walking each fused set in topological order, a native component joins the region its inputs come from if the
//!   region's estimate stays within the budget, and starts the next region of a pipeline otherwise. A native
//!   component is never cut, so no region does less than a native component did, and none costs more than the budget
//!   unless one component alone does.
//!
//! A cut follows the edges, so a slot's **stage** only grows along them, and the slots and fixed components stay
//! acyclic. The frame records each cut as a [`FusionVerdict::Pipelined`] decision.

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    fmt,
};

use saluki_core::execution::{AdmissionMode, AdmissionPointId};
use snafu::Snafu;

use super::{resolve, Role};
use crate::{
    canonical::{PlanHash, ResolvedValues},
    ir::{
        Namespace, NativeComponentId, NodeId, OperatorName, OutputDecl, OutputId, Params, Plan, PortName, PortRef,
        Provenance,
    },
    operators::{Boundary, OperatorRegistry, StateContract},
    validate::graph::{emit_output, Graph},
};

/// The estimated cost per event, in nanoseconds, within which R1 fuses a chain into one region whatever the costs of
/// its native components: such a region processes over 15 million events per second, more than one stream of the
/// sources it runs behind delivers (see [Pipelining](self#pipelining)).
pub const REGION_COST_FLOOR: u32 = 64;

/// Names the region host component of one slot, given the IDs of the boot plan's nodes in the slot, in plan order.
///
/// The host supplies the namer, so that a slot is named after the native components it replaces rather than after a
/// node. The name must be a valid component ID, and distinct from every other slot's. Returning an error fails the
/// frame with the error's message.
pub type SlotNamer<'a> = dyn Fn(&[NodeId]) -> Result<NativeComponentId, String> + 'a;

/// The index of a slot in its frame.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SlotId(u16);

impl SlotId {
    /// Creates a slot index.
    pub const fn new(index: u16) -> Self {
        Self(index)
    }

    /// Returns this index as a `usize`, for indexing.
    pub const fn get(self) -> usize {
        self.0 as usize
    }
}

impl fmt::Display for SlotId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// The index of an exit among its slot's exits.
///
/// A region host's outputs are its slot's exits, in this order, which is also the order it dispatches them in.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ExitIndex(u16);

impl ExitIndex {
    /// Creates an exit index.
    pub const fn new(index: u16) -> Self {
        Self(index)
    }

    /// Returns this index as a `usize`, for indexing.
    pub const fn get(self) -> usize {
        self.0 as usize
    }
}

impl fmt::Display for ExitIndex {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// Where a physical edge leads.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum FrameTarget {
    /// The input of a fixed component, by its node.
    Fixed(NodeId),

    /// The input of a slot's region host.
    Slot(SlotId),

    /// An output's encoder.
    Output(OutputId),
}

/// Where a physical edge starts.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum FrameSource {
    /// An admission point: an output port of a fixed component.
    Admission(AdmissionPointId),

    /// An exit of a slot.
    Exit {
        /// The slot.
        slot: SlotId,

        /// The exit.
        exit: ExitIndex,
    },
}

/// A physical edge: one connection the topology builder makes.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct FrameEdge {
    /// Where the edge starts.
    pub from: FrameSource,

    /// Where the edge leads.
    pub to: FrameTarget,
}

/// An admission point: one output port of a fixed component.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AdmissionPointInfo {
    /// The point's ID, unique within the frame.
    pub id: AdmissionPointId,

    /// The fixed component's node.
    pub node: NodeId,

    /// The output port.
    pub port: PortName,

    /// How the point stamps batches: sources admit, opaque hosts readmit.
    pub mode: AdmissionMode,

    /// Where the port's physical edge leads, or `None` if the boot plan drops everything the port carries, which needs
    /// no edge.
    pub route: Option<FrameTarget>,
}

/// A fixed component: a fence of the boot plan, run by a host component of its own.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FixedComponent {
    /// The node.
    pub node: NodeId,

    /// The node's operator in the boot plan.
    pub operator: OperatorName,

    /// The node's parameters in the boot plan.
    pub params: Params,

    /// The node's boundary: `io_host` or `opaque_host`.
    pub boundary: Boundary,

    /// Whether the node keeps state between batches, as its contract declares.
    pub state_owner: bool,

    /// What feeds the component's input, if it has one: an admission point, by a direct edge, or a slot's exit.
    pub input: Option<FrameSource>,

    /// The component's admission points, one per output port, in port order.
    pub admission_points: Vec<AdmissionPointId>,
}

/// One exit of a slot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Exit {
    /// The exit's index among its slot's exits.
    pub index: ExitIndex,

    /// The exit's name, which is the name of the region host's output: the target fixed component's node, the target
    /// slot, or the target output.
    pub name: NativeComponentId,

    /// Where the exit leads.
    pub target: FrameTarget,
}

/// One slot: a connected set of synchronous nodes run by one region host component.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Slot {
    /// The slot's index.
    pub id: SlotId,

    /// The region host component's ID.
    pub name: NativeComponentId,

    /// The boot plan's nodes in the slot, in plan order.
    pub nodes: Vec<NodeId>,

    /// What feeds the slot: admission points, in ID order, then other slots' exits, in slot and exit order.
    pub inputs: Vec<FrameSource>,

    /// The slot's exits, in order.
    pub exits: Vec<Exit>,

    /// The estimated cost per event of the slot's boot nodes, in nanoseconds: the sum of their operators' estimates.
    pub cost: u32,
}

impl Slot {
    /// Returns the exit leading to `target`, if the slot has one.
    pub fn exit_to(&self, target: &FrameTarget) -> Option<&Exit> {
        self.exits.iter().find(|exit| &exit.target == target)
    }
}

/// Where one node of the boot plan runs.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Site {
    /// In a fixed component of its own.
    Fixed,

    /// In a slot.
    Slot(SlotId),

    /// Nowhere: the node is an `emit` or `drop` fed straight by a fence, which is a direct edge or no edge.
    Direct,
}

/// Why an edge of the boot plan is or is not inside a slot: one decision of rewrite R1, fusion.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FusionDecision {
    /// The output port the edge leaves.
    pub from: PortRef,

    /// The input port the edge enters.
    pub to: PortRef,

    /// The decision.
    pub verdict: FusionVerdict,
}

/// The outcome of one fusion decision.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FusionVerdict {
    /// Both nodes are synchronous and nothing separates them: they share the slot.
    Fused {
        /// The slot.
        slot: SlotId,
    },

    /// The edge enters or leaves a fence, which fusion never crosses.
    Fence {
        /// The fence the edge enters or leaves; the upstream one when both ends are fences.
        node: NodeId,

        /// The fence's boundary.
        boundary: Boundary,
    },

    /// Both nodes are synchronous, but fusing them would form a cycle through a fence: some path leaves the upstream
    /// node's slot through the fence and reaches the downstream node.
    Separated {
        /// The upstream node's slot.
        from: SlotId,

        /// The downstream node's slot.
        to: SlotId,

        /// The fence on such a path, nearest the downstream node.
        through: Option<NodeId>,
    },

    /// Both nodes are synchronous, and nothing separates them, but R1's cost model cuts between them: fusing them
    /// would make the upstream node's region cost more per event than the budget, so the two slots pipeline.
    Pipelined {
        /// The upstream node's slot.
        from: SlotId,

        /// The downstream node's slot.
        to: SlotId,

        /// The budget, in estimated nanoseconds per event.
        budget: u32,
    },

    /// The edge leads from a fence to an `emit` or `drop`, which runs in no slot: the fence's output connects
    /// straight to the output, or to nothing.
    Direct {
        /// The fence.
        fence: NodeId,
    },
}

/// An error computing a frame.
#[derive(Clone, Debug, Eq, PartialEq, Snafu)]
#[snafu(context(suffix(false)))]
pub enum FrameError {
    /// The boot plan has structural problems.
    #[snafu(display(
        "The boot plan cannot be laid out, because it has structural problems: {}. Fix the plan so that it validates.",
        problems.join("; ")
    ))]
    InvalidPlan {
        /// What validation reports about the plan's structure.
        problems: Vec<String>,
    },

    /// The boot plan has a node that no slot can run in this stage.
    #[snafu(display(
        "Node `{}` cannot run in a region: {} requires stage {}. Remove it from the plan.",
        node,
        what,
        stage
    ))]
    Unsupported {
        /// The node.
        node: NodeId,

        /// What the node needs.
        what: String,

        /// The stage that provides it.
        stage: u8,
    },

    /// The slot namer refused a slot.
    #[snafu(display("The slot holding nodes {} has no name: {}", describe_nodes(nodes), message))]
    SlotName {
        /// The slot's nodes.
        nodes: Vec<NodeId>,

        /// The message the namer returned.
        message: String,
    },

    /// Two slots have the same name.
    #[snafu(display(
        "Two slots are named `{}`; the slot namer must give every slot a distinct name.",
        name
    ))]
    DuplicateSlotName {
        /// The name.
        name: NativeComponentId,
    },

    /// Two exits of one slot have the same name.
    #[snafu(display(
        "Slot `{}` has two exits named `{}`: a fixed component, slot, or output shares its name with another; \
         give them distinct names.",
        slot,
        name
    ))]
    ExitNameCollision {
        /// The slot's name.
        slot: NativeComponentId,

        /// The exit name.
        name: String,
    },

    /// The plan has more slots, exits, or admission points than a frame can number.
    #[snafu(display("The boot plan has more {} than a frame supports ({}).", what, limit))]
    TooLarge {
        /// What there are too many of.
        what: &'static str,

        /// The largest number supported.
        limit: usize,
    },
}

fn describe_nodes(nodes: &[NodeId]) -> String {
    nodes
        .iter()
        .map(|node| format!("`{}`", node))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The physical frame: the fixed structure of the topology that every generation runs in.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Frame {
    plan_hash: Option<PlanHash>,
    fixed: Vec<FixedComponent>,
    admission_points: Vec<AdmissionPointInfo>,
    slots: Vec<Slot>,
    outputs: Vec<OutputDecl>,
    edges: Vec<FrameEdge>,
    fusion: Vec<FusionDecision>,
    sites: BTreeMap<NodeId, Site>,
    cost_budget: u32,
}

impl Frame {
    /// Computes the frame of a boot plan.
    ///
    /// The plan must be structurally sound: every operator known and configured, every edge resolved, and no cycle.
    /// `namer` names each slot's region host component.
    ///
    /// # Errors
    ///
    /// Returns an error if the plan has structural problems, if it has a synchronous node that keeps state or reads a
    /// signal (no region runs one in this stage), if the namer refuses a slot or names two alike, if a slot's exits
    /// would share a name, or if the plan is too large to number.
    pub fn compute(plan: &Plan, registry: &OperatorRegistry, namer: &SlotNamer<'_>) -> Result<Self, FrameError> {
        let graph = resolve(plan, registry).map_err(|problems| FrameError::InvalidPlan { problems })?;
        let roles = super::roles(&graph);
        check_supported(&graph, &roles)?;

        let layout = Layout::compute(plan, &graph, &roles);
        let mut slots = Vec::with_capacity(layout.slots.len());
        let mut names = BTreeSet::new();
        for (index, members) in layout.slots.iter().enumerate() {
            let id = SlotId(u16::try_from(index).map_err(|_| too_large("slots"))?);
            let nodes = members.iter().map(|&node| graph.id(node).clone()).collect::<Vec<_>>();
            let name = namer(&nodes).map_err(|message| FrameError::SlotName {
                nodes: nodes.clone(),
                message,
            })?;
            if !names.insert(name.clone()) {
                return Err(FrameError::DuplicateSlotName { name });
            }
            let cost = members
                .iter()
                .map(|&node| layout.cost[node])
                .fold(0u32, u32::saturating_add);
            slots.push(Slot {
                id,
                name,
                nodes,
                inputs: Vec::new(),
                exits: Vec::new(),
                cost,
            });
        }

        let slot_of = |node: usize| layout.slot_of[node].map(|slot| SlotId(slot as u16));
        let slot_names = slots.iter().map(|slot| slot.name.clone()).collect::<Vec<_>>();

        // Exits: the edges leaving each slot, in plan order, then every output downstream of it.
        for slot in &mut slots {
            let members = &layout.slots[slot.id.get()];
            let mut targets = Vec::new();
            for &(from, to) in &layout.edges {
                if layout.slot_of[from.0] != Some(slot.id.get()) {
                    continue;
                }
                let target = match (roles[to.0], slot_of(to.0)) {
                    (Role::Fixed, _) => FrameTarget::Fixed(graph.id(to.0).clone()),
                    (_, Some(other)) if other != slot.id => FrameTarget::Slot(other),
                    _ => match graph.op(to.0).and_then(emit_output) {
                        Some(output) => FrameTarget::Output(output.clone()),
                        None => continue,
                    },
                };
                if !targets.contains(&target) {
                    targets.push(target);
                }
            }
            let downstream = downstream_outputs(&graph, members);
            for output in &plan.outputs {
                let target = FrameTarget::Output(output.id.clone());
                if downstream.contains(&output.id) && !targets.contains(&target) {
                    targets.push(target);
                }
            }

            let mut exit_names = BTreeSet::new();
            for (index, target) in targets.into_iter().enumerate() {
                let index = ExitIndex(u16::try_from(index).map_err(|_| too_large("exits in one slot"))?);
                let name = match &target {
                    FrameTarget::Fixed(node) => component_name(node),
                    FrameTarget::Slot(other) => slot_names[other.get()].to_string(),
                    FrameTarget::Output(output) => output.to_string(),
                };
                if !exit_names.insert(name.clone()) {
                    return Err(FrameError::ExitNameCollision {
                        slot: slot.name.clone(),
                        name,
                    });
                }
                // Node identifiers, output identifiers, and slot names are all valid component IDs.
                let name = NativeComponentId::new(name).expect("exit names are valid component IDs");
                slot.exits.push(Exit { index, name, target });
            }
        }

        // Admission points: every output port of every fixed component, in plan order.
        let mut admission_points = Vec::new();
        let mut point_of = BTreeMap::new();
        let mut fixed = Vec::new();
        for node in 0..graph.nodes.len() {
            if roles[node] != Role::Fixed {
                continue;
            }
            let op = graph.op(node).expect("a resolved plan configures every node");
            let mut points = Vec::new();
            for (port, output) in op.ports().outputs.iter().enumerate() {
                let id = AdmissionPointId::new(u32::try_from(admission_points.len()).map_err(|_| too_large("ports"))?);
                let route = graph.target(node, port).and_then(|(to, _)| match roles[to] {
                    Role::Fixed => Some(FrameTarget::Fixed(graph.id(to).clone())),
                    Role::Synchronous => slot_of(to).map(FrameTarget::Slot),
                    Role::Terminal => graph
                        .op(to)
                        .and_then(emit_output)
                        .map(|output| FrameTarget::Output(output.clone())),
                });
                point_of.insert((node, port), id);
                points.push(id);
                admission_points.push(AdmissionPointInfo {
                    id,
                    node: graph.id(node).clone(),
                    port: output.name.clone(),
                    mode: if op.contract().boundary == Boundary::IoHost {
                        AdmissionMode::Admit
                    } else {
                        AdmissionMode::Readmit
                    },
                    route,
                });
            }
            fixed.push((node, points));
        }

        let fixed = fixed
            .into_iter()
            .map(|(node, admission_points)| {
                let op = graph.op(node).expect("a resolved plan configures every node");
                let id = graph.id(node);
                let fed = (!op.ports().inputs.is_empty()).then(|| graph.source(node, 0)).flatten();
                let input = fed.and_then(|(from, port)| match roles[from] {
                    Role::Fixed => point_of.get(&(from, port)).copied().map(FrameSource::Admission),
                    _ => {
                        let slot = slot_of(from)?;
                        let exit = slots[slot.get()].exit_to(&FrameTarget::Fixed(id.clone()))?;
                        Some(FrameSource::Exit { slot, exit: exit.index })
                    }
                });
                FixedComponent {
                    node: id.clone(),
                    operator: graph.nodes[node].node.op.clone(),
                    params: graph.nodes[node].node.params.clone(),
                    boundary: op.contract().boundary,
                    state_owner: op.contract().state != StateContract::None,
                    input,
                    admission_points,
                }
            })
            .collect::<Vec<_>>();

        // Each slot's inputs, and the physical edges.
        let mut edges = Vec::new();
        for point in &admission_points {
            if let Some(route) = &point.route {
                edges.push(FrameEdge {
                    from: FrameSource::Admission(point.id),
                    to: route.clone(),
                });
                if let FrameTarget::Slot(slot) = route {
                    slots[slot.get()].inputs.push(FrameSource::Admission(point.id));
                }
            }
        }
        let mut slot_inputs = Vec::new();
        for slot in &slots {
            for exit in &slot.exits {
                let from = FrameSource::Exit {
                    slot: slot.id,
                    exit: exit.index,
                };
                edges.push(FrameEdge {
                    from,
                    to: exit.target.clone(),
                });
                if let FrameTarget::Slot(target) = exit.target {
                    slot_inputs.push((target, from));
                }
            }
        }
        for (target, from) in slot_inputs {
            slots[target.get()].inputs.push(from);
        }

        let fusion = layout
            .edges
            .iter()
            .map(|&((from, from_port), (to, to_port))| {
                let from_ref = PortRef::new(
                    graph.id(from).clone(),
                    graph.op(from).expect("configured").ports().outputs[from_port]
                        .name
                        .clone(),
                );
                let to_ref = PortRef::new(
                    graph.id(to).clone(),
                    graph.op(to).expect("configured").ports().inputs[to_port].name.clone(),
                );
                let boundary = |node: usize| graph.op(node).expect("configured").contract().boundary;
                let verdict = match (roles[from], roles[to]) {
                    (Role::Fixed, Role::Terminal) => FusionVerdict::Direct {
                        fence: graph.id(from).clone(),
                    },
                    (Role::Fixed, _) => FusionVerdict::Fence {
                        node: graph.id(from).clone(),
                        boundary: boundary(from),
                    },
                    (_, Role::Fixed) => FusionVerdict::Fence {
                        node: graph.id(to).clone(),
                        boundary: boundary(to),
                    },
                    _ => match (slot_of(from), slot_of(to)) {
                        (Some(a), Some(b)) if a == b => FusionVerdict::Fused { slot: a },
                        (Some(a), Some(b)) if layout.depth[from] == layout.depth[to] => FusionVerdict::Pipelined {
                            from: a,
                            to: b,
                            budget: layout.budget,
                        },
                        (Some(a), Some(b)) => FusionVerdict::Separated {
                            from: a,
                            to: b,
                            through: layout.via[to].map(|fence| graph.id(fence).clone()),
                        },
                        _ => unreachable!("synchronous nodes are always in a slot"),
                    },
                };
                FusionDecision {
                    from: from_ref,
                    to: to_ref,
                    verdict,
                }
            })
            .collect();

        let sites = (0..graph.nodes.len())
            .map(|node| {
                let site = match roles[node] {
                    Role::Fixed => Site::Fixed,
                    Role::Terminal => Site::Direct,
                    Role::Synchronous => Site::Slot(slot_of(node).expect("synchronous nodes are always in a slot")),
                };
                (graph.id(node).clone(), site)
            })
            .collect();

        Ok(Self {
            plan_hash: plan.hash(ResolvedValues::Include).ok(),
            fixed,
            admission_points,
            slots,
            outputs: plan.outputs.clone(),
            edges,
            fusion,
            sites,
            cost_budget: layout.budget,
        })
    }

    /// Returns the budget of R1's cost model: the estimated cost per event, in nanoseconds, within which a region
    /// fuses native components (see [Pipelining](self#pipelining)).
    pub fn cost_budget(&self) -> u32 {
        self.cost_budget
    }

    /// Returns the hash of the boot plan the frame was computed from, with resolved values, if it has one.
    pub fn plan_hash(&self) -> Option<PlanHash> {
        self.plan_hash
    }

    /// Returns the fixed components, in plan order.
    pub fn fixed(&self) -> &[FixedComponent] {
        &self.fixed
    }

    /// Returns the fixed component of `node`, if the frame has one.
    pub fn fixed_component(&self, node: &NodeId) -> Option<&FixedComponent> {
        self.fixed.iter().find(|fixed| &fixed.node == node)
    }

    /// Returns every admission point, in ID order.
    pub fn admission_points(&self) -> &[AdmissionPointInfo] {
        &self.admission_points
    }

    /// Returns the admission point with the given ID, if there is one.
    pub fn admission_point(&self, id: AdmissionPointId) -> Option<&AdmissionPointInfo> {
        self.admission_points.iter().find(|point| point.id == id)
    }

    /// Returns the admission point at one output port of a fixed component, if there is one.
    pub fn admission_point_at(&self, node: &NodeId, port: &PortName) -> Option<&AdmissionPointInfo> {
        self.admission_points
            .iter()
            .find(|point| &point.node == node && &point.port == port)
    }

    /// Returns the slots, in order.
    pub fn slots(&self) -> &[Slot] {
        &self.slots
    }

    /// Returns the slot with the given index, if there is one.
    pub fn slot(&self, id: SlotId) -> Option<&Slot> {
        self.slots.get(id.get())
    }

    /// Returns the boot plan's declared outputs, in plan order: the encoder streams of the topology.
    pub fn outputs(&self) -> &[OutputDecl] {
        &self.outputs
    }

    /// Returns the boot plan's declaration of `output`, if it has one.
    pub fn output(&self, output: &OutputId) -> Option<&OutputDecl> {
        self.outputs.iter().find(|decl| &decl.id == output)
    }

    /// Returns the physical edges: first each admission point's, in ID order, then each slot's exits, in slot and exit
    /// order.
    pub fn edges(&self) -> &[FrameEdge] {
        &self.edges
    }

    /// Returns the fusion decision for every edge of the boot plan, in plan order.
    pub fn fusion(&self) -> &[FusionDecision] {
        &self.fusion
    }

    /// Returns where a node of the boot plan runs, if the boot plan has the node.
    pub fn site(&self, node: &NodeId) -> Option<Site> {
        self.sites.get(node).copied()
    }

    /// Returns the name of a physical edge's start, as `<component>` or `<component>.<output>`.
    pub fn describe_source(&self, source: &FrameSource) -> String {
        match source {
            FrameSource::Admission(id) => self.admission_point(*id).map_or_else(
                || format!("admission point {}", id.get()),
                |point| format!("{}.{}", point.node, point.port),
            ),
            FrameSource::Exit { slot, exit } => match self.slot(*slot) {
                Some(slot) => slot.exits.get(exit.get()).map_or_else(
                    || format!("{}.{}", slot.name, exit),
                    |exit| format!("{}.{}", slot.name, exit.name),
                ),
                None => format!("slot {}.{}", slot, exit),
            },
        }
    }

    /// Returns the name of a physical edge's target: a fixed component's node, a slot's region host, or an output.
    pub fn describe_target(&self, target: &FrameTarget) -> String {
        match target {
            FrameTarget::Fixed(node) => node.to_string(),
            FrameTarget::Slot(slot) => self
                .slot(*slot)
                .map_or_else(|| format!("slot {}", slot), |slot| slot.name.to_string()),
            FrameTarget::Output(output) => format!("output {}", output),
        }
    }
}

fn too_large(what: &'static str) -> FrameError {
    FrameError::TooLarge {
        what,
        limit: usize::from(u16::MAX) + 1,
    }
}

/// Returns the component name of a fixed node: its local ID, prefixed with `policy_` in the policy namespace.
fn component_name(node: &NodeId) -> String {
    match node.namespace() {
        Namespace::Reserved => node.local().to_string(),
        Namespace::Policy => format!("policy_{}", node.local()),
    }
}

/// Rejects synchronous nodes that no region runs in this stage.
fn check_supported(graph: &Graph<'_>, roles: &[Role]) -> Result<(), FrameError> {
    for (node, role) in roles.iter().enumerate() {
        if *role == Role::Fixed {
            continue;
        }
        let op = graph.op(node).expect("a resolved plan configures every node");
        if op.contract().state != StateContract::None {
            return Err(FrameError::Unsupported {
                node: graph.id(node).clone(),
                what: "running an operator that keeps state in a region".to_string(),
                stage: 6,
            });
        }
        if op.contract().is_dynamic() {
            return Err(FrameError::Unsupported {
                node: graph.id(node).clone(),
                what: "running an operator that reads host signals".to_string(),
                stage: 10,
            });
        }
    }
    Ok(())
}

/// Returns the outputs that `emit` nodes downstream of `members` deliver to, through any node.
fn downstream_outputs(graph: &Graph<'_>, members: &[usize]) -> BTreeSet<OutputId> {
    let mut seen = vec![false; graph.nodes.len()];
    let mut pending = members.iter().copied().collect::<VecDeque<_>>();
    for &node in members {
        seen[node] = true;
    }
    let mut outputs = BTreeSet::new();
    while let Some(node) = pending.pop_front() {
        let Some(op) = graph.op(node) else { continue };
        if let Some(output) = emit_output(op) {
            outputs.insert(output.clone());
        }
        for port in 0..op.ports().outputs.len() {
            if let Some((next, _)) = graph.target(node, port) {
                if !seen[next] {
                    seen[next] = true;
                    pending.push_back(next);
                }
            }
        }
    }
    outputs
}

/// One port of one node, by position.
type Endpoint = (usize, usize);

/// The slots of a plan, by node position.
struct Layout {
    /// Each slot's nodes, in plan order; slots are ordered by their first node.
    slots: Vec<Vec<usize>>,

    /// Each node's slot, for synchronous nodes.
    slot_of: Vec<Option<usize>>,

    /// Each node's fence depth within its connected set of synchronous nodes.
    depth: Vec<u32>,

    /// For each node, the fence nearest it on a deepest path from its connected set, if any path crosses one.
    via: Vec<Option<usize>>,

    /// Each node's estimated cost per event, zero for nodes that run in no slot.
    cost: Vec<u32>,

    /// The budget of R1's cost model.
    budget: u32,

    /// Every edge of the plan, in plan order, by port positions.
    edges: Vec<(Endpoint, Endpoint)>,
}

impl Layout {
    fn compute(plan: &Plan, graph: &Graph<'_>, roles: &[Role]) -> Self {
        let count = graph.nodes.len();
        // Every edge, in plan order. A structurally sound plan resolves every edge on both ends.
        let edges = plan
            .edges
            .iter()
            .filter_map(|edge| {
                let from = graph.output_port(&edge.from.node, &edge.from.port)?;
                let to = graph.input_port(&edge.to.node, &edge.to.port)?;
                Some((from, to))
            })
            .collect::<Vec<_>>();

        // Connected sets of synchronous nodes.
        let synchronous = |node: usize| roles[node] == Role::Synchronous;
        let mut sets = UnionFind::new(count);
        for &((from, _), (to, _)) in &edges {
            if synchronous(from) && synchronous(to) {
                sets.union(from, to);
            }
        }

        // Fence depth of every node of each set, from that set's own nodes.
        let mut depth = vec![0u32; count];
        let mut via = vec![None; count];
        let mut roots = BTreeSet::new();
        for node in 0..count {
            if synchronous(node) {
                roots.insert(sets.find(node));
            }
        }
        for root in roots {
            let mut reach: Vec<Option<u32>> = vec![None; count];
            let mut nearest: Vec<Option<usize>> = vec![None; count];
            for &node in &graph.order {
                let mut best = (synchronous(node) && sets.find(node) == root).then_some(0u32);
                let mut best_via = None;
                let Some(op) = graph.op(node) else { continue };
                for port in 0..op.ports().inputs.len() {
                    let Some((from, _)) = graph.source(node, port) else {
                        continue;
                    };
                    let Some(reached) = reach[from] else { continue };
                    let fence = roles[from] == Role::Fixed;
                    let value = reached + u32::from(fence);
                    if best.is_none_or(|best| value > best) {
                        best = Some(value);
                        best_via = if fence { Some(from) } else { nearest[from] };
                    }
                }
                reach[node] = best;
                nearest[node] = best_via;
            }
            for node in 0..count {
                if synchronous(node) && sets.find(node) == root {
                    depth[node] = reach[node].unwrap_or(0);
                    via[node] = nearest[node];
                }
            }
        }

        // R1's cost model: the stage of each node within its set and depth, so that each stage's region stays within
        // the budget (see the module documentation).
        let cost = (0..count)
            .map(|node| match graph.op(node) {
                Some(op) if synchronous(node) => op.cost(),
                _ => 0,
            })
            .collect::<Vec<_>>();
        let component = |node: usize| match &graph.nodes[node].node.provenance {
            Some(Provenance::Native(component)) => component.to_string(),
            _ => graph.id(node).to_string(),
        };
        let mut component_cost: BTreeMap<String, u32> = BTreeMap::new();
        let mut group_component_cost: BTreeMap<(usize, u32, String), u32> = BTreeMap::new();
        for node in (0..count).filter(|&node| synchronous(node)) {
            let total = component_cost.entry(component(node)).or_default();
            *total = total.saturating_add(cost[node]);
            let in_group = group_component_cost
                .entry((sets.find(node), depth[node], component(node)))
                .or_default();
            *in_group = in_group.saturating_add(cost[node]);
        }
        let budget = component_cost
            .values()
            .copied()
            .max()
            .unwrap_or_default()
            .max(REGION_COST_FLOOR);
        let mut stage = vec![0u32; count];
        let mut stage_cost: BTreeMap<(usize, u32, u32), u32> = BTreeMap::new();
        let mut component_stage: BTreeMap<(usize, u32, String), u32> = BTreeMap::new();
        for &node in &graph.order {
            if !synchronous(node) {
                continue;
            }
            let group = (sets.find(node), depth[node]);
            let Some(op) = graph.op(node) else { continue };
            // A node runs no earlier than the stage of any input in its group.
            let mut at = (0..op.ports().inputs.len())
                .filter_map(|port| graph.source(node, port))
                .filter(|&(from, _)| synchronous(from) && depth[from] == depth[node])
                .map(|(from, _)| stage[from])
                .max()
                .unwrap_or(0);
            let key = (group.0, group.1, component(node));
            match component_stage.get(&key) {
                Some(&placed) => at = at.max(placed),
                None => {
                    let added = group_component_cost.get(&key).copied().unwrap_or_default();
                    let used = stage_cost.get(&(group.0, group.1, at)).copied().unwrap_or_default();
                    if used > 0 && used.saturating_add(added) > budget {
                        at += 1;
                    }
                    let used = stage_cost.entry((group.0, group.1, at)).or_default();
                    *used = used.saturating_add(added);
                    component_stage.insert(key, at);
                }
            }
            stage[node] = at;
        }

        // Slots: neighbors in one set with equal depth and stage.
        let mut groups = UnionFind::new(count);
        for &((from, _), (to, _)) in &edges {
            if synchronous(from) && synchronous(to) && depth[from] == depth[to] && stage[from] == stage[to] {
                groups.union(from, to);
            }
        }
        let mut by_root: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
        for node in 0..count {
            if synchronous(node) {
                by_root.entry(groups.find(node)).or_default().push(node);
            }
        }
        let mut slots = by_root.into_values().collect::<Vec<_>>();
        slots.sort_by_key(|members| members[0]);
        let mut slot_of = vec![None; count];
        for (index, members) in slots.iter().enumerate() {
            for &node in members {
                slot_of[node] = Some(index);
            }
        }

        Self {
            slots,
            slot_of,
            depth,
            via,
            cost,
            budget,
            edges,
        }
    }
}

/// A disjoint-set forest over node positions.
struct UnionFind {
    parent: Vec<usize>,
}

impl UnionFind {
    fn new(count: usize) -> Self {
        Self {
            parent: (0..count).collect(),
        }
    }

    fn find(&mut self, mut node: usize) -> usize {
        while self.parent[node] != node {
            self.parent[node] = self.parent[self.parent[node]];
            node = self.parent[node];
        }
        node
    }

    fn union(&mut self, a: usize, b: usize) {
        let (a, b) = (self.find(a), self.find(b));
        if a != b {
            // The smaller position becomes the root, so roots are deterministic.
            let (root, child) = if a < b { (a, b) } else { (b, a) };
            self.parent[child] = root;
        }
    }
}
