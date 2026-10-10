//! R3: fold constant `split`s and remove unreachable subgraphs.
//!
//! A `split` is **constant** when every record that can reach it leaves by the same port. Its other port is **dead**:
//! it carries nothing, and neither does anything that only that port feeds. R3 folds the `split` into an edge from its
//! input to what its live port leads to, and removes the subgraph only the dead port feeds. A `merge` that the dead
//! subgraph feeds, but that also has an input from outside it, keeps its other inputs, renumbered in order.
//!
//! One application folds one `split`, the first in plan order whose obligations hold, with everything it makes
//! unreachable; nested constant `split`s inside the removed subgraph go with it.
//!
//! # Obligations
//!
//! - **The predicate is constant given its parameters.** In structure, a `split` is constant when its validated port
//!   types say so: one of its ports carries no kind while its input carries some. That covers predicates whose
//!   literal constants decide them, after canonical form `const(true)` or `const(false)`, or a junction that literal
//!   constants decide (an `all` with a `const(false)` operand, an `any` with a `const(true)` one, a `not` of a
//!   constant), and predicates that `kind_in` leaves decide for every kind the input can carry, which validation
//!   reports as a predicate that can never match. A `flag` or `is_empty` is constant given its binding's resolved
//!   value, but structure never depends on a configuration value, so validation's port types, and this rewrite, treat
//!   them as opaque whatever their operands; [decision compilation](super::super::decision) folds them inside the
//!   compiled program instead, where a new value is a new generation's program and nothing else.
//! - **A folded `split` keeps its routing.** Every record that reaches the `split` leaves by its live port, so the
//!   live port's target receives exactly what it received, in the same order, and the dead port's target nothing.
//!   The live port carries the input's whole type, so no port type downstream changes.
//! - **Only metrics reach a `split`.** A compiled decision sends an event other than a metric down every `split`'s
//!   `false` port ([`Decision::evaluate`][crate::compile::Decision::evaluate]), so folding a `split` to its `true`
//!   port would deliver such an event where the original dropped it, or sent it elsewhere. Port types describe metrics
//!   alone, and the interpreter refuses records outside them; the fold is sound because regions receive nothing else:
//!   a region host's input type is metrics, so a topology connects nothing else to it (pinned by
//!   `regions_receive_only_metrics_which_folding_a_split_to_its_true_port_relies_on`). A region that accepts other
//!   events would need R3 to fold to `true` only where its input provably carries metrics alone.
//! - **Only unreachable nodes are removed.** A node is removed when every one of its inputs is fed by the dead port
//!   or by another removed node, so nothing ever reaches it. Removing it changes no output.
//! - **Exits stay.** No source, fence (I/O or opaque host), stateful operator, or dynamic operator is removed: they
//!   may produce records without input, or are part of the frame. A `split` whose dead subgraph reaches one is not
//!   folded. No declared output is removed: an `emit` that nothing reaches goes, and its output stays declared, with
//!   the frame's exit to it connected and idle. Nodes that a scope's inputs or exits name are never removed or
//!   renumbered, and neither is the port a folded `split`'s live port leads to when a scope exit names it.
//! - **Dynamic operators are never removed or folded**: a signal reader may change what it does at any time.
//!
//! R3 does not rely on the capacity rule. Every port it keeps carries the records it carried, in the same order: the
//! live port's target receives the `split`'s input, and a renumbered `merge` keeps its live inputs in their order and
//! loses only inputs that carried nothing. The order in which an output receives records follows from that alone
//! ([delivery order][crate::interpret#delivery-order]), so the rewritten plan delivers exactly what the original
//! delivers, in the same order, and under any capacity bound refuses exactly what it refuses.
//!
//! # Telemetry
//!
//! A folded `split` no longer reports `plan_node_events_total` for its ports: what its live port counted is no longer
//! counted at any node. Removed nodes received nothing, so their series were zero: a removed `split`, `tee`, or kernel
//! no longer reports its per-port counts (and a kernel its loss), and a removed `drop` its policy loss. Native series a
//! host maps to those ports go with them.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;

use super::{Rewrite, RewriteContext, RewriteId, Rewritten, TelemetryChange, TelemetryEffect};
use crate::{
    ir::{Edge, Node, NodeId, ParamName, Plan, PortName, PortRef},
    model::{Kind, KindSet},
    operators::{names, Cardinality, StateContract, FALSE_PORT, TRUE_PORT},
    predicate::Predicate,
    validate::{
        graph::{Endpoint, Graph},
        ValidationReport,
    },
};

/// R3, in structure: folds constant `split`s and removes the subgraphs they make unreachable.
#[derive(Clone, Copy, Debug, Default)]
pub struct R3;

impl Rewrite for R3 {
    fn id(&self) -> RewriteId {
        RewriteId::R3
    }

    fn apply(&self, ctx: &RewriteContext<'_>, plan: &Plan) -> Option<Rewritten> {
        let mut findings = ValidationReport::default();
        let graph = Graph::build(plan, ctx.registry(), &mut findings);
        if !findings.is_valid() || graph.cyclic {
            return None;
        }
        let scoped = ScopePorts::of(plan);
        (0..graph.nodes.len()).find_map(|split| {
            let site = Site::find(ctx, &graph, &scoped, split)?;
            let rewritten = site.rewrite(plan, &graph)?;
            (!ctx.is_rejected(RewriteId::R3, &rewritten.nodes)).then_some(rewritten)
        })
    }
}

/// Returns the value literal constants alone give a predicate, if they decide it.
///
/// Every leaf other than `const` is undecided, including `flag` and `is_empty`, whatever their operands, and `kind_in`.
/// `all` and `any` follow three-valued logic.
fn literal_value(predicate: &Predicate) -> Option<bool> {
    match predicate {
        Predicate::Const(value) => Some(*value),
        Predicate::All(operands) => junction_value(operands, false),
        Predicate::Any(operands) => junction_value(operands, true),
        Predicate::Not(operand) => literal_value(operand).map(|value| !value),
        Predicate::Flag(_)
        | Predicate::IsEmpty(_)
        | Predicate::Name(_)
        | Predicate::IntakeName(_)
        | Predicate::Host(_)
        | Predicate::HostUnset {}
        | Predicate::HasTag(_)
        | Predicate::Tag(_)
        | Predicate::HasOriginTag(_)
        | Predicate::OriginTag(_)
        | Predicate::KindIn(_) => None,
    }
}

/// Returns the value of an `all` (`decisive` false) or `any` (`decisive` true) that literal constants decide.
fn junction_value(operands: &[Predicate], decisive: bool) -> Option<bool> {
    let mut decided = Some(!decisive);
    for operand in operands {
        match literal_value(operand) {
            Some(value) if value == decisive => return Some(decisive),
            Some(_) => {}
            None => decided = None,
        }
    }
    decided
}

/// The ports that a plan's scopes name: their inputs (output ports) and exits (input ports).
struct ScopePorts<'p> {
    ports: BTreeSet<&'p PortRef>,
    nodes: BTreeSet<&'p NodeId>,
}

impl<'p> ScopePorts<'p> {
    fn of(plan: &'p Plan) -> Self {
        let ports = plan
            .scopes
            .values()
            .flat_map(|scope| scope.inputs.values().chain(scope.exits.values()))
            .collect::<BTreeSet<_>>();
        let nodes = ports.iter().map(|port| &port.node).collect();
        Self { ports, nodes }
    }

    fn names_node(&self, node: &NodeId) -> bool {
        self.nodes.contains(node)
    }

    fn names_port(&self, port: &PortRef) -> bool {
        self.ports.contains(port)
    }
}

/// Why a `split` is constant.
#[derive(Clone, Copy, Debug)]
enum Constant {
    /// Its predicate's literal constants decide it.
    Literal { predicate_is_const: bool },

    /// Its predicate has one value for every kind its input can carry.
    Kinds(KindSet),
}

/// One application of R3: a constant `split`, and what folding it removes and changes.
struct Site {
    /// The `split`, by position.
    split: usize,

    /// The port every record leaves by.
    live: usize,

    /// The port that carries nothing.
    dead: usize,

    /// The value of the predicate for every record that reaches the `split`.
    value: bool,

    /// Why the `split` is constant.
    constant: Constant,

    /// The nodes nothing reaches once the `split` is folded, by position.
    removed: BTreeSet<usize>,

    /// The `merge`s the removed subgraph feeds that stay, each with its input ports that lose their edges.
    merges: BTreeMap<usize, BTreeSet<usize>>,
}

impl Site {
    /// Returns the application at `split`, if it is a constant `split` whose obligations hold.
    fn find(ctx: &RewriteContext<'_>, graph: &Graph<'_>, scoped: &ScopePorts<'_>, split: usize) -> Option<Self> {
        let op = graph.op(split)?;
        if op.name() != names::SPLIT || op.contract().is_dynamic() {
            return None;
        }
        let id = graph.id(split);
        if scoped.names_node(id) {
            return None;
        }
        let predicate = op.params().predicate("predicate")?;
        let true_port = op.ports().output(&PortName::from_static(TRUE_PORT))?;
        let false_port = op.ports().output(&PortName::from_static(FALSE_PORT))?;

        let kinds_of = |node: usize, port: usize| {
            let name = op_output_name(graph, node, port)?;
            ctx.report()
                .port_type(&PortRef::new(graph.id(node).clone(), name))
                .map(|port_type| port_type.kinds)
        };
        let (from, from_port) = graph.source(split, 0)?;
        let input = kinds_of(from, from_port)?;
        if input.is_empty() {
            // Nothing reaches the `split`: it is part of a dead subgraph that R3 cannot remove, below a port of another
            // operator that carries nothing.
            return None;
        }
        let (live, dead, value) = if kinds_of(split, true_port)?.is_empty() {
            (false_port, true_port, false)
        } else if kinds_of(split, false_port)?.is_empty() {
            (true_port, false_port, true)
        } else {
            return None;
        };
        let constant = match literal_value(predicate) {
            Some(literal) if literal == value => Constant::Literal {
                predicate_is_const: matches!(predicate.canonical(), Predicate::Const(_)),
            },
            _ => Constant::Kinds(input),
        };

        // The nodes every one of whose inputs the dead port, or a node removed already, feeds. In topological order,
        // every node's inputs are decided before it is.
        let mut dead_inputs = BTreeSet::from([graph.target(split, dead)?]);
        let mut removed = BTreeSet::new();
        for &node in &graph.order {
            let Some(op) = graph.op(node) else { continue };
            let inputs = op.ports().inputs.len();
            if node == split || inputs == 0 || !(0..inputs).all(|port| dead_inputs.contains(&(node, port))) {
                continue;
            }
            let contract = op.contract();
            let removable = !contract.boundary.is_fence()
                && contract.state == StateContract::None
                && !contract.is_dynamic()
                && contract.cardinality != Cardinality::Source
                && !scoped.names_node(graph.id(node));
            if !removable {
                return None;
            }
            removed.insert(node);
            dead_inputs.extend((0..op.ports().outputs.len()).filter_map(|port| graph.target(node, port)));
        }

        // Every other node that loses an input must be a `merge` that keeps another.
        let mut merges = BTreeMap::<usize, BTreeSet<usize>>::new();
        for &(node, port) in &dead_inputs {
            if removed.contains(&node) {
                continue;
            }
            let op = graph.op(node)?;
            if op.name() != names::MERGE || scoped.names_node(graph.id(node)) {
                return None;
            }
            merges.entry(node).or_default().insert(port);
        }

        let (target, target_port) = graph.target(split, live)?;
        let target_port = PortRef::new(graph.id(target).clone(), input_name(graph, (target, target_port))?);
        if scoped.names_port(&target_port) {
            return None;
        }

        Some(Self {
            split,
            live,
            dead,
            value,
            constant,
            removed,
            merges,
        })
    }

    /// Returns the plan with the `split` folded and the unreachable subgraph removed, and what its record says.
    fn rewrite(&self, plan: &Plan, graph: &Graph<'_>) -> Option<Rewritten> {
        let split = graph.id(self.split);
        let removed = self.removed.iter().map(|&node| graph.id(node)).collect::<BTreeSet<_>>();

        // Each kept `merge` input port's new name, by its old one: its live inputs, renumbered in order.
        let mut renamed = BTreeMap::<(&NodeId, &PortName), PortName>::new();
        let mut inputs = BTreeMap::<&NodeId, usize>::new();
        for (&merge, dead) in &self.merges {
            let op = graph.op(merge)?;
            let live = (0..op.ports().inputs.len())
                .filter(|port| !dead.contains(port))
                .collect::<Vec<_>>();
            for (index, &port) in live.iter().enumerate() {
                let name = PortName::index(u16::try_from(index).ok()?)?;
                renamed.insert((graph.id(merge), &op.ports().inputs[port].name), name);
            }
            inputs.insert(graph.id(merge), live.len());
        }
        let rename = |port: &PortRef| PortRef {
            node: port.node.clone(),
            port: renamed
                .get(&(&port.node, &port.port))
                .cloned()
                .unwrap_or_else(|| port.port.clone()),
        };

        let inputs_param = ParamName::new("inputs").ok()?;
        let nodes = plan
            .nodes
            .iter()
            .filter(|node| &node.id != split && !removed.contains(&node.id))
            .map(|node| match inputs.get(&node.id) {
                Some(&count) => {
                    let mut params = node.params.clone();
                    params.insert(inputs_param.clone(), Value::from(count));
                    Node { params, ..node.clone() }
                }
                None => node.clone(),
            })
            .collect();

        let live_target = graph.target(self.split, self.live)?;
        let live_target = PortRef::new(graph.id(live_target.0).clone(), input_name(graph, live_target)?);
        let mut edges = Vec::with_capacity(plan.edges.len());
        for edge in &plan.edges {
            if &edge.to.node == split {
                // The `split` becomes an edge from its input to its live port's target.
                edges.push(Edge {
                    from: edge.from.clone(),
                    to: rename(&live_target),
                });
            } else if &edge.from.node == split || removed.contains(&edge.from.node) || removed.contains(&edge.to.node) {
                continue;
            } else {
                edges.push(Edge {
                    from: edge.from.clone(),
                    to: rename(&edge.to),
                });
            }
        }

        let rewritten = Plan {
            ir_version: plan.ir_version,
            provenance: plan.provenance.clone(),
            nodes,
            edges,
            outputs: plan.outputs.clone(),
            scopes: plan.scopes.clone(),
        };
        Some(Rewritten {
            plan: rewritten,
            nodes: self.nodes(graph),
            justification: self.justification(graph)?,
            telemetry: self.telemetry(graph),
            capacity_rule: false,
        })
    }

    /// Returns the nodes the application folds, removes, or changes, in plan order.
    fn nodes(&self, graph: &Graph<'_>) -> Vec<NodeId> {
        let mut positions = self
            .removed
            .iter()
            .chain(self.merges.keys())
            .copied()
            .collect::<Vec<_>>();
        positions.push(self.split);
        positions.sort_unstable();
        positions.into_iter().map(|node| graph.id(node).clone()).collect()
    }

    fn justification(&self, graph: &Graph<'_>) -> Option<String> {
        let split = graph.id(self.split);
        let (from, from_port) = graph.source(self.split, 0)?;
        let input = PortRef::new(graph.id(from).clone(), op_output_name(graph, from, from_port)?);
        let live_name = op_output_name(graph, self.split, self.live)?;
        let dead_name = op_output_name(graph, self.split, self.dead)?;
        let live_target = graph.target(self.split, self.live)?;
        let live_target = PortRef::new(graph.id(live_target.0).clone(), input_name(graph, live_target)?);

        let constant = match self.constant {
            Constant::Literal {
                predicate_is_const: true,
            } => format!("the predicate of `{}` is the constant `{}`", split, self.value),
            Constant::Literal {
                predicate_is_const: false,
            } => format!(
                "the literal constants of the predicate of `{}` decide it `{}` (`flag` and `is_empty` are not \
                 folded in structure)",
                split, self.value
            ),
            Constant::Kinds(kinds) => format!(
                "the predicate of `{}` is `{}` for every kind its input can carry ({})",
                split,
                self.value,
                kinds.iter().map(Kind::as_str).collect::<Vec<_>>().join(", ")
            ),
        };
        let mut parts = vec![format!(
            "constant: {}, so every record leaves by `{}`, and `{}` carries nothing; the split becomes an edge from \
             `{}` to `{}`",
            constant, live_name, dead_name, input, live_target
        )];

        let removed = self.removed.iter().map(|&node| format!("`{}`", graph.id(node)));
        let removed = removed.collect::<Vec<_>>();
        parts.push(match removed.len() {
            0 => "unreachable: nothing else is removed".to_string(),
            1 => format!(
                "unreachable: {} receives records only through `{}.{}`, so nothing reaches it",
                removed[0], split, dead_name
            ),
            _ => format!(
                "unreachable: {} receive records only through `{}.{}`, so nothing reaches them",
                removed.join(", "),
                split,
                dead_name
            ),
        });
        if !self.merges.is_empty() {
            let merges = self.merges.keys().map(|&node| format!("`{}`", graph.id(node)));
            let merges = merges.collect::<Vec<_>>();
            parts.push(if merges.len() == 1 {
                format!("merges: {} keeps its other inputs, renumbered in order", merges[0])
            } else {
                format!(
                    "merges: {} keep their other inputs, renumbered in order",
                    merges.join(", ")
                )
            });
        }
        let emits = self
            .removed
            .iter()
            .filter(|&&node| graph.op(node).is_some_and(|op| op.name() == names::EMIT))
            .count();
        parts.push(format!(
            "exits stay: no source, fence, stateful or dynamic operator, scope port, or declared output is removed{}",
            if emits > 0 {
                "; the outputs of removed `emit`s stay declared, and their exits connected and idle"
            } else {
                ""
            }
        ));
        parts.push(
            "capacity: not relied on; every port left carries what it carried, in the same order, so the plan \
             delivers, and under any capacity bound refuses, exactly what it did"
                .to_string(),
        );
        Some(parts.join("; "))
    }

    fn telemetry(&self, graph: &Graph<'_>) -> Vec<TelemetryEffect> {
        let mut positions = self.removed.iter().copied().collect::<Vec<_>>();
        positions.push(self.split);
        positions.sort_unstable();
        positions
            .into_iter()
            .filter_map(|node| {
                let op = graph.op(node)?;
                let id = graph.id(node);
                let detail = if node == self.split {
                    format!(
                        "`plan_node_events_total{{component_id:{}}}` for ports `true` and `false`: the split is \
                         folded, and what its live port counted is counted at no node",
                        id
                    )
                } else {
                    match op.name() {
                        names::SPLIT | names::TEE => format!(
                            "`plan_node_events_total{{component_id:{}}}` for every port, zero before: nothing reaches \
                             it",
                            id
                        ),
                        names::DROP => format!(
                            "`plan_loss_total{{component_id:{}, class:policy}}`, zero before: nothing reaches it",
                            id
                        ),
                        names::EMIT | names::MERGE => return None,
                        _ => format!(
                            "`plan_node_events_total{{component_id:{}}}` for every port, and its `plan_loss_total` \
                             series, zero before: nothing reaches it",
                            id
                        ),
                    }
                };
                Some(TelemetryEffect {
                    node: id.clone(),
                    change: TelemetryChange::Removed,
                    detail,
                })
            })
            .collect()
    }
}

/// Returns the name of one output port of a node.
fn op_output_name(graph: &Graph<'_>, node: usize, port: usize) -> Option<PortName> {
    Some(graph.op(node)?.ports().outputs.get(port)?.name.clone())
}

/// Returns the name of one input port of a node.
fn input_name(graph: &Graph<'_>, (node, port): Endpoint) -> Option<PortName> {
    Some(graph.op(node)?.ports().inputs.get(port)?.name.clone())
}

#[cfg(test)]
mod tests;
