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
//!   inputs can reach that slot: R1's cost model may have put it in a later slot of a pipeline than its inputs. Any
//!   other node's slot follows from its inputs: the slot an admission point's edge leads to, or the slot of the node
//!   feeding it. When its inputs come from several slots, it runs in the one that every other of them has an exit to.
//! - A state owner behind an isolation fence runs in an isolation slot, and nothing else does: its boot slot, or the
//!   isolation slot that the slot its input comes from has an exit to, so that one artifact can aggregate each branch
//!   with its own width in the one isolation slot. A state owner fed straight by an admission point whose edge leads to
//!   a fused slot runs in the isolation slot that slot has an exit to: the fused slot passes the point's batches
//!   through, unchanged. A node fed by a state owner runs in a fused slot that the state owner's isolation slot has an
//!   exit to. A state owner outside the boot plan's isolation slots needs a frame change. An isolation slot on
//!   several lanes holds a share of its state owner's state per lane, which its keyed exits route by context key
//!   ([`ExitRouting::Keyed`][super::frame::ExitRouting::Keyed]), so a state owner placed in it must have its state
//!   partitioned by that key ([`PartitionKey::Context`]); one whose state cannot be partitioned needs the slot on one
//!   lane, a frame change.
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
//! # Entries
//!
//! A slot's program maps each batch it receives to a logical input of the slot: an **entry**. A batch from a fixed
//! component is mapped by the admission point that stamped it ([`EntryKey::Admission`]). A batch from another slot
//! carries the entry in its header, set by the slot that sent it ([`EntryKey::Region`]), since several logical edges
//! can share one exit. Entry IDs are numbered per slot and per generation, in plan edge order. A slot that passes an
//! admission point's batches through to a state owner has an entry for the point whose input is the state owner's,
//! in the isolation slot, which has the matching entry from the passing slot's exit.
//!
//! A point with lanes is one admission point per lane ([`AdmissionPointInfo::lanes`]), each stamping the batches of
//! its lane, so a logical edge leaving it has one entry per lane's ID, in lane order, each with the same input: the
//! slot's program runs every lane's batches alike, and each lane of the slot receives its own lane's. The entry of
//! the isolation slot a passing slot leads to stays one: it is keyed by the entry its batches carry, which every
//! lane's exit writes alike.
//!
//! [`AdmissionPointInfo::lanes`]: super::frame::AdmissionPointInfo::lanes

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
};

use saluki_core::execution::{AdmissionPointId, EntryId};

use super::{
    frame::{ExitIndex, Frame, FrameSource, FrameTarget, Site, SlotId},
    program::KernelIndex,
    resolve, Role,
};
use crate::{
    ir::{NodeId, OutputId, Plan, PortRef, Provenance},
    operators::{OperatorRegistry, PartitionKey, StateContract},
    validate::{graph::emit_output, ValidationReport},
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

    /// Physical structure that only an artifact asks for, such as a new exit from a region to an isolation slot: the
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
    /// emits at a flush tick, a clock reset, its retirement, or shutdown. Only isolation slots have such entries; the
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
    /// state owner, the state owner's, in the isolation slot it passes the batches to.
    pub input: PortRef,
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
}

/// Places a validated candidate plan into a frame.
///
/// # Errors
///
/// Returns [`PlacementError::Invalid`] if the report has errors or the plan has structural problems, and
/// [`PlacementError::Unplaceable`] with every problem found if the candidate needs structure the frame does not have.
pub fn place(
    plan: &Plan, report: &ValidationReport, registry: &OperatorRegistry, frame: &Frame,
) -> Result<Placement, PlacementError> {
    if !report.is_valid() {
        return Err(PlacementError::Invalid {
            problems: report.errors().map(ToString::to_string).collect(),
        });
    }
    place_unvalidated(plan, registry, frame)
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
pub fn place_unvalidated(plan: &Plan, registry: &OperatorRegistry, frame: &Frame) -> Result<Placement, PlacementError> {
    let graph = resolve(plan, registry).map_err(|problems| PlacementError::Invalid { problems })?;
    let roles = super::roles(&graph);
    let mut problems = Vec::new();
    // Structure at a node an artifact wrote is the artifact's to ask for; anything else follows native configuration.
    // Outputs, host components, and the nodes the default program keeps are native: an artifact cannot declare them.
    let asked_by = |node: usize| match graph.nodes[node].node.provenance {
        Some(Provenance::Artifact(_)) => AskedBy::Artifact,
        _ => AskedBy::Native,
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
                        // its isolation slot, so its changes are state transitions the installer classifies, not host
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
                             without keyed state: a region runs a state owner only behind an isolation fence, in an \
                             isolation slot",
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
            // A composed candidate leaves a host component out because an artifact replaced the scope holding it; a
            // default program, because native configuration no longer has it.
            let asked = if plan
                .provenance
                .as_ref()
                .is_some_and(|provenance| !provenance.artifacts.is_empty())
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
    let isolation = |slot: SlotId| frame.slot(slot).is_some_and(|slot| slot.is_isolation());
    // A fused slot an admission point's edge leads to can pass the point's batches through, unchanged, to a state
    // owner in an isolation slot it has an exit to.
    let passes_through = |from: SlotId, to: SlotId| {
        !isolation(from)
            && isolation(to)
            && frame
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
        // A state owner runs only in an isolation slot, and every other node only in a fused one.
        let fits = |slot: SlotId| isolation(slot) == (role == Role::Isolated);
        // An admission point's route binds the node to the slot it leads to, or, for a state owner, to an isolation
        // slot that slot passes the point's batches through to.
        let reached_from =
            |fixed: SlotId, slot: SlotId| fixed == slot || (role == Role::Isolated && passes_through(fixed, slot));
        // A node keeps the slot it has at boot when its inputs reach it, so that a candidate runs pipelined as the boot
        // plan does.
        let boot_slot = match frame.site(id) {
            Some(Site::Slot(slot)) => Some(slot),
            _ => None,
        }
        .filter(|&slot| fits(slot) && fixed_slots.keys().all(|&fixed| reached_from(fixed, slot)) && receives_all(slot));
        let chosen = match (boot_slot, fixed_slots.keys().next()) {
            (Some(slot), _) => Some(slot),
            (None, Some(&slot)) if fits(slot) => receives_all(slot).then_some(slot),
            (None, Some(&slot)) => {
                // A state owner fed straight by a point whose route is a fused slot: the one isolation slot that slot
                // has an exit to.
                let exits = frame.slot(slot).map(|slot| slot.exits.as_slice()).unwrap_or_default();
                let targets = exits
                    .iter()
                    .filter_map(|exit| match exit.target {
                        FrameTarget::Slot(next) if fits(next) && reached_from(slot, next) && receives_all(next) => {
                            Some(next)
                        }
                        _ => None,
                    })
                    .collect::<BTreeSet<_>>();
                match targets.into_iter().collect::<Vec<_>>().as_slice() {
                    [target] => Some(*target),
                    _ => None,
                }
            }
            (None, None) => {
                // The slots that can run the node: those its inputs come from, and those an isolation slot it is fed
                // from has an exit to, since an isolation slot runs only its state owner; a state owner runs in an
                // isolation slot its inputs' slots have an exit to.
                let mut candidates = BTreeSet::new();
                for &slot in &fed_from {
                    if !isolation(slot) {
                        candidates.insert(slot);
                    }
                    if isolation(slot) || role == Role::Isolated {
                        let exits = frame.slot(slot).map(|slot| slot.exits.as_slice()).unwrap_or_default();
                        candidates.extend(exits.iter().filter_map(|exit| match exit.target {
                            FrameTarget::Slot(next) => Some(next),
                            _ => None,
                        }));
                    }
                }
                let sinks = candidates
                    .into_iter()
                    .filter(|&slot| fits(slot) && receives_all(slot))
                    .collect::<Vec<_>>();
                match sinks.as_slice() {
                    [slot] => Some(*slot),
                    _ => None,
                }
            }
        };
        match chosen {
            Some(slot) => slot_of[node] = Some(slot),
            None if role == Role::Isolated => {
                let regions = fixed_slots
                    .keys()
                    .chain(&fed_from)
                    .copied()
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .map(region)
                    .collect::<Vec<_>>();
                // Without an isolation slot the topology cannot run a state owner at all; with one, it is not where the
                // state owner's input can reach.
                let what = if frame.slots().iter().any(|slot| slot.is_isolation()) {
                    "a new exit from a region to an isolation slot"
                } else {
                    "adding an isolation slot"
                };
                problems.push(problem(
                    Some(id),
                    asked_by(node),
                    format!(
                        "it is a state owner, which runs only in an isolation slot that the region its input comes \
                         from has an exit to, and the topology built at boot has no such isolation slot for {}: {}",
                        regions.join(", "),
                        asked_by(node).needs(what)
                    ),
                ));
                unplaced[node] = true;
            }
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
        // Each lane of an isolation slot on several lanes holds the share of its state owners' state that the slot's
        // keyed exits route to it by context key, so a state owner there must have its state partitioned by that key.
        if let Some(slot) = slot_of[node].filter(|_| role == Role::Isolated) {
            let lanes = frame.slot(slot).map_or(1, |slot| slot.lanes);
            if lanes > 1 && op.partition_key() != Some(PartitionKey::Context) {
                problems.push(problem(
                    Some(id),
                    asked_by(node),
                    format!(
                        "it is a state owner whose state cannot be partitioned by context key, and the topology built \
                         at boot runs isolation slot {} on {} lanes, each holding the share of the state that the \
                         slot's keyed exits route to it by context key: {}",
                        region(slot),
                        lanes,
                        asked_by(node).needs("running the isolation slot on one lane")
                    ),
                ));
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
                        // The route's slot passes the point's batches through to the state owner's isolation slot.
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

    Ok(Placement {
        sites,
        slots,
        idle_fixed,
    })
}
