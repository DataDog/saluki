//! Placement: running a candidate plan in the physical frame.
//!
//! [`place`] assigns every node of a validated candidate plan to where it runs, and checks that the frame has the
//! physical structure the candidate needs:
//!
//! - A **fence** (source or opaque host) runs in the fixed component of the same node, configured as at boot. A host
//!   component keeps the configuration it was built with, so adding or reconfiguring one needs a later stage. A fixed
//!   component the candidate leaves out is accepted only if it would then be idle: if a slot's exit is all that feeds
//!   it and none of its outputs feeds a slot. A source is never left out: its host keeps producing.
//! - An `emit` or `drop` fed straight by a fence must match the frame's direct edge: the same output, or no edge.
//! - Every other node is synchronous and runs in a slot. A node the boot plan has runs in its boot slot, where its
//!   inputs can reach that slot: R1's cost model may have put it in a later slot of a pipeline than its inputs. Any
//!   other node's slot follows from its inputs: the slot an admission point's edge leads to, or the slot of the node
//!   feeding it. When its inputs come from several slots, it runs in the one that every other of them has an exit to.
//! - A state owner behind an isolation fence runs in an isolation slot, and nothing else does: its boot slot, or the
//!   isolation slot that the slot its input comes from has an exit to, so that one artifact can aggregate each branch
//!   with its own width in the one isolation slot. A state owner fed straight by an admission point whose edge leads to
//!   a fused slot runs in the isolation slot that slot has an exit to: the fused slot passes the point's batches
//!   through, unchanged. A node fed by a state owner runs in a fused slot that the state owner's isolation slot has an
//!   exit to. A state owner outside the boot plan's isolation slots needs resource generations.
//! - Every edge between slots, from a slot to a fixed component, and from a slot to an output must use one of the
//!   slot's exits, and every edge leaving a fixed component must use its physical edge.
//! - Every declared output must be an output of the frame, bound as at boot.
//!
//! What the candidate does not use stays idle: an exit that no edge uses carries nothing. A candidate that needs
//! structure the frame lacks is rejected, with one [problem][PlacementProblem] per node or edge, each naming the
//! capability it needs and the stage that provides it. Nothing is approximated.
//!
//! # Entries
//!
//! A slot's program maps each batch it receives to a logical input of the slot: an **entry**. A batch from a fixed
//! component is mapped by the admission point that stamped it ([`EntryKey::Admission`]). A batch from another slot
//! carries the entry in its header, set by the slot that sent it ([`EntryKey::Region`]), since several logical edges
//! can share one exit. Entry IDs are numbered per slot and per generation, in plan edge order. A slot that passes an
//! admission point's batches through to a state owner has an entry for the point whose input is the state owner's,
//! in the isolation slot, which has the matching entry from the passing slot's exit.

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
    ir::{NodeId, OutputId, Plan, PortRef},
    operators::{OperatorRegistry, StateContract},
    validate::{graph::emit_output, ValidationReport},
};

/// A capability a candidate needs that this stage does not have.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum MissingCapability {
    /// State owners that run in regions, and changes to state owners (stage 6).
    State,

    /// Resource generations, which add or change outputs, recipients, host components, and physical edges without a
    /// restart (stage 7).
    ResourceGenerations,

    /// Operators that read host signals (stage 10).
    Signals,
}

impl MissingCapability {
    /// Returns the stage that provides the capability.
    pub const fn stage(self) -> u8 {
        match self {
            Self::State => 6,
            Self::ResourceGenerations => 7,
            Self::Signals => 10,
        }
    }

    /// Returns the capability's name, as error messages say it after the stage number.
    pub const fn name(self) -> &'static str {
        match self {
            Self::State => "state transitions",
            Self::ResourceGenerations => "resource generations",
            Self::Signals => "signals",
        }
    }
}

impl fmt::Display for MissingCapability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "stage {} {}", self.stage(), self.name())
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
                f.write_str(
                    "The candidate plan cannot run in the physical structure built at boot, and needs a restart: ",
                )?;
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

    /// The slot's entries: first those from fixed components, then those from other slots, each in plan edge order.
    pub entries: Vec<PlacedEntry>,

    /// The exits the candidate uses, in exit order. The slot's other exits stay idle.
    pub exits_used: Vec<ExitIndex>,
}

impl SlotPlacement {
    /// Returns the entry of the logical edge entering `input`, if one enters the slot there.
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
    let needs = |capability: MissingCapability, what: &str| {
        format!(
            "{} requires {}, which change physical structure without a restart",
            what, capability
        )
    };

    // Outputs: the frame's encoders, bound as at boot.
    for output in &plan.outputs {
        match frame.output(&output.id) {
            None => problems.push(PlacementProblem::new(
                None,
                Some(MissingCapability::ResourceGenerations),
                format!(
                    "output `{}` is not an output of the topology built at boot: {}",
                    output.id,
                    needs(MissingCapability::ResourceGenerations, "adding an output")
                ),
            )),
            Some(boot) if boot != output => problems.push(PlacementProblem::new(
                None,
                Some(MissingCapability::ResourceGenerations),
                format!(
                    "output `{}` is bound differently than at boot (its accepted type, fingerprint, recipients, or \
                     delivery policy changed): {}",
                    output.id,
                    needs(MissingCapability::ResourceGenerations, "changing an output's binding")
                ),
            )),
            Some(_) => {}
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
                match frame.fixed_component(id) {
                    None => problems.push(PlacementProblem::new(
                        Some(id),
                        Some(MissingCapability::ResourceGenerations),
                        format!(
                            "the topology built at boot has no host component for this `{}` node: {}",
                            op.name(),
                            needs(MissingCapability::ResourceGenerations, "adding a host component")
                        ),
                    )),
                    Some(fixed) if fixed.operator != written.op || fixed.params != written.params => {
                        // A host component keeps the configuration it was built with: reconfiguring it needs resource
                        // generations. (Aggregation runs as a kernel in its isolation slot, so its changes are state
                        // transitions the installer classifies, not host reconfigurations.)
                        problems.push(PlacementProblem::new(
                            Some(id),
                            Some(MissingCapability::ResourceGenerations),
                            format!(
                                "its operator or parameters differ from the boot plan's: {}",
                                needs(
                                    MissingCapability::ResourceGenerations,
                                    "reconfiguring a host component, which keeps the configuration it was built with,",
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
            problems.push(PlacementProblem::new(
                Some(&fixed.node),
                Some(MissingCapability::ResourceGenerations),
                format!(
                    "the candidate leaves out this host-owned node, but {}: {}",
                    why,
                    needs(MissingCapability::ResourceGenerations, "removing a host component")
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
                        problems.push(PlacementProblem::new(
                            Some(id),
                            Some(MissingCapability::ResourceGenerations),
                            format!(
                                "it is fed by `{}`, which the topology built at boot connects straight to {}, with \
                                 no region to run this node in: {}",
                                port_ref(from, from_port, false),
                                leads,
                                needs(MissingCapability::ResourceGenerations, "adding a region after a host")
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
            problems.push(PlacementProblem::new(
                Some(id),
                Some(MissingCapability::ResourceGenerations),
                format!(
                    "it joins {}, which the topology built at boot connects to different regions: {}",
                    joined.join(" and "),
                    needs(MissingCapability::ResourceGenerations, "a new physical edge")
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
                problems.push(PlacementProblem::new(
                    Some(id),
                    Some(MissingCapability::ResourceGenerations),
                    format!(
                        "it is a state owner, which runs only in an isolation slot that the region its input comes \
                         from has an exit to, and the topology built at boot has no such isolation slot for {}: {}",
                        regions.join(", "),
                        needs(MissingCapability::ResourceGenerations, what)
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
                problems.push(PlacementProblem::new(
                    Some(id),
                    Some(MissingCapability::ResourceGenerations),
                    format!(
                        "it joins records from the regions {}, and no one of them receives from all the others: {}",
                        regions.join(", "),
                        needs(
                            MissingCapability::ResourceGenerations,
                            "connecting regions in a new way"
                        )
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
                        .map(|output| FrameTarget::Output(output.clone())),
                };
                if point.route != expected {
                    let built = point
                        .route
                        .as_ref()
                        .map_or_else(|| "nothing".to_string(), |target| frame.describe_target(target));
                    let wanted = expected
                        .as_ref()
                        .map_or_else(|| "nothing".to_string(), |target| frame.describe_target(target));
                    problems.push(PlacementProblem::new(
                        Some(graph.id(to)),
                        Some(MissingCapability::ResourceGenerations),
                        format!(
                            "{} needs `{}` connected to {}, but the topology built at boot connects it to {}: {}",
                            describe_edge(),
                            edge.from,
                            wanted,
                            built,
                            needs(MissingCapability::ResourceGenerations, "a new physical edge")
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
                            Some(output) => FrameTarget::Output(output.clone()),
                            None => continue,
                        },
                        Some(other) => FrameTarget::Slot(other),
                        None => continue,
                    },
                    Role::Terminal => continue,
                };
                let has_exit = frame.slot(slot).is_some_and(|slot| slot.exit_to(&target).is_some());
                if !has_exit {
                    let what = match &target {
                        FrameTarget::Output(output) if frame.output(output).is_none() => {
                            needs(MissingCapability::ResourceGenerations, "adding an output")
                        }
                        _ => needs(MissingCapability::ResourceGenerations, "a new exit from a region"),
                    };
                    problems.push(PlacementProblem::new(
                        Some(graph.id(to)),
                        Some(MissingCapability::ResourceGenerations),
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
                let entry = PlacedEntry {
                    key: EntryKey::Admission(point.id),
                    source: FrameSource::Admission(point.id),
                    from: edge.from.clone(),
                    input: edge.to.clone(),
                };
                match &point.route {
                    Some(FrameTarget::Slot(route)) if *route != target => {
                        // The route's slot passes the point's batches through to the state owner's isolation slot.
                        let exit = frame
                            .slot(*route)
                            .and_then(|slot| slot.exit_to(&FrameTarget::Slot(target)))
                            .expect("checked above")
                            .index;
                        slots[route.get()].entries.push(entry);
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
                    _ => slots[target.get()].entries.push(entry),
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
                        .map(|output: &OutputId| FrameTarget::Output(output.clone())),
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
