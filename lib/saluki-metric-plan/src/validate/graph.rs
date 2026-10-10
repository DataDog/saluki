//! The structure of a plan: nodes, their configured operators, and the edges between their ports.

use std::collections::HashMap;

use super::{Finding, Location, Rule, ValidationReport};
use crate::{
    ir::{Namespace, NodeId, OutputId, ParamValue, Plan, PortName, Provenance},
    operators::{names, Cardinality, ConfigureError, ConfiguredOperator, OperatorRegistry},
    predicate::Predicate,
};

/// One port of one node, by position: `(node, port)`.
pub(crate) type Endpoint = (usize, usize);

/// One node of the plan, with its configured operator.
pub(crate) struct GraphNode<'p> {
    /// The node as written.
    pub node: &'p crate::ir::Node,

    /// The configured operator, or `None` if the operator is unknown, its parameters are invalid, or the node's ID
    /// repeats an earlier node's.
    pub op: Option<ConfiguredOperator>,
}

/// A plan's nodes and edges, resolved to positions.
pub(crate) struct Graph<'p> {
    pub nodes: Vec<GraphNode<'p>>,
    index: HashMap<&'p NodeId, usize>,
    /// For each node and input port, the edge that enters it.
    in_edges: Vec<Vec<Option<usize>>>,
    /// For each node and output port, the edge that leaves it.
    out_edges: Vec<Vec<Option<usize>>>,
    /// For each edge, the output port it leaves, when that port exists.
    edge_from: Vec<Option<Endpoint>>,
    /// For each edge, the input port it enters, when that port exists.
    edge_to: Vec<Option<Endpoint>>,
    /// For each node, the nodes its edges lead to, whether or not the ports exist.
    successors: Vec<Vec<usize>>,
    /// The nodes in topological order, leaving out every node on a cycle or downstream of one.
    pub order: Vec<usize>,
    /// Whether the graph has a cycle.
    pub cyclic: bool,
}

impl<'p> Graph<'p> {
    /// Resolves the plan's structure, reporting every structural finding.
    pub fn build(plan: &'p Plan, registry: &OperatorRegistry, report: &mut ValidationReport) -> Self {
        let mut nodes = Vec::with_capacity(plan.nodes.len());
        let mut index = HashMap::with_capacity(plan.nodes.len());
        for node in &plan.nodes {
            let at = Location::Node {
                id: node.id.clone(),
                port: None,
            };
            if index.contains_key(&node.id) {
                report.push(Finding::new(
                    Rule::UniqueNodeIds,
                    at,
                    "another node already has this ID; give every node a distinct ID",
                ));
                nodes.push(GraphNode { node, op: None });
                continue;
            }
            index.insert(&node.id, nodes.len());

            // Only an artifact adds nodes in the policy namespace, and an artifact's limits apply to the nodes with its
            // provenance: a policy-namespace node without it would escape them.
            if node.id.namespace() == Namespace::Policy {
                let problem = match &node.provenance {
                    Some(Provenance::Artifact(_)) => None,
                    Some(Provenance::Native(_)) => Some("cannot have native provenance"),
                    None => Some("must have the provenance of the artifact that added them"),
                };
                if let Some(problem) = problem {
                    report.push(Finding::new(
                        Rule::ProvenanceNamespace,
                        at.clone(),
                        format!(
                            "nodes in the policy namespace come from artifacts, so they {}; give the node the \
                             provenance of its artifact revision",
                            problem
                        ),
                    ));
                }
            }

            let op = match registry.configure(node.op.as_str(), &node.params) {
                Ok(op) => Some(op),
                Err(error @ ConfigureError::UnknownOperator { .. }) => {
                    report.push(Finding::new(Rule::KnownOperator, at, error.to_string()));
                    None
                }
                Err(ConfigureError::InvalidParams { operator, errors }) => {
                    for error in errors {
                        report.push(Finding::new(
                            Rule::ValidParameters,
                            at.clone(),
                            format!("`{}` parameter `{}`: {}", operator, error.path, error.message),
                        ));
                    }
                    None
                }
            };
            nodes.push(GraphNode { node, op });
        }

        // A host source enters the plan once: two source nodes with one binding would each receive its records.
        let mut bindings = HashMap::new();
        for entry in &nodes {
            let Some(op) = entry.op.as_ref().filter(|op| op.name() == names::SOURCE) else {
                continue;
            };
            let Some(binding) = op.params().str("binding") else {
                continue;
            };
            if let Some(first) = bindings.insert(binding, &entry.node.id) {
                report.push(Finding::new(
                    Rule::UniqueSources,
                    Location::Node {
                        id: entry.node.id.clone(),
                        port: None,
                    },
                    format!(
                        "node `{}` already binds the `{}` source, which enters the plan once; fan its records out \
                         with a `tee` instead",
                        first, binding
                    ),
                ));
            }
        }

        let in_edges = nodes
            .iter()
            .map(|node| node.op.as_ref().map_or(0, |op| op.ports().inputs.len()))
            .map(|ports| vec![None; ports])
            .collect();
        let out_edges = nodes
            .iter()
            .map(|node| node.op.as_ref().map_or(0, |op| op.ports().outputs.len()))
            .map(|ports| vec![None; ports])
            .collect();
        let mut graph = Self {
            successors: vec![Vec::new(); nodes.len()],
            nodes,
            index,
            in_edges,
            out_edges,
            edge_from: vec![None; plan.edges.len()],
            edge_to: vec![None; plan.edges.len()],
            order: Vec::new(),
            cyclic: false,
        };

        graph.resolve_edges(plan, report);
        graph.check_unconnected_ports(report);
        graph.check_cycles(report);
        graph.check_reachability(report);
        graph
    }

    fn resolve_edges(&mut self, plan: &Plan, report: &mut ValidationReport) {
        for (i, edge) in plan.edges.iter().enumerate() {
            let at = || Location::Edge { index: i };
            let describe = format!("the edge from `{}` to `{}`", edge.from, edge.to);

            let from = self.index.get(&edge.from.node).copied();
            let to = self.index.get(&edge.to.node).copied();
            for (end, id) in [(from, &edge.from.node), (to, &edge.to.node)] {
                if end.is_none() {
                    report.push(Finding::new(
                        Rule::EdgeReferences,
                        at(),
                        format!("{} refers to node `{}`, which does not exist", describe, id),
                    ));
                }
            }
            if let (Some(from), Some(to)) = (from, to) {
                self.successors[from].push(to);
            }

            if let Some(from) = from {
                if let Some(port) = self.find_port(from, &edge.from.port, false, &describe, at(), report) {
                    if self.out_edges[from][port].is_some() {
                        report.push(Finding::new(
                            Rule::OneEdgePerPort,
                            self.port_location(from, &edge.from.port),
                            "this output port has more than one edge; fan out with a `tee`",
                        ));
                    } else {
                        self.out_edges[from][port] = Some(i);
                        self.edge_from[i] = Some((from, port));
                    }
                }
            }
            if let Some(to) = to {
                if let Some(port) = self.find_port(to, &edge.to.port, true, &describe, at(), report) {
                    if self.in_edges[to][port].is_some() {
                        report.push(Finding::new(
                            Rule::OneEdgePerPort,
                            self.port_location(to, &edge.to.port),
                            "this input port has more than one edge; fan in with a `merge`",
                        ));
                    } else {
                        self.in_edges[to][port] = Some(i);
                        self.edge_to[i] = Some((to, port));
                    }
                }
            }
        }
    }

    /// Returns the position of the named port of a node, reporting it if the node's operator has no such port.
    ///
    /// Returns `None` without a finding when the node's ports are not known.
    fn find_port(
        &self, node: usize, name: &PortName, input: bool, describe: &str, at: Location, report: &mut ValidationReport,
    ) -> Option<usize> {
        let op = self.nodes[node].op.as_ref()?;
        let (found, names) = if input {
            let ports = &op.ports().inputs;
            (
                op.ports().input(name),
                ports.iter().map(|port| port.name.as_str()).collect::<Vec<_>>(),
            )
        } else {
            let ports = &op.ports().outputs;
            (
                op.ports().output(name),
                ports.iter().map(|port| port.name.as_str()).collect::<Vec<_>>(),
            )
        };
        if found.is_none() {
            let direction = if input { "input" } else { "output" };
            let known = if names.is_empty() {
                format!("it has no {} ports", direction)
            } else {
                format!("its {} ports are: {}", direction, names.join(", "))
            };
            report.push(Finding::new(
                Rule::EdgeReferences,
                at,
                format!(
                    "{} uses port `{}` of node `{}`, but `{}` has no {} port `{}`; {}",
                    describe,
                    name,
                    self.id(node),
                    op.name(),
                    direction,
                    name,
                    known
                ),
            ));
        }
        found
    }

    fn check_unconnected_ports(&self, report: &mut ValidationReport) {
        for (node, entry) in self.nodes.iter().enumerate() {
            let Some(op) = &entry.op else { continue };
            for (port, edge) in op.ports().inputs.iter().zip(&self.in_edges[node]) {
                if edge.is_none() {
                    report.push(Finding::new(
                        Rule::OneEdgePerPort,
                        self.port_location(node, &port.name),
                        "this input port has no edge; connect it, or remove the node",
                    ));
                }
            }
            for (port, edge) in op.ports().outputs.iter().zip(&self.out_edges[node]) {
                if edge.is_none() {
                    report.push(Finding::new(
                        Rule::PathsEndInEmitOrDrop,
                        self.port_location(node, &port.name),
                        "this output port has no edge, so its records reach no `emit` or `drop`; connect it to an \
                         `emit`, or to a `drop` to discard them",
                    ));
                }
            }
        }
    }

    fn check_cycles(&mut self, report: &mut ValidationReport) {
        for mut component in strongly_connected_components(&self.successors) {
            let looped = component.len() > 1 || self.successors[component[0]].contains(&component[0]);
            if !looped {
                continue;
            }
            component.sort_unstable();
            let members = component
                .iter()
                .map(|&node| format!("`{}`", self.id(node)))
                .collect::<Vec<_>>()
                .join(", ");
            report.push(Finding::new(
                Rule::Acyclic,
                self.location(component[0]),
                format!("the nodes {} form a cycle; plans must be acyclic", members),
            ));
        }

        // Kahn's algorithm orders every node that is neither on a cycle nor downstream of one.
        let mut in_degree = vec![0usize; self.nodes.len()];
        for successors in &self.successors {
            for &next in successors {
                in_degree[next] += 1;
            }
        }
        let mut ready = (0..self.nodes.len())
            .filter(|&node| in_degree[node] == 0)
            .rev()
            .collect::<Vec<_>>();
        while let Some(node) = ready.pop() {
            self.order.push(node);
            for &next in self.successors[node].iter().rev() {
                in_degree[next] -= 1;
                if in_degree[next] == 0 {
                    ready.push(next);
                }
            }
        }
        self.cyclic = self.order.len() < self.nodes.len();
    }

    fn check_reachability(&self, report: &mut ValidationReport) {
        let mut reached = vec![false; self.nodes.len()];
        let mut pending = (0..self.nodes.len())
            .filter(|&node| {
                self.nodes[node]
                    .op
                    .as_ref()
                    .is_some_and(|op| op.contract().cardinality == Cardinality::Source)
            })
            .collect::<Vec<_>>();
        for &source in &pending {
            reached[source] = true;
        }
        while let Some(node) = pending.pop() {
            for &next in &self.successors[node] {
                if !reached[next] {
                    reached[next] = true;
                    pending.push(next);
                }
            }
        }

        for (node, reached) in reached.into_iter().enumerate() {
            // A node whose ID repeats another's has no edges of its own; it is reported as a duplicate.
            let duplicate = self.index.get(self.id(node)) != Some(&node);
            if !reached && !duplicate {
                report.push(Finding::new(
                    Rule::Reachable,
                    self.location(node),
                    "no path from a source reaches this node; connect it, or remove it",
                ));
            }
        }
    }

    /// Returns the ID of a node.
    pub fn id(&self, node: usize) -> &'p NodeId {
        &self.nodes[node].node.id
    }

    /// Returns the configured operator of a node, if it is known.
    pub fn op(&self, node: usize) -> Option<&ConfiguredOperator> {
        self.nodes[node].op.as_ref()
    }

    /// Returns the position of the node with the given ID.
    pub fn node_index(&self, id: &NodeId) -> Option<usize> {
        self.index.get(id).copied()
    }

    /// Returns the location of a node.
    pub fn location(&self, node: usize) -> Location {
        Location::Node {
            id: self.id(node).clone(),
            port: None,
        }
    }

    /// Returns the location of one port of a node.
    pub fn port_location(&self, node: usize, port: &PortName) -> Location {
        Location::Node {
            id: self.id(node).clone(),
            port: Some(port.clone()),
        }
    }

    /// Returns the input port that one output port feeds, if the edge between them resolved on both ends.
    pub fn target(&self, node: usize, port: usize) -> Option<Endpoint> {
        self.out_edges[node][port].and_then(|edge| self.edge_to[edge])
    }

    /// Returns `true` if an edge leaves one output port but does not resolve where it enters, as an edge into a node
    /// whose operator does not configure: where the port leads is not known.
    pub fn leads_somewhere_unknown(&self, node: usize, port: usize) -> bool {
        self.out_edges[node][port].is_some_and(|edge| self.edge_to[edge].is_none())
    }

    /// Returns the output port that feeds one input port, if the edge between them resolved on both ends.
    pub fn source(&self, node: usize, port: usize) -> Option<Endpoint> {
        self.in_edges[node][port].and_then(|edge| self.edge_from[edge])
    }

    /// Returns the output port named by a port reference, if it exists.
    pub fn output_port(&self, node: &NodeId, port: &PortName) -> Option<Endpoint> {
        let node = self.node_index(node)?;
        let port = self.op(node)?.ports().output(port)?;
        Some((node, port))
    }

    /// Returns the input port named by a port reference, if it exists.
    pub fn input_port(&self, node: &NodeId, port: &PortName) -> Option<Endpoint> {
        let node = self.node_index(node)?;
        let port = self.op(node)?.ports().input(port)?;
        Some((node, port))
    }

    /// Returns the name of one input port of a node.
    pub fn input_name(&self, (node, port): Endpoint) -> &str {
        self.op(node).map_or("?", |op| op.ports().inputs[port].name.as_str())
    }

    /// Returns the name of one output port of a node.
    pub fn output_name(&self, (node, port): Endpoint) -> &str {
        self.op(node).map_or("?", |op| op.ports().outputs[port].name.as_str())
    }
}

/// Returns the output an `emit` node delivers to.
pub(crate) fn emit_output(op: &ConfiguredOperator) -> Option<&OutputId> {
    if op.name() != names::EMIT {
        return None;
    }
    match op.params().get("output") {
        Some(ParamValue::Output(output)) => Some(output),
        _ => None,
    }
}

/// Returns the predicate of a `split` node.
pub(super) fn split_predicate(op: &ConfiguredOperator) -> Option<&Predicate> {
    if op.name() != names::SPLIT {
        return None;
    }
    op.params().predicate("predicate")
}

/// Returns the strongly connected components of a directed graph, given each node's successors.
///
/// This is the classic linear-time algorithm that tracks the lowest discovery index reachable from each node, written
/// iteratively so that deep plans cannot overflow the stack.
fn strongly_connected_components(successors: &[Vec<usize>]) -> Vec<Vec<usize>> {
    const UNVISITED: usize = usize::MAX;

    let count = successors.len();
    let mut index = vec![UNVISITED; count];
    let mut low = vec![0; count];
    let mut on_stack = vec![false; count];
    let mut stack = Vec::new();
    let mut next_index = 0;
    let mut components = Vec::new();

    for root in 0..count {
        if index[root] != UNVISITED {
            continue;
        }
        index[root] = next_index;
        low[root] = next_index;
        next_index += 1;
        stack.push(root);
        on_stack[root] = true;
        // Each frame is a node and the position of the next successor to visit.
        let mut frames = vec![(root, 0)];

        while let Some(&(node, position)) = frames.last() {
            if let Some(&next) = successors[node].get(position) {
                if let Some(frame) = frames.last_mut() {
                    frame.1 += 1;
                }
                if index[next] == UNVISITED {
                    index[next] = next_index;
                    low[next] = next_index;
                    next_index += 1;
                    stack.push(next);
                    on_stack[next] = true;
                    frames.push((next, 0));
                } else if on_stack[next] {
                    low[node] = low[node].min(index[next]);
                }
                continue;
            }

            frames.pop();
            if let Some(&(parent, _)) = frames.last() {
                low[parent] = low[parent].min(low[node]);
            }
            if low[node] == index[node] {
                let mut component = Vec::new();
                while let Some(member) = stack.pop() {
                    on_stack[member] = false;
                    component.push(member);
                    if member == node {
                        break;
                    }
                }
                components.push(component);
            }
        }
    }
    components
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sorted(mut components: Vec<Vec<usize>>) -> Vec<Vec<usize>> {
        for component in &mut components {
            component.sort_unstable();
        }
        components.sort();
        components
    }

    #[test]
    fn strongly_connected_components_find_every_cycle() {
        // 0 -> 1 -> 2 -> 0 is a cycle, 3 loops on itself, 4 -> 5 is acyclic, and 6 <-> 7 is unreachable from 0.
        let successors = vec![vec![1], vec![2], vec![0, 4], vec![3], vec![5], vec![], vec![7], vec![6]];
        assert_eq!(
            sorted(strongly_connected_components(&successors)),
            vec![vec![0, 1, 2], vec![3], vec![4], vec![5], vec![6, 7]]
        );
        assert!(strongly_connected_components(&[]).is_empty());
    }
}
