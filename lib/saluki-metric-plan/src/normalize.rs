//! Normalization: one form for programs that differ only in how they are written.
//!
//! The [pipeline syntax](../../../docs/design/programmable-metrics-pipeline-syntax.md#normalization) prints one form
//! of each graph, and equivalent programs must give the same document, and the same hash, whichever way they were
//! written: a default program lowered from native configuration, its export, and an unchanged program parsed back from
//! that export. Normalization runs at the end of native lowering and on every artifact before composition. It never
//! adds or removes a node, and never changes a node ID apart from renaming a derived terminal whose port it moves.
//!
//! The rules, each checked against the reference interpreter and for idempotence:
//!
//! 1. **Fill in defaults**: every parameter left out that has a [default](crate::ir::ParamSpec::default) is written.
//! 2. **Literal set matches** become `Eq`, `Prefix`, `In`, or `PrefixIn`, or `Const(false)` when empty; `In` and
//!    `PrefixIn` of one value become `Eq` and `Prefix`, and of none `Const(false)`. A value listed twice is kept once,
//!    where it is first listed. A match with an operand bound to configuration is left alone, whatever its value.
//! 3. **Predicates are simplified, keeping their operands' order**: junctions of one kind are flattened; constants
//!    are folded (a `flag` or `is_empty` of a literal is the constant it is, a junction drops its neutral constants and
//!    is decided by a decisive one, a junction of one operand is that operand, and `not` of a constant is the other
//!    constant); and double negations are removed. Operands are never sorted or deduplicated, unlike
//!    [`Predicate::canonical`], so the printed form keeps the order its author wrote.
//! 4. **Split polarity**: a `split(p)` whose `true` port leads to a `drop` and whose `false` port leads to anything
//!    else becomes `split(not p)`, with a double negation removed, and its two edges swap ports, so that it prints as
//!    `where not p`. A terminal (`drop` or `emit`) on either port whose ID is the [derived
//!    one](crate::ir::derived_terminal_id) for its port is renamed to the derived ID of the port it moves to: the drop
//!    from `<split>_true` to `<split>_false`, and a terminal on the other port from `<split>_false` to
//!    `<split>_true`. A terminal named otherwise keeps its ID, as does one whose new ID another node already has.
//!
//! Rules 1 to 3 are per node ([`normalize_params`]), and also give the [core form](crate::elaborate#the-core-form) its
//! predicates and defaults, so that a node that writes a default and one
//! that leaves it out build the same kernel, before normalization as after. Rule 4 is the one rule over the graph,
//! shared by plans and artifacts.
//!
//! An artifact's parameters may refer to blobs. Normalization reads a predicate through its blob references, and writes
//! it inline only when a rule changes it; a parameter it does not change keeps its references.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;

use crate::{
    ir::{
        derived_terminal_id, ArtifactNodeId, ArtifactPortRef, Blobs, NodeId, Operand, OperatorName, ParamName,
        ParamType, Params, Plan, PolicyArtifact, PortName, PortRef,
    },
    operators::{names, OperatorRegistry},
    predicate::{Predicate, SetMatch, StringMatch, TagMatch},
};

/// The parameter of a `split` that holds its predicate.
const PREDICATE: &str = "predicate";

/// Returns the normal form of an artifact.
///
/// See the [module documentation](self) for the rules.
pub fn normalize_artifact(artifact: &PolicyArtifact) -> PolicyArtifact {
    normalize_artifact_with_renames(artifact).0
}

/// Returns the normal form of an artifact, with the terminals rule 4 renamed, each with its new ID.
///
/// A tool that knows where each node of the artifact as written came from, such as an editor holding the spans of the
/// text it parsed, maps a node of the normal form back through the renames.
pub fn normalize_artifact_with_renames(
    artifact: &PolicyArtifact,
) -> (PolicyArtifact, BTreeMap<ArtifactNodeId, ArtifactNodeId>) {
    let mut normalized = artifact.clone();
    for node in &mut normalized.nodes {
        node.params = normalize_params_with(&node.op, &node.params, Some(&artifact.blobs));
    }

    // Rule 4, over the artifact's graph, with each artifact node standing for the reserved node of its local ID, as
    // composition makes it.
    let ids = normalized
        .nodes
        .iter()
        .map(|node| NodeId::reserved(node.id.to_local_id()))
        .collect::<Vec<_>>();
    let end = |port: &ArtifactPortRef| match port {
        ArtifactPortRef::Node { node, port } => Some(PortRef::new(NodeId::reserved(node.to_local_id()), port.clone())),
        ArtifactPortRef::Input(_) => None,
    };
    let edges = normalized
        .edges
        .iter()
        .map(|edge| (end(&edge.from), end(&edge.to)))
        .collect::<Vec<_>>();
    let graph = Graph {
        nodes: ids
            .iter()
            .zip(&normalized.nodes)
            .map(|(id, node)| {
                (
                    id,
                    &node.op,
                    split_predicate(&node.op, &node.params, Some(&normalized.blobs)),
                )
            })
            .collect(),
        edges: &edges,
    };
    let flips = polarity_flips(&graph);

    let renamed = |id: &ArtifactNodeId| {
        flips
            .renames
            .get(&NodeId::reserved(id.to_local_id()))
            .and_then(|new| ArtifactNodeId::new(new.local().as_str()).ok())
    };
    for flip in &flips.splits {
        let node = &mut normalized.nodes[flip.node];
        node.params = with_predicate(&node.params, &flip.predicate);
        for index in [flip.true_edge, flip.false_edge] {
            if let ArtifactPortRef::Node { port, .. } = &mut normalized.edges[index].from {
                *port = other_port(port);
            }
        }
    }
    let mut renames = BTreeMap::new();
    for node in &mut normalized.nodes {
        if let Some(new) = renamed(&node.id) {
            renames.insert(node.id.clone(), new.clone());
            node.id = new;
        }
    }
    for edge in &mut normalized.edges {
        for end in [&mut edge.from, &mut edge.to] {
            if let ArtifactPortRef::Node { node, .. } = end {
                if let Some(new) = renamed(node) {
                    *node = new;
                }
            }
        }
    }
    (normalized, renames)
}

/// Returns the normal form of a plan.
///
/// See the [module documentation](self) for the rules. A renamed terminal is renamed everywhere the plan names it: in
/// its edges, and in the native components of the plan's provenance.
pub fn normalize_plan(plan: &Plan) -> Plan {
    let mut normalized = plan.clone();
    for node in &mut normalized.nodes {
        node.params = normalize_params(&node.op, &node.params);
    }

    let edges = normalized
        .edges
        .iter()
        .map(|edge| (Some(edge.from.clone()), Some(edge.to.clone())))
        .collect::<Vec<_>>();
    let graph = Graph {
        nodes: normalized
            .nodes
            .iter()
            .map(|node| (&node.id, &node.op, split_predicate(&node.op, &node.params, None)))
            .collect(),
        edges: &edges,
    };
    let flips = polarity_flips(&graph);

    for flip in &flips.splits {
        let node = &mut normalized.nodes[flip.node];
        node.params = with_predicate(&node.params, &flip.predicate);
        for index in [flip.true_edge, flip.false_edge] {
            let from = &mut normalized.edges[index].from;
            from.port = other_port(&from.port);
        }
    }
    let rename = |id: &mut NodeId| {
        if let Some(new) = flips.renames.get(id) {
            *id = new.clone();
        }
    };
    for node in &mut normalized.nodes {
        rename(&mut node.id);
    }
    for edge in &mut normalized.edges {
        rename(&mut edge.from.node);
        rename(&mut edge.to.node);
    }
    if let Some(provenance) = &mut normalized.provenance {
        provenance.native_components = std::mem::take(&mut provenance.native_components)
            .into_iter()
            .map(|(mut id, component)| {
                rename(&mut id);
                (id, component)
            })
            .collect();
    }
    normalized
}

/// Returns the parameters of a node of operator `op` in normal form: every parameter left out that has a default
/// written with it (rule 1), and every predicate parameter normalized ([`normalize_predicate`], rules 2 and 3).
///
/// Parameters are interpreted against the native operator registry. A parameter the operator does not declare, a
/// predicate that does not read as one, and every parameter of an operator the registry does not know are left as
/// they are, for validation to report.
pub fn normalize_params(op: &OperatorName, params: &Params) -> Params {
    normalize_params_with(op, params, None)
}

/// Returns [`normalize_params`] of parameters whose blob references, if any, refer to `blobs`.
fn normalize_params_with(op: &OperatorName, params: &Params, blobs: Option<&Blobs>) -> Params {
    let Some(def) = OperatorRegistry::native().get(op.as_str()) else {
        return params.clone();
    };
    let mut normalized = params.clone();
    for spec in def.params() {
        let Ok(name) = ParamName::new(spec.name) else {
            continue;
        };
        match params.get(spec.name) {
            None => {
                if let Some(Ok(default)) = spec.default_value() {
                    normalized.insert(name, default);
                }
            }
            Some(value) if spec.ty == ParamType::Predicate => {
                let Some(predicate) = read_predicate(value, blobs) else {
                    continue;
                };
                let normal = normalize_predicate(&predicate);
                if normal != predicate {
                    if let Ok(value) = serde_json::to_value(&normal) {
                        normalized.insert(name, value);
                    }
                }
            }
            Some(_) => {}
        }
    }
    normalized
}

/// Reads a predicate parameter, through its blob references if there are `blobs`.
fn read_predicate(value: &Value, blobs: Option<&Blobs>) -> Option<Predicate> {
    let mut value = value.clone();
    if let Some(blobs) = blobs {
        blobs.inline(&mut value).ok()?;
    }
    serde_json::from_value(value).ok()
}

/// Returns the predicate of a `split` node, if it is one and its predicate reads.
fn split_predicate(op: &OperatorName, params: &Params, blobs: Option<&Blobs>) -> Option<Predicate> {
    if op.as_str() != names::SPLIT {
        return None;
    }
    read_predicate(params.get(PREDICATE)?, blobs)
}

/// Returns `params` with the predicate `predicate`.
fn with_predicate(params: &Params, predicate: &Predicate) -> Params {
    let mut params = params.clone();
    if let (Ok(name), Ok(value)) = (ParamName::new(PREDICATE), serde_json::to_value(predicate)) {
        params.insert(name, value);
    }
    params
}

/// Returns the predicate in normal form: rules 2 and 3 of the [module documentation](self).
///
/// The result matches exactly the metrics the predicate matches, and normalizing it again changes nothing.
pub fn normalize_predicate(predicate: &Predicate) -> Predicate {
    match predicate {
        Predicate::Flag(Operand::Literal(value)) => Predicate::Const(*value),
        Predicate::IsEmpty(Operand::Literal(values)) => Predicate::Const(values.is_empty()),
        Predicate::Name(matcher) => string_leaf(matcher, Predicate::Name),
        Predicate::IntakeName(matcher) => string_leaf(matcher, Predicate::IntakeName),
        Predicate::Host(matcher) => string_leaf(matcher, Predicate::Host),
        Predicate::Tag(TagMatch { key, value }) => string_leaf(value, |value| {
            Predicate::Tag(TagMatch {
                key: key.clone(),
                value,
            })
        }),
        Predicate::OriginTag(TagMatch { key, value }) => string_leaf(value, |value| {
            Predicate::OriginTag(TagMatch {
                key: key.clone(),
                value,
            })
        }),
        Predicate::All(operands) => junction(operands, true),
        Predicate::Any(operands) => junction(operands, false),
        Predicate::Not(operand) => negate(normalize_predicate(operand)),
        Predicate::Const(_)
        | Predicate::Flag(_)
        | Predicate::IsEmpty(_)
        | Predicate::HostUnset {}
        | Predicate::HasTag(_)
        | Predicate::HasOriginTag(_)
        | Predicate::KindIn(_) => predicate.clone(),
    }
}

/// Returns the negation of a predicate in normal form, in normal form.
fn negate(predicate: Predicate) -> Predicate {
    match predicate {
        Predicate::Const(value) => Predicate::Const(!value),
        Predicate::Not(operand) => *operand,
        operand => Predicate::Not(Box::new(operand)),
    }
}

/// Returns a string-match leaf in normal form: `leaf` of the match in normal form, or `Const(false)` for a match of no
/// values, which no metric matches, one without the field included.
fn string_leaf(matcher: &StringMatch, leaf: impl FnOnce(StringMatch) -> Predicate) -> Predicate {
    let normal = match matcher {
        StringMatch::Eq(_) | StringMatch::Prefix(_) => Some(matcher.clone()),
        StringMatch::In(values) => set_match(values, false),
        StringMatch::PrefixIn(values) => set_match(values, true),
        StringMatch::Matches(SetMatch {
            values: Operand::Literal(values),
            prefix: Operand::Literal(prefix),
        }) => set_match(values, *prefix),
        StringMatch::Matches(_) => Some(matcher.clone()),
    };
    normal.map_or(Predicate::Const(false), leaf)
}

/// Returns the normal form of a match against literal `values`, by prefix if `prefix`, or `None` if it has no values.
fn set_match(values: &[String], prefix: bool) -> Option<StringMatch> {
    let mut seen = BTreeSet::new();
    let mut values = values
        .iter()
        .filter(|value| seen.insert(value.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    match (values.len(), prefix) {
        (0, _) => None,
        (1, false) => Some(StringMatch::Eq(values.remove(0))),
        (1, true) => Some(StringMatch::Prefix(values.remove(0))),
        (_, false) => Some(StringMatch::In(values)),
        (_, true) => Some(StringMatch::PrefixIn(values)),
    }
}

/// Returns a junction in normal form: `all` when `is_all`, `any` otherwise.
///
/// Its operands are normalized, in order; an operand that is a junction of the same kind is replaced by its operands,
/// a neutral constant (`true` in an `all`, `false` in an `any`) is dropped, and a decisive one decides the junction.
/// A junction left with no operands is its neutral constant, and one left with one operand is that operand.
fn junction(operands: &[Predicate], is_all: bool) -> Predicate {
    let mut kept = Vec::with_capacity(operands.len());
    for operand in operands {
        match normalize_predicate(operand) {
            Predicate::Const(value) if value == is_all => {}
            Predicate::Const(_) => return Predicate::Const(!is_all),
            Predicate::All(inner) if is_all => kept.extend(inner),
            Predicate::Any(inner) if !is_all => kept.extend(inner),
            operand => kept.push(operand),
        }
    }
    match kept.len() {
        0 => Predicate::Const(is_all),
        1 => kept.remove(0),
        _ if is_all => Predicate::All(kept),
        _ => Predicate::Any(kept),
    }
}

/// A plan's or an artifact's graph, as rule 4 reads it.
struct Graph<'a> {
    /// Each node's ID, operator, and, for a `split` whose predicate reads, its predicate in normal form, in node order.
    nodes: Vec<(&'a NodeId, &'a OperatorName, Option<Predicate>)>,

    /// Each edge's ends, in edge order: `None` for a source of an artifact.
    edges: &'a [(Option<PortRef>, Option<PortRef>)],
}

/// What rule 4 changes in a graph.
struct Flips {
    /// The `split`s it turns.
    splits: Vec<Flip>,

    /// The terminals it renames, each with its new ID.
    renames: BTreeMap<NodeId, NodeId>,
}

/// One `split` rule 4 turns.
struct Flip {
    /// The index of the `split` node.
    node: usize,

    /// The `split`'s new predicate: the negation of its predicate.
    predicate: Predicate,

    /// The index of the edge that leaves its `true` port, to a `drop`.
    true_edge: usize,

    /// The index of the edge that leaves its `false` port.
    false_edge: usize,
}

/// Returns the `split`s of `graph` whose polarity rule 4 turns, and the terminals it renames.
fn polarity_flips(graph: &Graph<'_>) -> Flips {
    let operators = graph
        .nodes
        .iter()
        .map(|(id, op, _)| (*id, op.as_str()))
        .collect::<BTreeMap<_, _>>();
    let operator_at = |end: &Option<PortRef>| end.as_ref().and_then(|end| operators.get(&end.node).copied());
    let (on_true, on_false) = (PortName::from_static("true"), PortName::from_static("false"));

    let mut ids = graph
        .nodes
        .iter()
        .map(|(id, ..)| (*id).clone())
        .collect::<BTreeSet<_>>();
    let mut flips = Flips {
        splits: Vec::new(),
        renames: BTreeMap::new(),
    };
    for (index, (id, _, predicate)) in graph.nodes.iter().enumerate() {
        let Some(predicate) = predicate else { continue };
        let leaving = |port: &PortName| {
            let mut edges = graph.edges.iter().enumerate().filter(|(_, (from, _))| {
                from.as_ref()
                    .is_some_and(|from| from.node == **id && from.port == *port)
            });
            match (edges.next(), edges.next()) {
                (Some((edge, (_, to))), None) => Some((edge, to)),
                _ => None,
            }
        };
        let (Some((true_edge, dropped)), Some((false_edge, kept))) = (leaving(&on_true), leaving(&on_false)) else {
            continue;
        };
        if operator_at(dropped) != Some(names::DROP) || operator_at(kept) == Some(names::DROP) {
            continue;
        }
        flips.splits.push(Flip {
            node: index,
            predicate: negate(predicate.clone()),
            true_edge,
            false_edge,
        });

        // The derived terminals on the two ports trade names, as long as no other node has the name one moves to.
        let mut renames = Vec::new();
        for (end, from, to) in [(dropped, &on_true, &on_false), (kept, &on_false, &on_true)] {
            let Some(end) = end else { continue };
            let terminal = matches!(operator_at(&Some(end.clone())), Some(names::DROP | names::EMIT));
            if terminal && end.node == derived_terminal_id(id, from) {
                renames.push((end.node.clone(), derived_terminal_id(id, to)));
            }
        }
        for (old, _) in &renames {
            ids.remove(old);
        }
        if renames.iter().all(|(_, new)| !ids.contains(new)) {
            for (old, new) in renames {
                ids.insert(new.clone());
                flips.renames.insert(old, new);
            }
        } else {
            ids.extend(renames.into_iter().map(|(old, _)| old));
        }
    }
    flips
}

/// Returns the other port of a `split`.
fn other_port(port: &PortName) -> PortName {
    match port.as_str() {
        "true" => PortName::from_static("false"),
        _ => PortName::from_static("true"),
    }
}

#[cfg(test)]
mod tests;
