//! Plan diffs: what an installation changes, node by node, how each state owner moves, and which kernels and state
//! instances each slot keeps.

use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    sync::Arc,
    time::Duration,
};

use saluki_core::execution::{AdmissionMode, AdmissionPointId, GenerationId};

use super::core::StateBinding;
use crate::{
    compile::{
        Frame, KernelKey, LineageKey, MissingCapability, PlacementError, PlacementProblem, RegionProgram, SlotId,
    },
    extension::TransitionClass,
    ir::{Edge, Node, NodeId, OutputDecl, OutputId, Plan, PortRef},
    operators::{ConfiguredOperator, OperatorRegistry, StateContract, StateTime},
};

/// How one node differs between the active plan and a candidate.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NodeChange {
    /// The node is the same: operator, parameters with their resolved values, provenance, and edges.
    Unchanged,

    /// The node exists in both plans but differs.
    Changed {
        /// The operator differs.
        operator: bool,

        /// The parameters differ, including only in the resolved value of a configuration binding.
        params: bool,

        /// The provenance differs.
        provenance: bool,

        /// The edges at the node's ports differ.
        edges: bool,
    },

    /// Only the candidate has the node.
    Added,

    /// Only the active plan has the node.
    Removed,
}

/// One node of a plan diff.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NodeDiff {
    /// The node.
    pub id: NodeId,

    /// How it differs.
    pub change: NodeChange,
}

/// Whether a state owner is in both plans, or only in one.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StateOwnerChange {
    /// The node keeps state in both plans.
    Kept,

    /// The node keeps keyed state only in the candidate: it is new, or it kept no state before. Its lineage starts at
    /// the cut.
    Added,

    /// The node keeps keyed state only in the active plan: the candidate leaves it out. Its lineage ends at the cut.
    Removed,
}

/// How one state owner moves from the active plan to the candidate.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StateOwnerDiff {
    /// The node, which keeps keyed state in either plan, or opaque state in both.
    pub node: NodeId,

    /// The transition class of the move:
    ///
    /// | Active | Candidate | Class |
    /// |---|---|---|
    /// | keyed | keyed, same operator | what the contracts give ([`ConfiguredOperator::transition_to`]) |
    /// | absent, or no state | keyed | `cut`: the new lineage starts at the cut |
    /// | keyed | absent | `cut`: the old lineage ends at the cut |
    /// | keyed | another operator, or no keyed state | `reject` |
    /// | opaque | opaque | what the contracts give |
    /// | opaque | keyed | `reject` |
    ///
    /// A state owner added or removed is cut, rather than started or stopped at a batch boundary, so that a renamed
    /// state owner never emits two partial points for one bucket of one series.
    pub class: TransitionClass,

    /// Whether the node keeps state in both plans, or only in one.
    pub change: StateOwnerChange,

    /// The lineage of the node's state in the active plan and in the candidate, where it keeps keyed state.
    pub lineage: (Option<LineageKey>, Option<LineageKey>),

    /// The bucket width of the node's state in the active plan and in the candidate, where it keeps keyed state with a
    /// known width.
    pub widths: (Option<Duration>, Option<Duration>),
}

/// How one declared output differs between the active plan and a candidate.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OutputChange {
    /// Both plans declare it alike: the candidate reuses its encoder stream.
    Reused,

    /// Both plans declare it, with different bindings.
    Rebound,

    /// Only the candidate declares it.
    Added,

    /// Only the active plan declares it.
    Removed,
}

/// One output of a plan diff.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OutputDiff {
    /// The output.
    pub id: OutputId,

    /// How it differs.
    pub change: OutputChange,
}

/// What a candidate plan changes relative to the active one.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PlanDiff {
    /// Every node of either plan: the candidate's in its order, then the active plan's removed nodes in theirs.
    pub nodes: Vec<NodeDiff>,

    /// The candidate's edges the active plan does not have, in candidate order.
    pub edges_added: Vec<Edge>,

    /// The active plan's edges the candidate does not have, in active order.
    pub edges_removed: Vec<Edge>,

    /// Every state owner, with its transition class: every node that keeps keyed state in either plan, or opaque state
    /// in both, in candidate order, then the active plan's removed state owners in theirs.
    pub state_owners: Vec<StateOwnerDiff>,

    /// Every output either plan declares: the candidate's in its order, then the removed ones.
    pub outputs: Vec<OutputDiff>,
}

impl PlanDiff {
    /// Returns `true` if the plans are the same in everything the diff compares.
    pub fn is_empty(&self) -> bool {
        self.nodes.iter().all(|node| node.change == NodeChange::Unchanged)
            && self.edges_added.is_empty()
            && self.edges_removed.is_empty()
            && self.outputs.iter().all(|output| output.change == OutputChange::Reused)
    }

    /// Returns the nodes that are not unchanged.
    pub fn changes(&self) -> impl Iterator<Item = &NodeDiff> {
        self.nodes.iter().filter(|node| node.change != NodeChange::Unchanged)
    }

    /// Returns the strictest transition class of any state owner, or `None` if there is no state owner.
    pub fn strictest_transition(&self) -> Option<TransitionClass> {
        self.state_owners.iter().map(|owner| owner.class).max()
    }

    /// Returns the cut the installation needs, if it is a **cut installation**: some state owner's class is `cut`, and
    /// none is `reject`.
    ///
    /// The cut's affected owners are the state owners whose class is `cut`, and its widths every bucket width they have
    /// in either plan. A state owner whose class is `retain` is not affected, whatever else changes around it.
    pub fn cut(&self) -> Option<CutPlan> {
        if self.strictest_transition() != Some(TransitionClass::Cut) {
            return None;
        }
        let owners = self
            .state_owners
            .iter()
            .filter(|owner| owner.class == TransitionClass::Cut)
            .collect::<Vec<_>>();
        let mut widths = owners
            .iter()
            .flat_map(|owner| [owner.widths.0, owner.widths.1])
            .flatten()
            .collect::<Vec<_>>();
        widths.sort_unstable();
        widths.dedup();
        let unknown_widths = owners
            .iter()
            .filter(|owner| {
                let old = owner.change != StateOwnerChange::Added && owner.widths.0.is_none();
                let new = owner.change != StateOwnerChange::Removed && owner.widths.1.is_none();
                old || new
            })
            .map(|owner| owner.node.clone())
            .collect();
        Some(CutPlan {
            owners: owners.iter().map(|owner| owner.node.clone()).collect(),
            widths,
            unknown_widths,
        })
    }
}

/// The cut a cut installation needs: which state owners switch lineages at the cut, and the bucket widths the cut time
/// must line up with.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CutPlan {
    /// The state owners whose class is `cut`, in the order of the diff: the **affected** owners.
    pub owners: Vec<NodeId>,

    /// Every bucket width the affected owners have in either plan, in increasing order, without repeats: the cut time
    /// is a multiple of their least common multiple.
    pub widths: Vec<Duration>,

    /// The affected owners whose bucket width, in a plan that has them, is not known: no cut time lines up with an
    /// unknown width.
    pub unknown_widths: Vec<NodeId>,
}

impl CutPlan {
    /// Returns the slots that run an affected owner in either generation, in slot order, given the programs of every
    /// slot of the active generation and of the candidate.
    pub fn slots(&self, active: &[Arc<RegionProgram>], candidate: &[Arc<RegionProgram>]) -> Vec<SlotId> {
        let owners = self.owners.iter().collect::<BTreeSet<_>>();
        active
            .iter()
            .chain(candidate)
            .filter(|program| program.kernels().iter().any(|kernel| owners.contains(&kernel.key.node)))
            .map(|program| program.slot())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    /// Returns the admission points the cut is armed over, in ID order: the points upstream of the isolation slots that
    /// run an affected owner in either generation (see [`Frame::affected_points`]).
    pub fn affected_points(
        &self, frame: &Frame, active: &[Arc<RegionProgram>], candidate: &[Arc<RegionProgram>],
    ) -> Vec<AdmissionPointId> {
        frame.affected_points(&self.slots(active, candidate))
    }

    /// Checks that the frame can be cut over.
    ///
    /// Arming a cut and cancelling it lock admission points, and each lock is held while its batches fan out: that is
    /// deadlock-free, and stalls no admission point longer than the lock timeout, only while every admission point
    /// belongs to a source, so that no lock holder waits for another point's lock.
    ///
    /// # Errors
    ///
    /// Returns the placement problem if an admission point of the frame readmits a host component's records.
    pub fn check_frame(&self, frame: &Frame) -> Result<(), PlacementError> {
        let Some(point) = frame
            .admission_points()
            .iter()
            .find(|point| point.mode == AdmissionMode::Readmit)
        else {
            return Ok(());
        };
        let message = format!(
            "the candidate switches state owners at a cut, which locks admission points that belong to sources only, \
             but the topology built at boot readmits the records of host component `{}` at `{}.{}`: switching state \
             owners while a host component readmits records requires {}",
            point.node,
            point.node,
            point.port,
            MissingCapability::State
        );
        Err(PlacementError::Unplaceable {
            problems: vec![PlacementProblem {
                node: self.owners.first().cloned(),
                capability: Some(MissingCapability::State),
                message,
            }],
        })
    }
}

/// Diffs a candidate plan against the active one.
///
/// Nodes correspond by ID. Each state owner's transition class follows the table of [`StateOwnerDiff::class`], from
/// its old and new configured operators; a node whose operator does not configure is compared by what it says only,
/// and is no state owner.
pub fn diff(active: &Plan, candidate: &Plan, registry: &OperatorRegistry) -> PlanDiff {
    let old_nodes = active
        .nodes
        .iter()
        .map(|node| (&node.id, node))
        .collect::<BTreeMap<_, _>>();
    let new_ids = candidate.nodes.iter().map(|node| &node.id).collect::<BTreeSet<_>>();
    let old_edges = edges_by_node(active);
    let new_edges = edges_by_node(candidate);
    let configure = |node: &Node| registry.configure(node.op.as_str(), &node.params).ok();

    let mut result = PlanDiff::default();
    let mut removed_owners = Vec::new();
    for node in &candidate.nodes {
        let Some(old) = old_nodes.get(&node.id) else {
            result.nodes.push(NodeDiff {
                id: node.id.clone(),
                change: NodeChange::Added,
            });
            result
                .state_owners
                .extend(state_owner_diff(&node.id, None, configure(node).as_ref()));
            continue;
        };
        let operator = old.op != node.op;
        let params = old.params != node.params;
        let provenance = old.provenance != node.provenance;
        let edges = old_edges.get(&node.id) != new_edges.get(&node.id);
        let change = if operator || params || provenance || edges {
            NodeChange::Changed {
                operator,
                params,
                provenance,
                edges,
            }
        } else {
            NodeChange::Unchanged
        };
        result.nodes.push(NodeDiff {
            id: node.id.clone(),
            change,
        });

        if let (Some(old_op), Some(new_op)) = (configure(old), configure(node)) {
            result
                .state_owners
                .extend(state_owner_diff(&node.id, Some(&old_op), Some(&new_op)));
        }
    }
    for node in &active.nodes {
        if !new_ids.contains(&node.id) {
            result.nodes.push(NodeDiff {
                id: node.id.clone(),
                change: NodeChange::Removed,
            });
            removed_owners.extend(state_owner_diff(&node.id, configure(node).as_ref(), None));
        }
    }
    result.state_owners.extend(removed_owners);

    let old_set = active.edges.iter().collect::<HashSet<_>>();
    let new_set = candidate.edges.iter().collect::<HashSet<_>>();
    result.edges_added = candidate
        .edges
        .iter()
        .filter(|edge| !old_set.contains(edge))
        .cloned()
        .collect();
    result.edges_removed = active
        .edges
        .iter()
        .filter(|edge| !new_set.contains(edge))
        .cloned()
        .collect();

    let old_outputs = active
        .outputs
        .iter()
        .map(|output| (&output.id, output))
        .collect::<BTreeMap<_, &OutputDecl>>();
    for output in &candidate.outputs {
        let change = match old_outputs.get(&output.id) {
            None => OutputChange::Added,
            Some(old) if *old == output => OutputChange::Reused,
            Some(_) => OutputChange::Rebound,
        };
        result.outputs.push(OutputDiff {
            id: output.id.clone(),
            change,
        });
    }
    let new_outputs = candidate
        .outputs
        .iter()
        .map(|output| &output.id)
        .collect::<BTreeSet<_>>();
    for output in &active.outputs {
        if !new_outputs.contains(&output.id) {
            result.outputs.push(OutputDiff {
                id: output.id.clone(),
                change: OutputChange::Removed,
            });
        }
    }
    result
}

/// Returns how one node moves as a state owner, from its configured operator in the active plan to the one in the
/// candidate (`None` where a plan does not have the node), or `None` if the node is no state owner.
fn state_owner_diff(
    node: &NodeId, old: Option<&ConfiguredOperator>, new: Option<&ConfiguredOperator>,
) -> Option<StateOwnerDiff> {
    let state = |op: Option<&ConfiguredOperator>| op.map_or(StateContract::None, |op| op.contract().state);
    let keyed = |op: Option<&ConfiguredOperator>| matches!(state(op), StateContract::Keyed(_));
    let lineage = |op: Option<&ConfiguredOperator>| {
        op.filter(|op| keyed(Some(op)))
            .and_then(|op| LineageKey::of(node, op).ok())
    };
    let width = |op: Option<&ConfiguredOperator>| match state(op) {
        StateContract::Keyed(keyed) => match keyed.time {
            StateTime::Buckets { width } => width,
        },
        StateContract::None | StateContract::Opaque => None,
    };

    let (class, change) = match (old, new) {
        (Some(old_op), Some(new_op)) => match (state(old), state(new)) {
            (StateContract::Keyed(_), StateContract::Keyed(_)) if old_op.name() == new_op.name() => {
                (old_op.transition_to(new_op), StateOwnerChange::Kept)
            }
            // Another operator, or no keyed state, cannot take the state over.
            (StateContract::Keyed(_), _) | (StateContract::Opaque, StateContract::Keyed(_)) => {
                (TransitionClass::Reject, StateOwnerChange::Kept)
            }
            (StateContract::None, StateContract::Keyed(_)) => (TransitionClass::Cut, StateOwnerChange::Added),
            (StateContract::Opaque, StateContract::Opaque) => (old_op.transition_to(new_op), StateOwnerChange::Kept),
            (StateContract::None | StateContract::Opaque, StateContract::None | StateContract::Opaque) => return None,
        },
        (None, Some(_)) if keyed(new) => (TransitionClass::Cut, StateOwnerChange::Added),
        (Some(_), None) if keyed(old) => (TransitionClass::Cut, StateOwnerChange::Removed),
        _ => return None,
    };
    Some(StateOwnerDiff {
        node: node.clone(),
        class,
        change,
        lineage: (lineage(old), lineage(new)),
        widths: (width(old), width(new)),
    })
}

/// Returns the edges at each node's ports: every edge leaving or entering the node, sorted.
fn edges_by_node(plan: &Plan) -> BTreeMap<&NodeId, BTreeSet<(PortRef, PortRef)>> {
    let mut edges: BTreeMap<&NodeId, BTreeSet<(PortRef, PortRef)>> = BTreeMap::new();
    for edge in &plan.edges {
        let pair = (edge.from.clone(), edge.to.clone());
        edges.entry(&edge.from.node).or_default().insert(pair.clone());
        edges.entry(&edge.to.node).or_default().insert(pair);
    }
    edges
}

/// Which kernels one slot keeps, builds, and stops using when a candidate's program replaces the active one's.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SlotKernels {
    /// The slot.
    pub slot: SlotId,

    /// Whether the candidate's program is the active one's: equal fingerprints, so the region keeps running the
    /// program it has.
    pub unchanged: bool,

    /// The kernels the candidate's program shares with the active one's, in the candidate's kernel order: the region
    /// reuses those instances, caches included.
    pub reuse: Vec<KernelKey>,

    /// The kernels only the candidate's program has, in its kernel order: they are built new.
    pub build: Vec<KernelKey>,

    /// The kernels only the active program has, in its kernel order: the region drops them when the active generation
    /// retires.
    pub release: Vec<KernelKey>,
}

/// Returns which kernels every slot reuses, builds, and releases when `candidate`'s programs replace `active`'s.
///
/// Both are the programs of every slot of one frame, in slot order. Without an active generation, every kernel is
/// built.
pub fn kernel_reuse(active: Option<&[Arc<RegionProgram>]>, candidate: &[Arc<RegionProgram>]) -> Vec<SlotKernels> {
    candidate
        .iter()
        .enumerate()
        .map(|(index, program)| {
            let old = active.and_then(|programs| programs.get(index));
            let keys = |program: &RegionProgram| {
                program
                    .kernels()
                    .iter()
                    .map(|kernel| kernel.key.clone())
                    .collect::<Vec<_>>()
            };
            let new_keys = keys(program);
            let old_keys = old.map(|old| keys(old)).unwrap_or_default();
            let (reuse, build) = new_keys.iter().cloned().partition(|key| old_keys.contains(key));
            SlotKernels {
                slot: program.slot(),
                unchanged: old.is_some_and(|old| old.fingerprint() == program.fingerprint()),
                reuse,
                build,
                release: old_keys.into_iter().filter(|key| !new_keys.contains(key)).collect(),
            }
        })
        .collect()
}

/// Returns how each slot binds the candidate's state owners to instances of their state, in slot order: one list per
/// candidate program, empty for a program without state owners.
///
/// A state owner's kernel names its lineage ([`ProgramKernel::lineage`][crate::compile::ProgramKernel::lineage]).
/// `active` is the active generation with its programs, if one is active, and `cut` the affected owners of a cut
/// installation ([`CutPlan::owners`]), empty otherwise. In each slot:
///
/// - a state owner the cut does not affect, whose lineage the active program in the slot also has, binds the active
///   generation's instance ([`StateBinding::Retain`]): its state carries over;
/// - every other state owner of the candidate binds a new, empty instance ([`StateBinding::New`]);
/// - every lineage of the active program in the slot that belongs to an affected owner retires
///   ([`StateBinding::Retire`]): it makes its final flush at the cut. Only cut installations retire lineages.
///
/// Bindings are listed in the candidate's kernel order, an affected owner's retirement right after its new binding,
/// then the retirements of the affected owners the candidate leaves out, in the active program's kernel order.
pub fn state_bindings(
    active: Option<(GenerationId, &[Arc<RegionProgram>])>, candidate: &[Arc<RegionProgram>], cut: &BTreeSet<NodeId>,
) -> Vec<Vec<StateBinding>> {
    let lineages = |program: &RegionProgram| {
        program
            .kernels()
            .iter()
            .filter_map(|kernel| kernel.lineage.as_ref().map(|lineage| lineage.key.clone()))
            .collect::<Vec<_>>()
    };
    candidate
        .iter()
        .enumerate()
        .map(|(index, program)| {
            let old = active
                .and_then(|(generation, programs)| programs.get(index).map(|program| (generation, lineages(program))));
            bind_lineages(old, &lineages(program), cut)
        })
        .collect()
}

/// Returns the bindings of one slot, from the lineages of the candidate's state owners in the slot and the active
/// generation with the lineages of its program in the slot, if there is one (see [`state_bindings`]).
pub(super) fn bind_lineages(
    active: Option<(GenerationId, Vec<LineageKey>)>, candidate: &[LineageKey], cut: &BTreeSet<NodeId>,
) -> Vec<StateBinding> {
    let mut bindings = Vec::new();
    let old = |node: &NodeId| {
        active
            .as_ref()
            .and_then(|(generation, keys)| keys.iter().find(|key| &key.node == node).map(|key| (*generation, key)))
    };
    for key in candidate {
        let node = key.node.clone();
        match old(&key.node) {
            Some((from, old)) if cut.contains(&key.node) => {
                bindings.push(StateBinding::New {
                    node: node.clone(),
                    key: key.clone(),
                });
                bindings.push(StateBinding::Retire {
                    node,
                    key: old.clone(),
                    from,
                });
            }
            Some((from, old)) if old == key => bindings.push(StateBinding::Retain {
                node,
                key: key.clone(),
                from,
            }),
            // A lineage that is not affected keeps its key, since the key covers exactly what a `retain` keeps; one
            // whose key changed anyway cannot share the instance, and starts empty.
            _ => bindings.push(StateBinding::New { node, key: key.clone() }),
        }
    }
    if let Some((from, keys)) = &active {
        for key in keys {
            if cut.contains(&key.node) && !candidate.iter().any(|new| new.node == key.node) {
                bindings.push(StateBinding::Retire {
                    node: key.node.clone(),
                    key: key.clone(),
                    from: *from,
                });
            }
        }
    }
    bindings
}
