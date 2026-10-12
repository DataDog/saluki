//! The physical frame: the structure of the topology, fixed at boot, that every generation runs in.
//!
//! A topology is built once, and a component's outputs are fixed when it is added, so the physical structure cannot
//! follow each candidate plan. [`Frame::compute`] derives it once from the boot plan (the default program):
//!
//! - **Fixed components** are the plan's fences, each run by a component of its own: sources (`io_host`) and opaque
//!   hosts (`opaque_host`). Each of their output ports is an **admission point**, where batches are stamped with their
//!   admission time and generation: sources admit (they stamp what is not stamped yet), and opaque hosts readmit (they
//!   stamp everything again, so nothing crosses them).
//! - The **encode hosts** are fixed: one per delivery host the host builds ([`FrameHosts`]), each running, as leases,
//!   the encoder streams of the outputs bound to its delivery host, which change live. An output without a binding is
//!   a **fixed destination**, a component of its own, fixed too.
//! - **Slots** hold the synchronous operators between fences. A slot is a connected set of synchronous nodes, as large
//!   as fusion allows (rewrite R1): two synchronous neighbors share a slot unless some path leaves one of them through
//!   a fence and comes back to the other, since fusing them would then form a cycle through that fence, or unless
//!   R1's cost model cuts between them, so that a chain too costly for one task pipelines across several (see
//!   [below](#pipelining)). A slot may have several inputs. Each slot is run by one region host component, named by
//!   the host's [slot namer][SlotNamer].
//! - **Isolation slots** each hold one state owner behind a declared isolation fence
//!   ([`Boundary::Isolated`]), such as aggregation as a kernel: it is never fused, and its region owns its state, its
//!   watermark, and its flush ticks (see [below](#isolation-slots)).
//! - Each slot's **exits** are its outputs: one for every edge of the boot plan that leaves the slot, to a fixed
//!   component or to another slot, one for every output the boot plan delivers to downstream of the slot (to its
//!   delivery host's encode host, or to its fixed destination), whether or not the boot plan emits to it from the
//!   slot, and, for a slot with an output downstream, one to every encode host, so that an output bound later to any
//!   delivery host has a path. Exits nothing uses stay connected and idle, so that later generations can use them
//!   without changing the topology. Exits are named after their target: the fixed component, the slot, the encode
//!   host, or the destination's output. A batch an exit sends to an encode host names its output's lease in its
//!   header's entry.
//! - The **edges** connect admission points and exits to their targets. An `emit` or `drop` whose only input comes
//!   straight from a fence is not run in a slot: it becomes a direct edge from the admission point to the fixed
//!   destination, or no edge at all. A direct edge to an encode host is an error, since only a region writes a lease.
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
//! - In a region, `split`s and `tee`s joined by edges are one **decision**, run as one function (see
//!   [`decision`][super::decision]), so they are never cut, and their `split`s are priced together as the function
//!   runs them: derived fields such as the intake name are computed once, and a memoizable decision costs one memo
//!   lookup (R4) in place of its predicates. A `tee` keeps its copies' estimate. The **units** R1 never cuts are the
//!   native components, each joined with every other that one of its decisions reaches into.
//! - Walking each fused set in topological order, a unit joins the regions its inputs come from, on their stage, if
//!   the joined region's estimate stays within the budget, and starts the next stage of a pipeline otherwise. Regions
//!   are tracked as they form, so parallel branches, which run in regions of their own, never add up unless a node
//!   joins them; a unit that adds no work to the one region it joins never moves, so no region holds only a terminal.
//!   No region does less than a native component did, and none costs more than the budget unless one unit alone does.
//!
//! A cut follows the edges, so a slot's **stage** only grows along them, and the slots and fixed components stay
//! acyclic. The frame records each cut as a [`FusionVerdict::Pipelined`] decision.
//!
//! # Isolation slots
//!
//! A state owner behind an isolation fence counts as a fence in the fence-depth walk, as a host does, and runs in an
//! isolation slot of its own ([`SlotKind::Isolation`]); R1 neither fuses it nor prices it. Its exits are only the
//! targets of its boot edges: what it emits at its flush ticks goes where the boot plan sends it. Every edge into or
//! out of it is recorded as a [`FusionVerdict::Isolation`] decision.
//!
//! The slot's watermark is made of the latest admission times of the admission points **upstream** of it: those whose
//! physical edge leads to it through fused slots only. Exactly one admission point feeds a state owner, over exactly
//! one path, until repartition edges ([`FrameError::StateOwnerPoints`], [`FrameError::StateOwnerPaths`]). That point
//! [feeds state][AdmissionPointInfo::feeds_state], so it sends heartbeats toward the slot and a cut is armed over it,
//! and every exit on its path [forwards its progress][Exit::progress_points]. The slot stamps its time-driven output
//! as if its output were an admission point of its own, numbered after every admission point of the fixed
//! components.

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
        HostId, Namespace, NativeComponentId, NodeId, OperatorName, OutputDecl, OutputId, Params, Plan, PortName,
        PortRef, Provenance,
    },
    operators::{decision_cost, names, Boundary, OperatorRegistry, StateContract},
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

    /// The encode host of a delivery host, by the delivery host: it runs the lease of every output bound to the
    /// delivery host, and each batch names its output's lease in its header's entry.
    EncodeHost(HostId),

    /// A fixed destination: an output without a binding, run by a component of its own, such as an internal tap.
    Destination(OutputId),
}

impl FrameTarget {
    /// Returns where the records an output's `emit`s deliver go: the encode host of its delivery host when it is bound
    /// to one, and the output's own component, a fixed destination, otherwise.
    pub fn of_output(output: &OutputDecl) -> Self {
        match &output.binding {
            Some(binding) => Self::EncodeHost(binding.host.clone()),
            None => Self::Destination(output.id.clone()),
        }
    }

    /// Returns where the records an `emit` to `output` deliver go in `plan`: as [`of_output`][Self::of_output] says
    /// for the plan's declaration, and a fixed destination for an output the plan does not declare, which validation
    /// reports.
    pub fn of_emit(plan: &Plan, output: &OutputId) -> Self {
        plan.output(output)
            .map_or_else(|| Self::Destination(output.clone()), Self::of_output)
    }
}

/// The encode host of one delivery host: the fixed component that runs, as leases, the encoder streams of every output
/// bound to the delivery host.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FrameHost {
    /// The delivery host.
    pub host: HostId,

    /// The component ID of its encode host, which names the slots' exits that lead to it.
    pub encoder: NativeComponentId,
}

/// The delivery hosts of a frame, each with its encode host: fixed at boot, as every component is.
///
/// The host supplies them. Every fused slot that has an output downstream gets an exit to every encode host, so that an
/// output bound to any of them later, such as a failover output enabled live, has a physical path; an exit nothing uses
/// carries nothing.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct FrameHosts {
    hosts: Vec<FrameHost>,
}

impl FrameHosts {
    /// Creates a set of frame hosts without any host: every output is then a fixed destination.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a delivery host with the component ID of its encode host, replacing any encode host it had.
    pub fn with_host(mut self, host: HostId, encoder: NativeComponentId) -> Self {
        match self.hosts.iter_mut().find(|existing| existing.host == host) {
            Some(existing) => existing.encoder = encoder,
            None => self.hosts.push(FrameHost { host, encoder }),
        }
        self
    }

    /// Returns the delivery host with its encode host, if the frame has it.
    pub fn get(&self, host: &HostId) -> Option<&FrameHost> {
        self.hosts.iter().find(|existing| &existing.host == host)
    }

    /// Returns every delivery host with its encode host, in the order they were added.
    pub fn iter(&self) -> impl Iterator<Item = &FrameHost> {
        self.hosts.iter()
    }

    /// Returns `true` if the frame has no delivery host.
    pub fn is_empty(&self) -> bool {
        self.hosts.is_empty()
    }
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

    /// Whether the point feeds a state owner: whether it is upstream of an isolation slot, so that it sends heartbeats
    /// toward it and a cut is armed over it.
    pub feeds_state: bool,
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

    /// The admission points whose progress the exit forwards: those upstream of the state owners downstream of it,
    /// whose watermarks need progress through the exit even when a batch leaves the slot without output. Empty for
    /// an exit that leads to no state owner.
    pub progress_points: Vec<AdmissionPointId>,
}

/// What kind of slot a slot is.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SlotKind {
    /// A slot of fused synchronous nodes.
    Fused,

    /// An isolation slot: the region of a state owner behind a declared isolation fence, which owns its state, its
    /// watermark, and its flush ticks.
    Isolation {
        /// The ID that stamps the slot's time-driven output: numbered after every admission point of the frame's fixed
        /// components, and only ever seen in headers, never registered.
        output_point: AdmissionPointId,

        /// The admission points with a physical path to the slot, in ID order, whose latest admission times make up
        /// its watermark.
        upstream: Vec<AdmissionPointId>,
    },
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

    /// What kind of slot it is: fused synchronous nodes, or an isolation slot.
    pub kind: SlotKind,

    /// The estimated cost per event of the slot's boot nodes, in nanoseconds: the sum of their operators' estimates,
    /// with the `split`s of each decision priced together (see [Pipelining](self#pipelining)).
    pub cost: u32,
}

impl Slot {
    /// Returns the exit leading to `target`, if the slot has one.
    pub fn exit_to(&self, target: &FrameTarget) -> Option<&Exit> {
        self.exits.iter().find(|exit| &exit.target == target)
    }

    /// Returns `true` if the slot is an isolation slot.
    pub fn is_isolation(&self) -> bool {
        matches!(self.kind, SlotKind::Isolation { .. })
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

    /// The edge enters or leaves an isolation slot, which runs its state owner alone.
    Isolation {
        /// The isolation slot.
        slot: SlotId,
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

    /// The boot plan binds an output to a delivery host the frame has no encode host for.
    #[snafu(display(
        "Output `{}` is bound to delivery host `{}`, which the frame has no encode host for; give the frame every \
         delivery host its outputs deliver through.",
        output,
        host
    ))]
    UnknownHost {
        /// The output.
        output: OutputId,

        /// The delivery host.
        host: HostId,
    },

    /// An admission point's records would go straight to an encode host, with no region to write each batch's lease.
    #[snafu(display(
        "`{}` feeds `emit {}` directly, but the output is bound to delivery host `{}`, whose encode host needs each \
         batch to name its lease, which only a region writes. Put a region between the host and the output.",
        port,
        output,
        host
    ))]
    DirectToEncodeHost {
        /// The admission point's port.
        port: PortRef,

        /// The output.
        output: OutputId,

        /// The delivery host.
        host: HostId,
    },

    /// An isolation slot has more than one admission point upstream of it.
    ///
    /// A state owner's watermark is the earliest latest admission time of the points upstream of it, and its clock
    /// resets at a point's reset. Resets at several points need a rule that does not close one point's open buckets
    /// early at the reset of another, so a state owner is fed by one admission point, until repartition edges.
    #[snafu(display(
        "State owner slot `{}` is fed by several admission points ({}), and a state owner is fed by one admission \
         point until stage 9 adds repartition edges. Feed the state owner from one source.",
        slot,
        describe_ports(points)
    ))]
    StateOwnerPoints {
        /// The isolation slot.
        slot: NativeComponentId,

        /// The admission points upstream of it.
        points: Vec<PortRef>,
    },

    /// An admission point reaches an isolation slot over more than one physical path.
    ///
    /// A state owner's watermark relies on batches from one admission point arriving in the order they were stamped,
    /// which holds only along one path.
    #[snafu(display(
        "Admission point `{}` reaches state owner slot `{}` over more than one path, and a state owner's watermark \
         needs one path from each admission point. Join the paths before the state owner's input, in one region.",
        point,
        slot
    ))]
    StateOwnerPaths {
        /// The isolation slot.
        slot: NativeComponentId,

        /// The admission point.
        point: PortRef,
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

fn describe_ports(ports: &[PortRef]) -> String {
    ports
        .iter()
        .map(|port| format!("`{}`", port))
        .collect::<Vec<_>>()
        .join(", ")
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
    hosts: FrameHosts,
    edges: Vec<FrameEdge>,
    fusion: Vec<FusionDecision>,
    sites: BTreeMap<NodeId, Site>,
    cost_budget: u32,
}

impl Frame {
    /// Computes the frame of a boot plan without delivery hosts: every output is a fixed destination.
    ///
    /// This is [`compute_with_hosts`][Self::compute_with_hosts] with no hosts; see there.
    ///
    /// # Errors
    ///
    /// Returns an error for any reason `compute_with_hosts` does, and so for a boot plan with a bound output.
    pub fn compute(plan: &Plan, registry: &OperatorRegistry, namer: &SlotNamer<'_>) -> Result<Self, FrameError> {
        Self::compute_with_hosts(plan, registry, namer, &FrameHosts::new())
    }

    /// Computes the frame of a boot plan, with the delivery hosts and encode hosts the host builds.
    ///
    /// The plan must be structurally sound: every operator known and configured, every edge resolved, and no cycle.
    /// `namer` names each slot's region host component. A bound output delivers through the encode host of its
    /// delivery host, which `hosts` must have; an output without a binding is a fixed destination.
    ///
    /// # Errors
    ///
    /// Returns an error if the plan has structural problems, if it has a node that keeps state without an isolation
    /// fence, one behind an isolation fence that keeps no keyed state, or one that reads a signal (no region runs one
    /// in this stage), if a state owner is fed by no admission point, by several, or by one over several paths, if the
    /// namer refuses a slot or names two alike, if a slot's exits would share a name, if an output is bound to a
    /// delivery host `hosts` does not have, if an admission point would feed an encode host directly, or if the plan
    /// is too large to number.
    pub fn compute_with_hosts(
        plan: &Plan, registry: &OperatorRegistry, namer: &SlotNamer<'_>, hosts: &FrameHosts,
    ) -> Result<Self, FrameError> {
        let graph = resolve(plan, registry).map_err(|problems| FrameError::InvalidPlan { problems })?;
        let roles = super::roles(&graph);
        check_supported(&graph, &roles)?;
        for output in &plan.outputs {
            if let Some(binding) = &output.binding {
                if hosts.get(&binding.host).is_none() {
                    return Err(FrameError::UnknownHost {
                        output: output.id.clone(),
                        host: binding.host.clone(),
                    });
                }
            }
        }

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
                kind: SlotKind::Fused,
                cost,
            });
        }

        let slot_of = |node: usize| layout.slot_of[node].map(|slot| SlotId(slot as u16));
        let slot_names = slots.iter().map(|slot| slot.name.clone()).collect::<Vec<_>>();

        // Exits: the edges leaving each slot, in plan order, then every output downstream of it and every encode host.
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
                        Some(output) => FrameTarget::of_emit(plan, output),
                        None => continue,
                    },
                };
                if !targets.contains(&target) {
                    targets.push(target);
                }
            }
            // A fused slot has an exit to every output downstream of it, and, if it has any, to every encode host, so
            // that later generations can emit to them, an output bound live included; an isolation slot only those of
            // its boot edges, since only its state owner runs in it.
            let isolated = members.iter().any(|&node| roles[node] == Role::Isolated);
            let downstream = if isolated {
                BTreeSet::new()
            } else {
                downstream_outputs(&graph, members)
            };
            for output in &plan.outputs {
                let target = FrameTarget::of_output(output);
                if downstream.contains(&output.id) && !targets.contains(&target) {
                    targets.push(target);
                }
            }
            if !downstream.is_empty() {
                for host in hosts.iter() {
                    let target = FrameTarget::EncodeHost(host.host.clone());
                    if !targets.contains(&target) {
                        targets.push(target);
                    }
                }
            }

            let mut exit_names = BTreeSet::new();
            for (index, target) in targets.into_iter().enumerate() {
                let index = ExitIndex(u16::try_from(index).map_err(|_| too_large("exits in one slot"))?);
                let name = match &target {
                    FrameTarget::Fixed(node) => component_name(node),
                    FrameTarget::Slot(other) => slot_names[other.get()].to_string(),
                    // Every bound output's host is a frame host, checked above.
                    FrameTarget::EncodeHost(host) => hosts
                        .get(host)
                        .map_or_else(|| host.to_string(), |host| host.encoder.to_string()),
                    FrameTarget::Destination(output) => output.to_string(),
                };
                if !exit_names.insert(name.clone()) {
                    return Err(FrameError::ExitNameCollision {
                        slot: slot.name.clone(),
                        name,
                    });
                }
                // Node identifiers, output identifiers, and slot names are all valid component IDs.
                let name = NativeComponentId::new(name).expect("exit names are valid component IDs");
                slot.exits.push(Exit {
                    index,
                    name,
                    target,
                    progress_points: Vec::new(),
                });
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
                    Role::Synchronous | Role::Isolated => slot_of(to).map(FrameTarget::Slot),
                    Role::Terminal => graph
                        .op(to)
                        .and_then(emit_output)
                        .map(|output| FrameTarget::of_emit(plan, output)),
                });
                if let Some(FrameTarget::EncodeHost(host)) = &route {
                    // Only a region writes a batch's lease, so nothing would tell the encode host which output's.
                    let (to, _) = graph.target(node, port).expect("a route has a target");
                    return Err(FrameError::DirectToEncodeHost {
                        port: PortRef::new(graph.id(node).clone(), output.name.clone()),
                        output: graph
                            .op(to)
                            .and_then(emit_output)
                            .cloned()
                            .expect("a route to an encode host is an emit's"),
                        host: host.clone(),
                    });
                }
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
                    feeds_state: false,
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

        // Isolation slots: their output points, and the admission points upstream of them, with the exits that carry
        // those points' progress.
        let mut next_point = admission_points.len();
        for (index, members) in layout.slots.iter().enumerate() {
            if members.iter().any(|&node| roles[node] == Role::Isolated) {
                let point = AdmissionPointId::new(u32::try_from(next_point).map_err(|_| too_large("ports"))?);
                next_point += 1;
                slots[index].kind = SlotKind::Isolation {
                    output_point: point,
                    upstream: Vec::new(),
                };
            }
        }
        state_upstream(&graph, &layout, &mut admission_points, &mut slots)?;

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
                    (Role::Isolated, _) | (_, Role::Isolated) => {
                        let isolated = if roles[from] == Role::Isolated { from } else { to };
                        FusionVerdict::Isolation {
                            slot: slot_of(isolated).expect("every state owner behind an isolation fence has a slot"),
                        }
                    }
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
                    Role::Synchronous | Role::Isolated => {
                        Site::Slot(slot_of(node).expect("every node a frame runs in a region is in a slot"))
                    }
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
            hosts: hosts.clone(),
            edges,
            fusion,
            sites,
            cost_budget: layout.budget,
        })
    }

    /// Returns the frame's delivery hosts, each with its encode host.
    pub fn hosts(&self) -> &FrameHosts {
        &self.hosts
    }

    /// Returns the delivery host with its encode host, if the frame has it.
    pub fn encode_host(&self, host: &HostId) -> Option<&FrameHost> {
        self.hosts.get(host)
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

    /// Returns the admission points upstream of the isolation slots among `slots`, in ID order: the points a cut of
    /// the state owners in those slots is armed over.
    ///
    /// Fused slots have no upstream points of their own, so they add nothing.
    pub fn affected_points(&self, slots: &[SlotId]) -> Vec<AdmissionPointId> {
        let mut points = BTreeSet::new();
        for slot in slots.iter().filter_map(|&slot| self.slot(slot)) {
            if let SlotKind::Isolation { upstream, .. } = &slot.kind {
                points.extend(upstream.iter().copied());
            }
        }
        points.into_iter().collect()
    }

    /// Returns every admission point in the order a cut's cancellation locks them: the points that feed no state
    /// owner first, then those that do, each in ID order.
    ///
    /// A point's lock is held while its batches fan out, so taking the points that feed state owners last holds them
    /// only for the check and the store, never while waiting for another point's lock.
    pub fn cancel_lock_order(&self) -> Vec<AdmissionPointId> {
        let (state, other): (Vec<_>, Vec<_>) = self.admission_points.iter().partition(|point| point.feeds_state);
        other.iter().chain(&state).map(|point| point.id).collect()
    }

    /// Returns the boot plan's declared outputs, in plan order: its fixed destinations, which a candidate declares as
    /// at boot, and the outputs bound to the frame's delivery hosts, whose leases later generations may change.
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

    /// Returns the name of a physical edge's target: a fixed component's node, a slot's region host, an encode host,
    /// or a fixed destination's output.
    pub fn describe_target(&self, target: &FrameTarget) -> String {
        match target {
            FrameTarget::Fixed(node) => node.to_string(),
            FrameTarget::Slot(slot) => self
                .slot(*slot)
                .map_or_else(|| format!("slot {}", slot), |slot| slot.name.to_string()),
            FrameTarget::EncodeHost(host) => self.encode_host(host).map_or_else(
                || format!("the encode host of {}", host),
                |found| format!("encode host {} of {}", found.encoder, host),
            ),
            FrameTarget::Destination(output) => format!("output {}", output),
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

/// Rejects nodes that no region runs in this stage.
///
/// A region runs a node that keeps state only behind a declared isolation fence, in an isolation slot of its own, and
/// a node behind such a fence only if it keeps keyed state.
fn check_supported(graph: &Graph<'_>, roles: &[Role]) -> Result<(), FrameError> {
    for (node, role) in roles.iter().enumerate() {
        if *role == Role::Fixed {
            continue;
        }
        let op = graph.op(node).expect("a resolved plan configures every node");
        let keyed = matches!(op.contract().state, StateContract::Keyed(_));
        if *role == Role::Isolated && !keyed {
            return Err(FrameError::Unsupported {
                node: graph.id(node).clone(),
                what: "running an operator behind an isolation fence that keeps no keyed state".to_string(),
                stage: 10,
            });
        }
        if *role != Role::Isolated && op.contract().state != StateContract::None {
            return Err(FrameError::Unsupported {
                node: graph.id(node).clone(),
                what: "running an operator that keeps state in a region without an isolation fence".to_string(),
                stage: 10,
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

/// Finds the admission points upstream of each isolation slot, whose latest admission times make up its state owner's
/// watermark, and the exits that carry their progress toward it.
///
/// A point is upstream of an isolation slot when its physical edge leads to the slot through fused slots only: a fixed
/// component re-admits what it receives, and an isolation slot stamps its own time-driven output, so neither passes a
/// point's stamps on. A state owner is fed by exactly one admission point over exactly one path, since resets at
/// several points need a rule for how the reset of one point closes the open buckets of another, and a watermark relies
/// on a point's batches arriving in the order they were stamped, which only one path keeps. Every exit on that path
/// carries the point's progress, so that the watermark advances even when a batch leaves a slot without output.
fn state_upstream(
    graph: &Graph<'_>, layout: &Layout, points: &mut [AdmissionPointInfo], slots: &mut [Slot],
) -> Result<(), FrameError> {
    let targets = slots
        .iter()
        .filter(|slot| slot.is_isolation())
        .map(|slot| slot.id)
        .collect::<Vec<_>>();
    for target in targets {
        let mut paths = vec![None; slots.len()];
        let mut upstream = Vec::new();
        for point in points.iter() {
            let Some(FrameTarget::Slot(first)) = &point.route else {
                continue;
            };
            let count = paths_to(*first, target, slots, &mut paths);
            if count > 1 {
                return Err(FrameError::StateOwnerPaths {
                    slot: slots[target.get()].name.clone(),
                    point: PortRef::new(point.node.clone(), point.port.clone()),
                });
            }
            if count == 1 {
                upstream.push((point.id, *first));
            }
        }
        if upstream.len() > 1 {
            return Err(FrameError::StateOwnerPoints {
                slot: slots[target.get()].name.clone(),
                points: upstream
                    .iter()
                    .filter_map(|(id, _)| points.iter().find(|point| point.id == *id))
                    .map(|point| PortRef::new(point.node.clone(), point.port.clone()))
                    .collect(),
            });
        }
        let Some(&(point, first)) = upstream.first() else {
            let node = layout.slots[target.get()][0];
            return Err(FrameError::Unsupported {
                node: graph.id(node).clone(),
                what: "running a state owner that no admission point feeds, such as one fed by another state owner's \
                       time-driven output"
                    .to_string(),
                stage: 10,
            });
        };

        // The one path from the point: every exit on it forwards the point's progress.
        let mut on_path = Vec::new();
        let mut current = first;
        while current != target {
            let exit = slots[current.get()].exits.iter().find(|exit| match exit.target {
                FrameTarget::Slot(next) => paths_to(next, target, slots, &mut paths) > 0,
                _ => false,
            });
            let Some(exit) = exit else { break };
            on_path.push((current, exit.index));
            let FrameTarget::Slot(next) = exit.target else { break };
            current = next;
        }
        for (slot, exit) in on_path {
            let progress = &mut slots[slot.get()].exits[exit.get()].progress_points;
            if !progress.contains(&point) {
                progress.push(point);
                progress.sort();
            }
        }
        if let SlotKind::Isolation { upstream, .. } = &mut slots[target.get()].kind {
            *upstream = vec![point];
        }
        if let Some(info) = points.iter_mut().find(|info| info.id == point) {
            info.feeds_state = true;
        }
    }
    Ok(())
}

/// Returns the number of physical paths from `slot` to the isolation slot `target` through fused slots, saturating,
/// memoized in `paths` by slot: one for `target` itself, and none from another isolation slot.
fn paths_to(slot: SlotId, target: SlotId, slots: &[Slot], paths: &mut [Option<u64>]) -> u64 {
    if slot == target {
        return 1;
    }
    if let Some(count) = paths[slot.get()] {
        return count;
    }
    let count = if slots[slot.get()].is_isolation() {
        0
    } else {
        slots[slot.get()]
            .exits
            .iter()
            .filter_map(|exit| match exit.target {
                FrameTarget::Slot(next) => Some(next),
                _ => None,
            })
            .fold(0u64, |count, next| {
                count.saturating_add(paths_to(next, target, slots, paths))
            })
    };
    paths[slot.get()] = Some(count);
    count
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

    /// Each node's slot, for synchronous nodes and state owners behind an isolation fence.
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
                    // An isolation fence separates as a host does: a state owner's slot is a fence for fusion.
                    let fence = matches!(roles[from], Role::Fixed | Role::Isolated);
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

        // R1's cost model: the stage of each node within its set and depth, so that each region stays within the
        // budget (see the module documentation).
        let native_cost = (0..count)
            .map(|node| match graph.op(node) {
                Some(op) if synchronous(node) => op.cost(),
                _ => 0,
            })
            .collect::<Vec<_>>();
        let component = |node: usize| match &graph.nodes[node].node.provenance {
            Some(Provenance::Native(component)) => component.to_string(),
            _ => graph.id(node).to_string(),
        };
        // The budget is what the costliest native component costs natively.
        let mut component_cost: BTreeMap<String, u32> = BTreeMap::new();
        for node in (0..count).filter(|&node| synchronous(node)) {
            let total = component_cost.entry(component(node)).or_default();
            *total = total.saturating_add(native_cost[node]);
        }
        let budget = component_cost
            .values()
            .copied()
            .max()
            .unwrap_or_default()
            .max(REGION_COST_FLOOR);

        // Decisions: `split`s and `tee`s joined by edges run as one function, so a region prices their `split`s
        // together, as the function runs them (`tee`s keep their copies' estimates), on the first `split` of each.
        let decides = |node: usize| {
            synchronous(node)
                && graph
                    .op(node)
                    .is_some_and(|op| matches!(op.name(), names::SPLIT | names::TEE))
        };
        let mut decisions = UnionFind::new(count);
        for &((from, _), (to, _)) in &edges {
            if decides(from) && decides(to) && depth[from] == depth[to] {
                decisions.union(from, to);
            }
        }
        let mut cost = native_cost;
        let mut splits_of: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
        for &node in &graph.order {
            if decides(node) && graph.op(node).is_some_and(|op| op.name() == names::SPLIT) {
                splits_of.entry(decisions.find(node)).or_default().push(node);
            }
        }
        for splits in splits_of.values() {
            let predicates = splits
                .iter()
                .filter_map(|&split| graph.op(split)?.params().predicate("predicate"))
                .collect::<Vec<_>>();
            for &split in splits {
                cost[split] = 0;
            }
            cost[splits[0]] = decision_cost(&predicates);
        }

        // Units: what R1 never cuts. Each is a native component (its nodes in one set and depth), joined with every
        // other that one of its decisions reaches into, since a decision is never cut either.
        let mut units = UnionFind::new(count);
        let mut first_of_component: BTreeMap<(usize, u32, String), usize> = BTreeMap::new();
        for node in (0..count).filter(|&node| synchronous(node)) {
            let first = *first_of_component
                .entry((sets.find(node), depth[node], component(node)))
                .or_insert(node);
            units.union(first, node);
        }
        for node in (0..count).filter(|&node| decides(node)) {
            units.union(decisions.find(node), node);
        }
        let mut unit_cost = vec![0u32; count];
        for node in (0..count).filter(|&node| synchronous(node)) {
            let unit = units.find(node);
            unit_cost[unit] = unit_cost[unit].saturating_add(cost[node]);
        }

        // Walking each set in plan order, a unit joins the regions of its inputs on their stage unless the joined
        // region would cost more than the budget. Regions are tracked as they form, so that parallel branches, which
        // run in regions of their own, are never summed unless a node joins them.
        let mut stage = vec![0u32; count];
        let mut unit_stage: Vec<Option<u32>> = vec![None; count];
        let mut regions = UnionFind::new(count);
        let mut region_cost = vec![0u32; count];
        for &node in &graph.order {
            if !synchronous(node) {
                continue;
            }
            let Some(op) = graph.op(node) else { continue };
            // A node runs no earlier than the stage of any input in its group.
            let inputs = (0..op.ports().inputs.len())
                .filter_map(|port| graph.source(node, port))
                .map(|(from, _)| from)
                .filter(|&from| synchronous(from) && depth[from] == depth[node])
                .collect::<Vec<_>>();
            let mut at = inputs.iter().map(|&from| stage[from]).max().unwrap_or(0);
            let unit = units.find(node);
            let added = match unit_stage[unit] {
                Some(placed) => {
                    at = at.max(placed);
                    0
                }
                None => {
                    let added = unit_cost[unit];
                    let mut joined = inputs
                        .iter()
                        .filter(|&&from| stage[from] == at)
                        .map(|&from| regions.find(from))
                        .collect::<Vec<_>>();
                    joined.sort_unstable();
                    joined.dedup();
                    let merged = joined
                        .iter()
                        .map(|&region| region_cost[region])
                        .fold(added, u32::saturating_add);
                    // A unit that adds no work to the one region it joins stays with it, even past the budget (behind
                    // a unit that alone costs more): moving it would only add a region for a lone `emit` or `drop`.
                    if !joined.is_empty() && merged > budget && (added > 0 || joined.len() > 1) {
                        at += 1;
                    }
                    unit_stage[unit] = Some(at);
                    added
                }
            };
            stage[node] = at;
            region_cost[node] = added;
            for &from in &inputs {
                let (own, other) = (regions.find(node), regions.find(from));
                if stage[from] == at && own != other {
                    let total = region_cost[own].saturating_add(region_cost[other]);
                    regions.union(own, other);
                    region_cost[regions.find(own)] = total;
                }
            }
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
        // Each state owner behind an isolation fence runs alone, in an isolation slot of its own.
        slots.extend(
            (0..count)
                .filter(|&node| roles[node] == Role::Isolated)
                .map(|node| vec![node]),
        );
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
