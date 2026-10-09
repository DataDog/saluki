//! Plan diffs: what an installation changes, node by node, and which kernels each slot keeps.

use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    sync::Arc,
};

use crate::{
    compile::{KernelKey, RegionProgram, SlotId},
    extension::TransitionClass,
    ir::{Edge, Node, NodeId, OutputDecl, OutputId, Plan, PortRef},
    operators::{ConfiguredOperator, OperatorRegistry, StateContract},
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

/// How one state owner moves from the active plan to the candidate.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StateOwnerDiff {
    /// The node, which keeps state in both plans.
    pub node: NodeId,

    /// The transition class its old and new contracts give.
    pub class: TransitionClass,
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

    /// Every node that keeps state in both plans, with its transition class, in candidate order.
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

    /// Returns the strictest transition class of any state owner, or `None` if no node keeps state in both plans.
    pub fn strictest_transition(&self) -> Option<TransitionClass> {
        self.state_owners.iter().map(|owner| owner.class).max()
    }
}

/// Diffs a candidate plan against the active one.
///
/// Nodes correspond by ID. Each state owner's transition class comes from its old and new configured operators
/// ([`ConfiguredOperator::transition_to`]); a node whose operator does not configure is compared by what it says only,
/// and has no transition class.
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
    let stateful = |op: &ConfiguredOperator| op.contract().state != StateContract::None;

    let mut result = PlanDiff::default();
    for node in &candidate.nodes {
        let Some(old) = old_nodes.get(&node.id) else {
            result.nodes.push(NodeDiff {
                id: node.id.clone(),
                change: NodeChange::Added,
            });
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
            if stateful(&old_op) && stateful(&new_op) {
                result.state_owners.push(StateOwnerDiff {
                    node: node.id.clone(),
                    class: old_op.transition_to(&new_op),
                });
            }
        }
    }
    for node in &active.nodes {
        if !new_ids.contains(&node.id) {
            result.nodes.push(NodeDiff {
                id: node.id.clone(),
                change: NodeChange::Removed,
            });
        }
    }

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
