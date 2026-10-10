//! Filters as R5a and R6 see them, the operators a filter can cross, and the plan edits that move and copy filters.
//!
//! A **filter** is an operator with `filter` cardinality, or a `split` whose one port leads directly to a `drop`. Its
//! **drop condition** is what it removes: for a `split`, its predicate when its `true` port leads to the drop, and the
//! predicate's negation otherwise, in canonical form. Two `split` filters with equal drop conditions remove the same
//! records; two filter operators do when they are the same operator with equal parameters.

use std::{
    collections::{BTreeSet, HashMap, HashSet},
    fmt::Write as _,
    str::FromStr,
};

use serde_json::{json, Value};

use crate::{
    ir::{Edge, Namespace, Node, NodeId, Plan, PortName, PortRef, Provenance, MAX_IDENTIFIER_LEN},
    model::{Field, FieldSet, Kind, KindSet, PortType},
    operators::{
        names, Boundary, Cardinality, ConfiguredOperator, OrderContract, PerKind, StateContract, TimestampMap,
    },
    predicate::Predicate,
    validate::{graph::Graph, ValidationReport},
};

/// The output port of a `split` that matching records leave by.
const TRUE: usize = 0;

/// The output port of a `split` that other records leave by.
const FALSE: usize = 1;

/// How a filter removes records.
#[derive(Clone, Debug)]
pub(super) enum FilterForm {
    /// A `split` one of whose ports leads directly to a `drop`.
    Split {
        /// The split's predicate, as decoded.
        predicate: Predicate,

        /// Whether the `true` port is the one that leads to the drop.
        drops_true: bool,

        /// The `drop` node.
        drop: usize,
    },

    /// An operator of `filter` cardinality, with one input and one output port.
    Operator,
}

/// A filter of a plan, by node position.
#[derive(Clone, Debug)]
pub(super) struct Filter {
    /// The filter's node: the `split`, or the filter operator.
    pub node: usize,

    /// How it removes records.
    pub form: FilterForm,
}

impl Filter {
    /// Returns the filter at `node`, if the node is one.
    ///
    /// A `split` both of whose ports lead to a `drop` removes everything; it is not a filter here. A filter operator
    /// must be order-independent: one that keeps records by their order, such as the first of each name, would keep
    /// others once it runs elsewhere, as on each input of a `merge`.
    pub fn at(graph: &Graph<'_>, node: usize) -> Option<Self> {
        let op = graph.op(node)?;
        let contract = op.contract();
        if contract.boundary != Boundary::Synchronous || contract.state != StateContract::None {
            return None;
        }
        if op.name() == names::SPLIT {
            let predicate = op.params().predicate("predicate")?.clone();
            let drop_at = |port| {
                graph
                    .target(node, port)
                    .filter(|&(target, _)| graph.op(target).is_some_and(|op| op.name() == names::DROP))
                    .map(|(target, _)| target)
            };
            let form = match (drop_at(TRUE), drop_at(FALSE)) {
                (Some(drop), None) => FilterForm::Split {
                    predicate,
                    drops_true: true,
                    drop,
                },
                (None, Some(drop)) => FilterForm::Split {
                    predicate,
                    drops_true: false,
                    drop,
                },
                _ => return None,
            };
            return Some(Self { node, form });
        }
        let ports = op.ports();
        (contract.cardinality == Cardinality::Filter
            && contract.order == OrderContract::Independent
            && ports.inputs.len() == 1
            && ports.outputs.len() == 1)
            .then_some(Self {
                node,
                form: FilterForm::Operator,
            })
    }

    /// Returns the fields the filter reads, per kind.
    pub fn reads(&self, graph: &Graph<'_>) -> PerKind<FieldSet> {
        match &self.form {
            FilterForm::Split { predicate, .. } => PerKind::uniform(predicate.reads()),
            FilterForm::Operator => graph
                .op(self.node)
                .map_or_else(|| PerKind::uniform(FieldSet::ALL), |op| op.contract().reads.clone()),
        }
    }

    /// Returns the output port the records the filter keeps leave by.
    pub fn keep_port(&self) -> usize {
        match self.form {
            FilterForm::Split { drops_true: true, .. } => FALSE,
            FilterForm::Split { drops_true: false, .. } => TRUE,
            FilterForm::Operator => 0,
        }
    }

    /// Returns the `drop` node of a `split` filter.
    pub fn drop(&self) -> Option<usize> {
        match self.form {
            FilterForm::Split { drop, .. } => Some(drop),
            FilterForm::Operator => None,
        }
    }

    /// Returns the drop condition of a `split` filter, in canonical form.
    pub fn drop_condition(&self) -> Option<Predicate> {
        match &self.form {
            FilterForm::Split {
                predicate, drops_true, ..
            } => Some(drop_condition(predicate, *drops_true)),
            FilterForm::Operator => None,
        }
    }

    /// Returns what a `split` filter removes as a drop condition and the kinds it removes it from; see
    /// [`restriction`].
    pub fn restriction(&self) -> Option<(Predicate, KindSet)> {
        match &self.form {
            FilterForm::Split {
                predicate, drops_true, ..
            } => Some(restriction(predicate, *drops_true)),
            FilterForm::Operator => None,
        }
    }

    /// Returns the filter's nodes: the filter's own node, then its `drop`, if it has one.
    pub fn nodes(&self) -> Vec<usize> {
        std::iter::once(self.node).chain(self.drop()).collect()
    }

    /// Returns `true` if one of the filter's nodes has native provenance.
    ///
    /// A host reports native series under a native node's ID, such as the native filtered-metrics counter of a
    /// filterlist, which counts what one port of its `split` carries. A copy of the filter would not carry them, so
    /// native filters are only ever moved whole, keeping their IDs and their counts.
    pub fn is_native(&self, graph: &Graph<'_>) -> bool {
        self.nodes()
            .into_iter()
            .any(|node| matches!(graph.nodes[node].node.provenance, Some(Provenance::Native(_))))
    }

    /// Returns `true` if the filter reads a host signal, which makes it dynamic: it is never moved.
    pub fn is_dynamic(&self, graph: &Graph<'_>) -> bool {
        graph.op(self.node).is_none_or(|op| op.contract().is_dynamic())
    }

    /// Returns `true` if this filter and `other` remove the same records: two `split` filters with equal drop
    /// conditions, or two filter operators of the same operator with equal parameters.
    pub fn removes_what(&self, other: &Self, graph: &Graph<'_>) -> bool {
        match (&self.form, &other.form) {
            (FilterForm::Split { .. }, FilterForm::Split { .. }) => self.drop_condition() == other.drop_condition(),
            (FilterForm::Operator, FilterForm::Operator) => {
                let (a, b) = (&graph.nodes[self.node].node, &graph.nodes[other.node].node);
                a.op == b.op && a.params == b.params
            }
            _ => false,
        }
    }

    /// Describes the filter for a justification, such as ``policy/f` (a `split` whose `false` port leads to
    /// `policy/f_drop`)`.
    pub fn describe(&self, graph: &Graph<'_>) -> String {
        let id = graph.id(self.node);
        match &self.form {
            FilterForm::Split { drops_true, drop, .. } => format!(
                "`{}` (a `split` whose `{}` port leads to `{}`)",
                id,
                if *drops_true { "true" } else { "false" },
                graph.id(*drop)
            ),
            FilterForm::Operator => format!(
                "`{}` (`{}`, of `filter` cardinality)",
                id,
                graph.op(self.node).map_or("?", |op| op.name())
            ),
        }
    }
}

/// Returns the drop condition of a `split` with `predicate` that drops its `true` port if `drops_true`, and its
/// `false` port otherwise, in canonical form.
pub(super) fn drop_condition(predicate: &Predicate, drops_true: bool) -> Predicate {
    if drops_true {
        predicate.canonical()
    } else {
        Predicate::Not(Box::new(predicate.clone())).canonical()
    }
}

/// Returns the predicate, as decoded and as written, of a copy of a `split` filter restricted to the kinds `kinds`:
/// it drops what the filter drops among records of those kinds, by the same port, and keeps every other record.
pub(super) fn restricted(predicate: &Predicate, raw: &Value, drops_true: bool, kinds: KindSet) -> (Predicate, Value) {
    if drops_true {
        let kind = Predicate::KindIn(kinds);
        let raw = json!({"all": [to_value(&kind), raw.clone()]});
        (Predicate::All(vec![kind, predicate.clone()]), raw)
    } else {
        let kind = Predicate::KindIn(KindSet::ALL.difference(kinds));
        let raw = json!({"any": [to_value(&kind), raw.clone()]});
        (Predicate::Any(vec![kind, predicate.clone()]), raw)
    }
}

/// Returns what a `split` with `predicate`, dropping its `true` port if `drops_true`, removes: a drop condition in
/// canonical form, and the kinds of the records it removes them from.
///
/// A copy [`restricted`] to the kinds `K` removes what the drop condition of its unrestricted predicate `p` removes,
/// from records of `K` only: `all(kind_in(K), p)` dropping its `true` port gives `(p, K)`, and `any(kind_in(L), p)`
/// dropping its `false` port gives `(not(p), every kind but L)`. Any other filter removes what its drop condition
/// removes from every kind. Comparing these pairs, rather than the copies' drop conditions, recognizes a copy
/// restricted to more kinds as covering one restricted to fewer.
pub(super) fn restriction(predicate: &Predicate, drops_true: bool) -> (Predicate, KindSet) {
    match (predicate, drops_true) {
        (Predicate::All(operands), true) => {
            if let [Predicate::KindIn(kinds), rest] = operands.as_slice() {
                return (drop_condition(rest, true), *kinds);
            }
        }
        (Predicate::Any(operands), false) => {
            if let [Predicate::KindIn(others), rest] = operands.as_slice() {
                return (drop_condition(rest, false), KindSet::ALL.difference(*others));
            }
        }
        _ => {}
    }
    (drop_condition(predicate, drops_true), KindSet::ALL)
}

fn to_value(predicate: &Predicate) -> Value {
    // A `kind_in` leaf always serializes.
    serde_json::to_value(predicate).unwrap_or(Value::Null)
}

/// Returns why a filter cannot cross the operator `op`, or `None` if it can for the kinds that commute.
///
/// A filter crosses only a synchronous map without state, that reads no signal, whose output does not depend on the
/// order of its input, and whose one output port carries records of the kinds and in the timestamp form they arrived
/// in, apart from what its contract says it writes. A map with several inputs must be a `merge`, a per-record union,
/// which is what lets a copy of the filter run on each input. Filters never cross other filters, so that two of them
/// cannot trade places forever, nor expansions: above one, the filter would see one record where it saw each record
/// the expansion made, and its counts would change.
pub(super) fn uncrossable(op: &ConfiguredOperator) -> Option<&'static str> {
    let contract = op.contract();
    let ports = op.ports();
    if contract.boundary != Boundary::Synchronous {
        return Some("it runs in a host component, a fence");
    }
    if contract.state != StateContract::None {
        return Some("it keeps state, which R5b covers");
    }
    if contract.is_dynamic() {
        return Some("it reads a host signal");
    }
    if contract.order != OrderContract::Independent {
        return Some("its output depends on the order of its input");
    }
    if ports.outputs.len() != 1 || ports.inputs.is_empty() {
        return Some(
            "it does not have exactly one output port, so a filter below it applies to only part of its output",
        );
    }
    if contract.cardinality != Cardinality::Map {
        return Some("it is not a map: it filters, expands, or ends a path");
    }
    if ports.inputs.len() > 1 && op.name() != names::MERGE {
        return Some("it joins several inputs, but is not a `merge`, so a filter on each input need not be the same");
    }
    if ports.outputs[0].mapping.timestamps != TimestampMap::Preserve {
        return Some("it changes which points its output carries");
    }
    None
}

/// Returns the kinds, among `kinds`, for which a filter that reads `reads` commutes with the operator `op`: those for
/// which `op` changes neither the kind nor any field the filter reads.
pub(super) fn commuting_kinds(reads: &PerKind<FieldSet>, op: &ConfiguredOperator, kinds: KindSet) -> KindSet {
    let contract = op.contract();
    let mapping = op.ports().outputs.first().map(|port| port.mapping);
    kinds
        .iter()
        .filter(|&kind| {
            let writes = *contract.writes.get(kind);
            !writes.contains(Field::Kind)
                && mapping.is_some_and(|mapping| mapping.kinds.get(kind) == KindSet::of(&[kind]))
                && !reads.get(kind).intersects(writes)
        })
        .collect()
}

/// Returns the kinds that can reach the input ports of `node`: the union of the types of the ports feeding them.
///
/// A port without a type, which a valid plan does not have, counts as carrying every kind.
pub(super) fn reaching_kinds(graph: &Graph<'_>, report: &ValidationReport, node: usize) -> KindSet {
    let inputs = graph.op(node).map_or(0, |op| op.ports().inputs.len());
    (0..inputs)
        .map(|port| input_kinds(graph, report, node, port))
        .fold(KindSet::EMPTY, KindSet::union)
}

/// Returns the kinds that can reach the input port `port` of `node`: the type of the port feeding it.
///
/// A port without a type, which a valid plan does not have, counts as carrying every kind; an input nothing feeds
/// receives nothing.
pub(super) fn input_kinds(graph: &Graph<'_>, report: &ValidationReport, node: usize, port: usize) -> KindSet {
    graph.source(node, port).map_or(KindSet::EMPTY, |from| {
        report
            .port_type(&output_ref(graph, from))
            .unwrap_or(PortType::ANY)
            .kinds
    })
}

/// Returns `true` if every record of the kinds `kinds` reaching the input ports of `node` already passed a `split`
/// filter that removes what the drop condition `condition`, which reads `reads`, removes, from records of at least
/// those kinds, through nothing that changes what the condition reads.
///
/// This is how R5a recognizes the copy that a partial push-down already inserted above an operator, whether it is
/// still directly above it or has moved further up since. A copy is compared by its [`restriction`]: the copy that
/// the first application inserts narrows what reaches the operator, so the kinds a second application would restrict a
/// copy to are fewer, and the copy above covers them.
pub(super) fn applied_above(
    graph: &Graph<'_>, report: &ValidationReport, node: usize, condition: &Predicate, kinds: KindSet, reads: FieldSet,
) -> bool {
    let mut memo = HashMap::new();
    let inputs = graph.op(node).map_or(0, |op| op.ports().inputs.len());
    let removes = Removes {
        condition,
        kinds,
        reads,
    };
    inputs > 0
        && (0..inputs).all(|port| {
            graph
                .source(node, port)
                .is_some_and(|(above, _)| applied_at(graph, report, above, &removes, &mut memo))
        })
}

/// What [`applied_above`] looks for: a filter that removes what `condition` removes, from records of `kinds`.
struct Removes<'a> {
    condition: &'a Predicate,
    kinds: KindSet,
    reads: FieldSet,
}

fn applied_at(
    graph: &Graph<'_>, report: &ValidationReport, node: usize, removes: &Removes<'_>, memo: &mut HashMap<usize, bool>,
) -> bool {
    if let Some(&known) = memo.get(&node) {
        return known;
    }
    let reads = removes.reads;
    let covers = Filter::at(graph, node)
        .and_then(|filter| filter.restriction())
        .is_some_and(|(condition, kinds)| &condition == removes.condition && removes.kinds.is_subset(kinds));
    let applied = if covers {
        true
    } else {
        match graph.op(node) {
            None => false,
            Some(op) => {
                let contract = op.contract();
                let inputs = op.ports().inputs.len();
                let kinds = reaching_kinds(graph, report, node);
                let transparent = contract.boundary == Boundary::Synchronous
                    && contract.state == StateContract::None
                    && !contract.is_dynamic()
                    && inputs > 0
                    && kinds.iter().all(|kind| {
                        let writes = *contract.writes.get(kind);
                        !writes.contains(Field::Kind)
                            && !writes.intersects(reads)
                            && op
                                .ports()
                                .outputs
                                .iter()
                                .all(|port| port.mapping.kinds.get(kind).is_subset(KindSet::of(&[kind])))
                    });
                transparent
                    && (0..inputs).all(|port| {
                        graph
                            .source(node, port)
                            .is_some_and(|(above, _)| applied_at(graph, report, above, removes, memo))
                    })
            }
        }
    };
    memo.insert(node, applied);
    applied
}

/// Returns a reference to one output port of a node.
pub(super) fn output_ref(graph: &Graph<'_>, (node, port): (usize, usize)) -> PortRef {
    let name = graph
        .op(node)
        .and_then(|op| op.ports().outputs.get(port))
        .map(|port| port.name.clone());
    PortRef::new(
        graph.id(node).clone(),
        name.unwrap_or_else(|| PortName::from_static("out")),
    )
}

/// Returns a reference to one input port of a node.
pub(super) fn input_ref(graph: &Graph<'_>, (node, port): (usize, usize)) -> PortRef {
    let name = graph
        .op(node)
        .and_then(|op| op.ports().inputs.get(port))
        .map(|port| port.name.clone());
    PortRef::new(
        graph.id(node).clone(),
        name.unwrap_or_else(|| PortName::from_static("in")),
    )
}

/// The ports where a plan's scopes begin and end.
///
/// A filter never moves across them: a scope is the part of a plan an artifact replaces, and a rewrite neither moves
/// work out of a scope nor into one.
pub(super) struct ScopeBoundaries {
    inputs: BTreeSet<PortRef>,
    exits: BTreeSet<PortRef>,
}

impl ScopeBoundaries {
    /// Collects the boundaries of every scope of `plan`.
    pub fn of(plan: &Plan) -> Self {
        let mut boundaries = Self {
            inputs: BTreeSet::new(),
            exits: BTreeSet::new(),
        };
        for scope in plan.scopes.values() {
            boundaries.inputs.extend(scope.inputs.values().cloned());
            boundaries.exits.extend(scope.exits.values().cloned());
        }
        boundaries
    }

    /// Returns `true` if the edge from the output port `from` into the input port `to` crosses a scope boundary: it
    /// leaves a scope's input, or enters a scope's exit.
    pub fn crossed_by(&self, from: &PortRef, to: &PortRef) -> bool {
        self.inputs.contains(from) || self.is_exit(to)
    }

    /// Returns `true` if the input port `to` is a scope's exit: what enters it leaves the scope.
    ///
    /// A filter inserted on an edge into an exit would join the scope its input is in.
    pub fn is_exit(&self, to: &PortRef) -> bool {
        self.exits.contains(to)
    }

    /// Returns `true` if a port of the filter's own nodes is a scope boundary: an output port of the filter a scope's
    /// input, or an input port of the filter or its `drop` a scope's exit.
    ///
    /// Moving such a filter would move the boundary's work across it: below a scope input at the filter's kept port,
    /// the operator the filter moves above would join the scope. Removing it, as a copy into the inputs of a merge or a
    /// hoist does, would leave the scope naming a port the plan no longer has.
    pub fn bound_at(&self, graph: &Graph<'_>, filter: &Filter) -> bool {
        filter.nodes().into_iter().any(|node| {
            let Some(op) = graph.op(node) else { return true };
            let ports = op.ports();
            (0..ports.outputs.len()).any(|port| self.inputs.contains(&output_ref(graph, (node, port))))
                || (0..ports.inputs.len()).any(|port| self.exits.contains(&input_ref(graph, (node, port))))
        })
    }
}

/// A rewritten copy of a plan, edited by port references.
pub(super) struct Edit {
    plan: Plan,
    taken: HashSet<NodeId>,
}

impl Edit {
    /// Starts editing a copy of `plan`.
    pub fn of(plan: &Plan) -> Self {
        Self {
            taken: plan.nodes.iter().map(|node| node.id.clone()).collect(),
            plan: plan.clone(),
        }
    }

    /// Returns the edited plan.
    pub fn finish(self) -> Plan {
        self.plan
    }

    /// Returns a node ID that no node of the plan has, in the namespace of `base`, derived from it: `<base>_<tag><n>`,
    /// with `base` shortened if the identifier would be too long.
    pub fn fresh_id(&mut self, base: &NodeId, tag: &str) -> Option<NodeId> {
        let local = base.local().to_string();
        (0u32..).find_map(|n| {
            let suffix = format!("_{}{}", tag, n);
            let keep = MAX_IDENTIFIER_LEN.saturating_sub(suffix.len()).min(local.len());
            let stem = local[..keep].trim_end_matches('_');
            let mut candidate = String::new();
            if base.namespace() == Namespace::Policy {
                candidate.push_str("policy/");
            }
            let _ = write!(candidate, "{}{}", stem, suffix);
            let id = NodeId::from_str(&candidate).ok()?;
            self.taken.insert(id.clone()).then_some(id)
        })
    }

    /// Returns a copy of the node `id`, with the ID `copy`.
    pub fn copy_node(&self, id: &NodeId, copy: NodeId) -> Option<Node> {
        let mut node = self.plan.nodes.iter().find(|node| &node.id == id)?.clone();
        node.id = copy;
        Some(node)
    }

    /// Returns `node` with its parameter `name` replaced by `value`.
    pub fn with_param(mut node: Node, name: &str, value: Value) -> Option<Node> {
        node.params.insert(name.parse().ok()?, value);
        Some(node)
    }

    /// Inserts `nodes` before the node `before`, in order, removing any of them that the plan already has from where
    /// it was.
    pub fn place_before(&mut self, before: &NodeId, nodes: Vec<Node>) {
        let ids = nodes.iter().map(|node| node.id.clone()).collect::<HashSet<_>>();
        self.plan.nodes.retain(|node| !ids.contains(&node.id));
        let at = self
            .plan
            .nodes
            .iter()
            .position(|node| &node.id == before)
            .unwrap_or(self.plan.nodes.len());
        self.plan.nodes.splice(at..at, nodes);
    }

    /// Removes the nodes `ids`, and every edge into or out of them.
    pub fn remove_nodes(&mut self, ids: &[NodeId]) {
        self.plan.nodes.retain(|node| !ids.contains(&node.id));
        self.plan
            .edges
            .retain(|edge| !ids.contains(&edge.from.node) && !ids.contains(&edge.to.node));
    }

    /// Returns the position of the edge that enters the input port `to`.
    pub fn edge_into(&self, to: &PortRef) -> Option<usize> {
        self.plan.edges.iter().position(|edge| &edge.to == to)
    }

    /// Returns the position of the edge that leaves the output port `from`.
    pub fn edge_from(&self, from: &PortRef) -> Option<usize> {
        self.plan.edges.iter().position(|edge| &edge.from == from)
    }

    /// Makes the edge at `position` lead from `from` to `to`.
    pub fn set_edge(&mut self, position: usize, from: PortRef, to: PortRef) {
        if let Some(edge) = self.plan.edges.get_mut(position) {
            *edge = Edge { from, to };
        }
    }

    /// Inserts the edges `edges` after position `after`, in order.
    pub fn insert_edges(&mut self, after: usize, edges: Vec<(PortRef, PortRef)>) {
        let at = (after + 1).min(self.plan.edges.len());
        self.plan
            .edges
            .splice(at..at, edges.into_iter().map(|(from, to)| Edge { from, to }));
    }
}

/// Returns the names of a set of fields, such as `{name, tags}`.
pub(super) fn fields(fields: FieldSet) -> String {
    let names = fields.iter().map(Field::as_str).collect::<Vec<_>>();
    format!("{{{}}}", names.join(", "))
}

/// Returns the names of a set of kinds, such as `{counter, gauge}`.
pub(super) fn kinds(kinds: KindSet) -> String {
    let names = kinds.iter().map(Kind::as_str).collect::<Vec<_>>();
    format!("{{{}}}", names.join(", "))
}

/// The port names of a filter, for wiring copies of it.
pub(super) struct FilterPorts {
    /// The filter's input port.
    input: PortName,

    /// The output port the records it keeps leave by.
    keep: PortName,

    /// For a `split` filter: the output port that leads to its `drop`, and the `drop`'s input port.
    drop: Option<(PortName, PortName)>,
}

impl FilterPorts {
    /// Returns the port names of `filter`.
    pub fn of(graph: &Graph<'_>, filter: &Filter) -> Self {
        let drop = match filter.form {
            FilterForm::Split { drops_true, drop, .. } => {
                let port = if drops_true { TRUE } else { FALSE };
                Some((
                    output_ref(graph, (filter.node, port)).port,
                    input_ref(graph, (drop, 0)).port,
                ))
            }
            FilterForm::Operator => None,
        };
        Self {
            input: input_ref(graph, (filter.node, 0)).port,
            keep: output_ref(graph, (filter.node, filter.keep_port())).port,
            drop,
        }
    }

    /// Returns the input port of the filter, or of a copy of it, `node`.
    pub fn input(&self, node: &NodeId) -> PortRef {
        PortRef::new(node.clone(), self.input.clone())
    }

    /// Returns the port that the filter, or a copy of it, `node` keeps records on.
    pub fn keep(&self, node: &NodeId) -> PortRef {
        PortRef::new(node.clone(), self.keep.clone())
    }

    /// Returns the edge from the `split` filter, or a copy of it, `node` to its `drop`, `drop`; `None` for a filter
    /// operator.
    pub fn drop_edge(&self, node: &NodeId, drop: Option<&NodeId>) -> Option<(PortRef, PortRef)> {
        let ((port, input), drop) = (self.drop.as_ref()?, drop?);
        Some((
            PortRef::new(node.clone(), port.clone()),
            PortRef::new(drop.clone(), input.clone()),
        ))
    }
}
