//! Plan diffs: what an installation changes, node by node and resource by resource, how each state owner moves, and
//! which kernels and state instances each slot keeps.

use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    sync::Arc,
    time::Duration,
};

use saluki_core::execution::{AdmissionMode, AdmissionPointId, GenerationId};

use super::core::StateBinding;
use crate::{
    compile::{
        ClockId, Frame, KernelKey, LineageKey, MissingCapability, Placement, PlacementError, PlacementProblem,
        RegionKey, RegionProgram, SlotId,
    },
    extension::TransitionClass,
    ir::{Edge, HostId, Node, NodeId, OutputDecl, OutputId, Plan, PortRef, RecipientId},
    operators::{ConfiguredOperator, OperatorRegistry, StateContract, StateTime},
};

/// How one node differs between the active plan and a candidate.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NodeChange {
    /// The node is the same: operator, parameters with their resolved values, and edges.
    Unchanged,

    /// The node exists in both plans but differs.
    Changed {
        /// The operator differs.
        operator: bool,

        /// The parameters differ, including only in the resolved value of a configuration binding, or where one plan
        /// binds a value and the other writes the same literal.
        params: bool,

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
    /// state owner never emits two partial points for one bucket of one series. Once the two generations are placed,
    /// a state owner kept whose region or upstream points differ between them is cut too ([`PlanDiff::place`]).
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
    /// Both plans bind it alike ([`Plan::binds_alike`]), but perhaps for who declared its binding: the candidate reuses
    /// its encoder stream.
    Reused,

    /// Both plans declare it, with different bindings, or with a recipient whose protocol changed: the candidate gets a
    /// new encoder stream (a lease), and the old one flushes early and drains.
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

/// How one recipient, or one delivery host's settings, differ between the active plan and a candidate.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResourceChange {
    /// Both plans declare it alike, but perhaps for who declared it: the candidate reuses it as it runs.
    Reused,

    /// Both plans declare it, with other settings: a resource transaction applies them in place.
    Reconfigured,

    /// Only the candidate declares it.
    Added,

    /// Only the active plan declares it.
    Removed,
}

impl ResourceChange {
    /// Returns the change's name, as tools show it: `reused`, `reconfigured`, `added`, or `removed`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Reused => "reused",
            Self::Reconfigured => "reconfigured",
            Self::Added => "added",
            Self::Removed => "removed",
        }
    }
}

/// One recipient of a plan diff.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecipientDiff {
    /// The recipient.
    pub id: RecipientId,

    /// How it differs.
    pub change: ResourceChange,
}

/// One delivery host's settings in a plan diff.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostDiff {
    /// The delivery host.
    pub host: HostId,

    /// How its settings differ.
    pub change: ResourceChange,
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

    /// Every recipient either plan's resources declare: the candidate's in its order, then the removed ones.
    pub recipients: Vec<RecipientDiff>,

    /// The settings of every delivery host either plan's resources declare: the candidate's in its order, then the
    /// removed ones.
    pub hosts: Vec<HostDiff>,
}

impl PlanDiff {
    /// Returns `true` if the plans are the same in everything the diff compares.
    pub fn is_empty(&self) -> bool {
        self.nodes.iter().all(|node| node.change == NodeChange::Unchanged)
            && self.edges_added.is_empty()
            && self.edges_removed.is_empty()
            && self.outputs.iter().all(|output| output.change == OutputChange::Reused)
            && self
                .recipients
                .iter()
                .all(|recipient| recipient.change == ResourceChange::Reused)
            && self.hosts.iter().all(|host| host.change == ResourceChange::Reused)
    }

    /// Returns the nodes that are not unchanged.
    pub fn changes(&self) -> impl Iterator<Item = &NodeDiff> {
        self.nodes.iter().filter(|node| node.change != NodeChange::Unchanged)
    }

    /// Returns the strictest transition class of any state owner, or `None` if there is no state owner.
    pub fn strictest_transition(&self) -> Option<TransitionClass> {
        self.state_owners.iter().map(|owner| owner.class).max()
    }

    /// Classifies as `cut` every state owner kept with another class but `reject` whose region (slot, and region of the
    /// slot), or whose set of upstream points, differs between the active generation's placement and the candidate's.
    ///
    /// An instance of state cannot move between regions, and a state owner whose upstream points changed has a
    /// watermark over other inputs: its new lineage starts at a cut. An upstream point of a state owner downstream of
    /// another is that one's derived point, which stays the same while the upstream state owner moves or changes its
    /// own points, so a cut of the upstream state owner alone leaves it kept.
    pub fn place(&mut self, active: &Placement, candidate: &Placement) {
        for owner in &mut self.state_owners {
            if owner.change != StateOwnerChange::Kept || owner.class != TransitionClass::Retain {
                continue;
            }
            let moved = active.region_of(&owner.node) != candidate.region_of(&owner.node);
            let resourced = upstream_ids(active, &owner.node) != upstream_ids(candidate, &owner.node);
            if moved || resourced {
                owner.class = TransitionClass::Cut;
            }
        }
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
            boundary: Vec::new(),
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

    /// The retained state owners upstream of an affected one, in the order of the diff: each makes a boundary flush at
    /// the cut's time, and their widths are among [`widths`][Self::widths].
    pub boundary: Vec<NodeId>,
}

impl CutPlan {
    /// Returns the cut with its boundary: the state owners of `diff` the cut does not affect, kept in both
    /// generations, that are upstream of an affected one in the active generation's placement or the candidate's,
    /// through the derived points between them, in the order of the diff. Their bucket widths join the cut's, so that
    /// its time lines up with their buckets, at which each makes its boundary flush.
    pub fn with_boundary(mut self, diff: &PlanDiff, active: &Placement, candidate: &Placement) -> Self {
        let affected = self.owners.iter().collect::<BTreeSet<_>>();
        let mut upstream = BTreeSet::new();
        for owner in &self.owners {
            upstream.extend(owners_upstream(active, owner));
            upstream.extend(owners_upstream(candidate, owner));
        }
        let boundary = diff
            .state_owners
            .iter()
            .filter(|owner| {
                owner.change == StateOwnerChange::Kept
                    && !affected.contains(&owner.node)
                    && upstream.contains(&owner.node)
            })
            .collect::<Vec<_>>();
        self.widths.extend(
            boundary
                .iter()
                .flat_map(|owner| [owner.widths.0, owner.widths.1])
                .flatten(),
        );
        self.widths.sort_unstable();
        self.widths.dedup();
        self.unknown_widths.extend(
            boundary
                .iter()
                .filter(|owner| owner.widths.0.is_none() || owner.widths.1.is_none())
                .map(|owner| owner.node.clone()),
        );
        self.boundary = boundary.into_iter().map(|owner| owner.node.clone()).collect();
        self
    }

    /// Returns the admission points the cut is armed over, in ID order: the points upstream of every affected owner in
    /// either generation, through the derived points of state owners upstream of it (see
    /// [`Placement::admission_upstream`]).
    pub fn affected_points(&self, active: &Placement, candidate: &Placement) -> Vec<AdmissionPointId> {
        let mut points = BTreeSet::new();
        for owner in &self.owners {
            points.extend(active.admission_upstream(owner));
            points.extend(candidate.admission_upstream(owner));
        }
        points.into_iter().collect()
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

/// Returns every admission point of the frame in the order a cut's cancellation locks them: the points upstream of no
/// state owner of the active generation or the candidate first, then those upstream of one, each in ID order.
///
/// A point's lock is held while its batches fan out, so taking the points upstream of state owners last holds them only
/// for the check and the store, never while waiting for another point's lock.
pub fn cancel_lock_order(frame: &Frame, active: &Placement, candidate: &Placement) -> Vec<AdmissionPointId> {
    let state = [active, candidate]
        .into_iter()
        .flat_map(|placement| placement.slots())
        .flat_map(|slot| &slot.regions)
        .flat_map(|region| &region.upstream)
        .filter(|point| !point.derived)
        .map(|point| point.id)
        .collect::<BTreeSet<_>>();
    let mut points = frame.admission_point_ids().collect::<Vec<_>>();
    points.sort_unstable();
    let (state, other): (Vec<_>, Vec<_>) = points.into_iter().partition(|point| state.contains(point));
    other.into_iter().chain(state).collect()
}

/// Returns the IDs of the points upstream of a state owner in a placement, real and derived: those of the region it
/// runs in, or `None` if it runs in none.
fn upstream_ids(placement: &Placement, node: &NodeId) -> Option<BTreeSet<AdmissionPointId>> {
    let (slot, key) = placement.region_of(node)?;
    let region = placement.slot(slot)?.regions.iter().find(|region| region.key == key)?;
    Some(region.upstream.iter().map(|point| point.id).collect())
}

/// Returns the state owners upstream of `node` in a placement, through the derived points of each to the next: every
/// state owner whose time-driven output reaches it.
pub(crate) fn owners_upstream(placement: &Placement, node: &NodeId) -> BTreeSet<NodeId> {
    let mut found = BTreeSet::new();
    let mut pending = vec![node.clone()];
    while let Some(owner) = pending.pop() {
        let Some((slot, key)) = placement.region_of(&owner) else {
            continue;
        };
        let Some(region) = placement
            .slot(slot)
            .and_then(|placed| placed.regions.iter().find(|region| region.key == key))
        else {
            continue;
        };
        for point in region.upstream.iter().filter(|point| point.derived) {
            if let ClockId::Derived(upstream) = &point.clock {
                if found.insert(upstream.clone()) {
                    pending.push(upstream.clone());
                }
            }
        }
    }
    found
}

/// Diffs a candidate plan against the active one.
///
/// Both are core plans ([`elaborate`](crate::elaborate)), so parameters compare as written. Nodes correspond by ID. Each state owner's transition class follows the table of [`StateOwnerDiff::class`], from
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
        // Both plans are core plans, so equal parameters are written alike: a parameter left out and its default, or
        // two sets written in another order, were made one by elaboration.
        let params = old.params != node.params;
        let edges = old_edges.get(&node.id) != new_edges.get(&node.id);
        let change = if operator || params || edges {
            NodeChange::Changed {
                operator,
                params,
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
            Some(_) if active.binds_alike(candidate, &output.id) => OutputChange::Reused,
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

    let (recipients, hosts) = resource_changes(active, candidate);
    result.recipients = recipients;
    result.hosts = hosts;
    result
}

/// Diffs a composed candidate plan against the active one by what they do: as [`diff`], over both plans without the
/// nodes that do nothing as configured ([`eliminate`](crate::export::eliminate), as the detached export leaves them
/// out).
///
/// So a program written from the detached export, which leaves out an empty filterlist, diffs as no change in what it
/// does against the default program, which has it, and an edit of it lists only what it changes. This is the diff a
/// tool shows before publishing; [`Composed::ids`](crate::compose::Composed::ids) leads from a plan node back to the
/// artifact node it came from.
pub fn diff_operations(active: &Plan, candidate: &Plan, registry: &OperatorRegistry) -> PlanDiff {
    let active = crate::export::eliminate(active, registry).plan;
    let candidate = crate::export::eliminate(candidate, registry).plan;
    diff(&active, &candidate, registry)
}

/// Returns how every recipient, and every delivery host's settings, differ between the active plan and a candidate:
/// the candidate's in its order, then those only the active plan declares, in its order.
///
/// A recipient declared alike but for its provenance is reused, as it runs: the resources revision that declares it
/// changed, not the recipient.
pub fn resource_changes(active: &Plan, candidate: &Plan) -> (Vec<RecipientDiff>, Vec<HostDiff>) {
    let mut recipients = Vec::new();
    for recipient in &candidate.resources.recipients {
        let change = match active.resources.recipient(&recipient.id) {
            None => ResourceChange::Added,
            Some(old) if old.same_settings(recipient) => ResourceChange::Reused,
            Some(_) => ResourceChange::Reconfigured,
        };
        recipients.push(RecipientDiff {
            id: recipient.id.clone(),
            change,
        });
    }
    for recipient in &active.resources.recipients {
        if candidate.resources.recipient(&recipient.id).is_none() {
            recipients.push(RecipientDiff {
                id: recipient.id.clone(),
                change: ResourceChange::Removed,
            });
        }
    }

    let mut hosts = Vec::new();
    for binding in &candidate.resources.hosts {
        let change = match active.resources.host(&binding.host) {
            None => ResourceChange::Added,
            Some(old) if old == binding => ResourceChange::Reused,
            Some(_) => ResourceChange::Reconfigured,
        };
        hosts.push(HostDiff {
            host: binding.host.clone(),
            change,
        });
    }
    for binding in &active.resources.hosts {
        if candidate.resources.host(&binding.host).is_none() {
            hosts.push(HostDiff {
                host: binding.host.clone(),
                change: ResourceChange::Removed,
            });
        }
    }
    (recipients, hosts)
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

/// Which kernels one region of a slot keeps, builds, and stops using when a candidate's program replaces the active
/// one's.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SlotKernels {
    /// The slot.
    pub slot: SlotId,

    /// The region of the slot.
    pub region: RegionKey,

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

/// Returns which kernels every region reuses, builds, and releases when `candidate`'s programs replace `active`'s, one
/// per candidate program, in its order.
///
/// Each is a list of programs of regions of one frame, such as the program of every slot's main region, in slot order;
/// a candidate program replaces the active program of the same region of the same slot. Without an active generation,
/// or for a region the active generation does not run, every kernel is built.
pub fn kernel_reuse(active: Option<&[Arc<RegionProgram>]>, candidate: &[Arc<RegionProgram>]) -> Vec<SlotKernels> {
    candidate
        .iter()
        .map(|program| {
            let old = active.and_then(|programs| same_region(programs, program));
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
                region: program.region().clone(),
                unchanged: old.is_some_and(|old| old.fingerprint() == program.fingerprint()),
                reuse,
                build,
                release: old_keys.into_iter().filter(|key| !new_keys.contains(key)).collect(),
            }
        })
        .collect()
}

/// Returns the program of `programs` for the region of the slot `program` is for, if there is one.
fn same_region<'a>(programs: &'a [Arc<RegionProgram>], program: &RegionProgram) -> Option<&'a Arc<RegionProgram>> {
    programs
        .iter()
        .find(|old| old.slot() == program.slot() && old.region() == program.region())
}

/// Returns the lineages of a program's state owners, in its kernel order.
fn lineages(program: &RegionProgram) -> Vec<LineageKey> {
    program
        .kernels()
        .iter()
        .filter_map(|kernel| kernel.lineage.as_ref().map(|lineage| lineage.key.clone()))
        .collect()
}

/// Returns how each region binds the candidate's state owners to instances of their state, one list per candidate
/// program, in its order, empty for a program without state owners.
///
/// A state owner's kernel names its lineage ([`ProgramKernel::lineage`][crate::compile::ProgramKernel::lineage]).
/// `active` is the active generation with its programs, if one is active, `cut` the affected owners of a cut
/// installation ([`CutPlan::owners`]), and `boundary` the cut's retained owners upstream of them
/// ([`CutPlan::boundary`]), both empty otherwise. A candidate program's region is the active program's of the same
/// region of the same slot, in which:
///
/// - a state owner the cut does not affect, whose lineage the active program in the region also has, binds the active
///   generation's instance ([`StateBinding::Retain`]): its state carries over; one upstream of an affected owner makes
///   a boundary flush at the cut ([`StateBinding::Boundary`]);
/// - every other state owner of the candidate binds a new, empty instance ([`StateBinding::New`]);
/// - every lineage of the active program in the region that belongs to an affected owner retires
///   ([`StateBinding::Retire`]): it makes its final flush at the cut. Only cut installations retire lineages.
///
/// Bindings are listed in the candidate's kernel order, an affected owner's retirement right after its new binding,
/// then the retirements of the affected owners the candidate leaves out, in the active program's kernel order. The
/// retirements in regions the candidate runs no program for are [`retirements`]'.
pub fn state_bindings(
    active: Option<(GenerationId, &[Arc<RegionProgram>])>, candidate: &[Arc<RegionProgram>], cut: &BTreeSet<NodeId>,
    boundary: &BTreeSet<NodeId>,
) -> Vec<Vec<StateBinding>> {
    candidate
        .iter()
        .map(|program| {
            let old = active.and_then(|(generation, programs)| {
                same_region(programs, program).map(|program| (generation, lineages(program)))
            });
            bind_lineages(old, &lineages(program), cut, boundary)
        })
        .collect()
}

/// Returns the retirements of a cut installation in the regions of the active generation the candidate runs no program
/// for, as when the cut removes the state owner of a sub-region, or moves it to another region: each region's program
/// with the bindings that retire its affected lineages, for the regions that have any.
pub fn retirements<'a>(
    active: (GenerationId, &'a [Arc<RegionProgram>]), candidate: &[Arc<RegionProgram>], cut: &BTreeSet<NodeId>,
) -> Vec<(&'a Arc<RegionProgram>, Vec<StateBinding>)> {
    let (generation, programs) = active;
    programs
        .iter()
        .filter(|program| same_region(candidate, program).is_none())
        .map(|program| {
            let bindings = bind_lineages(Some((generation, lineages(program))), &[], cut, &BTreeSet::new());
            (program, bindings)
        })
        .filter(|(_, bindings)| !bindings.is_empty())
        .collect()
}

/// Returns the bindings of one region, from the lineages of the candidate's state owners in the region and the active
/// generation with the lineages of its program in the region, if there is one (see [`state_bindings`]).
pub(super) fn bind_lineages(
    active: Option<(GenerationId, Vec<LineageKey>)>, candidate: &[LineageKey], cut: &BTreeSet<NodeId>,
    boundary: &BTreeSet<NodeId>,
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
            Some((from, old)) if old == key && boundary.contains(&key.node) => bindings.push(StateBinding::Boundary {
                node,
                key: key.clone(),
                from,
            }),
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

#[cfg(test)]
mod tests;
