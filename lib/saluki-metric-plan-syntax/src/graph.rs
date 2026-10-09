//! What the parser and the printer both need to know about an artifact's graph: each node's ports, the names of
//! inline terminals, and the order nodes and edges are written in.

use std::collections::HashMap;

use saluki_metric_plan::{
    ir::{
        derived_terminal_id, ArtifactEdge, ArtifactNode, ArtifactNodeId, ArtifactPortRef, LocalId, NodeId,
        OperatorName, ParamSpec, ParamType, Params, PortName, SCOPE_INPUT_NODE,
    },
    operators::{OperatorRegistry, IN, OUT},
};

/// The ports of one node, as its operator declares them for its parameters.
///
/// The ports are unknown when the operator is not registered or the parameters do not configure it. Such a node is
/// invalid, which validation reports; the syntax then assumes the conventional single input `in` and single output
/// `out`, and accepts any port names written explicitly.
#[derive(Clone, Debug, Default)]
pub(crate) struct NodeShape {
    ports: Option<Ports>,
}

/// The declared ports of a node, with the position of each, so that looking a port up takes the same time however many
/// ports the node has.
#[derive(Clone, Debug)]
struct Ports {
    inputs: Vec<PortName>,
    outputs: Vec<PortName>,
    input_positions: HashMap<PortName, usize>,
    output_positions: HashMap<PortName, usize>,
}

impl NodeShape {
    /// Returns the shape of a node running `op` with `params`.
    pub(crate) fn of(op: &OperatorName, params: &Params) -> Self {
        let Ok(configured) = OperatorRegistry::native().configure(op.as_str(), params) else {
            return Self::default();
        };
        let ports = configured.ports();
        let inputs = ports.inputs.iter().map(|port| port.name.clone()).collect::<Vec<_>>();
        let outputs = ports.outputs.iter().map(|port| port.name.clone()).collect::<Vec<_>>();
        Self {
            ports: Some(Ports {
                input_positions: positions(&inputs),
                output_positions: positions(&outputs),
                inputs,
                outputs,
            }),
        }
    }

    /// Returns the declared input ports, if they are known.
    pub(crate) fn inputs(&self) -> Option<&[PortName]> {
        self.ports.as_ref().map(|ports| &ports.inputs[..])
    }

    /// Returns the declared output ports, if they are known.
    pub(crate) fn outputs(&self) -> Option<&[PortName]> {
        self.ports.as_ref().map(|ports| &ports.outputs[..])
    }

    /// Returns whether `port` is a declared input port, if the ports are known.
    pub(crate) fn has_input(&self, port: &PortName) -> Option<bool> {
        self.ports
            .as_ref()
            .map(|ports| ports.input_positions.contains_key(port))
    }

    /// Returns whether `port` is a declared output port, if the ports are known.
    pub(crate) fn has_output(&self, port: &PortName) -> Option<bool> {
        self.ports
            .as_ref()
            .map(|ports| ports.output_positions.contains_key(port))
    }

    /// Returns the position of `port` among the declared output ports, if the ports are known and it is one of them.
    fn output_position(&self, port: &PortName) -> Option<usize> {
        self.ports
            .as_ref()
            .and_then(|ports| ports.output_positions.get(port).copied())
    }

    /// Returns the input port an edge reaches when it names the node without a port: the only input port, or `in`
    /// when the ports are unknown. Returns `None` when the node has several input ports, or none.
    pub(crate) fn default_input(&self) -> Option<PortName> {
        sole_port(self.inputs(), IN)
    }

    /// Returns the output port an edge leaves when its line names no port: the only output port, or `out` when the
    /// ports are unknown. Returns `None` when the node has several output ports, or none.
    pub(crate) fn default_output(&self) -> Option<PortName> {
        sole_port(self.outputs(), OUT)
    }
}

/// Returns the position of each port in `ports`, keeping the first of any repeated name.
fn positions(ports: &[PortName]) -> HashMap<PortName, usize> {
    let mut positions = HashMap::with_capacity(ports.len());
    for (position, port) in ports.iter().enumerate() {
        positions.entry(port.clone()).or_insert(position);
    }
    positions
}

fn sole_port(ports: Option<&[PortName]>, conventional: &str) -> Option<PortName> {
    match ports {
        Some([port]) => Some(port.clone()),
        Some(_) => None,
        None => PortName::new(conventional).ok(),
    }
}

/// Returns the ID of the inline terminal at `port` of the node `parent`, or of the scope input `port` when `parent`
/// is `input`, as [`derived_terminal_id`] names it.
pub(crate) fn terminal_id(parent: &str, port: &PortName) -> Option<ArtifactNodeId> {
    let parent = NodeId::reserved(LocalId::new(parent).ok()?);
    ArtifactNodeId::new(derived_terminal_id(&parent, port).local().as_str()).ok()
}

/// The parent name of edges that leave a scope input, which names their inline terminals with [`terminal_id`].
pub(crate) const INPUT_PARENT: &str = SCOPE_INPUT_NODE;

/// Returns the parameter schema of the operator `op`, if it is registered.
pub(crate) fn param_specs(op: &str) -> Option<&'static [ParamSpec]> {
    OperatorRegistry::native().get(op).map(|def| def.params())
}

/// Returns the name of the operator's sole parameter when it is a predicate, which the syntax lets an author pass
/// without its name, as in `split(<predicate>)`.
pub(crate) fn positional_predicate(op: &str) -> Option<&'static str> {
    match param_specs(op)? {
        [spec] if spec.ty == ParamType::Predicate => Some(spec.name),
        _ => None,
    }
}

/// The order in which an artifact's nodes and edges are written, as permutations of their positions.
pub(crate) struct Order {
    /// Node positions, in written order.
    pub(crate) nodes: Vec<usize>,

    /// Edge positions, in written order.
    pub(crate) edges: Vec<usize>,
}

/// Computes the canonical order of an artifact's nodes and edges.
///
/// The order depends only on the graph, never on the order the nodes and edges are given in (apart from tie-breaks
/// between nodes with the same ID, or edges with the same ends, which are invalid). It is a depth-first traversal from
/// the scope inputs:
///
/// 1. The edges that leave the scope inputs are taken in order of input name (then of target).
/// 2. Taking an edge appends it to the edge order. When it is the last edge into its target node to be taken, the
///    target is visited at once: it is appended to the node order, and its outgoing edges are taken in the order of
///    its operator's output ports.
/// 3. Nodes the traversal does not reach are visited afterwards in order of ID: first those that no edge enters, then
///    the rest (nodes on a cycle, or fed by one), and edges from nodes that do not exist come last.
///
/// For an acyclic graph this is a topological order, and it is the order lowering writes plans in, so a node comes
/// right after the edge that completes its inputs: the default program's nodes and edges are already in this order.
///
/// `shapes` holds the shape of each node, in the order of `nodes`.
pub(crate) fn canonical_order(nodes: &[ArtifactNode], edges: &[ArtifactEdge], shapes: &[NodeShape]) -> Order {
    debug_assert_eq!(nodes.len(), shapes.len(), "every node has a shape");
    let mut index: HashMap<&str, usize> = HashMap::new();
    for (i, node) in nodes.iter().enumerate() {
        index.entry(node.id.as_str()).or_insert(i);
    }
    let node_of = |port: &ArtifactPortRef| match port {
        ArtifactPortRef::Node { node, .. } => index.get(node.as_str()).copied(),
        ArtifactPortRef::Input(_) => None,
    };

    let mut remaining = vec![0usize; nodes.len()];
    let mut leaving: Vec<Vec<(usize, &PortName)>> = vec![Vec::new(); nodes.len()];
    let mut inputs = Vec::new();
    let mut dangling = Vec::new();
    for (i, edge) in edges.iter().enumerate() {
        if let Some(target) = node_of(&edge.to) {
            remaining[target] += 1;
        }
        match &edge.from {
            ArtifactPortRef::Input(_) => inputs.push(i),
            ArtifactPortRef::Node { port, .. } => match node_of(&edge.from) {
                Some(source) => leaving[source].push((i, port)),
                None => dangling.push(i),
            },
        }
    }
    // Edges with the same ends are invalid; they keep their relative order.
    let by_ends = |edge: &usize| (&edges[*edge].from, &edges[*edge].to);
    inputs.sort_by_key(by_ends);
    dangling.sort_by_key(by_ends);
    let outgoing = leaving
        .into_iter()
        .zip(shapes)
        .map(|(mut out, shape)| {
            out.sort_by_key(|&(edge, port)| (port_rank(shape, port), &edges[edge].to, edge));
            out.into_iter().map(|(edge, _)| edge).collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();

    let mut traversal = Traversal {
        visited: vec![false; nodes.len()],
        order: Order {
            nodes: Vec::with_capacity(nodes.len()),
            edges: Vec::with_capacity(edges.len()),
        },
        remaining,
    };
    for edge in inputs {
        traversal.take(edge, node_of(&edges[edge].to), &outgoing, edges, &node_of);
    }

    let mut leftover = (0..nodes.len()).filter(|&i| !traversal.visited[i]).collect::<Vec<_>>();
    leftover.sort_by(|a, b| nodes[*a].id.cmp(&nodes[*b].id).then_with(|| a.cmp(b)));
    for unentered in [true, false] {
        for &node in &leftover {
            if !traversal.visited[node] && (!unentered || traversal.remaining[node] == 0) {
                traversal.visit(node, &outgoing, edges, &node_of);
            }
        }
    }
    traversal.order.edges.extend(dangling);
    traversal.order
}

/// The state of the depth-first traversal of [`canonical_order`].
struct Traversal {
    visited: Vec<bool>,
    remaining: Vec<usize>,
    order: Order,
}

impl Traversal {
    /// Takes the edge at position `edge`, whose target is the node at position `target`, if it is a node.
    fn take<F>(
        &mut self, edge: usize, target: Option<usize>, outgoing: &[Vec<usize>], edges: &[ArtifactEdge], node_of: &F,
    ) where
        F: Fn(&ArtifactPortRef) -> Option<usize>,
    {
        self.order.edges.push(edge);
        if let Some(target) = target {
            self.remaining[target] = self.remaining[target].saturating_sub(1);
            if self.remaining[target] == 0 && !self.visited[target] {
                self.visit(target, outgoing, edges, node_of);
            }
        }
    }

    /// Visits the node at position `start`, and every node its edges complete, depth first.
    ///
    /// The traversal keeps its own stack, so a long chain of nodes cannot exhaust the thread's stack.
    fn visit<F>(&mut self, start: usize, outgoing: &[Vec<usize>], edges: &[ArtifactEdge], node_of: &F)
    where
        F: Fn(&ArtifactPortRef) -> Option<usize>,
    {
        self.visited[start] = true;
        self.order.nodes.push(start);
        let mut stack = vec![(start, 0usize)];
        while let Some((node, next)) = stack.last_mut() {
            let Some(&edge) = outgoing[*node].get(*next) else {
                stack.pop();
                continue;
            };
            *next += 1;
            self.order.edges.push(edge);
            if let Some(target) = node_of(&edges[edge].to) {
                self.remaining[target] = self.remaining[target].saturating_sub(1);
                if self.remaining[target] == 0 && !self.visited[target] {
                    self.visited[target] = true;
                    self.order.nodes.push(target);
                    stack.push((target, 0));
                }
            }
        }
    }
}

/// Returns the sort key of an output port of a node: declared ports in declaration order, then any other ports, by
/// index and then by name.
fn port_rank<'p>(shape: &NodeShape, port: &'p PortName) -> (bool, Option<usize>, bool, Option<u16>, &'p str) {
    let declared = shape.output_position(port);
    (
        declared.is_none(),
        declared,
        port.as_index().is_none(),
        port.as_index(),
        port.as_str(),
    )
}
