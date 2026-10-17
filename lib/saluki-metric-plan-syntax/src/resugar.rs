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
//!    - a `merge` is a **tuple**, always, since a tuple is the core form of a merge;
//!    - an `emit` or `drop` whose parameters a sink can write is a **sink**;
//!    - a `split` whose `false` port leads to a `drop` with the derived ID [`ids::terminal`]`(split, "false")` is a
//!      **`where`**;
//!    - a `tee` with the derived ID [`ids::fanout`] of the stream before it, when that stream can be named, is an
//!      **implicit tee**: the stream is used once per port, in port order;
//!    - any other `tee` with the ID `<name>_fanout`, for a `<name>` that no node, scope input, or other `let` has, after
//!      a stream a reference can name (a port of an implicit tee or another alias included), is an **alias**: the
//!      statement `let <name> = <stream>`, one use of the stream, then `<name>` used once per port, in port order;
//!    - anything else is a **multi-output** node (a `let` of its own, whose ports are written `x.p`, as any other
//!      `tee` is: `let t = x | tee(ports: 2)`, then `t.0 | ...` and `t.1 | ...`), a **plain** node with one output
//!      port, or a **final** node without one (a `let` of its own too, since lowering exempts such a `let` from being
//!      used).
//! 2. **Streams.** A port is **named** when it must be written as a reference: a scope input, a port of a
//!    multi-output node, or the output of a node that is used more than once (through an implicit tee) or feeds a
//!    tuple. Every other port continues its pipeline into the one node it feeds.
//! 3. **Chains.** Each use of a named stream starts a pipeline with a reference to it, and each tuple starts a pipeline
//!    with the tuple. A pipeline follows its stream through plain nodes and `where`s until it reaches a sink or an exit
//!    (a statement without `let`), or a named stream, multi-output node, or final node (a `let` named for its last
//!    node). Nodes that no scope input reaches start pipelines of their own, from a reference to whatever feeds them,
//!    so that lowering reports the problem.
//! 4. **Names.** A node whose ID is not the one lowering would derive where it is written is named by a `let`
//!    wherever one can end at it: the pipeline is split into several `let`s, each named for its last node, the merge
//!    of a tuple included (`let m = (a, b)`). Among the ways to split it in which no `let` but the last writes an `as`
//!    that a `let` could replace, the one with the fewest `as` clauses wins, then the one with the fewest statements.
//! 5. **`as`.** `as` is the last resort, for the nodes no `let` can name: a sink whose ID is not [`ids::terminal`] of
//!    the node and port that feed it, and a node named like a scope input that the artifact reads, which a reference
//!    to the input would name instead (see `let_names` in [`Graph`]). Whether a stage needs one depends on the ID
//!    lowering derives at its position: from the statement's anchor and the stage's ordinal (every stage of a
//!    pipeline counts, a `where` as `split`), or the `let`'s name for its last stage.
//! 6. **Order.** Statements are ordered by the canonical position of the edge each starts from, subject to every
//!    `let` coming before its uses, and to the uses of each implicit tee and alias coming in port order (lowering
//!    numbers them in source order). When no order satisfies both, the tee involved is written in the core form
//!    instead, and the program is resugared again.
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
    ast::{Base, Head, Name, OpStage, Pipeline, Program, Ref, Sink, SinkKind, Stage, Statement, Tuple, WhereStage},
    diagnostic::Span,
    graph::{canonical_order, param_specs, NodeShape},
    ids, PrintOptions,
};

/// The words that the parser reads as the start of a stage, not as a reference, where a pipeline starts and neither
/// `|` nor `.` follows: a stream named one of them can head an alias, `let x = <name>`, only with a port.
const STAGE_HEADS: [&str; 6] = ["where", "emit", "drop", "exit", "fork", "as"];

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
    /// The name of the `let` each node ends, if it ends one: its ID, or for a node whose ID is the name of a scope input
    /// that edges leave, which no `let` can take since references to the input would name it, a name of its own,
    /// `<id>_let`, numbered if another name has it. The `let`'s last stage then has an `as` with the node's ID.
    let_names: Vec<ArtifactNodeId>,
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
        let mut taken = nodes
            .iter()
            .map(|node| node.id.to_string())
            .chain(input_names.iter().map(|name| name.to_string()))
            .collect::<HashSet<_>>();
        let let_names = nodes
            .iter()
            .map(|node| {
                if !input_names.contains(node.id.as_str()) {
                    return node.id.clone();
                }
                let first = ids::join(node.id.as_str(), "let");
                (1..)
                    .map(|k| {
                        if k == 1 {
                            first.clone()
                        } else {
                            ids::join(first.as_str(), &k.to_string())
                        }
                    })
                    .find(|name| taken.insert(name.to_string()))
                    .expect("the names are endless")
            })
            .collect();
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
            let_names,
        }
    }

    fn id(&self, node: usize) -> &ArtifactNodeId {
        &self.nodes[node].id
    }

    /// Returns the name a reference to a stream writes: the name of the `let` that ends at its node, or of the alias
    /// its node is written as, or of the scope input.
    fn spelled(&self, roles: &[Role], head: &HeadRef) -> ArtifactNodeId {
        match &head.stream {
            Some(Stream::Port(node, _)) => match &roles[*node] {
                Role::Alias { name, .. } => name.clone(),
                _ => self.let_names[*node].clone(),
            },
            _ => head.name.clone(),
        }
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

    /// Returns the role a node has before implicit tees are recognized.
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
            // A pipeline continues from a node whose ports are unknown at its conventional output port, `out`, so
            // only that port continues it; any other port it has is written as a reference to the port.
            [port]
                if self.shapes[node].outputs().is_some()
                    || self.shapes[node].default_output().as_ref() == Some(port) =>
            {
                Role::Plain(port.clone())
            }
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

    /// Returns every node's role, with the implicit tees in `demoted` written in the core form.
    fn roles(&self, demoted: &HashSet<usize>) -> Vec<Role> {
        let mut roles = (0..self.nodes.len())
            .map(|node| self.base_role(node))
            .collect::<Vec<_>>();
        // In canonical order, so that the role of the node before a tee is final when the tee is looked at.
        for &node in &self.node_order {
            if !self.is_tee(node) || demoted.contains(&node) || self.outputs[node].len() < 2 {
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
            if self.leaving(&upstream) != [edge] {
                continue;
            }
            if self.nameable(&roles, &upstream) && self.fanout_id(&roles, &upstream).as_ref() == Some(self.id(node)) {
                roles[node] = Role::Implicit { upstream, uses };
            } else if let Some(name) = self.alias_name(&roles, node, &upstream) {
                roles[node] = Role::Alias { name, uses };
            }
        }
        roles
    }

    /// Returns the name of the alias a `tee` is written as, `let <name> = <upstream>` used once per port, if it can be
    /// one: its ID is `<name>_fanout`, as lowering derives it for the alias, `<name>` is free (no node, scope input, or
    /// other `let` has it), and a reference can name the stream it copies.
    fn alias_name(&self, roles: &[Role], tee: usize, upstream: &Stream) -> Option<ArtifactNodeId> {
        if !self.unique[tee] {
            return None;
        }
        let name = ArtifactNodeId::new(self.id(tee).as_str().strip_suffix("_fanout")?).ok()?;
        if ids::fanout(name.as_str(), None) != *self.id(tee)
            || self.index.contains_key(name.as_str())
            || self.input_names.contains(name.as_str())
            || self.let_names.contains(&name)
        {
            return None;
        }
        // The reference the alias is written with: a port of a stream used several times, or of another alias, is
        // named through it.
        let (word, port) = match upstream {
            Stream::Input(input) => (input.to_string(), false),
            Stream::Port(node, _) => match &roles[*node] {
                Role::Implicit { upstream, .. } => match upstream {
                    Stream::Input(input) => (input.to_string(), false),
                    Stream::Port(node, _) => {
                        let (_, port) = self.written(roles, upstream)?;
                        (self.let_names[*node].to_string(), port.is_some())
                    }
                },
                Role::Alias { name, .. } => (name.to_string(), false),
                _ if self.nameable(roles, upstream) => {
                    let (_, port) = self.written(roles, upstream)?;
                    (self.let_names[*node].to_string(), port.is_some())
                }
                _ => return None,
            },
        };
        // A reference alone that is a word starting a stage would be read as the stage.
        (port || !STAGE_HEADS.contains(&word.as_str())).then_some(name)
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
            if let Some(alias) = chain.alias {
                definer.insert(alias, c);
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
        // By the tee's canonical position, so that the arcs, and the cycle found among them, depend only on the graph.
        let mut tee_uses: BTreeMap<(usize, usize), Vec<TeeUse>> = BTreeMap::new();
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
                    tee_uses
                        .entry((self.node_rank[tee], tee))
                        .or_default()
                        .push((port, c, item));
                }
            }
        }
        let mut infeasible: Option<usize> = None;
        for (&(_, tee), uses) in &mut tee_uses {
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

/// One use of an implicit tee: the tee's port, and the chain and item of its head that use it.
type TeeUse = (usize, usize, usize);

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

/// How a node is written.
#[derive(Clone, Debug)]
enum Role {
    /// A `merge`: a tuple.
    Tuple,
    /// An inline terminal.
    Sink(SinkKind),
    /// A `split` written as `where`, with the `drop` its `false` port leads to.
    Where { drop: usize },
    /// A `tee` that lowering makes for a stream used once per port, with the edge leaving each port.
    Implicit { upstream: Stream, uses: Vec<usize> },
    /// A `tee` that lowering makes for an alias, `let <name> = <stream>`, used once per port, with the edge leaving
    /// each port.
    Alias { name: ArtifactNodeId, uses: Vec<usize> },
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

/// How a chain ends, when it ends in a sink or an exit.
enum ChainEnd {
    Sink { node: usize, parent: Parent },
    Exit(ExitName),
}

/// A pipeline found in the graph, before it is split into statements.
struct Chain {
    head: ChainHead,
    /// The nodes of its stages, in order.
    stages: Vec<usize>,
    end: Option<ChainEnd>,
    /// Whether its last node (or the merge of its tuple, if it has no stages) is a `let`.
    is_let: bool,
    /// The `tee` of the alias the chain is, `let <name> = <head>`, if it is one.
    alias: Option<usize>,
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
            Target::Node(node) => !matches!(
                self.roles[node],
                Role::Tuple | Role::Implicit { .. } | Role::Alias { .. }
            ),
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
                Role::Sink(_) | Role::Implicit { .. } | Role::Alias { .. } | Role::Final => false,
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
            Some(Stream::Port(tee, port)) if matches!(self.roles[tee], Role::Alias { .. }) => {
                let Role::Alias { name, uses } = &self.roles[tee] else {
                    return None;
                };
                let index = uses.iter().position(|&e| e == edge)?;
                let id = graph.id(tee);
                let head = HeadRef {
                    name: name.clone(),
                    port: None,
                    anchor: ids::tee_port(id.as_str(), index),
                    stream: Some(Stream::Port(tee, port.clone())),
                    via: Some((tee, index)),
                };
                Some((head, (id.to_string(), port)))
            }
            Some(stream) => {
                let parent = graph.parent(&stream)?;
                let (name, port) = match graph.written(&self.roles, &stream) {
                    Some(written) => written,
                    // A port no reference names, such as the `false` port of a `where`: written as it is, which
                    // lowering reports.
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

        // A pipeline for each use of each named stream and alias, except tuple items.
        let mut seen = HashSet::new();
        let mut aliases = HashSet::new();
        for &edge in &graph.edge_order {
            let Some(stream) = graph.source(edge) else {
                continue;
            };
            if let Stream::Port(tee, _) = &stream {
                if let Role::Alias { uses, .. } = &self.roles[*tee] {
                    if aliases.insert(*tee) {
                        for edge in uses.clone() {
                            if let Some((head, parent)) = self.reference(edge) {
                                self.push_use(&mut chains, head, edge, parent);
                            }
                        }
                    }
                    continue;
                }
            }
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
                let head = HeadRef {
                    name: name.clone(),
                    port: port.clone(),
                    anchor: use_.anchor,
                    stream: Some(stream.clone()),
                    via: use_.via,
                };
                self.push_use(&mut chains, head, use_.edge, use_.parent);
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
                alias: None,
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
            if let (Role::Alias { .. }, Some(((head, _), edge))) = (&self.roles[node], &fed) {
                self.placed[node] = true;
                chains.push(Chain {
                    head: ChainHead::Ref(head.clone()),
                    stages: Vec::new(),
                    end: None,
                    is_let: true,
                    alias: Some(node),
                    key: graph.edge_rank[*edge],
                });
                continue;
            }
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
                alias: None,
                key,
            });
        }

        Plan {
            roles: self.roles,
            chains,
        }
    }

    /// Adds the chain of one use of a stream, which starts from `edge` with `head`, unless it is an item of a tuple,
    /// which the chain of the tuple writes: the alias the edge enters, if it enters one, or the pipeline that follows it.
    fn push_use(&mut self, chains: &mut Vec<Chain>, head: HeadRef, edge: usize, parent: Parent) {
        let graph = self.graph;
        let key = graph.edge_rank[edge];
        if let Target::Node(target) = graph.target(edge) {
            match self.roles[target] {
                Role::Tuple => return,
                Role::Alias { .. } if !self.placed[target] => {
                    self.placed[target] = true;
                    chains.push(Chain {
                        head: ChainHead::Ref(head),
                        stages: Vec::new(),
                        end: None,
                        is_let: true,
                        alias: Some(target),
                        key,
                    });
                    return;
                }
                _ => {}
            }
        }
        let (stages, end, is_let) = self.follow(Start::Edge(edge), parent);
        chains.push(Chain {
            head: ChainHead::Ref(head),
            stages,
            end,
            is_let,
            alias: None,
            key,
        });
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
                Role::Tuple | Role::Implicit { .. } | Role::Alias { .. } => return (stages, None, false),
                Role::Sink(_) => {
                    self.placed[node] = true;
                    return (stages, Some(ChainEnd::Sink { node, parent }), false);
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
    /// Returns whether a chain can be split into a `let` after its unit `j` (see [`Self::chain`]): a stage, or the merge
    /// of the chain's tuple, that a `let` can be named for, and that something in the chain follows.
    fn cuttable(&self, chain: &Chain, units: &[usize], j: usize) -> bool {
        let graph = self.graph;
        let node = units[j];
        let nameable = match self.roles[node] {
            Role::Plain(_) | Role::Where { .. } => true,
            // A reference to the merge's `let` continues from its only output port, which must be the one the chain
            // follows. Only the first unit can be a merge.
            Role::Tuple => graph.shapes[node].default_output().as_ref() == graph.outputs[node].first(),
            _ => false,
        };
        let followed = j + 1 < units.len() || (!chain.is_let && chain.end.is_some());
        nameable && followed && !graph.input_names.contains(graph.id(node).as_str())
    }

    /// Returns, for each unit `j` of a chain, the units `i < j` a `let` from `i` to `j` can start at: those whose ID
    /// is the first one a `let` named for unit `j` derives (the merge `<name>_in`, or the stage `<name>_<operator>`),
    /// and those named like a scope input, which take an `as` wherever they are.
    ///
    /// An ID that is not shortened is the first derived ID of at most one name, so this takes time proportional to the
    /// chain's length, apart from shortened IDs and nodes named like scope inputs, each compared with every later unit.
    fn starts(&self, chain: &Chain, units: &[usize], tuple: bool) -> Vec<Vec<usize>> {
        let graph = self.graph;
        let ends = (0..units.len())
            .filter(|&j| self.cuttable(chain, units, j))
            .collect::<Vec<_>>();
        let by_name = ends
            .iter()
            .map(|&j| (graph.let_names[units[j]].as_str(), j))
            .collect::<HashMap<_, _>>();
        let mut starts = vec![Vec::new(); units.len()];
        for (i, &node) in units.iter().enumerate() {
            let id = graph.id(node);
            let later = ends.iter().copied().filter(|&j| j > i);
            if graph.input_names.contains(id.as_str()) {
                later.for_each(|j| starts[j].push(i));
                continue;
            }
            let merged = i == 0 && tuple;
            let suffix = if merged { "in" } else { graph.nodes[node].op.as_str() };
            let first = |name: &str| {
                if merged {
                    Some(ids::merge(name))
                } else {
                    stage_id(name, suffix, 1)
                }
            };
            if is_shortened(id.as_str()) {
                for j in later {
                    if first(graph.let_names[units[j]].as_str()).as_ref() == Some(id) {
                        starts[j].push(i);
                    }
                }
            } else if let Some(name) = id.as_str().strip_suffix(suffix).and_then(|rest| rest.strip_suffix('_')) {
                if let Some(&j) = by_name.get(name) {
                    if j > i && first(name).as_ref() == Some(id) {
                        starts[j].push(i);
                    }
                }
            }
        }
        starts
    }

    /// Writes a chain as one or more statements, ending a `let` at each node whose ID it would not otherwise derive.
    ///
    /// The chain's **units** are the merge of its tuple, if it starts with one, then its stages. A `let` can end after
    /// any unit that [`Self::cuttable`] allows, and is named for it, so a node a `let` ends needs no `as`. Among the ways
    /// to split the chain in which no `let` but the last writes an `as` that a `let` could replace, the one with the
    /// fewest `as` clauses wins, then the one with the fewest statements.
    fn chain(&self, chain: &Chain) -> Vec<Statement> {
        let graph = self.graph;
        if let (Some(alias), ChainHead::Ref(head)) = (chain.alias, &chain.head) {
            let Role::Alias { name, .. } = &self.roles[alias] else {
                return Vec::new();
            };
            return vec![Statement::Let {
                name: name_of(name),
                pipeline: Pipeline {
                    head: Head::Ref(reference(&graph.spelled(self.roles, head), head.port.as_ref())),
                    stages: Vec::new(),
                    end: None,
                    span: Span::default(),
                },
                span: Span::default(),
            }];
        }
        let merge = match &chain.head {
            ChainHead::Tuple { merge, .. } => Some(*merge),
            ChainHead::Ref(_) => None,
        };
        let units = merge
            .into_iter()
            .chain(chain.stages.iter().copied())
            .collect::<Vec<_>>();
        let n = units.len();
        let ctx = match (chain.is_let, units.last()) {
            (false, _) => Ctx::Bare,
            (true, Some(&last)) => Ctx::Let(graph.let_names[last].clone()),
            // A reference alone names nothing new.
            (true, None) => return Vec::new(),
        };
        // The statement that writes units `i..k`: its head, and its stages, which leave out the merge of a tuple head.
        let segment = |i: usize, k: usize| {
            let head = if i == 0 {
                SegHead::Chain(&chain.head)
            } else {
                SegHead::After(units[i - 1])
            };
            (head, &units[i.max(usize::from(merge.is_some()))..k])
        };

        // A node named like a scope input the artifact reads, which a `let` cannot be named for.
        let unnameable = |node: usize| graph.input_names.contains(graph.id(node).as_str());
        // reach[k]: the fewest `as` clauses, then `let`s, that write the first k units in `let`s that write an `as`
        // only for an unnameable node, and where the last of them starts.
        let starts = self.starts(chain, &units, merge.is_some());
        let mut reach: Vec<Option<((usize, usize), usize)>> = vec![None; n + 1];
        reach[0] = Some(((0, 0), 0));
        for j in 0..n {
            if !self.cuttable(chain, &units, j) {
                continue;
            }
            let ctx = Ctx::Let(graph.let_names[units[j]].clone());
            // From the latest start to the earliest, so that among equal splits the shortest last `let` wins.
            for i in std::iter::once(j).chain(starts[j].iter().rev().copied()) {
                let Some(((aliases, count), _)) = reach[i] else {
                    continue;
                };
                let (head, stages) = segment(i, j + 1);
                let aliased = self.decide(&ctx, head, stages, None);
                if !aliased.iter().all(|&node| unnameable(node)) {
                    continue;
                }
                let cost = (aliases + aliased.len(), count + 1);
                if reach[j + 1].is_none_or(|(best, _)| cost < best) {
                    reach[j + 1] = Some((cost, i));
                }
            }
        }

        // The last statement: the split with the fewest `as` clauses, then the fewest statements, then the longest
        // last statement. Whether the sink and the last statement's first stage need an `as` bounds its cost from
        // below, which rules most splits out without writing the statement.
        let mut ending = HashSet::new();
        if let Some(end) = &chain.end {
            self.decide_end(end, &mut ending);
        }
        let lower = |c: usize| {
            let first = c.max(usize::from(merge.is_some()));
            let anchor = match (&ctx, c) {
                // The last stage of a `let` is named by it.
                (Ctx::Let(_), _) if first + 1 >= n => return ending.len(),
                (Ctx::Let(name), _) => name.clone(),
                (Ctx::Bare, 0) => match &chain.head {
                    ChainHead::Ref(head) => head.anchor.clone(),
                    ChainHead::Tuple { merge, .. } => graph.id(*merge).clone(),
                },
                (Ctx::Bare, _) => graph.id(units[c - 1]).clone(),
            };
            let first_aliased = units.get(first).is_some_and(|&node| {
                stage_id(anchor.as_str(), graph.nodes[node].op.as_str(), 1).as_ref() != Some(graph.id(node))
            });
            ending.len() + usize::from(first_aliased)
        };
        let mut best: Option<((usize, usize), usize, HashSet<usize>)> = None;
        for c in (0..=n).rev() {
            let Some(((aliases, count), _)) = reach[c] else {
                continue;
            };
            if best
                .as_ref()
                .is_some_and(|(least, _, _)| (aliases + lower(c), count) > *least)
            {
                continue;
            }
            let (head, stages) = segment(c, n);
            let aliased = self.decide(&ctx, head, stages, chain.end.as_ref());
            let cost = (aliases + aliased.len(), count);
            if best.as_ref().is_none_or(|(least, _, _)| cost <= *least) {
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
            runs.push((i, k));
            k = i;
        }
        runs.reverse();

        let mut statements = runs
            .into_iter()
            .map(|(i, k)| {
                let ctx = Ctx::Let(graph.let_names[units[k - 1]].clone());
                let (head, stages) = segment(i, k);
                let aliased = self.decide(&ctx, head, stages, None);
                self.statement(&ctx, head, stages, None, &aliased)
            })
            .collect::<Vec<_>>();
        let (head, stages) = segment(c, n);
        statements.push(self.statement(&ctx, head, stages, chain.end.as_ref(), &aliased));
        statements
    }

    /// Returns the nodes of one statement whose IDs lowering would not derive, and which therefore need an `as`.
    fn decide(&self, ctx: &Ctx, head: SegHead<'_>, stages: &[usize], end: Option<&ChainEnd>) -> HashSet<usize> {
        let graph = self.graph;
        let mut aliased = HashSet::new();
        let (anchor, let_name) = match (ctx, head) {
            (Ctx::Let(name), SegHead::Chain(ChainHead::Tuple { merge, .. })) => {
                // The merge is `<name>_in`, or the `let`'s own node when the `let` has no stages.
                let derived = if stages.is_empty() {
                    name.clone()
                } else {
                    ids::merge(name.as_str())
                };
                if &derived != graph.id(*merge) {
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
            self.decide_end(end, &mut aliased);
        }
        aliased
    }

    /// Adds the stages whose IDs are not derived from `anchor` and their ordinal to `aliased`. The last stage of a
    /// `let` is the `let`'s node, aliased only when the `let` has a name of its own.
    fn decide_stages(
        &self, anchor: &ArtifactNodeId, stages: &[usize], let_name: Option<&ArtifactNodeId>,
        aliased: &mut HashSet<usize>,
    ) {
        let mut ordinals = ids::Ordinals::default();
        for (k, &stage) in stages.iter().enumerate() {
            let node = &self.graph.nodes[stage];
            let ordinal = ordinals.next(node.op.as_str());
            if let Some(name) = let_name.filter(|_| k + 1 == stages.len()) {
                if name != &node.id {
                    aliased.insert(stage);
                }
                continue;
            }
            if stage_id(anchor.as_str(), node.op.as_str(), ordinal).as_ref() != Some(&node.id) {
                aliased.insert(stage);
            }
        }
    }

    /// Adds a sink whose ID is not the one derived from the node and port that feed it to `aliased`.
    fn decide_end(&self, end: &ChainEnd, aliased: &mut HashSet<usize>) {
        if let ChainEnd::Sink { node, parent } = end {
            if &ids::terminal(&parent.0, &parent.1) != self.graph.id(*node) {
                aliased.insert(*node);
            }
        }
    }

    /// Writes one statement.
    fn statement(
        &self, ctx: &Ctx, head: SegHead<'_>, stages: &[usize], end: Option<&ChainEnd>, aliased: &HashSet<usize>,
    ) -> Statement {
        let graph = self.graph;
        let head = match head {
            SegHead::Chain(ChainHead::Ref(head)) => {
                Head::Ref(reference(&graph.spelled(self.roles, head), head.port.as_ref()))
            }
            SegHead::Chain(ChainHead::Tuple { merge, items }) => Head::Tuple(Tuple {
                items: items
                    .iter()
                    .map(|item| reference(&graph.spelled(self.roles, item), item.port.as_ref()))
                    .collect(),
                id: self.alias(*merge, aliased),
                span: Span::default(),
            }),
            SegHead::After(node) => Head::Ref(reference(&graph.let_names[node], None)),
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

    fn end(&self, end: &ChainEnd, aliased: &HashSet<usize>) -> Sink {
        match end {
            ChainEnd::Sink { node, .. } => Sink {
                kind: match &self.roles[*node] {
                    Role::Sink(kind) => kind.clone(),
                    _ => SinkKind::Drop,
                },
                id: self.alias(*node, aliased),
                span: Span::default(),
            },
            ChainEnd::Exit(name) => Sink {
                kind: SinkKind::Exit {
                    name: name.clone(),
                    span: Span::default(),
                },
                id: None,
                span: Span::default(),
            },
        }
    }
}

/// Returns whether an ID may be one that [`ids::join`] shortened: one that ends in `_` and eight hexadecimal digits.
fn is_shortened(id: &str) -> bool {
    let bytes = id.as_bytes();
    bytes.len() > 9
        && bytes[bytes.len() - 9] == b'_'
        && bytes[bytes.len() - 8..]
            .iter()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte))
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
