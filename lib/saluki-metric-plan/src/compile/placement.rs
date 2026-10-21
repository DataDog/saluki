//! Placement: running a candidate plan in the physical frame.
//!
//! [`place`] assigns every node of a validated candidate plan to where it runs, and checks that the frame has the
//! physical structure the candidate needs:
//!
//! - A **fence** (source or opaque host) runs in the fixed component of the same node, configured as at boot. A host
//!   component keeps the configuration it was built with, so adding or reconfiguring one needs a restart. A fixed
//!   component the candidate leaves out is accepted only if it would then be idle: if a slot's exit is all that feeds
//!   it and none of its outputs feeds a slot. A source is never left out: its host keeps producing.
//! - An `emit` or `drop` fed straight by a fence must match the frame's direct edge: the same fixed destination, or no
//!   edge. A host never feeds an encode host directly, since only a region writes the lease it reads.
//! - Every other node is synchronous and runs in a slot. A node the boot plan has runs in its boot slot, where its
//!   inputs can reach that slot: in a frame that isolates state, a boot state owner's slot and the slots after it are
//!   apart from the slots its inputs come from. Any other node's slot follows from its inputs: the slot an admission
//!   point's edge leads to, or the slot of the node feeding it. When its inputs come from several slots, it runs in the
//!   one that every other of them has an exit to.
//! - A state owner behind an isolation fence places as any synchronous node: a boot state owner in its boot slot,
//!   which the slot an admission point feeding it straight leads to passes the point's batches through to, unchanged,
//!   and any other by its inputs. Each slot then runs it in a region of its own (see [below](#regions)).
//! - Every edge between slots, from a slot to a fixed component, and from a slot to an output must use one of the
//!   slot's exits, and every edge leaving a fixed component must use its physical edge. An edge to an `emit` leads to
//!   the encode host of the output's delivery host when the output is bound, and to its fixed destination otherwise.
//! - Every bound output places by its delivery host, whose encode host the frame must have: the output itself, and
//!   its binding, can be new, since it runs as a lease of the encode host. An output without a binding must be a fixed
//!   destination of the frame, declared as at boot.
//!
//! What the candidate does not use stays idle: an exit that no edge uses carries nothing. A candidate that needs
//! structure the frame lacks is rejected, with one [problem][PlacementProblem] per node or edge, each naming what it
//! needs. Nothing is approximated. Structure that native configuration asks for needs a restart, since the frame is
//! built from native configuration at boot ([`MissingCapability::Restart`]); structure only an artifact asks for, at a
//! node the artifact wrote, is not in the frame built from the default program, and a restart does not add it
//! ([`MissingCapability::FrameChanges`], stage 10).
//!
//! # Regions
//!
//! Each slot's nodes are partitioned into **regions**, in topological order ([`SlotPlacement::regions`]). The main
//! region runs the slot's entries. A region runs at most one state owner. A state owner runs in the highest region
//! among those of its inputs in the slot, the main region when it has none, if that region runs no state owner yet and
//! has one lane, or is the main region of the state owner's boot slot, so that a boot state owner runs in its slot's
//! own task, as at boot; otherwise it opens a region of its own, a **sub-region** ([`RegionKey::State`]): a task of the
//! slot's component, fed through internal exits of the regions before it. Since every sub-region runs a state owner,
//! only the main region takes one in. Any other node runs in the highest region among those of its inputs in the slot,
//! so that a state owner's consumers in its slot run in its region and write the slot's exits.
//! Edges between regions of a slot go only to later regions: the regions of a slot form a DAG, with no loop through
//! their bounded inputs. A sub-region runs on one lane, fed by every lane of the regions before it.
//!
//! Each state owner's **upstream points**, whose latest admission times make up its watermark, are found per
//! generation by walking its inputs back through stateless nodes: the admission points of fixed components, which
//! readmit what they receive, and the **derived points** of the state owners feeding it, which stamp their
//! time-driven output ([`DerivedPoints`]). Each point reaches the state owner over one physical path, along which its
//! batches keep their order: a point that reaches it over several is refused. Every exit and internal exit on that
//! path forwards the point's progress ([`RegionPlacement::progress`]). With lanes, a state owner fed by another on
//! several lanes, and one over several clocks one of which stamps several of its points (a point with lanes), are
//! refused; so is a state owner whose state cannot be partitioned by context key in a main region on several lanes,
//! each of which holds the share of its state that the slot's keyed exits route to it
//! ([`ExitRouting::Keyed`][super::frame::ExitRouting::Keyed], [`PartitionKey::Context`]).
//!
//! # Entries
//!
//! A slot's program maps each batch it receives to a logical input of the slot: an **entry**. A batch from a fixed
//! component is mapped by the admission point that stamped it ([`EntryKey::Admission`]). A batch from another slot
//! carries the entry in its header, set by the slot that sent it ([`EntryKey::Region`]), since several logical edges
//! can share one exit. Entry IDs are numbered per slot and per generation, in plan edge order. A slot that passes an
//! admission point's batches through to a boot state owner has an entry for the point whose input is the state
//! owner's, in its boot slot, which has the matching entry from the passing slot's exit. The slot's entries are its
//! main region's; a sub-region's program has an entry for each edge entering it, from the slot's other regions or from
//! outside the slot through its main region, in plan edge order.
//!
//! A point with lanes is one admission point per lane ([`AdmissionPointInfo::lanes`]), each stamping the batches of
//! its lane, so a logical edge leaving it has one entry per lane's ID, in lane order, each with the same input: the
//! slot's program runs every lane's batches alike, and each lane of the slot receives its own lane's. The entry of
//! the boot slot a passing slot leads to stays one: it is keyed by the entry its batches carry, which every
//! lane's exit writes alike.
//!
//! [`AdmissionPointInfo::lanes`]: super::frame::AdmissionPointInfo::lanes

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
};

use saluki_core::execution::{AdmissionPointId, EntryId};

use super::{
    frame::{AdmissionPointInfo, ExitIndex, ExitRouting, Frame, FrameSource, FrameTarget, Site, SlotId},
    program::{KernelIndex, StepExit},
    resolve, Role,
};
use crate::{
    ir::{NodeId, OutputId, Plan, PortRef},
    operators::{OperatorRegistry, PartitionKey, StateContract},
    validate::{
        graph::{emit_output, Graph},
        ValidationReport,
    },
};

/// A capability a candidate needs that the running host does not have.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum MissingCapability {
    /// State owners that run in regions, and changes to state owners (stage 6).
    State,

    /// A restart: the physical structure native configuration describes is built at boot, so structure that the
    /// current configuration needs and the frame lacks, such as a new host component or physical edge, is built by
    /// restarting with that configuration.
    Restart,

    /// Physical structure that only an artifact asks for, such as a new exit from a region to another: the
    /// frame is built from the default program, so a restart does not add it (stage 10).
    FrameChanges,

    /// Operators that read host signals (stage 10).
    Signals,
}

impl MissingCapability {
    /// Returns the stage that provides the capability, or `None` for a restart, which every stage provides.
    pub const fn stage(self) -> Option<u8> {
        match self {
            Self::State => Some(6),
            Self::Restart => None,
            Self::FrameChanges | Self::Signals => Some(10),
        }
    }

    /// Returns the capability's name, as error messages say it after the stage number.
    pub const fn name(self) -> &'static str {
        match self {
            Self::State => "state transitions",
            Self::Restart => "a restart",
            Self::FrameChanges => "frame changes",
            Self::Signals => "signals",
        }
    }
}

impl fmt::Display for MissingCapability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.stage() {
            Some(stage) => write!(f, "stage {} {}", stage, self.name()),
            None => f.write_str(self.name()),
        }
    }
}

/// Who asks for structure the frame lacks, which decides what a candidate needs to get it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AskedBy {
    /// Native configuration: the default program, or the parts of a composed plan it gives. A restart with the
    /// current configuration builds the frame from it.
    Native,

    /// An artifact: a restart builds the frame from the default program, which does not have it.
    Artifact,
}

impl AskedBy {
    /// Returns the capability that provides what was asked for.
    const fn capability(self) -> MissingCapability {
        match self {
            Self::Native => MissingCapability::Restart,
            Self::Artifact => MissingCapability::FrameChanges,
        }
    }

    /// Says that `what` is missing from the frame, and what provides it.
    fn needs(self, what: &str) -> String {
        match self {
            Self::Native => format!(
                "{} needs a restart, since the physical structure is built from native configuration at boot",
                what
            ),
            Self::Artifact => format!(
                "{} requires {}: the physical structure is built from the default program at boot, so a restart \
                 does not add it",
                what,
                MissingCapability::FrameChanges
            ),
        }
    }
}

/// One reason a candidate cannot be placed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlacementProblem {
    /// The node the problem is at, if it is at one.
    pub node: Option<NodeId>,

    /// The capability the candidate needs, if one would let it be placed.
    pub capability: Option<MissingCapability>,

    /// What the candidate needs, and why the frame cannot give it.
    pub message: String,
}

impl PlacementProblem {
    fn new(node: Option<&NodeId>, capability: Option<MissingCapability>, message: String) -> Self {
        Self {
            node: node.cloned(),
            capability,
            message,
        }
    }
}

impl fmt::Display for PlacementProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.node {
            Some(node) => write!(f, "node `{}`: {}", node, self.message),
            None => f.write_str(&self.message),
        }
    }
}

/// Why a candidate cannot be placed into a frame.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PlacementError {
    /// The candidate is not valid.
    Invalid {
        /// The validation errors, or the structural problems found.
        problems: Vec<String>,
    },

    /// The candidate needs structure the frame does not have.
    Unplaceable {
        /// Every problem found.
        problems: Vec<PlacementProblem>,
    },
}

impl PlacementError {
    /// Returns the capabilities the candidate needs, without repeats, in stage order.
    pub fn capabilities(&self) -> Vec<MissingCapability> {
        match self {
            Self::Invalid { .. } => Vec::new(),
            Self::Unplaceable { problems } => problems
                .iter()
                .filter_map(|problem| problem.capability)
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect(),
        }
    }
}

impl fmt::Display for PlacementError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid { problems } => write!(
                f,
                "The candidate plan is not valid, so it cannot be placed: {}. Fix the plan so that it validates.",
                problems.join("; ")
            ),
            Self::Unplaceable { problems } => {
                f.write_str("The candidate plan cannot run in the physical structure built at boot: ")?;
                let problems = problems.iter().map(ToString::to_string).collect::<Vec<_>>();
                write!(f, "{}.", problems.join("; "))
            }
        }
    }
}

impl std::error::Error for PlacementError {}

/// How a slot's program recognizes the logical input a batch belongs to.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum EntryKey {
    /// A batch from a fixed component, by the admission point that stamped it.
    Admission(AdmissionPointId),

    /// A batch from another slot, by the entry the sending slot set in its header.
    Region(EntryId),

    /// The time-driven output of the state owner whose kernel is at this index of the program's kernel table: what it
    /// emits at a flush tick, a clock reset, its retirement, or shutdown. Only a state owner's region has such an entry;
    /// the
    /// region runs them itself, never for a batch it receives.
    StateOutput(KernelIndex),
}

impl fmt::Display for EntryKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Admission(point) => write!(f, "admission point {}", point.get()),
            Self::Region(entry) => write!(f, "entry {}", entry.get()),
            Self::StateOutput(kernel) => write!(f, "the time-driven output of kernel {}", kernel),
        }
    }
}

/// One entry of a slot: a logical edge entering the slot from outside it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlacedEntry {
    /// How the slot recognizes the entry's batches.
    pub key: EntryKey,

    /// The physical source of the entry's batches: the admission point, or the other slot's exit.
    pub source: FrameSource,

    /// The output port, outside the slot, that the logical edge leaves.
    pub from: PortRef,

    /// The input port that the logical edge enters: inside the slot, or, for an entry the slot passes through to a
    /// boot state owner, the state owner's, in the boot slot it passes the batches to.
    pub input: PortRef,
}

/// Which region of a slot a node runs in.
///
/// A slot's component runs its main region on its lanes, and may run a **sub-region** per state owner: a task of the
/// component with an input of its own, created when a generation that has the state owner is prepared and retired with
/// the last such generation.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum RegionKey {
    /// The slot's own region, which runs its entries and every node before its first state owner, and a state owner
    /// all of whose inputs enter the slot from outside it.
    Main,

    /// The sub-region of a state owner, which runs the state owner and the stateless nodes after it in the slot.
    State(NodeId),
}

impl fmt::Display for RegionKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Main => f.write_str("main"),
            Self::State(node) => write!(f, "state `{}`", node),
        }
    }
}

/// The clock that stamps an upstream point: the points of one clock reset together.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum ClockId {
    /// The clock of a fixed component, which stamps every lane's point of its outputs.
    Source(NodeId),

    /// The output clock of a state owner, which stamps its time-driven output at its derived point.
    Derived(NodeId),
}

/// One point whose latest admission time makes up a state owner's watermark.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct UpstreamPoint {
    /// The point: an admission point of a fixed component, or the derived point of a state owner upstream.
    pub id: AdmissionPointId,

    /// The clock that stamps it.
    pub clock: ClockId,

    /// Whether the point is a state owner's derived point, whose batches hold buckets closed before their stamp.
    pub derived: bool,
}

/// What one region of a slot runs in a placed candidate.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RegionPlacement {
    /// The region.
    pub key: RegionKey,

    /// The candidate's nodes in the region, in plan order.
    pub nodes: Vec<NodeId>,

    /// The region's state owner, if it runs one: at most one.
    pub state_owner: Option<NodeId>,

    /// The region's entries from outside the slot, as in [`SlotPlacement::entries`]: the main region's are the slot's.
    pub entries: Vec<PlacedEntry>,

    /// The regions of the same slot the region sends to, each with how it sends to their lanes: always to a later
    /// region, so that the regions of a slot form a DAG rooted at the main region.
    pub internal: Vec<(RegionKey, ExitRouting)>,

    /// The points upstream of the region's state owner, in ID order: empty for a region without one.
    pub upstream: Vec<UpstreamPoint>,

    /// The exits the region writes, of the slot or to its other regions, that forward the progress of points upstream
    /// of a state owner downstream of them, in exit order, each with those points in ID order: a point with lanes by
    /// every lane's ID.
    pub progress: Vec<(StepExit, Vec<AdmissionPointId>)>,

    /// The derived points that stamp the region's state owner's time-driven output, one per lane of the region, in
    /// lane order: empty for a region without one.
    pub output_points: Vec<AdmissionPointId>,
}

/// What one slot runs in a placed candidate.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SlotPlacement {
    /// The slot.
    pub slot: SlotId,

    /// The candidate's nodes in the slot, in plan order.
    pub nodes: Vec<NodeId>,

    /// The slot's entries: first those from fixed components, then those from other slots, each in plan edge order;
    /// an edge from a point with lanes has one entry per lane, in lane order.
    pub entries: Vec<PlacedEntry>,

    /// The exits the candidate uses, in exit order. The slot's other exits stay idle.
    pub exits_used: Vec<ExitIndex>,

    /// The slot's regions: the main region first, then a sub-region per state owner that needs one, in topological
    /// order.
    pub regions: Vec<RegionPlacement>,
}

impl SlotPlacement {
    /// Returns the entry of the logical edge entering `input`, if one enters the slot there: the first lane's, for an
    /// edge from a point with lanes.
    pub fn entry_at(&self, input: &PortRef) -> Option<&PlacedEntry> {
        self.entries.iter().find(|entry| &entry.input == input)
    }
}

/// Where every node of a candidate plan runs in a frame.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Placement {
    sites: BTreeMap<NodeId, Site>,
    slots: Vec<SlotPlacement>,
    idle_fixed: Vec<NodeId>,
}

impl Placement {
    /// Returns where a node of the candidate runs, if the candidate has the node.
    pub fn site(&self, node: &NodeId) -> Option<Site> {
        self.sites.get(node).copied()
    }

    /// Returns what every slot of the frame runs, in slot order.
    pub fn slots(&self) -> &[SlotPlacement] {
        &self.slots
    }

    /// Returns what one slot runs.
    pub fn slot(&self, slot: SlotId) -> Option<&SlotPlacement> {
        self.slots.get(slot.get())
    }

    /// Returns the fixed components of the frame that the candidate leaves out, which stay idle.
    pub fn idle_fixed(&self) -> &[NodeId] {
        &self.idle_fixed
    }

    /// Returns `true` if the candidate leaves the exit idle.
    pub fn is_idle(&self, slot: SlotId, exit: ExitIndex) -> bool {
        self.slot(slot).is_none_or(|placed| !placed.exits_used.contains(&exit))
    }

    /// Returns the slot and region a node of the candidate runs in, if it runs in a slot.
    pub fn region_of(&self, node: &NodeId) -> Option<(SlotId, RegionKey)> {
        self.slots.iter().find_map(|placed| {
            placed
                .regions
                .iter()
                .find(|region| region.nodes.contains(node))
                .map(|region| (placed.slot, region.key.clone()))
        })
    }

    /// Returns the admission points upstream of a state owner of the candidate: the real points of its upstream, and,
    /// through each derived point, those upstream of the state owner whose point it is. Empty for a node that is no
    /// state owner of the candidate.
    pub fn admission_upstream(&self, node: &NodeId) -> BTreeSet<AdmissionPointId> {
        let mut points = BTreeSet::new();
        let mut visited = BTreeSet::new();
        let mut pending = vec![node.clone()];
        while let Some(owner) = pending.pop() {
            if !visited.insert(owner.clone()) {
                continue;
            }
            let region = self
                .slots
                .iter()
                .flat_map(|placed| &placed.regions)
                .find(|region| region.nodes.contains(&owner));
            for point in region.map(|region| region.upstream.as_slice()).unwrap_or_default() {
                match &point.clock {
                    ClockId::Derived(upstream) if point.derived => pending.push(upstream.clone()),
                    _ => {
                        points.insert(point.id);
                    }
                }
            }
        }
        points
    }
}

/// The **derived point** of every state owner: the IDs that stamp its time-driven output, one per lane of its region,
/// which a state owner downstream of it has upstream.
///
/// A boot state owner keeps the points the frame numbered for its boot slot ([`Frame::boot_output_points`]); any other
/// state owner gets one point, numbered after them ([`Frame::derived_point_base`]) and after every point already
/// assigned. The installer keeps one instance for its life, assigns the state owners of each candidate before compiling
/// it, and removes a node's points once no live generation has it, so that a state owner keeps its points while any
/// generation runs it.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DerivedPoints {
    points: BTreeMap<NodeId, Vec<AdmissionPointId>>,
}

impl DerivedPoints {
    /// Creates an empty set of derived points.
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the derived points of every state owner of a plan, assigned afresh: what a plan compiled on its own,
    /// such as the boot plan, has.
    ///
    /// A plan with structural problems has no state owners to assign; compiling it reports them.
    pub fn for_plan(frame: &Frame, plan: &Plan, registry: &OperatorRegistry) -> Self {
        let mut points = Self::new();
        points.assign(frame, &state_owners(plan, registry));
        points
    }

    /// Assigns points to every state owner of `owners` that has none, in the order given, keeping those already
    /// assigned.
    pub fn assign<'a>(&mut self, frame: &Frame, owners: impl IntoIterator<Item = &'a NodeId>) {
        for owner in owners {
            if self.points.contains_key(owner) {
                continue;
            }
            let points = match frame.boot_output_points(owner) {
                Some(points) => points.to_vec(),
                None => vec![AdmissionPointId::new(self.next(frame))],
            };
            self.points.insert(owner.clone(), points);
        }
    }

    /// Returns the points of a state owner, one per lane of its region, if it has any.
    pub fn get(&self, node: &NodeId) -> Option<&[AdmissionPointId]> {
        self.points.get(node).map(Vec::as_slice)
    }

    /// Returns every state owner with its points, in node order.
    pub fn iter(&self) -> impl Iterator<Item = (&NodeId, &[AdmissionPointId])> {
        self.points.iter().map(|(node, points)| (node, points.as_slice()))
    }

    /// Keeps only the points of the state owners `keep` accepts.
    pub fn retain(&mut self, mut keep: impl FnMut(&NodeId) -> bool) {
        self.points.retain(|node, _| keep(node));
    }

    /// Returns the next free ID: after the frame's points and every point assigned.
    fn next(&self, frame: &Frame) -> u32 {
        self.points
            .values()
            .flatten()
            .map(|point| point.get().saturating_add(1))
            .fold(frame.derived_point_base(), u32::max)
    }
}

/// Returns the state owners of a plan, in plan order: every node run in a region whose operator keeps state. Empty for
/// a plan with structural problems.
pub fn state_owners(plan: &Plan, registry: &OperatorRegistry) -> Vec<NodeId> {
    let Ok(graph) = resolve(plan, registry) else {
        return Vec::new();
    };
    let roles = super::roles(&graph);
    (0..graph.nodes.len())
        .filter(|&node| !matches!(roles[node], Role::Fixed | Role::Terminal))
        .filter(|&node| {
            graph
                .op(node)
                .is_some_and(|op| op.contract().state != StateContract::None)
        })
        .map(|node| graph.id(node).clone())
        .collect()
}

/// Places a validated candidate plan into a frame, with the derived points of its state owners.
///
/// # Errors
///
/// Returns [`PlacementError::Invalid`] if the report has errors or the plan has structural problems, and
/// [`PlacementError::Unplaceable`] with every problem found if the candidate needs structure the frame does not have.
pub fn place(
    plan: &Plan, report: &ValidationReport, registry: &OperatorRegistry, frame: &Frame, derived: &DerivedPoints,
) -> Result<Placement, PlacementError> {
    if !report.is_valid() {
        return Err(PlacementError::Invalid {
            problems: report.errors().map(ToString::to_string).collect(),
        });
    }
    place_unvalidated(plan, registry, frame, derived)
}

/// Places a candidate plan into a frame whatever its validation report says, as long as the plan is structurally
/// sound: every operator known and configured, every edge resolved, and no cycle.
///
/// This is for the default program, which runs even when validation reports errors in it, and is then reported
/// degraded: it is what native configuration says, and nothing else could run in its place. Every other candidate is
/// placed with [`place`].
///
/// # Errors
///
/// Returns [`PlacementError::Invalid`] if the plan has structural problems, and [`PlacementError::Unplaceable`] with
/// every problem found if the candidate needs structure the frame does not have.
pub fn place_unvalidated(
    plan: &Plan, registry: &OperatorRegistry, frame: &Frame, derived: &DerivedPoints,
) -> Result<Placement, PlacementError> {
    let graph = resolve(plan, registry).map_err(|problems| PlacementError::Invalid { problems })?;
    let roles = super::roles(&graph);
    let mut problems = Vec::new();
    // Structure at a node of a program an artifact composes is the artifact's to ask for, whether its author changed
    // the node or kept it; anything else follows native configuration. Outputs and host components are native: an
    // artifact cannot declare them.
    let program = crate::export::program_nodes(plan)
        .into_iter()
        .flat_map(|(_, nodes)| nodes)
        .collect::<std::collections::BTreeSet<_>>();
    let asked_by = |node: usize| {
        if program.contains(graph.id(node)) {
            AskedBy::Artifact
        } else {
            AskedBy::Native
        }
    };
    let problem = |node: Option<&NodeId>, asked: AskedBy, message: String| {
        PlacementProblem::new(node, Some(asked.capability()), message)
    };

    // Outputs: a bound output places by its delivery host, whose encode host the frame must have, and runs as a lease
    // there; an output without a binding is a fixed destination, declared as at boot.
    let native = AskedBy::Native;
    for output in &plan.outputs {
        match (&output.binding, frame.output(&output.id)) {
            (Some(binding), _) => {
                if frame.encode_host(&binding.host).is_none() {
                    problems.push(problem(
                        None,
                        native,
                        format!(
                            "output `{}` delivers through delivery host `{}`, which the topology built at boot has \
                             no encode host for: {}",
                            output.id,
                            binding.host,
                            native.needs("adding a delivery host")
                        ),
                    ));
                }
            }
            (None, Some(boot)) if boot.binding.is_none() => {
                if boot != output {
                    problems.push(problem(
                        None,
                        native,
                        format!(
                            "output `{}` is a fixed destination declared differently than at boot (its accepted \
                             type, fingerprint, recipients, or delivery policy changed): {}",
                            output.id,
                            native.needs("changing a fixed destination")
                        ),
                    ));
                }
            }
            (None, _) => problems.push(problem(
                None,
                native,
                format!(
                    "output `{}` has no binding, and is not a fixed destination of the topology built at boot: {}",
                    output.id,
                    native.needs("adding a fixed destination")
                ),
            )),
        }
    }

    // Fixed components: the same nodes, configured as at boot.
    let mut present = BTreeSet::new();
    for (node, role) in roles.iter().enumerate() {
        let id = graph.id(node);
        let written = graph.nodes[node].node;
        let op = graph.op(node).expect("a resolved plan configures every node");
        match role {
            Role::Fixed => {
                present.insert(id.clone());
                // Only native configuration places host components, so the structure they need is native.
                match frame.fixed_component(id) {
                    None => problems.push(problem(
                        Some(id),
                        native,
                        format!(
                            "the topology built at boot has no host component for this `{}` node: {}",
                            op.name(),
                            native.needs("adding a host component")
                        ),
                    )),
                    Some(fixed) if fixed.operator != written.op || fixed.params != written.params => {
                        // A host component keeps the configuration it was built with. (Aggregation runs as a kernel in
                        // a region of its own, so its changes are state transitions the installer classifies, not host
                        // reconfigurations.)
                        problems.push(problem(
                            Some(id),
                            native,
                            format!(
                                "its operator or parameters differ from the boot plan's: {}",
                                native.needs(
                                    "reconfiguring a host component, which keeps the configuration it was built with,"
                                )
                            ),
                        ));
                    }
                    Some(_) => {}
                }
            }
            Role::Synchronous | Role::Isolated => {
                // A state owner runs in a region only behind an isolation fence, which needs keyed state.
                let keyed = matches!(op.contract().state, StateContract::Keyed(_));
                let stateless = op.contract().state == StateContract::None;
                if (*role == Role::Isolated && !keyed) || (*role == Role::Synchronous && !stateless) {
                    problems.push(PlacementProblem::new(
                        Some(id),
                        Some(MissingCapability::State),
                        format!(
                            "this `{}` node keeps state without an isolation fence, or declares an isolation fence \
                             without keyed state: a region runs a state owner only behind an isolation fence, in a \
                             region of its own",
                            op.name(),
                        ),
                    ));
                } else if op.contract().is_dynamic() {
                    problems.push(PlacementProblem::new(
                        Some(id),
                        Some(MissingCapability::Signals),
                        format!(
                            "this `{}` node reads host signals: running it requires {}",
                            op.name(),
                            MissingCapability::Signals
                        ),
                    ));
                }
            }
            Role::Terminal => {}
        }
    }

    // Fixed components the candidate leaves out stay idle, if nothing would then send them records or leave theirs
    // with nowhere to go.
    let mut idle_fixed = Vec::new();
    for fixed in frame.fixed() {
        if present.contains(&fixed.node) {
            continue;
        }
        let feeds_slot = fixed.admission_points.iter().any(|&point| {
            frame
                .admission_point(point)
                .is_some_and(|point| matches!(point.route, Some(FrameTarget::Slot(_))))
        });
        let fed_by_slot = matches!(fixed.input, Some(FrameSource::Exit { .. }));
        if fixed.input.is_none() || !fed_by_slot || feeds_slot {
            let why = if fixed.input.is_none() {
                "its host component keeps producing records, which would have nowhere to go"
            } else if feeds_slot {
                "its host component keeps running, and its records would reach a region that has nowhere to send \
                 them"
            } else {
                "its host component keeps receiving records straight from another host"
            };
            // A composed candidate leaves a host component out because the program does not hold it; a default
            // program, because native configuration no longer has it.
            let asked = if plan
                .provenance
                .as_ref()
                .is_some_and(|provenance| provenance.is_composed())
            {
                AskedBy::Artifact
            } else {
                AskedBy::Native
            };
            problems.push(problem(
                Some(&fixed.node),
                asked,
                format!(
                    "the candidate leaves out this host-owned node, but {}: {}",
                    why,
                    asked.needs("removing a host component")
                ),
            ));
        } else {
            idle_fixed.push(fixed.node.clone());
        }
    }

    // Slots of synchronous nodes, from their inputs, in topological order.
    let port_ref = |node: usize, port: usize, input: bool| {
        let ports = graph.op(node).expect("configured").ports();
        let name = if input {
            ports.inputs[port].name.clone()
        } else {
            ports.outputs[port].name.clone()
        };
        PortRef::new(graph.id(node).clone(), name)
    };
    let route_of = |node: usize, port: usize| {
        let from = port_ref(node, port, false);
        frame.admission_point_at(&from.node, &from.port)
    };
    let mut slot_of: Vec<Option<SlotId>> = vec![None; graph.nodes.len()];
    let mut unplaced = vec![false; graph.nodes.len()];
    // The slot an admission point's edge leads to can pass the point's batches through, unchanged, to a boot state
    // owner in its boot slot, which it has an exit to.
    let passes_through = |from: SlotId, to: SlotId| {
        frame
            .slot(from)
            .is_some_and(|from| from.exit_to(&FrameTarget::Slot(to)).is_some())
    };
    for &node in &graph.order {
        let role = roles[node];
        if !matches!(role, Role::Synchronous | Role::Isolated) {
            continue;
        }
        let id = graph.id(node);
        let op = graph.op(node).expect("configured");
        // An admission point's edge leads to one slot, which the node must run in; a node fed from other slots runs
        // in the one that all of them have an exit to.
        let mut fixed_slots: BTreeMap<SlotId, PortRef> = BTreeMap::new();
        let mut fed_from = BTreeSet::new();
        for port in 0..op.ports().inputs.len() {
            let Some((from, from_port)) = graph.source(node, port) else {
                continue;
            };
            match roles[from] {
                Role::Fixed => match route_of(from, from_port).map(|point| &point.route) {
                    Some(Some(FrameTarget::Slot(slot))) => {
                        fixed_slots
                            .entry(*slot)
                            .or_insert_with(|| port_ref(from, from_port, false));
                    }
                    Some(route) => {
                        let leads = route
                            .as_ref()
                            .map_or_else(|| "nothing".to_string(), |target| frame.describe_target(target));
                        problems.push(problem(
                            Some(id),
                            asked_by(node),
                            format!(
                                "it is fed by `{}`, which the topology built at boot connects straight to {}, with \
                                 no region to run this node in: {}",
                                port_ref(from, from_port, false),
                                leads,
                                asked_by(node).needs("adding a region after a host")
                            ),
                        ));
                        unplaced[node] = true;
                    }
                    // A fixed node the frame does not have is reported above.
                    None => unplaced[node] = true,
                },
                Role::Synchronous | Role::Isolated => match slot_of[from] {
                    Some(slot) => {
                        fed_from.insert(slot);
                    }
                    None => unplaced[node] = true,
                },
                Role::Terminal => {}
            }
        }
        if unplaced[node] {
            continue;
        }
        let region = |slot: SlotId| format!("`{}`", frame.describe_target(&FrameTarget::Slot(slot)));
        if fixed_slots.len() > 1 {
            let joined = fixed_slots
                .iter()
                .map(|(slot, from)| format!("`{}` (to {})", from, region(*slot)))
                .collect::<Vec<_>>();
            problems.push(problem(
                Some(id),
                asked_by(node),
                format!(
                    "it joins {}, which the topology built at boot connects to different regions: {}",
                    joined.join(" and "),
                    asked_by(node).needs("a new physical edge")
                ),
            ));
            unplaced[node] = true;
            continue;
        }
        let receives_all = |slot: SlotId| {
            fed_from.iter().all(|&other| {
                other == slot
                    || frame
                        .slot(other)
                        .is_some_and(|other| other.exit_to(&FrameTarget::Slot(slot)).is_some())
            })
        };
        // A boot state owner fed straight by an admission point keeps its boot slot when the slot the point's route
        // leads to passes the point's batches through to it.
        let reached_from = |fixed: SlotId, slot: SlotId| {
            fixed == slot
                || (role == Role::Isolated && frame.boot_output_points(id).is_some() && passes_through(fixed, slot))
        };
        // A node keeps the slot it has at boot when its inputs reach it, so that a candidate runs where the boot plan
        // does: in a frame that isolates state, a boot state owner in its own slot and its consumers after it.
        let boot_slot = match frame.site(id) {
            Some(Site::Slot(slot)) => Some(slot),
            _ => None,
        }
        .filter(|&slot| fixed_slots.keys().all(|&fixed| reached_from(fixed, slot)) && receives_all(slot));
        let chosen = match (boot_slot, fixed_slots.keys().next()) {
            (Some(slot), _) => Some(slot),
            (None, Some(&slot)) => receives_all(slot).then_some(slot),
            // The one slot its inputs come from that receives from every other.
            (None, None) => {
                let sinks = fed_from
                    .iter()
                    .copied()
                    .filter(|&slot| receives_all(slot))
                    .collect::<Vec<_>>();
                match sinks.as_slice() {
                    [slot] => Some(*slot),
                    _ => None,
                }
            }
        };
        match chosen {
            Some(slot) => slot_of[node] = Some(slot),
            None => {
                let regions = fixed_slots
                    .keys()
                    .chain(&fed_from)
                    .copied()
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .map(region)
                    .collect::<Vec<_>>();
                problems.push(problem(
                    Some(id),
                    asked_by(node),
                    format!(
                        "it joins records from the regions {}, and no one of them receives from all the others: {}",
                        regions.join(", "),
                        asked_by(node).needs("connecting regions in a new way")
                    ),
                ));
                unplaced[node] = true;
            }
        }
    }

    // Every edge must use the frame's physical structure.
    for edge in &plan.edges {
        let (Some((from, from_port)), Some((to, _))) = (
            graph.output_port(&edge.from.node, &edge.from.port),
            graph.input_port(&edge.to.node, &edge.to.port),
        ) else {
            continue;
        };
        let describe_edge = || format!("the edge from `{}` to `{}`", edge.from, edge.to);
        // An edge is an artifact's to ask for when it leaves or enters a node the artifact wrote.
        let asked = if asked_by(from) == AskedBy::Artifact || asked_by(to) == AskedBy::Artifact {
            AskedBy::Artifact
        } else {
            AskedBy::Native
        };
        match (roles[from], roles[to]) {
            (Role::Fixed, target_role) => {
                let Some(point) = route_of(from, from_port) else {
                    continue;
                };
                let expected = match target_role {
                    Role::Fixed => Some(FrameTarget::Fixed(graph.id(to).clone())),
                    Role::Isolated => match (slot_of[to], &point.route) {
                        // The route's slot passes the point's batches through to the state owner.
                        (Some(slot), Some(FrameTarget::Slot(route))) if passes_through(*route, slot) => continue,
                        (Some(slot), _) => Some(FrameTarget::Slot(slot)),
                        // An unplaced node is reported where its placement failed.
                        (None, _) => continue,
                    },
                    Role::Synchronous => match slot_of[to] {
                        Some(slot) => Some(FrameTarget::Slot(slot)),
                        // An unplaced node is reported where its placement failed.
                        None => continue,
                    },
                    Role::Terminal => graph
                        .op(to)
                        .and_then(emit_output)
                        .map(|output| FrameTarget::of_emit(plan, output)),
                };
                if point.route != expected {
                    let built = point
                        .route
                        .as_ref()
                        .map_or_else(|| "nothing".to_string(), |target| frame.describe_target(target));
                    let wanted = expected
                        .as_ref()
                        .map_or_else(|| "nothing".to_string(), |target| frame.describe_target(target));
                    // Only a region writes the lease an encode host reads, so a host cannot feed one directly.
                    let what = if matches!(expected, Some(FrameTarget::EncodeHost(_))) {
                        "a region between the host and the encode host, which reads the lease only a region writes,"
                    } else {
                        "a new physical edge"
                    };
                    problems.push(problem(
                        Some(graph.id(to)),
                        asked,
                        format!(
                            "{} needs `{}` connected to {}, but the topology built at boot connects it to {}: {}",
                            describe_edge(),
                            edge.from,
                            wanted,
                            built,
                            asked.needs(what)
                        ),
                    ));
                }
            }
            (Role::Synchronous | Role::Isolated, target_role) => {
                let Some(slot) = slot_of[from] else { continue };
                let target = match target_role {
                    Role::Fixed => FrameTarget::Fixed(graph.id(to).clone()),
                    Role::Synchronous | Role::Isolated => match slot_of[to] {
                        Some(other) if other == slot => match graph.op(to).and_then(emit_output) {
                            Some(output) => FrameTarget::of_emit(plan, output),
                            None => continue,
                        },
                        Some(other) => FrameTarget::Slot(other),
                        None => continue,
                    },
                    Role::Terminal => continue,
                };
                let has_exit = frame.slot(slot).is_some_and(|slot| slot.exit_to(&target).is_some());
                if !has_exit {
                    // A declared output whose encode host or fixed destination the frame lacks is reported with the
                    // outputs, once.
                    let reported = match &target {
                        FrameTarget::Destination(output) => {
                            plan.output(output).is_some()
                                && frame.output(output).is_none_or(|boot| boot.binding.is_some())
                        }
                        FrameTarget::EncodeHost(host) => frame.encode_host(host).is_none(),
                        FrameTarget::Fixed(_) | FrameTarget::Slot(_) => false,
                    };
                    if reported {
                        continue;
                    }
                    let what = asked.needs("a new exit from a region");
                    problems.push(problem(
                        Some(graph.id(to)),
                        asked,
                        format!(
                            "{} needs region `{}` to send records to {}, which it has no exit to: {}",
                            describe_edge(),
                            frame.describe_target(&FrameTarget::Slot(slot)),
                            frame.describe_target(&target),
                            what
                        ),
                    ));
                }
            }
            (Role::Terminal, _) => {}
        }
    }

    if !problems.is_empty() {
        return Err(PlacementError::Unplaceable { problems });
    }

    // What each slot runs: its nodes, its entries, and the exits it uses.
    let mut slots = frame
        .slots()
        .iter()
        .map(|slot| SlotPlacement {
            slot: slot.id,
            nodes: Vec::new(),
            entries: Vec::new(),
            exits_used: Vec::new(),
            regions: Vec::new(),
        })
        .collect::<Vec<_>>();
    let mut sites = BTreeMap::new();
    for (node, role) in roles.iter().enumerate() {
        let site = match role {
            Role::Fixed => Site::Fixed,
            Role::Terminal => Site::Direct,
            Role::Synchronous | Role::Isolated => {
                let slot = slot_of[node].expect("every node of a placeable plan that runs in a region is placed");
                slots[slot.get()].nodes.push(graph.id(node).clone());
                Site::Slot(slot)
            }
        };
        sites.insert(graph.id(node).clone(), site);
    }

    let mut region_entries = Vec::new();
    let mut exits_used: Vec<BTreeSet<ExitIndex>> = vec![BTreeSet::new(); slots.len()];
    for edge in &plan.edges {
        let (Some((from, from_port)), Some((to, _))) = (
            graph.output_port(&edge.from.node, &edge.from.port),
            graph.input_port(&edge.to.node, &edge.to.port),
        ) else {
            continue;
        };
        match (roles[from], slot_of[from], slot_of[to]) {
            (Role::Fixed, _, Some(target)) => {
                let point = route_of(from, from_port).expect("checked above");
                // One entry per lane of the point, each lane's batches keyed by the point that stamped them.
                let entries = point.lane_ids().map(|id| PlacedEntry {
                    key: EntryKey::Admission(id),
                    source: FrameSource::Admission(id),
                    from: edge.from.clone(),
                    input: edge.to.clone(),
                });
                match &point.route {
                    Some(FrameTarget::Slot(route)) if *route != target => {
                        // The route's slot passes the point's batches through to the boot state owner's slot.
                        let exit = frame
                            .slot(*route)
                            .and_then(|slot| slot.exit_to(&FrameTarget::Slot(target)))
                            .expect("checked above")
                            .index;
                        slots[route.get()].entries.extend(entries);
                        exits_used[route.get()].insert(exit);
                        region_entries.push((
                            target,
                            PlacedEntry {
                                key: EntryKey::Region(EntryId::new(0)),
                                source: FrameSource::Exit { slot: *route, exit },
                                from: edge.from.clone(),
                                input: edge.to.clone(),
                            },
                        ));
                    }
                    _ => slots[target.get()].entries.extend(entries),
                }
            }
            (Role::Synchronous | Role::Isolated, Some(slot), target) => {
                let frame_slot = frame.slot(slot).expect("placed slots exist");
                let exit_target = match (roles[to], target) {
                    (Role::Fixed, _) => Some(FrameTarget::Fixed(graph.id(to).clone())),
                    (_, Some(other)) if other != slot => Some(FrameTarget::Slot(other)),
                    _ => graph
                        .op(to)
                        .and_then(emit_output)
                        .map(|output: &OutputId| FrameTarget::of_emit(plan, output)),
                };
                let Some(exit_target) = exit_target else { continue };
                let exit = frame_slot.exit_to(&exit_target).expect("checked above").index;
                exits_used[slot.get()].insert(exit);
                if let FrameTarget::Slot(other) = exit_target {
                    region_entries.push((
                        other,
                        PlacedEntry {
                            key: EntryKey::Region(EntryId::new(0)),
                            source: FrameSource::Exit { slot, exit },
                            from: edge.from.clone(),
                            input: edge.to.clone(),
                        },
                    ));
                }
            }
            _ => {}
        }
    }
    for (slot, mut entry) in region_entries {
        let placed = &mut slots[slot.get()];
        let count = placed
            .entries
            .iter()
            .filter(|entry| matches!(entry.key, EntryKey::Region(_)))
            .count();
        let Ok(id) = u16::try_from(count) else {
            return Err(PlacementError::Unplaceable {
                problems: vec![PlacementProblem::new(
                    None,
                    None,
                    format!(
                        "region `{}` would have more than {} entries from other regions",
                        frame.describe_target(&FrameTarget::Slot(slot)),
                        u16::MAX
                    ),
                )],
            });
        };
        entry.key = EntryKey::Region(EntryId::new(id));
        placed.entries.push(entry);
    }
    for (placed, used) in slots.iter_mut().zip(exits_used) {
        placed.exits_used = used.into_iter().collect();
    }
    // Each slot's regions, and the points upstream of each state owner.
    let regions = Regions::partition(&graph, &roles, &slot_of, frame);
    let mut problems = regions.lane_problems(&asked_by);
    let (upstream, progress) = regions.upstream(derived, &route_of, &asked_by, &mut problems);
    let outputs = regions.output_points(derived, &mut problems);
    if !problems.is_empty() {
        return Err(PlacementError::Unplaceable { problems });
    }
    for placed in &mut slots {
        placed.regions = regions.placements(placed, &upstream, &progress, &outputs);
    }

    Ok(Placement {
        sites,
        slots,
        idle_fixed,
    })
}

/// One physical hop a batch takes toward a state owner, by which [`Regions::upstream`] counts paths and finds the
/// exits that forward progress.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum Hop {
    /// An admission point's edge into the main region of a slot.
    Entry(AdmissionPointId, SlotId),

    /// An exit of a slot, written by one of its regions, by position.
    Exit(SlotId, usize, ExitIndex),

    /// An internal exit from one region of a slot to another, by position.
    Internal(SlotId, usize, usize),
}

/// What feeds a state owner: an admission point, by its fixed node and output port, or another state owner, whose
/// derived point it is.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum Feed {
    Point(usize, usize),
    Owner(usize),
}

/// The progress each region's exits forward, by slot and region position.
type Progress = BTreeMap<(SlotId, usize), BTreeMap<StepExit, BTreeSet<AdmissionPointId>>>;

/// The regions of every slot of a placed candidate (see [regions](self#regions)).
struct Regions<'a, 'p> {
    graph: &'a Graph<'p>,
    roles: &'a [Role],
    slot_of: &'a [Option<SlotId>],
    frame: &'a Frame,
    /// The position of each node's region among its slot's regions, by graph position: zero, the main region, for a
    /// node outside every slot.
    region_of: Vec<usize>,
    /// The regions of each slot, in order: the main region, then a sub-region per state owner that opens one.
    keys: Vec<Vec<RegionKey>>,
}

impl<'a, 'p> Regions<'a, 'p> {
    /// Partitions each slot's nodes into regions, in topological order: a state owner runs in the highest region among
    /// those of its inputs in the slot, the main region if it has none, when that region runs no state owner yet and
    /// has one lane, or is the main region of the state owner's boot slot, and opens a region of its own otherwise; any
    /// other node runs in the highest region among those of its inputs in the slot, the main region if it has none.
    fn partition(graph: &'a Graph<'p>, roles: &'a [Role], slot_of: &'a [Option<SlotId>], frame: &'a Frame) -> Self {
        let mut region_of = vec![0; graph.nodes.len()];
        let mut keys = vec![vec![RegionKey::Main]; frame.slots().len()];
        let mut main_owner = vec![false; frame.slots().len()];
        for &node in &graph.order {
            let Some(slot) = slot_of[node] else { continue };
            let ports = graph.op(node).map_or(0, |op| op.ports().inputs.len());
            let inputs = (0..ports)
                .filter_map(|port| graph.source(node, port))
                .filter(|&(from, _)| slot_of[from] == Some(slot))
                .map(|(from, _)| region_of[from])
                .collect::<Vec<_>>();
            let highest = inputs.into_iter().max().unwrap_or(0);
            if roles[node] != Role::Isolated {
                region_of[node] = highest;
                continue;
            }
            let id = graph.id(node);
            let lanes = frame.slot(slot).map_or(1, |slot| slot.lanes);
            let boot = frame.boot_output_points(id).is_some() && frame.site(id) == Some(Site::Slot(slot));
            // Every sub-region runs its state owner, so only the main region can take one in.
            if highest == 0 && !main_owner[slot.get()] && (lanes == 1 || boot) {
                main_owner[slot.get()] = true;
            } else {
                keys[slot.get()].push(RegionKey::State(id.clone()));
                region_of[node] = keys[slot.get()].len() - 1;
            }
        }
        Self {
            graph,
            roles,
            slot_of,
            frame,
            region_of,
            keys,
        }
    }

    /// Returns the number of lanes a region runs on: the slot's for its main region, one for a sub-region.
    fn lanes(&self, slot: SlotId, region: usize) -> u16 {
        if region == 0 {
            self.frame.slot(slot).map_or(1, |slot| slot.lanes)
        } else {
            1
        }
    }

    /// Refuses a state owner whose state cannot be partitioned by context key in a main region on several lanes, each
    /// of which holds the share of the state that the slot's keyed exits route to it by context key.
    fn lane_problems(&self, asked_by: &impl Fn(usize) -> AskedBy) -> Vec<PlacementProblem> {
        let mut problems = Vec::new();
        for node in 0..self.graph.nodes.len() {
            let Some(slot) = self.slot_of[node].filter(|_| self.roles[node] == Role::Isolated) else {
                continue;
            };
            let lanes = self.lanes(slot, self.region_of[node]);
            let op = self.graph.op(node).expect("configured");
            if lanes > 1 && op.partition_key() != Some(PartitionKey::Context) {
                let asked = asked_by(node);
                problems.push(PlacementProblem::new(
                    Some(self.graph.id(node)),
                    Some(asked.capability()),
                    format!(
                        "it is a state owner whose state cannot be partitioned by context key, and the topology built \
                         at boot runs region `{}` on {} lanes, each holding the share of the state that the slot's \
                         keyed exits route to it by context key: {}",
                        self.frame.describe_target(&FrameTarget::Slot(slot)),
                        lanes,
                        asked.needs("running the region on one lane")
                    ),
                ));
            }
        }
        problems
    }

    /// Returns the hops an edge from `(from, port)` to the node `to` takes: from an admission point into the main region
    /// of the slot its route leads to, and on to another slot when that slot passes the batches through; between slots
    /// by an exit of the sending region, into the receiving slot's main region; and between regions of a slot by an
    /// internal exit.
    fn hops<'r>(
        &self, from: usize, port: usize, to: usize, route_of: &impl Fn(usize, usize) -> Option<&'r AdmissionPointInfo>,
    ) -> Vec<Hop> {
        let Some(slot) = self.slot_of[to] else {
            return Vec::new();
        };
        let region = self.region_of[to];
        let exit = |from_slot: SlotId| {
            self.frame
                .slot(from_slot)
                .and_then(|placed| placed.exit_to(&FrameTarget::Slot(slot)))
                .map(|exit| exit.index)
        };
        let mut hops = Vec::new();
        match self.slot_of[from] {
            Some(from_slot) if from_slot == slot => {
                if self.region_of[from] != region {
                    hops.push(Hop::Internal(slot, self.region_of[from], region));
                }
                return hops;
            }
            Some(from_slot) => {
                hops.extend(exit(from_slot).map(|index| Hop::Exit(from_slot, self.region_of[from], index)))
            }
            None => {
                let Some(point) = route_of(from, port) else {
                    return hops;
                };
                let Some(FrameTarget::Slot(route)) = point.route else {
                    return hops;
                };
                hops.push(Hop::Entry(point.id, route));
                if route != slot {
                    hops.extend(exit(route).map(|index| Hop::Exit(route, 0, index)));
                }
            }
        }
        if region != 0 {
            hops.push(Hop::Internal(slot, 0, region));
        }
        hops
    }

    /// Returns the points upstream of every state owner, by graph position, and the progress each region's exits
    /// forward, refusing a point that reaches a state owner over several paths, and, with lanes, a state owner fed by
    /// another on several lanes, or over several clocks one of which stamps several of its points.
    ///
    /// A state owner's upstream points are found by walking its inputs back through stateless nodes, to the admission
    /// points of fixed components, which readmit what they receive, and to other state owners' derived points. Every
    /// exit on a path from such a point to the state owner forwards the point's progress.
    fn upstream<'r>(
        &self, derived: &DerivedPoints, route_of: &impl Fn(usize, usize) -> Option<&'r AdmissionPointInfo>,
        asked_by: &impl Fn(usize) -> AskedBy, problems: &mut Vec<PlacementProblem>,
    ) -> (BTreeMap<usize, Vec<UpstreamPoint>>, Progress) {
        let graph = self.graph;
        let mut upstream = BTreeMap::new();
        let mut progress = Progress::new();
        let inputs = |node: usize| {
            let ports = graph.op(node).map_or(0, |op| op.ports().inputs.len());
            (0..ports).filter_map(move |port| graph.source(node, port))
        };
        let outputs = |node: usize| {
            let ports = graph.op(node).map_or(0, |op| op.ports().outputs.len());
            (0..ports).filter_map(move |port| graph.target(node, port).map(|(to, _)| (port, to)))
        };
        for owner in 0..graph.nodes.len() {
            if self.roles[owner] != Role::Isolated || self.slot_of[owner].is_none() {
                continue;
            }
            let id = graph.id(owner);
            // The stateless nodes between the state owner and what feeds it, and what feeds it.
            let mut between = BTreeSet::new();
            let mut feeds = BTreeSet::new();
            let mut pending = vec![owner];
            while let Some(node) = pending.pop() {
                for (from, port) in inputs(node) {
                    match self.roles[from] {
                        Role::Fixed => {
                            feeds.insert(Feed::Point(from, port));
                        }
                        Role::Isolated if self.slot_of[from].is_some() => {
                            feeds.insert(Feed::Owner(from));
                        }
                        Role::Synchronous if self.slot_of[from].is_some() && between.insert(from) => {
                            pending.push(from);
                        }
                        _ => {}
                    }
                }
            }
            let on_path = |node: usize| node == owner || between.contains(&node);

            let mut points = Vec::new();
            for feed in feeds {
                let (start, start_port, ids, clock, is_derived, name) = match feed {
                    Feed::Point(fixed, port) => {
                        let Some(point) = route_of(fixed, port) else { continue };
                        let name = format!("admission point `{}.{}`", point.node, point.port);
                        let clock = ClockId::Source(graph.id(fixed).clone());
                        (fixed, port, point.lane_ids().collect::<Vec<_>>(), clock, false, name)
                    }
                    Feed::Owner(from) => {
                        let from_slot = self.slot_of[from].expect("a state owner that feeds another is placed");
                        let lanes = self.lanes(from_slot, self.region_of[from]);
                        let from_id = graph.id(from);
                        if lanes > 1 {
                            let asked = asked_by(owner);
                            problems.push(PlacementProblem::new(
                                Some(id),
                                Some(asked.capability()),
                                format!(
                                    "it is fed by state owner `{}`, which runs on {} lanes, each stamping its output at \
                                     a derived point of its own, and a state owner fed by another on several lanes is \
                                     not supported: {}",
                                    from_id,
                                    lanes,
                                    asked.needs("running that state owner on one lane")
                                ),
                            ));
                            continue;
                        }
                        let Some(first) = derived.get(from_id).and_then(|ids| ids.first()) else {
                            problems.push(PlacementProblem::new(
                                Some(id),
                                None,
                                format!(
                                    "it is fed by state owner `{}`, which has no derived point: the installer assigns \
                                     one to every state owner of a candidate before placing it",
                                    from_id
                                ),
                            ));
                            continue;
                        };
                        let name = format!("the derived point of state owner `{}`", from_id);
                        (from, 0, vec![*first], ClockId::Derived(from_id.clone()), true, name)
                    }
                };

                // The paths from the feed to the state owner, each as the hops it takes, up to two, and every hop on
                // them.
                let Some((first, _)) = graph.target(start, start_port).filter(|&(to, _)| on_path(to)) else {
                    continue;
                };
                let mut paths: BTreeMap<usize, BTreeSet<Vec<Hop>>> = BTreeMap::new();
                let first_hops = self.hops(start, start_port, first, route_of);
                let mut used = first_hops.iter().copied().collect::<BTreeSet<_>>();
                paths.entry(first).or_default().insert(first_hops);
                for &node in &graph.order {
                    if !between.contains(&node) {
                        continue;
                    }
                    let Some(here) = paths.get(&node).cloned() else {
                        continue;
                    };
                    for (port, to) in outputs(node) {
                        if !on_path(to) {
                            continue;
                        }
                        let hops = self.hops(node, port, to, route_of);
                        used.extend(hops.iter().copied());
                        let there = paths.entry(to).or_default();
                        for path in &here {
                            // Two paths are enough to refuse, so no more are kept.
                            if there.len() < 2 {
                                there.insert(path.iter().chain(&hops).copied().collect());
                            }
                        }
                    }
                }
                if paths.get(&owner).is_some_and(|paths| paths.len() > 1) {
                    problems.push(PlacementProblem::new(
                        Some(id),
                        None,
                        format!(
                            "{} reaches this state owner over more than one path, and a state owner's watermark needs \
                             one path from each point upstream of it, along which its batches keep their order: join \
                             the paths before the state owner's input, in one region",
                            name
                        ),
                    ));
                    continue;
                }
                for hop in used {
                    let (unit, exit) = match hop {
                        Hop::Entry(..) => continue,
                        Hop::Exit(slot, region, index) => ((slot, region), StepExit::Slot(index)),
                        Hop::Internal(slot, from, to) => {
                            ((slot, from), StepExit::Region(self.keys[slot.get()][to].clone()))
                        }
                    };
                    progress
                        .entry(unit)
                        .or_default()
                        .entry(exit)
                        .or_default()
                        .extend(ids.iter().copied());
                }
                points.extend(ids.into_iter().map(|point| UpstreamPoint {
                    id: point,
                    clock: clock.clone(),
                    derived: is_derived,
                }));
            }
            points.sort_by_key(|point| point.id);
            points.dedup();

            // A state owner over several clocks has one point of each: the reset window of a clock of several points,
            // its lanes, would close buckets at the resets of that clock alone.
            let mut clocks: BTreeMap<&ClockId, usize> = BTreeMap::new();
            for point in &points {
                *clocks.entry(&point.clock).or_default() += 1;
            }
            if clocks.len() > 1 {
                if let Some((clock, count)) = clocks.iter().find(|(_, &count)| count > 1) {
                    let asked = asked_by(owner);
                    let clock = match clock {
                        ClockId::Source(node) => format!("`{}`", node),
                        ClockId::Derived(node) => format!("state owner `{}`", node),
                    };
                    problems.push(PlacementProblem::new(
                        Some(id),
                        Some(asked.capability()),
                        format!(
                            "its upstream points come from {} clocks, and the clock of {} stamps {} of them, one per \
                             lane, and a state owner over several clocks one of which stamps several of its points is \
                             not supported: {}",
                            clocks.len(),
                            clock,
                            count,
                            asked.needs("running the source on one lane")
                        ),
                    ));
                }
            }
            upstream.insert(owner, points);
        }
        (upstream, progress)
    }

    /// Returns the derived points of every state owner that stamp its time-driven output, one per lane of its region,
    /// by graph position, refusing a state owner without them.
    fn output_points(
        &self, derived: &DerivedPoints, problems: &mut Vec<PlacementProblem>,
    ) -> BTreeMap<usize, Vec<AdmissionPointId>> {
        let mut outputs = BTreeMap::new();
        for owner in 0..self.graph.nodes.len() {
            let Some(slot) = self.slot_of[owner].filter(|_| self.roles[owner] == Role::Isolated) else {
                continue;
            };
            let lanes = usize::from(self.lanes(slot, self.region_of[owner]));
            let id = self.graph.id(owner);
            match derived.get(id) {
                Some(points) if points.len() >= lanes => {
                    outputs.insert(owner, points[..lanes].to_vec());
                }
                _ => problems.push(PlacementProblem::new(
                    Some(id),
                    None,
                    format!(
                        "it is a state owner without a derived point for each of the {} lanes of its region: the \
                         installer assigns them to every state owner of a candidate before placing it",
                        lanes
                    ),
                )),
            }
        }
        outputs
    }

    /// Returns the regions of one placed slot, from the partition, the points upstream of its state owners, the
    /// progress its regions' exits forward, and its state owners' derived points.
    fn placements(
        &self, placed: &SlotPlacement, upstream: &BTreeMap<usize, Vec<UpstreamPoint>>, progress: &Progress,
        outputs: &BTreeMap<usize, Vec<AdmissionPointId>>,
    ) -> Vec<RegionPlacement> {
        let graph = self.graph;
        let slot = placed.slot;
        let keys = &self.keys[slot.get()];
        let in_slot = |node: usize| self.slot_of[node] == Some(slot);
        // The internal exits: between regions of the slot along its edges, and from the main region to each sub-region
        // an entry of the slot leads into.
        let mut internal = vec![BTreeSet::new(); keys.len()];
        for node in (0..graph.nodes.len()).filter(|&node| in_slot(node)) {
            let ports = graph.op(node).map_or(0, |op| op.ports().inputs.len());
            for (from, _) in (0..ports).filter_map(|port| graph.source(node, port)) {
                if in_slot(from) && self.region_of[from] != self.region_of[node] {
                    internal[self.region_of[from]].insert(self.region_of[node]);
                }
            }
        }
        for entry in &placed.entries {
            if let Some((node, _)) = graph.input_port(&entry.input.node, &entry.input.port) {
                if in_slot(node) && self.region_of[node] != 0 {
                    internal[0].insert(self.region_of[node]);
                }
            }
        }
        keys.iter()
            .enumerate()
            .map(|(region, key)| {
                let members = (0..graph.nodes.len())
                    .filter(|&node| in_slot(node) && self.region_of[node] == region)
                    .collect::<Vec<_>>();
                let owner = members.iter().copied().find(|&node| self.roles[node] == Role::Isolated);
                let progress = progress.get(&(slot, region)).map_or_else(Vec::new, |exits| {
                    exits
                        .iter()
                        .map(|(exit, points)| (exit.clone(), points.iter().copied().collect()))
                        .collect()
                });
                RegionPlacement {
                    key: key.clone(),
                    nodes: members.iter().map(|&node| graph.id(node).clone()).collect(),
                    state_owner: owner.map(|owner| graph.id(owner).clone()),
                    entries: if region == 0 {
                        placed.entries.clone()
                    } else {
                        Vec::new()
                    },
                    internal: internal[region]
                        .iter()
                        .map(|&to| (keys[to].clone(), ExitRouting::One))
                        .collect(),
                    upstream: owner
                        .and_then(|owner| upstream.get(&owner))
                        .cloned()
                        .unwrap_or_default(),
                    progress,
                    output_points: owner.and_then(|owner| outputs.get(&owner)).cloned().unwrap_or_default(),
                }
            })
            .collect()
    }
}
