//! R5a: push a filter upstream across a stateless operator.
//!
//! A **filter** is an operator with `filter` cardinality, or a `split` whose one port leads directly to `drop`.
//!
//! # Obligations
//!
//! For a filter `F` directly below a stateless operator `O`:
//!
//! - **Reads and writes are disjoint for the kinds involved.** For each kind `k` that can reach `O`, with `O`'s
//!   effect contract for `k`: `F.reads(k) ∩ O.writes(k) = ∅`, and `O` keeps the kind (it neither writes `kind` for
//!   `k` nor maps `k` to another kind on its output). The filter then reads, for every record of kind `k`, the same
//!   fields above `O` as below it, so it keeps exactly the records whose outputs it kept below `O`, in their order.
//! - **The filter applies to every consumer of `O`'s output.** `O` has exactly one output port, which leads to `F`
//!   (every port has one edge): a filter that only one branch below `O` applies cannot move above `O`, where it would
//!   apply to every branch.
//! - **`O` is stateless and plain.** `O` is synchronous (never an `io_host` or `opaque_host`: a filter never moves
//!   across a fence or into a host), keeps no state (R5b), reads no signal, is order-independent, is a map (one
//!   record for each record received: above an expansion, the filter and its `drop` would count one record where they
//!   counted each record it made), and carries its input's points in the form they arrived in. It is not itself a
//!   filter, so two filters never trade places forever. A filter operator is order-independent too.
//! - **Partial push-down.** When the first obligation holds only for some kinds, the rewrite inserts above `O` a copy
//!   of `F` restricted to those kinds (it passes every other kind through) and keeps `F` below `O`, unchanged, as the
//!   residual, which still checks the kinds the copy does not. A `split` filter is restricted by its predicate: a
//!   copy that drops its `true` port gets `all(kind_in(K), p)`, one that drops its `false` port `any(kind_in(not K),
//!   p)`. A filter operator cannot be restricted to kinds without a `split` and a `merge` around it, which would
//!   reorder the records of one batch, so it is only ever pushed for every kind. No copy is inserted where it could
//!   drop nothing, or where one that covers it already applies to every record reaching `O`: a filter above that
//!   removes what `p` removes from at least the kinds `K`, such as the copy an earlier application inserted, wherever
//!   it has moved since. That copy narrows what reaches `O`, so a second application would restrict a copy to fewer
//!   kinds, which the first one covers.
//! - **Across a merge**, a per-record union of its inputs, the filter moves into the input branches of the merge, a
//!   copy each, since every branch's records reach it; the merge changes nothing, so this always holds for every
//!   kind. An input of whose kinds the filter can remove nothing keeps its edge, without a copy. Only a `merge` is
//!   crossed this way: for another operator with several inputs, filtering each input need not filter its output.
//! - **Native filters move only whole.** A host reports native series under a native node's ID, such as a
//!   filterlist's filtered-metrics counter on one port of its `split`; copies would not carry them. A filter with
//!   native provenance is therefore never copied (neither across a merge nor partially): it is only moved, whole,
//!   keeping its ID and its counts exactly.
//! - **Scopes.** The filter never crosses a scope's input or exit, and no port of its own nodes is one: a rewrite
//!   moves no work out of or into the part of the plan an artifact replaces, and never removes a node a scope names.
//! - **The frame is fixed.** The pipeline checks that the rewritten plan places into the boot frame, with every
//!   kernel in the slot it ran in before, and skips the application, recording why, if it does not.
//! - **Dynamic operators** are never moved, and no filter moves across one.
//!
//! Moving a filter above `O` changes the per-port counts of `O` (a kernel's, or a one-port `tee`'s), and copies add
//! and remove per-port and policy-loss series, which the record lists. Since no rewrite crosses a fence, every record
//! reaching an output does so in the same order through the rewritten plan as through the original, and an output
//! with a bounded capacity accepts the same records. An operator that can lose records to capacity processes only
//! what the filter keeps once the filter runs first, which can only reduce its capacity loss: such applications rely
//! on the capacity rule, and the filter's counts then include what `O` sheds. No native stateless operator declares
//! capacity loss.

use std::fmt::Write as _;

use super::{
    filters::{
        applied_above, commuting_kinds, drop_condition, fields, input_kinds, input_ref, kinds, output_ref,
        reaching_kinds, restricted, uncrossable, Edit, Filter, FilterForm, FilterPorts, ScopeBoundaries,
    },
    Rewrite, RewriteContext, RewriteId, Rewritten, TelemetryChange, TelemetryEffect,
};
use crate::{
    ir::{NodeId, Plan},
    model::KindSet,
    operators::{names, LossClass},
    validate::{graph::Graph, ValidationReport},
};

/// R5a: pushes a filter upstream across a stateless operator, per kind.
#[derive(Clone, Copy, Debug, Default)]
pub struct R5a;

impl Rewrite for R5a {
    fn id(&self) -> RewriteId {
        RewriteId::R5a
    }

    fn apply(&self, ctx: &RewriteContext<'_>, plan: &Plan) -> Option<Rewritten> {
        let mut findings = ValidationReport::default();
        let graph = Graph::build(plan, ctx.registry(), &mut findings);
        if !findings.is_valid() || graph.cyclic {
            return None;
        }
        let scopes = ScopeBoundaries::of(plan);
        (0..graph.nodes.len())
            .filter_map(|node| Filter::at(&graph, node))
            .filter_map(|filter| Site::find(ctx.report(), &graph, &scopes, filter))
            .filter_map(|site| site.rewrite(plan, &graph))
            .find(|rewritten| !ctx.is_rejected(RewriteId::R5a, &rewritten.nodes))
    }
}

/// How a filter moves above the operator it is below.
enum Push {
    /// The whole filter moves above an operator with one input.
    Whole,

    /// A copy of the filter moves into the inputs of a `merge` whose records it could remove (these, in input order),
    /// and the filter goes.
    IntoInputs(Vec<usize>),

    /// A copy restricted to the kinds that commute moves above an operator with one input, and the filter stays.
    Partial,
}

/// What one way of pushing a filter did to the plan, for its record.
struct Edited {
    /// What it did, for the justification.
    action: String,

    /// What it changes in internal telemetry.
    telemetry: Vec<TelemetryEffect>,

    /// The nodes it inserted, in plan order.
    inserted: Vec<NodeId>,
}

/// One site where R5a applies: a filter, the operator above it, and how the filter moves.
struct Site {
    filter: Filter,
    above: usize,
    reaching: KindSet,
    commuting: KindSet,
    push: Push,
}

impl Site {
    /// Returns the site of `filter`, if R5a's obligations hold for it.
    fn find(report: &ValidationReport, graph: &Graph<'_>, scopes: &ScopeBoundaries, filter: Filter) -> Option<Self> {
        if filter.is_dynamic(graph) {
            return None;
        }
        let (above, port) = graph.source(filter.node, 0)?;
        let op = graph.op(above)?;
        if uncrossable(op).is_some() {
            return None;
        }
        let inputs = op.ports().inputs.len();
        // The filter must not leave the scope it is in, nor join one: between `O` and the filter is no scope's input
        // or exit, and no input of `O` is an exit, where the filter would join the scope above it. An input of `O`
        // may be a scope's input: the filter then stays first in that scope. No port of the filter's own nodes is a
        // boundary either: `O` would move below a scope input at the filter's kept port, into the scope, and a filter
        // the rewrite removes would leave a scope naming a port the plan no longer has.
        if scopes.crossed_by(&output_ref(graph, (above, port)), &input_ref(graph, (filter.node, 0)))
            || (0..inputs).any(|input| scopes.is_exit(&input_ref(graph, (above, input))))
            || scopes.bound_at(graph, &filter)
        {
            return None;
        }

        let reaching = reaching_kinds(graph, report, above);
        let commuting = commuting_kinds(&filter.reads(graph), op, reaching);
        if commuting.is_empty() {
            return None;
        }
        let push = if commuting == reaching {
            if inputs == 1 {
                Push::Whole
            } else if filter.is_native(graph) {
                return None;
            } else {
                // `uncrossable` admits several inputs only for a `merge`. A copy goes only on the inputs whose records
                // the filter could remove: every record of another input passes it.
                let condition = filter.drop_condition();
                let copied = (0..inputs)
                    .filter(|&input| {
                        condition.as_ref().is_none_or(|condition| {
                            !condition
                                .kinds_where(true, input_kinds(graph, report, above, input))
                                .is_empty()
                        })
                    })
                    .collect::<Vec<_>>();
                if copied.is_empty() {
                    return None;
                }
                Push::IntoInputs(copied)
            }
        } else {
            let FilterForm::Split {
                predicate, drops_true, ..
            } = &filter.form
            else {
                return None;
            };
            if inputs != 1 || filter.is_native(graph) {
                return None;
            }
            let raw = graph.nodes[filter.node].node.params.get("predicate")?;
            let (copy, _) = restricted(predicate, raw, *drops_true, commuting);
            let condition = drop_condition(&copy, *drops_true);
            // A copy that could drop nothing would only add work.
            if condition.kinds_where(true, commuting).is_empty() {
                return None;
            }
            // A copy that covers these kinds may already be above: the first application's, restricted to more kinds
            // than this one would be, since its kept port narrows what reaches `O`.
            let removes = drop_condition(predicate, *drops_true);
            if applied_above(graph, report, above, &removes, commuting, removes.reads()) {
                return None;
            }
            Push::Partial
        };
        Some(Self {
            filter,
            above,
            reaching,
            commuting,
            push,
        })
    }

    /// Returns the rewritten plan, or `None` if the plan is not shaped as a valid plan is.
    fn rewrite(&self, plan: &Plan, graph: &Graph<'_>) -> Option<Rewritten> {
        let op = graph.op(self.above)?;
        let mut sites = self.filter.nodes();
        sites.push(self.above);
        sites.sort_unstable();
        let mut nodes = sites.into_iter().map(|node| graph.id(node).clone()).collect::<Vec<_>>();

        let mut edit = Edit::of(plan);
        let edited = match &self.push {
            Push::Whole => self.move_whole(graph, &mut edit),
            Push::IntoInputs(copied) => self.copy_into_inputs(graph, &mut edit, copied),
            Push::Partial => self.copy_restricted(graph, &mut edit),
        }?;
        nodes.extend(edited.inserted);
        Some(Rewritten {
            plan: edit.finish(),
            nodes,
            justification: self.justification(graph, &edited.action),
            telemetry: edited.telemetry,
            capacity_rule: op.contract().loss.has_class(LossClass::Capacity),
        })
    }

    /// Moves the filter above `O`, which has one input: `X -> O -> F -> Y` becomes `X -> F -> O -> Y`.
    fn move_whole(&self, graph: &Graph<'_>, edit: &mut Edit) -> Option<Edited> {
        let filter = &self.filter;
        let ports = FilterPorts::of(graph, filter);
        let (f, o) = (graph.id(filter.node), graph.id(self.above));
        let (o_in, o_out) = (input_ref(graph, (self.above, 0)), output_ref(graph, (self.above, 0)));
        let from = output_ref(graph, graph.source(self.above, 0)?);
        let to = input_ref(graph, graph.target(filter.node, filter.keep_port())?);

        let into_o = edit.edge_into(&o_in)?;
        let below_o = edit.edge_from(&o_out)?;
        let kept = edit.edge_from(&ports.keep(f))?;
        edit.set_edge(into_o, from, ports.input(f));
        edit.set_edge(below_o, ports.keep(f), o_in);
        edit.set_edge(kept, o_out, to);
        let moved = filter
            .nodes()
            .into_iter()
            .map(|node| graph.id(node))
            .map(|id| edit.copy_node(id, id.clone()))
            .collect::<Option<Vec<_>>>()?;
        edit.place_before(o, moved);

        let mut telemetry = self.changed_filter(graph);
        telemetry.extend(self.changed_operator(graph));
        Some(Edited {
            action: format!("moved `{}` whole above `{}`", f, o),
            telemetry,
            inserted: Vec::new(),
        })
    }

    /// Replaces the filter below `O`, a `merge`, by a copy on each of the inputs `copied`: `X_i -> O.i, O -> F -> Y`
    /// becomes `X_i -> F_i -> O.i, O -> Y`. Every other input's records all pass the filter, and keep their edge.
    fn copy_into_inputs(&self, graph: &Graph<'_>, edit: &mut Edit, copied: &[usize]) -> Option<Edited> {
        let filter = &self.filter;
        let ports = FilterPorts::of(graph, filter);
        let (f, d, o) = (
            graph.id(filter.node),
            filter.drop().map(|drop| graph.id(drop)),
            graph.id(self.above),
        );
        let o_out = output_ref(graph, (self.above, 0));
        let to = input_ref(graph, graph.target(filter.node, filter.keep_port())?);
        let inputs = graph.op(self.above)?.ports().inputs.len();

        let below_o = edit.edge_from(&o_out)?;
        edit.set_edge(below_o, o_out, to);
        let mut telemetry = vec![TelemetryEffect {
            node: f.clone(),
            change: TelemetryChange::Removed,
            detail: "`plan_node_events_total`: the copies on each input report its counts between them".to_string(),
        }];
        if let Some(d) = d {
            telemetry.push(TelemetryEffect {
                node: d.clone(),
                change: TelemetryChange::Removed,
                detail: "`plan_loss_total`: the copies' drops report its policy loss between them".to_string(),
            });
        }

        let (mut copies, mut inserted, mut rewired) = (Vec::new(), Vec::new(), Vec::new());
        for &input in copied {
            let o_in = input_ref(graph, (self.above, input));
            let from = output_ref(graph, graph.source(self.above, input)?);
            let copy = edit.fresh_id(f, "r5a")?;
            copies.push(edit.copy_node(f, copy.clone())?);
            let drop = match d {
                Some(d) => {
                    let drop = edit.fresh_id(d, "r5a")?;
                    copies.push(edit.copy_node(d, drop.clone())?);
                    Some(drop)
                }
                None => None,
            };
            let mut edges = vec![(ports.keep(&copy), o_in.clone())];
            edges.extend(ports.drop_edge(&copy, drop.as_ref()));
            rewired.push((edit.edge_into(&o_in)?, from, ports.input(&copy), edges));

            telemetry.push(TelemetryEffect {
                node: copy.clone(),
                change: TelemetryChange::Added,
                detail: format!(
                    "`plan_node_events_total`: the per-port counts of the copy on input `{}` of `{}`",
                    o_in.port, o
                ),
            });
            inserted.push(copy);
            if let Some(drop) = drop {
                telemetry.push(TelemetryEffect {
                    node: drop.clone(),
                    change: TelemetryChange::Added,
                    detail: format!(
                        "`plan_loss_total`: the policy loss of the copy on input `{}`",
                        o_in.port
                    ),
                });
                inserted.push(drop);
            }
        }
        // From the last edge back, so that inserting edges leaves the positions still to rewire where they are.
        rewired.sort_by_key(|(position, ..)| std::cmp::Reverse(*position));
        for (position, from, to, edges) in rewired {
            edit.set_edge(position, from, to);
            edit.insert_edges(position, edges);
        }
        edit.place_before(o, copies);
        edit.remove_nodes(&std::iter::once(f.clone()).chain(d.cloned()).collect::<Vec<_>>());

        let action = if copied.len() == inputs {
            format!(
                "copied `{}` into each of the {} inputs of `{}`, and removed it below",
                f, inputs, o
            )
        } else {
            let names = copied
                .iter()
                .map(|&input| format!("`{}`", input_ref(graph, (self.above, input)).port))
                .collect::<Vec<_>>();
            format!(
                "copied `{}` into the inputs {} of the {} inputs of `{}`, and removed it below; it removes no record \
                 of the kinds the others carry",
                f,
                names.join(", "),
                inputs,
                o
            )
        };
        Some(Edited {
            action,
            telemetry,
            inserted,
        })
    }

    /// Inserts above `O`, which has one input, a copy of the filter restricted to the kinds that commute, keeping the
    /// filter as the residual: `X -> O -> F -> Y` becomes `X -> F' -> O -> F -> Y`.
    fn copy_restricted(&self, graph: &Graph<'_>, edit: &mut Edit) -> Option<Edited> {
        let filter = &self.filter;
        let FilterForm::Split {
            predicate, drops_true, ..
        } = &filter.form
        else {
            return None;
        };
        let ports = FilterPorts::of(graph, filter);
        let (f, d, o) = (
            graph.id(filter.node),
            filter.drop().map(|drop| graph.id(drop)),
            graph.id(self.above),
        );
        let o_in = input_ref(graph, (self.above, 0));
        let from = output_ref(graph, graph.source(self.above, 0)?);
        let raw = graph.nodes[filter.node].node.params.get("predicate")?;
        let (_, raw) = restricted(predicate, raw, *drops_true, self.commuting);

        let copy = edit.fresh_id(f, "r5a")?;
        let mut copies = vec![Edit::with_param(edit.copy_node(f, copy.clone())?, "predicate", raw)?];
        let drop = match d {
            Some(d) => {
                let drop = edit.fresh_id(d, "r5a")?;
                copies.push(edit.copy_node(d, drop.clone())?);
                Some(drop)
            }
            None => None,
        };
        let into_o = edit.edge_into(&o_in)?;
        edit.set_edge(into_o, from, ports.input(&copy));
        let mut edges = vec![(ports.keep(&copy), o_in)];
        edges.extend(ports.drop_edge(&copy, drop.as_ref()));
        edit.insert_edges(into_o, edges);
        edit.place_before(o, copies);

        let restricted_to = kinds(self.commuting);
        let mut telemetry = vec![TelemetryEffect {
            node: f.clone(),
            change: TelemetryChange::Changed,
            detail: format!(
                "`plan_node_events_total`: its drop port no longer sees the records of {} that the copy `{}` drops \
                 first",
                restricted_to, copy
            ),
        }];
        if let Some(d) = d {
            telemetry.push(TelemetryEffect {
                node: d.clone(),
                change: TelemetryChange::Changed,
                detail: format!(
                    "`plan_loss_total`: the policy loss of records of {} moves to the copy's drop",
                    restricted_to
                ),
            });
        }
        telemetry.extend(self.changed_operator(graph));
        telemetry.push(TelemetryEffect {
            node: copy.clone(),
            change: TelemetryChange::Added,
            detail: format!(
                "`plan_node_events_total`: the per-port counts of the copy restricted to {}",
                restricted_to
            ),
        });
        let mut inserted = vec![copy];
        if let Some(drop) = drop {
            telemetry.push(TelemetryEffect {
                node: drop.clone(),
                change: TelemetryChange::Added,
                detail: format!(
                    "`plan_loss_total`: the policy loss of the copy restricted to {}",
                    restricted_to
                ),
            });
            inserted.push(drop);
        }

        Some(Edited {
            action: format!(
                "inserted above `{}` a copy of `{}` restricted to {}, keeping `{}` below as the residual",
                o, f, restricted_to, f
            ),
            telemetry,
            inserted,
        })
    }

    /// Returns the telemetry effect on `O` of a filter moving above it, if `O` reports series of its own, whose counts
    /// then count only what the filter keeps: a kernel (its per-port counts and its own series) or a decision, a
    /// one-port `tee` (its per-port counts). A `merge` reports none.
    fn changed_operator(&self, graph: &Graph<'_>) -> Option<TelemetryEffect> {
        let op = graph.op(self.above)?;
        let detail = if op.kernel().is_some() {
            "`plan_node_events_total` and the kernel's own series count only the records the filter keeps, since the \
             filter now runs first"
        } else if matches!(op.name(), names::TEE | names::SPLIT) {
            "`plan_node_events_total`: its port counts only the records the filter keeps, since the filter now runs \
             first"
        } else {
            return None;
        };
        Some(TelemetryEffect {
            node: graph.id(self.above).clone(),
            change: TelemetryChange::Changed,
            detail: detail.to_string(),
        })
    }

    /// Returns the telemetry effects on the filter, moved whole above `O`, if `O` can lose records to capacity: the
    /// filter, and its `drop`, then also count the records `O` sheds, which reach the filter first.
    fn changed_filter(&self, graph: &Graph<'_>) -> Vec<TelemetryEffect> {
        let sheds = graph
            .op(self.above)
            .is_some_and(|op| op.contract().loss.has_class(LossClass::Capacity));
        if !sheds {
            return Vec::new();
        }
        let o = graph.id(self.above);
        let mut effects = vec![TelemetryEffect {
            node: graph.id(self.filter.node).clone(),
            change: TelemetryChange::Changed,
            detail: format!(
                "`plan_node_events_total`: it also counts the records `{}` loses to capacity, which now reach it first",
                o
            ),
        }];
        if let Some(drop) = self.filter.drop() {
            effects.push(TelemetryEffect {
                node: graph.id(drop).clone(),
                change: TelemetryChange::Changed,
                detail: format!(
                    "`plan_loss_total`: its policy loss includes records `{}` lost to capacity before, which `{}` no \
                     longer counts",
                    o, o
                ),
            });
        } else {
            let _ = write!(
                effects[0].detail,
                "; `plan_loss_total`: its policy loss includes records `{}` lost to capacity before",
                o
            );
        }
        effects
    }

    /// Returns the record's justification: each obligation and how it holds, then what the rewrite did.
    fn justification(&self, graph: &Graph<'_>, action: &str) -> String {
        let filter = &self.filter;
        let o = graph.id(self.above);
        let op = graph.op(self.above).map_or("?", |op| op.name());
        let reads = filter.reads(graph).over(self.reaching);
        let writes = graph
            .op(self.above)
            .map(|op| op.contract().writes.over(self.commuting))
            .unwrap_or_default();
        let writes = if writes.is_empty() {
            "no field".to_string()
        } else {
            fields(writes)
        };
        let mut text = format!(
            "reads and writes: {} reads {}, and for {} `{}` (`{}`) writes {} and keeps the kind",
            filter.describe(graph),
            fields(reads),
            kinds(self.commuting),
            o,
            op,
            writes,
        );
        let others = self.reaching.difference(self.commuting);
        if !others.is_empty() {
            let _ = write!(
                text,
                "; for {} it writes a field the filter reads or changes the kind, so the filter is pushed for {} only",
                kinds(others),
                kinds(self.commuting)
            );
        }
        let _ = write!(
            text,
            "; every consumer: `{}` has one output port, and it leads to the filter; stateless: `{}` is a synchronous \
             map, keeps no state, reads no signal, and is order-independent",
            o, o
        );
        let _ = write!(text, "; {}", action);
        text
    }
}

#[cfg(test)]
mod tests;
