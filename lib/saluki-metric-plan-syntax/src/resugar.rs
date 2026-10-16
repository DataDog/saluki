//! Resugaring: an artifact to the [syntax tree](crate::ast) a person would write for it.
//!
//! Resugaring is the inverse of [lowering](crate::lower): for every well-formed artifact in canonical order, lowering
//! the tree this module makes gives back the artifact, node for node, parameter for parameter, and edge for edge, with
//! the same IDs. Each sugar form is used only where its rewrite into the core gives back exactly the nodes, edges, and
//! IDs it was recognized from; everywhere else the core form is written. Formatting turns the tree into text.
//!
//! # The algorithm
//!
//! 1. **Roles.** Every node gets a role from its operator, its parameters, and its neighbors, in priority order:
//!    - a `merge` is a **tuple**, always, since a tuple is the merge's core form;
//!    - an `emit` or `drop` whose parameters a sink can write is a **sink**;
//!    - a `split` whose `false` port leads to a `drop` with the derived ID [`ids::terminal`]`(split, "false")` is a
//!      **`where`**;
//!    - a `tee` whose ports each lead, through stages that each have one input and one output, to a sink, an exit, or
//!      another such `tee`, is a **fork**, nested at most [`MAX_FORK_DEPTH`] deep;
//!    - a `tee` with the derived ID [`ids::fanout`] of the stream before it, when that stream can be named, is an
//!      **implicit tee**: the stream is used once per port, in port order;
//!    - anything else is a **multi-output** node (a `let` of its own, whose ports are written `x.p`), a **plain**
//!      node with one output port, or a **final** node without one (a `let` of its own too, since lowering exempts
//!      such a `let` from being used).
//! 2. **Streams.** A port is **named** when it must be written as a reference: a scope input, a port of a
//!    multi-output node, or the output of a node that is used more than once (through an implicit tee) or feeds a
//!    tuple. Every other port continues its pipeline into the one node it feeds.
//! 3. **Chains.** Each use of a named stream starts a pipeline with a reference to it, and each tuple starts a pipeline
//!    with the tuple. A pipeline follows its stream through plain nodes and `where`s until it reaches a sink, an exit,
//!    or a fork (a statement without `let`), or a named stream, multi-output node, or final node (a `let` named for its
//!    last node). Nodes that no scope input reaches start pipelines of their own, from a reference to whatever feeds
//!    them, so that lowering reports the problem.
//! 4. **Intermediate names.** A pipeline is split into several `let`s where a person evidently named an intermediate
//!    stream: where a run of two or more nodes (a tuple's merge counts) would all have their derived IDs as a `let`
//!    named for the run's last node. Among the ways to split it, the one with the fewest `as` clauses wins, then the
//!    one with the fewest statements.
//! 5. **`as`.** Every node whose ID differs from the one lowering derives at its position gets an `as`: stages from
//!    the statement's anchor and their ordinal (every stage of a pipeline or fork branch counts, a `where` as `split`,
//!    and each branch counts afresh), a bare tuple's merge from its first item's anchor, a fork's tee from the
//!    statement's anchor, and a sink from the node and port that feed it.
//! 6. **Order.** Statements are ordered by the canonical position of the edge each starts from, subject to every
//!    `let` coming before its uses, and to the uses of each implicit tee coming in port order (lowering numbers them
//!    in source order). When no order satisfies both, the implicit tee involved is written in the core form instead,
//!    and the program is resugared again.
//! 7. **Parameters.** Parameters equal to their registry default are left out, and the positional parameter, when
//!    present, is written first.
//!
//! Resugaring is total: an artifact that is not well-formed still gives a tree, from which lowering reports the
//! problem.

use std::{
    cmp::Reverse,
    collections::{BTreeMap, BinaryHeap, HashMap, HashSet},
};

use saluki_metric_plan::{
    ir::{
        ArtifactEdge, ArtifactNode, ArtifactNodeId, ArtifactPortRef, ExitName, InputName, OutputId, ParamName,
        ParamSpec, Params, PolicyArtifact, PortName, IR_VERSION, SCOPE_INPUT_NODE,
    },
    operators::{names, EMIT_TO, FALSE_PORT, OUT, TRUE_PORT},
};
use serde_json::{Map, Value};

use crate::{
    ast::{
        Base, Branch, End, Fork, Head, Name, OpStage, Pipeline, Program, Ref, Sink, SinkKind, Stage, Statement, Tuple,
        WhereStage,
    },
    diagnostic::Span,
    graph::{canonical_order, param_specs, NodeShape},
    ids, PrintOptions,
};

/// The deepest that forks nest. A `tee` whose branches would nest deeper is written in the core form, so that the
/// tree, and the text formatted from it, stay shallow however the artifact nests its `tee`s.
const MAX_FORK_DEPTH: usize = 16;

/// Returns the tree that describes `artifact`, with sugar wherever lowering it gives back exactly the same graph.
///
/// Total: any artifact gives a tree. For a well-formed artifact in canonical order, lowering the tree gives back the
/// artifact. `options` says whether the tree has a `base` statement, and whether it lists output fingerprints.
pub(crate) fn resugar(artifact: &PolicyArtifact, options: &PrintOptions) -> Program {
    let graph = Graph::new(artifact);
    let mut demoted = HashSet::new();
    let (plan, order) = loop {
        let plan = Builder::new(&graph, &demoted).build();
        match graph.order(&plan) {
            Ok(order) => break (plan, order),
            Err(tee) => {
                if !demoted.insert(tee) {
                    // Unreachable, since a demoted tee is never implicit; ordering by position keeps resugaring total.
                    let order = by_key(&plan.chains, (0..plan.chains.len()).collect());
                    break (plan, order);
                }
            }
        }
    };

    let emitter = Emitter {
        graph: &graph,
        roles: &plan.roles,
    };
    let statements = order
        .into_iter()
        .flat_map(|chain| emitter.chain(&plan.chains[chain]))
        .collect();

    Program {
        head: Span::default(),
        scope: artifact.scope.clone(),
        ir_version: (artifact.ir_version != IR_VERSION).then_some(artifact.ir_version),
        base: (!options.elide_base).then(|| Base {
            program_hash: artifact.base.program_hash,
            outputs: if options.elide_fingerprints {
                BTreeMap::new()
            } else {
                artifact.base.outputs.clone()
            },
        }),
        blobs: (!artifact.blobs.is_empty()).then(|| {
            artifact
                .blobs
                .iter()
                .map(|(hash, value)| (hash.to_string(), value.clone()))
                .collect::<Map<String, Value>>()
        }),
        statements,
        close: Span::default(),
    }
}

/// An output port that edges leave: a scope input, or a port of a node, by position.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
enum Stream {
    Input(InputName),
    Port(usize, PortName),
}

/// What an edge enters.
enum Target {
    Node(usize),
    Exit(ExitName),
    /// A node the artifact does not have.
    Missing,
}

/// The artifact's graph, indexed for resugaring.
struct Graph<'a> {
    nodes: &'a [ArtifactNode],
    edges: &'a [ArtifactEdge],
    shapes: Vec<NodeShape>,
    /// The canonical position of each node.
    node_rank: Vec<usize>,
    /// The canonical position of each edge.
    edge_rank: Vec<usize>,
    /// Node positions, in canonical order.
    node_order: Vec<usize>,
    /// Edge positions, in canonical order.
    edge_order: Vec<usize>,
    /// The position of the first node with each ID.
    index: HashMap<&'a str, usize>,
    /// Whether each node's ID is unique.
    unique: Vec<bool>,
    /// Each node's output ports: the declared ones, or for a node whose ports are unknown (an unregistered operator, or
    /// parameters that do not configure it), those its edges leave.
    outputs: Vec<Vec<PortName>>,
    /// The edges entering each node, in canonical order.
    entering: Vec<Vec<usize>>,
    /// The edges leaving each stream, in canonical order.
    leaving: HashMap<Stream, Vec<usize>>,
    /// The names of the scope inputs that edges leave.
    input_names: HashSet<&'a str>,
}

impl<'a> Graph<'a> {
    fn new(artifact: &'a PolicyArtifact) -> Self {
        let nodes = &artifact.nodes[..];
        let edges = &artifact.edges[..];
        let shapes = nodes
            .iter()
            .map(|node| NodeShape::of(&node.op, &node.params))
            .collect::<Vec<_>>();
        let order = canonical_order(nodes, edges, &shapes);
        let (node_order, node_rank) = ranks(order.nodes, nodes.len());
        let (edge_order, edge_rank) = ranks(order.edges, edges.len());

        let mut index = HashMap::new();
        let mut count: HashMap<&str, usize> = HashMap::new();
        for (i, node) in nodes.iter().enumerate() {
            index.entry(node.id.as_str()).or_insert(i);
            *count.entry(node.id.as_str()).or_default() += 1;
        }
        let unique = nodes.iter().map(|node| count[node.id.as_str()] == 1).collect();

        let mut entering = vec![Vec::new(); nodes.len()];
        let mut leaving: HashMap<Stream, Vec<usize>> = HashMap::new();
        let mut used: Vec<Vec<PortName>> = vec![Vec::new(); nodes.len()];
        let mut input_names = HashSet::new();
        for &e in &edge_order {
            let edge = &edges[e];
            if let ArtifactPortRef::Node { node, .. } = &edge.to {
                if let Some(&target) = index.get(node.as_str()) {
                    entering[target].push(e);
                }
            }
            match &edge.from {
                ArtifactPortRef::Input(name) => {
                    input_names.insert(name.as_str());
                    leaving.entry(Stream::Input(name.clone())).or_default().push(e);
                }
                ArtifactPortRef::Node { node, port } => {
                    if let Some(&source) = index.get(node.as_str()) {
                        if !used[source].contains(port) {
                            used[source].push(port.clone());
                        }
                        leaving.entry(Stream::Port(source, port.clone())).or_default().push(e);
                    }
                }
                ArtifactPortRef::Exit(_) => {}
            }
        }
        let outputs = shapes
            .iter()
            .zip(used)
            .map(|(shape, used)| match shape.outputs() {
                Some(ports) => ports.to_vec(),
                // Without edges leaving it, a node whose ports are unknown ends its pipeline.
                None => used,
            })
            .collect();

        Self {
            nodes,
            edges,
            shapes,
            node_rank,
            edge_rank,
            node_order,
            edge_order,
            index,
            unique,
            outputs,
            entering,
            leaving,
            input_names,
        }
    }

    fn id(&self, node: usize) -> &ArtifactNodeId {
        &self.nodes[node].id
    }

    fn leaving(&self, stream: &Stream) -> &[usize] {
        self.leaving.get(stream).map_or(&[], Vec::as_slice)
    }

    fn source(&self, edge: usize) -> Option<Stream> {
        match &self.edges[edge].from {
            ArtifactPortRef::Input(name) => Some(Stream::Input(name.clone())),
            ArtifactPortRef::Node { node, port } => self
                .index
                .get(node.as_str())
                .map(|&source| Stream::Port(source, port.clone())),
            ArtifactPortRef::Exit(_) => None,
        }
    }

    fn target(&self, edge: usize) -> Target {
        match &self.edges[edge].to {
            ArtifactPortRef::Node { node, .. } => match self.index.get(node.as_str()) {
                Some(&target) => Target::Node(target),
                None => Target::Missing,
            },
            ArtifactPortRef::Exit(name) => Target::Exit(name.clone()),
            ArtifactPortRef::Input(_) => Target::Missing,
        }
    }

    fn to_port(&self, edge: usize) -> Option<&PortName> {
        match &self.edges[edge].to {
            ArtifactPortRef::Node { port, .. } => Some(port),
            ArtifactPortRef::Input(_) | ArtifactPortRef::Exit(_) => None,
        }
    }

    /// Returns the only edge leaving `stream`, if exactly one does.
    fn sole(&self, stream: &Stream) -> Option<usize> {
        match self.leaving(stream) {
            [edge] => Some(*edge),
            _ => None,
        }
    }

    /// Returns whether the node is a `tee` whose parameters are exactly its port count.
    fn is_tee(&self, node: usize) -> bool {
        let n = &self.nodes[node];
        n.op.as_str() == names::TEE
            && only_param(&n.params, "ports")
            && self.shapes[node].outputs().is_some()
            && !self.outputs[node].is_empty()
            && n.params.get("ports").and_then(Value::as_u64) == Some(self.outputs[node].len() as u64)
    }

    /// Returns the role a node has before forks and implicit tees are recognized.
    fn base_role(&self, node: usize) -> Role {
        let n = &self.nodes[node];
        if n.op.as_str() == names::MERGE {
            return Role::Tuple;
        }
        if self.unique[node] && self.shapes[node].outputs().is_some_and(<[_]>::is_empty) {
            if let Some(kind) = sink_kind(n) {
                return Role::Sink(kind);
            }
        }
        if let Some(role) = self.where_role(node) {
            return role;
        }
        match self.outputs[node].as_slice() {
            [] => Role::Final,
            [port] => Role::Plain(port.clone()),
            _ => Role::Multi,
        }
    }

    /// Returns the `where` role of a `split` whose `false` port leads to exactly the `drop` a `where` makes.
    fn where_role(&self, node: usize) -> Option<Role> {
        let n = &self.nodes[node];
        if n.op.as_str() != names::SPLIT || !only_param(&n.params, "predicate") {
            return None;
        }
        let (on_true, on_false) = (PortName::new(TRUE_PORT).ok()?, PortName::new(FALSE_PORT).ok()?);
        if self.shapes[node].outputs() != Some(&[on_true, on_false.clone()][..]) {
            return None;
        }
        let edge = self.sole(&Stream::Port(node, on_false.clone()))?;
        let Target::Node(drop) = self.target(edge) else {
            return None;
        };
        let d = &self.nodes[drop];
        let is_where_drop = d.op.as_str() == names::DROP
            && d.params.is_empty()
            && self.unique[drop]
            && self.entering[drop] == [edge]
            && self.to_port(edge) == self.shapes[drop].default_input().as_ref()
            && self.shapes[drop].outputs().is_some_and(<[_]>::is_empty)
            && d.id == ids::terminal(n.id.as_str(), &on_false);
        is_where_drop.then_some(Role::Where { drop })
    }

    /// Returns, for each node, whether it is a `tee` to write as a fork.
    fn forks(&self, base: &[Role]) -> Vec<bool> {
        let mut state = vec![Term::Unknown; self.nodes.len()];
        for node in 0..self.nodes.len() {
            if self.is_tee(node) {
                self.terminates(node, base, &mut state);
            }
        }
        (0..self.nodes.len())
            .map(|node| self.is_tee(node) && matches!(state[node], Term::Done(Some(_))))
            .collect()
    }

    /// Returns where a pipeline entering `node` goes next, if it can be part of a fork's branch: one entry per port it
    /// continues from, `None` for an exit. Returns `None` if the node cannot be part of a branch.
    fn branch_next(&self, node: usize, base: &[Role]) -> Option<Vec<Option<usize>>> {
        if self.entering[node].len() != 1 {
            return None;
        }
        let ports = if self.is_tee(node) {
            self.outputs[node].clone()
        } else {
            match &base[node] {
                Role::Sink(_) => Vec::new(),
                Role::Where { .. } => vec![PortName::new(TRUE_PORT).ok()?],
                Role::Plain(port) => vec![port.clone()],
                _ => return None,
            }
        };
        ports
            .into_iter()
            .map(|port| match self.target(self.sole(&Stream::Port(node, port))?) {
                Target::Exit(_) => Some(None),
                Target::Node(next) => Some(Some(next)),
                Target::Missing => None,
            })
            .collect()
    }

    /// Works out whether a pipeline entering `start` ends, through stages with one input and one output, in sinks,
    /// exits, and forks, and how deeply its forks nest. Iterative, so a long chain cannot exhaust the stack.
    fn terminates(&self, start: usize, base: &[Role], state: &mut [Term]) {
        let mut stack = vec![start];
        while let Some(&node) = stack.last() {
            match state[node] {
                Term::Done(_) => {
                    stack.pop();
                }
                Term::Unknown => {
                    state[node] = Term::Busy;
                    if let Some(next) = self.branch_next(node, base) {
                        for next in next.into_iter().flatten() {
                            if matches!(state[next], Term::Unknown) {
                                stack.push(next);
                            }
                        }
                    }
                }
                Term::Busy => {
                    // Everything pushed after this node has been worked out, or is on a cycle with it.
                    let value = self.branch_next(node, base).and_then(|next| {
                        let mut depth = 0;
                        for next in next.into_iter().flatten() {
                            match state[next] {
                                Term::Done(Some(d)) => depth = depth.max(d),
                                _ => return None,
                            }
                        }
                        if self.is_tee(node) {
                            (depth < MAX_FORK_DEPTH).then_some(depth + 1)
                        } else {
                            Some(depth)
                        }
                    });
                    state[node] = Term::Done(value);
                    stack.pop();
                }
            }
        }
    }

    /// Returns every node's role, with the implicit tees in `demoted` written in the core form.
    fn roles(&self, demoted: &HashSet<usize>) -> Vec<Role> {
        let mut roles = (0..self.nodes.len())
            .map(|node| self.base_role(node))
            .collect::<Vec<_>>();
        let forks = self.forks(&roles);
        for (node, fork) in forks.into_iter().enumerate() {
            if fork {
                roles[node] = Role::Fork;
            }
        }
        // In canonical order, so that the role of the node before a tee is final when the tee is looked at.
        for &node in &self.node_order {
            if !self.is_tee(node)
                || matches!(roles[node], Role::Fork)
                || demoted.contains(&node)
                || self.outputs[node].len() < 2
            {
                continue;
            }
            let Some(uses) = self.outputs[node]
                .iter()
                .map(|port| self.sole(&Stream::Port(node, port.clone())))
                .collect::<Option<Vec<_>>>()
            else {
                continue;
            };
            let &[edge] = self.entering[node].as_slice() else {
                continue;
            };
            let Some(upstream) = self.source(edge) else {
                continue;
            };
            if self.leaving(&upstream) != [edge] || !self.nameable(&roles, &upstream) {
                continue;
            }
            if self.fanout_id(&roles, &upstream).as_ref() == Some(self.id(node)) {
                roles[node] = Role::Implicit { upstream, uses };
            }
        }
        roles
    }

    /// Returns how a reference to a stream is written: the name, and the port, written only for a port of a
    /// multi-output node. Returns `None` for a port no reference can name.
    fn written(&self, roles: &[Role], stream: &Stream) -> Option<(ArtifactNodeId, Option<PortName>)> {
        match stream {
            Stream::Input(name) => ArtifactNodeId::new(name.as_str()).ok().map(|name| (name, None)),
            Stream::Port(node, port) => {
                let id = self.id(*node).clone();
                match &roles[*node] {
                    Role::Multi => Some((id, Some(port.clone()))),
                    Role::Plain(only) if only == port => Some((id, None)),
                    Role::Where { .. } if port.as_str() == TRUE_PORT => Some((id, None)),
                    Role::Tuple if self.outputs[*node].first() == Some(port) => Some((id, None)),
                    _ => None,
                }
            }
        }
    }

    /// Returns whether a stream can be referred to by a `let` or scope input name that lowering resolves to it.
    fn nameable(&self, roles: &[Role], stream: &Stream) -> bool {
        match (stream, self.written(roles, stream)) {
            (_, None) => false,
            (Stream::Input(_), Some(_)) => true,
            (Stream::Port(..), Some((name, _))) => !self.input_names.contains(name.as_str()),
        }
    }

    /// Returns the ID of the implicit tee after a stream.
    fn fanout_id(&self, roles: &[Role], stream: &Stream) -> Option<ArtifactNodeId> {
        match stream {
            Stream::Input(name) => Some(ids::fanout(SCOPE_INPUT_NODE, Some(&PortName::new(name.as_str()).ok()?))),
            Stream::Port(..) => {
                let (name, port) = self.written(roles, stream)?;
                Some(ids::fanout(name.as_str(), port.as_ref()))
            }
        }
    }

    /// Returns the anchor of a pipeline that starts from a stream used once.
    fn anchor(&self, roles: &[Role], stream: &Stream) -> Option<ArtifactNodeId> {
        match stream {
            Stream::Input(name) => Some(ids::input_anchor(name.as_str())),
            Stream::Port(..) => {
                let (name, port) = self.written(roles, stream)?;
                Some(ids::port_anchor(name.as_str(), port.as_ref()))
            }
        }
    }

    /// Returns the node and port a stream leaves, as a sink right after it derives its ID from.
    fn parent(&self, stream: &Stream) -> Option<Parent> {
        match stream {
            Stream::Input(name) => Some((SCOPE_INPUT_NODE.to_string(), PortName::new(name.as_str()).ok()?)),
            Stream::Port(node, port) => Some((self.id(*node).to_string(), port.clone())),
        }
    }

    /// Orders the chains of a plan, or returns the implicit tee to write in the core form because no order puts its
    /// uses in port order.
    fn order(&self, plan: &Plan) -> Result<Vec<usize>, usize> {
        let chains = &plan.chains;
        let mut definer = HashMap::new();
        for (c, chain) in chains.iter().enumerate() {
            if !chain.is_let {
                continue;
            }
            match (chain.stages.last(), &chain.head) {
                (Some(&last), _) => definer.insert(last, c),
                (None, ChainHead::Tuple { merge, .. }) => definer.insert(*merge, c),
                (None, ChainHead::Ref(_)) => None,
            };
        }

        // Arcs between chains: (before, after, the implicit tee whose port order requires it).
        let mut arcs: Vec<(usize, usize, Option<usize>)> = Vec::new();
        let mut tee_uses: BTreeMap<usize, Vec<(usize, usize, usize)>> = BTreeMap::new();
        for (c, chain) in chains.iter().enumerate() {
            for (item, head) in chain.head.refs().iter().enumerate() {
                if let Some(Stream::Port(node, _)) = &head.stream {
                    if let Some(&d) = definer.get(node) {
                        if d != c {
                            arcs.push((d, c, None));
                        }
                    }
                }
                if let Some((tee, port)) = head.via {
                    tee_uses.entry(tee).or_default().push((port, c, item));
                }
            }
        }
        let mut infeasible: Option<usize> = None;
        for (&tee, uses) in &mut tee_uses {
            uses.sort_unstable();
            for pair in uses.windows(2) {
                let ((_, c1, k1), (_, c2, k2)) = (pair[0], pair[1]);
                if c1 != c2 {
                    arcs.push((c1, c2, Some(tee)));
                } else if k1 >= k2 {
                    infeasible = Some(self.later(infeasible, tee));
                }
            }
        }
        if let Some(tee) = infeasible {
            return Err(tee);
        }

        let n = chains.len();
        let mut indegree = vec![0usize; n];
        let mut out = vec![Vec::new(); n];
        for (a, &(from, to, _)) in arcs.iter().enumerate() {
            indegree[to] += 1;
            out[from].push(a);
        }
        let mut ready = (0..n)
            .filter(|&c| indegree[c] == 0)
            .map(|c| Reverse((chains[c].key, c)))
            .collect::<BinaryHeap<_>>();
        let mut done = vec![false; n];
        let mut order = Vec::with_capacity(n);
        while let Some(Reverse((_, c))) = ready.pop() {
            done[c] = true;
            order.push(c);
            for &a in &out[c] {
                let to = arcs[a].1;
                indegree[to] -= 1;
                if indegree[to] == 0 {
                    ready.push(Reverse((chains[to].key, to)));
                }
            }
        }
        if order.len() == n {
            return Ok(order);
        }

        // The rest are on or after a cycle. Every one of them has an arc from another, so walking arcs backwards from
        // any of them finds a cycle; an implicit tee's port order is on it unless the artifact itself has a cycle.
        let mut incoming = vec![Vec::new(); n];
        for (a, &(from, to, _)) in arcs.iter().enumerate() {
            if !done[from] && !done[to] {
                incoming[to].push(a);
            }
        }
        let rest = by_key(chains, (0..n).filter(|&c| !done[c]).collect());
        let mut seen = HashMap::new();
        let mut walked = Vec::new();
        let mut cycle: &[usize] = &[];
        let mut at = rest[0];
        loop {
            if let Some(&start) = seen.get(&at) {
                cycle = &walked[start..];
                break;
            }
            seen.insert(at, walked.len());
            let Some(&a) = incoming[at].first() else {
                break;
            };
            walked.push(a);
            at = arcs[a].0;
        }
        let tee = cycle
            .iter()
            .filter_map(|&a| arcs[a].2)
            .fold(None, |later, tee| Some(self.later(later, tee)));
        match tee {
            Some(tee) => Err(tee),
            None => {
                order.extend(rest);
                Ok(order)
            }
        }
    }

    /// Returns whichever of two nodes comes later in canonical order.
    fn later(&self, a: Option<usize>, b: usize) -> usize {
        match a {
            Some(a) if (self.node_rank[a], a) > (self.node_rank[b], b) => a,
            _ => b,
        }
    }
}

/// Returns positions in `order`, with any position it lacks appended, and the rank of each position.
fn ranks(mut order: Vec<usize>, len: usize) -> (Vec<usize>, Vec<usize>) {
    let mut rank = vec![usize::MAX; len];
    order.retain(|&i| i < len);
    for (r, &i) in order.iter().enumerate() {
        rank[i] = r;
    }
    for (i, r) in rank.iter_mut().enumerate() {
        if *r == usize::MAX {
            *r = order.len();
            order.push(i);
        }
    }
    (order, rank)
}

/// Returns chains in order of their keys.
fn by_key(chains: &[Chain], mut positions: Vec<usize>) -> Vec<usize> {
    positions.sort_by_key(|&c| (chains[c].key, c));
    positions
}

/// Returns whether `params` has exactly the one parameter `name`.
fn only_param(params: &Params, name: &str) -> bool {
    params.iter().count() == 1 && params.get(name).is_some()
}

/// Returns the sink an `emit` or `drop` node is written as, if its parameters are exactly what a sink writes.
fn sink_kind(node: &ArtifactNode) -> Option<SinkKind> {
    match node.op.as_str() {
        names::DROP if node.params.is_empty() => Some(SinkKind::Drop),
        names::EMIT => {
            let mut params = node.params.iter();
            match (params.next(), params.next()) {
                (Some((name, Value::String(output))), None) if name.as_str() == "output" => Some(SinkKind::Output {
                    output: OutputId::new(output.as_str()).ok()?,
                    span: Span::default(),
                }),
                (Some((name, Value::Array(recipients))), None)
                    if name.as_str() == EMIT_TO && !recipients.is_empty() =>
                {
                    let names = recipients
                        .iter()
                        .map(|recipient| recipient.as_str().map(str::to_string))
                        .collect::<Option<Vec<_>>>()?;
                    Some(SinkKind::Recipients {
                        names,
                        span: Span::default(),
                    })
                }
                _ => None,
            }
        }
        _ => None,
    }
}

/// The state of [`Graph::terminates`] for one node: unknown, being worked out, or done, with the depth its forks nest
/// to if a pipeline entering it ends in sinks, exits, and forks.
#[derive(Clone, Copy, Debug)]
enum Term {
    Unknown,
    Busy,
    Done(Option<usize>),
}

/// How a node is written.
#[derive(Clone, Debug)]
enum Role {
    /// A `merge`: a tuple.
    Tuple,
    /// An inline terminal.
    Sink(SinkKind),
    /// A `split` written as `where`, with the `drop` its `false` port leads to.
    Where { drop: usize },
    /// A `tee` written as `fork`.
    Fork,
    /// A `tee` that lowering makes for a stream used once per port, with the edge leaving each port.
    Implicit { upstream: Stream, uses: Vec<usize> },
    /// A node with several output ports: the last stage of a `let`, whose ports references name.
    Multi,
    /// A node with one output port, which pipelines continue from.
    Plain(PortName),
    /// A node without output ports that is not a sink: the last stage of a `let`.
    Final,
}

/// The node ID and port a sink derives its ID from.
type Parent = (String, PortName);

/// A reference at the head of a pipeline, or an item of a tuple.
#[derive(Clone, Debug)]
struct HeadRef {
    name: ArtifactNodeId,
    port: Option<PortName>,
    /// The anchor lowering derives from the reference, used when it heads a statement without `let`.
    anchor: ArtifactNodeId,
    /// The stream it refers to, if it exists.
    stream: Option<Stream>,
    /// The implicit tee and port the reference is a use of, if it is one.
    via: Option<(usize, usize)>,
}

/// How a chain starts.
enum ChainHead {
    Ref(HeadRef),
    Tuple { merge: usize, items: Vec<HeadRef> },
}

impl ChainHead {
    fn refs(&self) -> &[HeadRef] {
        match self {
            Self::Ref(head) => std::slice::from_ref(head),
            Self::Tuple { items, .. } => items,
        }
    }
}

/// How a chain ends, when it ends in a sink, an exit, or a fork.
enum ChainEnd {
    Sink { node: usize, parent: Parent },
    Exit(ExitName),
    Fork(ForkTree),
}

/// A fork's tee and its branches.
struct ForkTree {
    tee: usize,
    branches: Vec<BranchTree>,
}

/// One branch of a fork.
struct BranchTree {
    stages: Vec<usize>,
    end: Option<ChainEnd>,
}

/// A pipeline found in the graph, before it is split into statements.
struct Chain {
    head: ChainHead,
    /// The nodes of its stages, in order.
    stages: Vec<usize>,
    end: Option<ChainEnd>,
    /// Whether its last node (or its tuple's merge, if it has no stages) is a `let`.
    is_let: bool,
    /// The canonical position of the edge it starts from, which orders statements.
    key: usize,
}

/// The roles of the nodes and the chains they make.
struct Plan {
    roles: Vec<Role>,
    chains: Vec<Chain>,
}

/// One use of a named stream: the edge it starts from, its anchor, the parent of a sink right after it, and the
/// implicit tee port it comes from, if any.
struct Use {
    edge: usize,
    anchor: ArtifactNodeId,
    parent: Parent,
    via: Option<(usize, usize)>,
}

/// Where a chain starts following the graph.
enum Start {
    Edge(usize),
    /// A node no edge enters.
    Node(usize),
}

/// Finds the chains of the graph.
struct Builder<'g, 'a> {
    graph: &'g Graph<'a>,
    roles: Vec<Role>,
    placed: Vec<bool>,
}

impl<'g, 'a> Builder<'g, 'a> {
    fn new(graph: &'g Graph<'a>, demoted: &HashSet<usize>) -> Self {
        Self {
            graph,
            roles: graph.roles(demoted),
            placed: vec![false; graph.nodes.len()],
        }
    }

    /// Returns whether a pipeline can continue into what the edge enters.
    fn enterable(&self, edge: usize) -> bool {
        match self.graph.target(edge) {
            Target::Exit(_) => true,
            Target::Node(node) => !matches!(self.roles[node], Role::Tuple | Role::Implicit { .. }),
            Target::Missing => false,
        }
    }

    /// Returns whether a stream is written as a reference, rather than continuing the pipeline it is on.
    fn named(&self, stream: &Stream) -> bool {
        match stream {
            Stream::Input(_) => true,
            Stream::Port(node, port) => match &self.roles[*node] {
                Role::Multi => true,
                Role::Plain(only) if only != port => false,
                Role::Where { .. } if port.as_str() != TRUE_PORT => false,
                Role::Tuple if self.graph.outputs[*node].first() != Some(port) => false,
                Role::Plain(_) | Role::Where { .. } | Role::Tuple => match self.graph.leaving(stream) {
                    [] => false,
                    [edge] => !self.enterable(*edge),
                    _ => true,
                },
                Role::Sink(_) | Role::Fork | Role::Implicit { .. } | Role::Final => false,
            },
        }
    }

    /// Returns the uses of a named stream: one per port of its implicit tee, or one per edge leaving it.
    fn uses(&self, stream: &Stream) -> Vec<Use> {
        let graph = self.graph;
        if let Some(edge) = graph.sole(stream) {
            if let Target::Node(tee) = graph.target(edge) {
                if let Role::Implicit { uses, .. } = &self.roles[tee] {
                    let id = graph.id(tee);
                    return uses
                        .iter()
                        .enumerate()
                        .map(|(port, &edge)| Use {
                            edge,
                            anchor: ids::tee_port(id.as_str(), port),
                            parent: (id.to_string(), graph.outputs[tee][port].clone()),
                            via: Some((tee, port)),
                        })
                        .collect();
                }
            }
        }
        let (Some(anchor), Some(parent)) = (graph.anchor(&self.roles, stream), graph.parent(stream)) else {
            return Vec::new();
        };
        graph
            .leaving(stream)
            .iter()
            .map(|&edge| Use {
                edge,
                anchor: anchor.clone(),
                parent: parent.clone(),
                via: None,
            })
            .collect()
    }

    /// Returns the reference for the port an edge leaves, as a tuple item or the head of a pipeline that starts there,
    /// with the parent a sink right after it derives its ID from.
    fn reference(&self, edge: usize) -> Option<(HeadRef, Parent)> {
        let graph = self.graph;
        match graph.source(edge) {
            Some(Stream::Port(tee, port)) if matches!(self.roles[tee], Role::Implicit { .. }) => {
                let Role::Implicit { upstream, uses } = &self.roles[tee] else {
                    return None;
                };
                let index = uses.iter().position(|&e| e == edge)?;
                let (name, written) = graph.written(&self.roles, upstream)?;
                let id = graph.id(tee);
                let head = HeadRef {
                    name,
                    port: written,
                    anchor: ids::tee_port(id.as_str(), index),
                    stream: Some(upstream.clone()),
                    via: Some((tee, index)),
                };
                Some((head, (id.to_string(), port)))
            }
            Some(stream) => {
                let parent = graph.parent(&stream)?;
                let (name, port) = match graph.written(&self.roles, &stream) {
                    Some(written) => written,
                    // A port no reference names, such as one of a fork's: written as it is, which lowering reports.
                    None => match &stream {
                        Stream::Port(node, port) => (graph.id(*node).clone(), Some(port.clone())),
                        Stream::Input(_) => return None,
                    },
                };
                let anchor = ids::port_anchor(name.as_str(), port.as_ref());
                let anchor = graph.anchor(&self.roles, &stream).unwrap_or(anchor);
                let head = HeadRef {
                    name,
                    port,
                    anchor,
                    stream: Some(stream),
                    via: None,
                };
                Some((head, parent))
            }
            None => match &graph.edges[edge].from {
                // An edge from a node the artifact does not have: a reference to it, which lowering reports.
                ArtifactPortRef::Node { node, port } => {
                    let head = HeadRef {
                        name: node.clone(),
                        port: Some(port.clone()),
                        anchor: ids::port_anchor(node.as_str(), Some(port)),
                        stream: None,
                        via: None,
                    };
                    Some((head, (node.to_string(), port.clone())))
                }
                ArtifactPortRef::Input(_) | ArtifactPortRef::Exit(_) => None,
            },
        }
    }

    fn build(mut self) -> Plan {
        let graph = self.graph;
        let mut chains = Vec::new();

        // A pipeline for each use of each named stream, except tuple items.
        let mut seen = HashSet::new();
        for &edge in &graph.edge_order {
            let Some(stream) = graph.source(edge) else {
                continue;
            };
            if !self.named(&stream) || seen.contains(&stream) {
                continue;
            }
            seen.insert(stream.clone());
            let Some((name, port)) = graph.written(&self.roles, &stream) else {
                continue;
            };
            for use_ in self.uses(&stream) {
                if let Some((tee, _)) = use_.via {
                    self.placed[tee] = true;
                }
                if let Target::Node(target) = graph.target(use_.edge) {
                    if matches!(self.roles[target], Role::Tuple) {
                        continue;
                    }
                }
                let head = HeadRef {
                    name: name.clone(),
                    port: port.clone(),
                    anchor: use_.anchor,
                    stream: Some(stream.clone()),
                    via: use_.via,
                };
                let (stages, end, is_let) = self.follow(Start::Edge(use_.edge), use_.parent);
                chains.push(Chain {
                    head: ChainHead::Ref(head),
                    stages,
                    end,
                    is_let,
                    key: graph.edge_rank[use_.edge],
                });
            }
        }

        // A pipeline for each tuple.
        for &merge in &graph.node_order {
            if self.placed[merge] || !matches!(self.roles[merge], Role::Tuple) {
                continue;
            }
            self.placed[merge] = true;
            let mut entering = graph.entering[merge].clone();
            entering.sort_by_key(|&edge| {
                let port = graph.to_port(edge);
                let index = port.and_then(PortName::as_index);
                (index.is_none(), index, port.cloned(), graph.edge_rank[edge])
            });
            entering.dedup_by_key(|edge| graph.to_port(*edge).cloned());
            let items = entering
                .iter()
                .filter_map(|&edge| self.reference(edge).map(|(head, _)| head))
                .collect();
            let key = entering
                .iter()
                .map(|&edge| graph.edge_rank[edge])
                .max()
                .unwrap_or(usize::MAX);
            let out = graph.outputs[merge]
                .first()
                .cloned()
                .or_else(|| PortName::new(OUT).ok());
            let (stages, end, is_let) = match out {
                Some(out) => {
                    let stream = Stream::Port(merge, out.clone());
                    let parent = (graph.id(merge).to_string(), out);
                    if self.named(&stream) {
                        (Vec::new(), None, true)
                    } else {
                        match graph.leaving(&stream).first() {
                            Some(&edge) => self.follow(Start::Edge(edge), parent),
                            None => (Vec::new(), None, false),
                        }
                    }
                }
                None => (Vec::new(), None, false),
            };
            chains.push(Chain {
                head: ChainHead::Tuple { merge, items },
                stages,
                end,
                is_let,
                key,
            });
        }

        // A pipeline for each node no scope input reaches, from a reference to what feeds it, or to itself.
        for &node in &graph.node_order {
            if self.placed[node] {
                continue;
            }
            let id = graph.id(node).clone();
            let fed = graph.entering[node]
                .first()
                .and_then(|&edge| self.reference(edge).map(|found| (found, edge)));
            let (head, start, parent, key) = match fed {
                Some(((head, parent), edge)) => (head, Start::Edge(edge), parent, graph.edge_rank[edge]),
                None => {
                    let Ok(out) = PortName::new(OUT) else {
                        continue;
                    };
                    let head = HeadRef {
                        name: id.clone(),
                        port: None,
                        anchor: id.clone(),
                        stream: None,
                        via: None,
                    };
                    (head, Start::Node(node), (id.to_string(), out), usize::MAX)
                }
            };
            let (stages, end, is_let) = self.follow(start, parent);
            if stages.is_empty() && end.is_none() {
                continue;
            }
            chains.push(Chain {
                head: ChainHead::Ref(head),
                stages,
                end,
                is_let,
                key,
            });
        }

        Plan {
            roles: self.roles,
            chains,
        }
    }

    /// Follows a pipeline from `start`, whose stream leaves `parent`: returns its stages, its end, and whether its last
    /// stage is a `let`.
    fn follow(&mut self, start: Start, mut parent: Parent) -> (Vec<usize>, Option<ChainEnd>, bool) {
        let graph = self.graph;
        let mut stages = Vec::new();
        let mut target = match start {
            Start::Edge(edge) => graph.target(edge),
            Start::Node(node) => Target::Node(node),
        };
        loop {
            let node = match target {
                Target::Exit(name) => return (stages, Some(ChainEnd::Exit(name)), false),
                Target::Missing => return (stages, None, false),
                Target::Node(node) => node,
            };
            if self.placed[node] {
                return (stages, None, false);
            }
            let port = match &self.roles[node] {
                Role::Tuple | Role::Implicit { .. } => return (stages, None, false),
                Role::Sink(_) => {
                    self.placed[node] = true;
                    return (stages, Some(ChainEnd::Sink { node, parent }), false);
                }
                Role::Fork => {
                    let tree = self.fork(node);
                    return (stages, Some(ChainEnd::Fork(tree)), false);
                }
                Role::Multi | Role::Final => {
                    self.placed[node] = true;
                    stages.push(node);
                    return (stages, None, true);
                }
                Role::Where { drop } => {
                    self.placed[node] = true;
                    self.placed[*drop] = true;
                    stages.push(node);
                    match PortName::new(TRUE_PORT) {
                        Ok(port) => port,
                        Err(_) => return (stages, None, false),
                    }
                }
                Role::Plain(port) => {
                    self.placed[node] = true;
                    stages.push(node);
                    port.clone()
                }
            };
            let stream = Stream::Port(node, port.clone());
            parent = (graph.id(node).to_string(), port);
            if self.named(&stream) {
                return (stages, None, true);
            }
            target = match graph.leaving(&stream).first() {
                Some(&edge) => graph.target(edge),
                None => return (stages, None, false),
            };
        }
    }

    /// Follows the branches of a fork. Forks nest at most [`MAX_FORK_DEPTH`] deep, which bounds the recursion.
    fn fork(&mut self, tee: usize) -> ForkTree {
        let graph = self.graph;
        self.placed[tee] = true;
        let branches = graph.outputs[tee]
            .iter()
            .map(|port| {
                let parent = (graph.id(tee).to_string(), port.clone());
                match graph.leaving(&Stream::Port(tee, port.clone())).first() {
                    Some(&edge) => {
                        let (stages, end, _) = self.follow(Start::Edge(edge), parent);
                        BranchTree { stages, end }
                    }
                    None => BranchTree {
                        stages: Vec::new(),
                        end: None,
                    },
                }
            })
            .collect();
        ForkTree { tee, branches }
    }
}

/// What a statement is: a `let` with its name, or a statement without one.
enum Ctx {
    Let(ArtifactNodeId),
    Bare,
}

/// The head of one statement of a chain: the chain's own, or a reference to the `let` before it.
#[derive(Clone, Copy)]
enum SegHead<'c> {
    Chain(&'c ChainHead),
    After(usize),
}

/// Writes chains as statements.
struct Emitter<'p, 'g, 'a> {
    graph: &'p Graph<'a>,
    roles: &'g [Role],
}

impl Emitter<'_, '_, '_> {
    /// Returns whether a chain can be split into a `let` after its stage `j`.
    fn cuttable(&self, chain: &Chain, j: usize) -> bool {
        matches!(self.roles[chain.stages[j]], Role::Plain(_) | Role::Where { .. })
            && (j + 1 < chain.stages.len() || (!chain.is_let && chain.end.is_some()))
    }

    /// Writes a chain as one or more statements, splitting it where a person evidently named an intermediate stream.
    fn chain(&self, chain: &Chain) -> Vec<Statement> {
        let graph = self.graph;
        let stages = &chain.stages;
        let m = stages.len();
        let ctx = match (chain.is_let, stages.last(), &chain.head) {
            (false, _, _) => Ctx::Bare,
            (true, Some(&last), _) => Ctx::Let(graph.id(last).clone()),
            (true, None, ChainHead::Tuple { merge, .. }) => Ctx::Let(graph.id(*merge).clone()),
            // A reference alone names nothing new.
            (true, None, ChainHead::Ref(_)) => return Vec::new(),
        };
        let head_at = |i: usize| {
            if i == 0 {
                SegHead::Chain(&chain.head)
            } else {
                SegHead::After(stages[i - 1])
            }
        };

        // reach[k]: the fewest `let`s, each with every ID derived, that write the first k stages, and where the last
        // of them starts.
        let mut reach: Vec<Option<(usize, usize)>> = vec![None; m + 1];
        reach[0] = Some((0, 0));
        for j in 0..m {
            if !self.cuttable(chain, j) {
                continue;
            }
            let name = graph.id(stages[j]);
            if graph.input_names.contains(name.as_str()) {
                continue;
            }
            for i in (0..=j).rev() {
                let Some((count, _)) = reach[i] else {
                    continue;
                };
                let tuple = i == 0 && matches!(chain.head, ChainHead::Tuple { .. });
                if j - i + 1 + usize::from(tuple) < 2 {
                    continue;
                }
                // The first stage of a run has ordinal 1, which rules most runs out cheaply.
                if i < j {
                    let first = stages[i];
                    if stage_id(name.as_str(), graph.nodes[first].op.as_str(), 1).as_ref() != Some(graph.id(first)) {
                        continue;
                    }
                }
                let ctx = Ctx::Let(name.clone());
                if !self.decide(&ctx, head_at(i), &stages[i..=j], None).is_empty() {
                    continue;
                }
                if reach[j + 1].is_none_or(|(best, _)| count + 1 < best) {
                    reach[j + 1] = Some((count + 1, i));
                }
            }
        }

        // The last statement: the split with the fewest `as` clauses, then the fewest statements.
        let mut best: Option<((usize, usize), usize, HashSet<usize>)> = None;
        for (c, found) in reach.iter().enumerate() {
            let Some((count, _)) = *found else {
                continue;
            };
            let aliased = self.decide(&ctx, head_at(c), &stages[c..], chain.end.as_ref());
            let cost = (aliased.len(), count);
            if best.as_ref().is_none_or(|(least, _, _)| cost < *least) {
                best = Some((cost, c, aliased));
            }
        }
        let Some((_, c, aliased)) = best else {
            return Vec::new();
        };

        let mut runs = Vec::new();
        let mut k = c;
        while k > 0 {
            let Some((_, i)) = reach[k] else {
                break;
            };
            runs.push((i, k - 1));
            k = i;
        }
        runs.reverse();

        let mut statements = runs
            .into_iter()
            .map(|(i, j)| {
                let ctx = Ctx::Let(graph.id(stages[j]).clone());
                self.statement(&ctx, head_at(i), &stages[i..=j], None, &HashSet::new())
            })
            .collect::<Vec<_>>();
        statements.push(self.statement(&ctx, head_at(c), &stages[c..], chain.end.as_ref(), &aliased));
        statements
    }

    /// Returns the nodes of one statement whose IDs lowering would not derive, and which therefore need an `as`.
    fn decide(&self, ctx: &Ctx, head: SegHead<'_>, stages: &[usize], end: Option<&ChainEnd>) -> HashSet<usize> {
        let graph = self.graph;
        let mut aliased = HashSet::new();
        let (anchor, let_name) = match (ctx, head) {
            (Ctx::Let(name), SegHead::Chain(ChainHead::Tuple { merge, .. })) => {
                if !stages.is_empty() && &ids::merge(name.as_str()) != graph.id(*merge) {
                    aliased.insert(*merge);
                }
                (name.clone(), Some(name))
            }
            (Ctx::Let(name), _) => (name.clone(), Some(name)),
            (Ctx::Bare, SegHead::Chain(ChainHead::Ref(head))) => (head.anchor.clone(), None),
            (Ctx::Bare, SegHead::Chain(ChainHead::Tuple { merge, items })) => {
                let id = graph.id(*merge);
                if items.first().map(|item| ids::merge(item.anchor.as_str())).as_ref() != Some(id) {
                    aliased.insert(*merge);
                }
                (id.clone(), None)
            }
            (Ctx::Bare, SegHead::After(node)) => (graph.id(node).clone(), None),
        };
        self.decide_stages(&anchor, stages, let_name, &mut aliased);
        if let Some(end) = end {
            self.decide_end(&anchor, end, &mut aliased);
        }
        aliased
    }

    /// Adds the stages whose IDs are not derived from `anchor` and their ordinal to `aliased`. The last stage of a
    /// `let` is the `let`'s node, and is never aliased.
    fn decide_stages(
        &self, anchor: &ArtifactNodeId, stages: &[usize], let_name: Option<&ArtifactNodeId>,
        aliased: &mut HashSet<usize>,
    ) {
        let mut ordinals = ids::Ordinals::default();
        for (k, &stage) in stages.iter().enumerate() {
            let node = &self.graph.nodes[stage];
            let ordinal = ordinals.next(node.op.as_str());
            if let_name.is_some() && k + 1 == stages.len() {
                continue;
            }
            if stage_id(anchor.as_str(), node.op.as_str(), ordinal).as_ref() != Some(&node.id) {
                aliased.insert(stage);
            }
        }
    }

    fn decide_end(&self, anchor: &ArtifactNodeId, end: &ChainEnd, aliased: &mut HashSet<usize>) {
        let graph = self.graph;
        match end {
            ChainEnd::Sink { node, parent } => {
                if &ids::terminal(&parent.0, &parent.1) != graph.id(*node) {
                    aliased.insert(*node);
                }
            }
            ChainEnd::Exit(_) => {}
            ChainEnd::Fork(tree) => {
                let id = graph.id(tree.tee);
                if &ids::fork(anchor.as_str()) != id {
                    aliased.insert(tree.tee);
                }
                for (i, branch) in tree.branches.iter().enumerate() {
                    let anchor = ids::tee_port(id.as_str(), i);
                    self.decide_stages(&anchor, &branch.stages, None, aliased);
                    if let Some(end) = &branch.end {
                        self.decide_end(&anchor, end, aliased);
                    }
                }
            }
        }
    }

    /// Writes one statement.
    fn statement(
        &self, ctx: &Ctx, head: SegHead<'_>, stages: &[usize], end: Option<&ChainEnd>, aliased: &HashSet<usize>,
    ) -> Statement {
        let graph = self.graph;
        let head = match head {
            SegHead::Chain(ChainHead::Ref(head)) => Head::Ref(reference(&head.name, head.port.as_ref())),
            SegHead::Chain(ChainHead::Tuple { merge, items }) => Head::Tuple(Tuple {
                items: items
                    .iter()
                    .map(|item| reference(&item.name, item.port.as_ref()))
                    .collect(),
                id: self.alias(*merge, aliased),
                span: Span::default(),
            }),
            SegHead::After(node) => Head::Ref(reference(graph.id(node), None)),
        };
        let stages = stages.iter().map(|&stage| self.stage(stage, aliased)).collect();
        match ctx {
            Ctx::Let(name) => Statement::Let {
                name: name_of(name),
                pipeline: Pipeline {
                    head,
                    stages,
                    end: None,
                    span: Span::default(),
                },
                span: Span::default(),
            },
            Ctx::Bare => Statement::Run {
                pipeline: Pipeline {
                    head,
                    stages,
                    end: end.map(|end| self.end(end, aliased)),
                    span: Span::default(),
                },
                span: Span::default(),
            },
        }
    }

    fn alias(&self, node: usize, aliased: &HashSet<usize>) -> Option<Name> {
        aliased.contains(&node).then(|| name_of(self.graph.id(node)))
    }

    fn stage(&self, stage: usize, aliased: &HashSet<usize>) -> Stage {
        let node = &self.graph.nodes[stage];
        let id = self.alias(stage, aliased);
        if let Role::Where { .. } = self.roles[stage] {
            return Stage::Where(WhereStage {
                predicate: node.params.get("predicate").cloned().unwrap_or(Value::Null),
                param_spans: Vec::new(),
                id,
                span: Span::default(),
            });
        }
        let (params, positional) = written_params(param_specs(node.op.as_str()), &node.params);
        Stage::Op(OpStage {
            op: node.op.clone(),
            op_span: Span::default(),
            params,
            positional,
            param_spans: Vec::new(),
            id,
            span: Span::default(),
        })
    }

    fn end(&self, end: &ChainEnd, aliased: &HashSet<usize>) -> End {
        match end {
            ChainEnd::Sink { node, .. } => End::Sink(Sink {
                kind: match &self.roles[*node] {
                    Role::Sink(kind) => kind.clone(),
                    _ => SinkKind::Drop,
                },
                id: self.alias(*node, aliased),
                span: Span::default(),
            }),
            ChainEnd::Exit(name) => End::Sink(Sink {
                kind: SinkKind::Exit {
                    name: name.clone(),
                    span: Span::default(),
                },
                id: None,
                span: Span::default(),
            }),
            ChainEnd::Fork(tree) => End::Fork(Fork {
                id: self.alias(tree.tee, aliased),
                branches: tree
                    .branches
                    .iter()
                    .map(|branch| Branch {
                        stages: branch.stages.iter().map(|&stage| self.stage(stage, aliased)).collect(),
                        // A branch always ends for a fork the graph has; the fallback keeps resugaring total.
                        end: match &branch.end {
                            Some(end) => self.end(end, aliased),
                            None => End::Sink(Sink {
                                kind: SinkKind::Drop,
                                id: None,
                                span: Span::default(),
                            }),
                        },
                        span: Span::default(),
                    })
                    .collect(),
                span: Span::default(),
            }),
        }
    }
}

/// Returns [`ids::stage`], or `None` where its suffix, the operator and ordinal, would be longer than an identifier
/// may be, which no lowered ID can equal.
fn stage_id(anchor: &str, op: &str, ordinal: usize) -> Option<ArtifactNodeId> {
    if ordinal > 1 && PortName::new(format!("{}_{}", op, ordinal)).is_err() {
        return None;
    }
    Some(ids::stage(anchor, op, ordinal))
}

fn name_of(id: &ArtifactNodeId) -> Name {
    Name {
        id: id.clone(),
        span: Span::default(),
    }
}

fn reference(name: &ArtifactNodeId, port: Option<&PortName>) -> Ref {
    Ref {
        name: name_of(name),
        port: port.map(|port| (port.clone(), Span::default())),
        span: Span::default(),
    }
}

/// Returns the parameters a stage writes, leaving out those equal to their registry default, and the positional
/// parameter, if the operator declares one and it is written.
fn written_params(specs: Option<&[ParamSpec]>, params: &Params) -> (Params, Option<ParamName>) {
    let spec = |name: &str| specs.and_then(|specs| specs.iter().find(|spec| spec.name == name));
    let written = params
        .iter()
        .filter(|(name, value)| {
            let default = spec(name.as_str())
                .and_then(|spec| spec.default)
                .and_then(|json| serde_json::from_str::<Value>(json).ok());
            default.as_ref() != Some(*value)
        })
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect::<Params>();
    let positional = specs
        .and_then(|specs| specs.iter().find(|spec| spec.positional))
        .and_then(|spec| written.iter().find(|(name, _)| name.as_str() == spec.name))
        .map(|(name, _)| name.clone());
    (written, positional)
}

#[cfg(test)]
mod tests;
