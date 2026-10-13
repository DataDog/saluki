//! The memory a region driver declares: its kernels', by their operators, its decisions' predicates, the buffers it
//! holds between steps and, at keyed exits, across calls, and what its decisions keep: their memos, their counts per
//! exit set, and the scratch a decision step reuses.

use saluki_core::{
    accounting::{MemoryBounds, MemoryBoundsBuilder, UsageExpr},
    data_model::event::Event,
    topology::EventsBuffer,
};

use super::{driver::routed_buffer_events, RegionError};
use crate::{
    compile::{
        DecisionMemo, ExitRouting, ExitSet, ExitSetCounts, RegionProgram, SharedPredicate, Slot, COUNTED_EXIT_SETS,
    },
    ir::{Namespace, NodeId, Plan},
    limits,
    operators::{ConfiguredOperator, OperatorRegistry, MAX_HISTOGRAM_SERIES},
};

/// Returns the name, under its region's component, of the subcomponent whose bounds and resource group a kernel's
/// node is accounted under: the node's local identifier for a reserved node, and `policy` for every node in the policy
/// namespace, which share one.
pub fn kernel_accounting_name(node: &NodeId) -> String {
    match node.namespace() {
        Namespace::Reserved => node.local().to_string(),
        Namespace::Policy => "policy".to_string(),
    }
}

/// Returns the name, under the component of the isolation slot `region`, of the subcomponent a state owner's kernel is
/// accounted under, or `None` if the kernel is accounted in the region's own group, with its bounds at the region's
/// top level.
///
/// An isolation slot named after the reserved state owner it runs stands for that node's native component, as the
/// isolation slot of aggregation stands for the native `dsd_agg` transform: its kernel keeps that component's resource
/// group and bounds path, such as `dsd_agg.aggregate_context_limit`. Every other kernel is accounted under
/// [`kernel_accounting_name`]. Fused slots account every kernel under [`kernel_accounting_name`], even one named
/// after the node it runs, as host enrichment's `metrics_enrich` is; [`kernel_accounting_in`] applies both rules.
pub fn kernel_accounting_path(region: &str, node: &NodeId) -> Option<String> {
    match node.namespace() {
        Namespace::Reserved if node.local().as_str() == region => None,
        _ => Some(kernel_accounting_name(node)),
    }
}

/// Returns the name, under the component of `slot`'s region, of the subcomponent the kernel of `node` is accounted
/// under, or `None` if it is accounted in the region's own group: [`kernel_accounting_path`] in an isolation slot,
/// and [`kernel_accounting_name`] in a fused one.
pub fn kernel_accounting_in(slot: &Slot, node: &NodeId) -> Option<String> {
    if slot.is_isolation() {
        kernel_accounting_path(slot.name.as_str(), node)
    } else {
        Some(kernel_accounting_name(node))
    }
}

/// The memory one predicate of a decision holds.
#[derive(Clone, Debug)]
struct PredicateBounds {
    /// The first `split` using it, whose subcomponent it is accounted under.
    node: NodeId,

    /// The predicate's values ([`Predicate::value_bytes`][crate::predicate::Predicate::value_bytes]).
    values: usize,

    /// Its compiled matchers' entries
    /// ([`CompiledPredicate::matcher_bytes`][crate::predicate::CompiledPredicate::matcher_bytes]).
    matchers: usize,
}

/// The memory bounds of a region driver running a program: each kernel's, as its operator declares them, with the state
/// a state owner draws from its host's state budget
/// ([`specify_state_bounds`][crate::operators::OperatorDef::specify_state_bounds]), under the subcomponent its node is
/// accounted under ([`kernel_accounting_name`]); each predicate of its decisions, its values
/// and its compiled matchers' entries, under the subcomponent of the first `split` using it; one full events buffer
/// per register of the program, which the driver may hold between steps; and what its decisions keep. The driver
/// allocates, with its generation, the memo of each memoized decision (R4) and the counts per exit set of every
/// decision ([`COUNTED_EXIT_SETS`] sets each); and once, for any generation, the scratch a decision step reuses: an
/// exit set per event of a full batch, and per exit of the widest decision a count and an output buffer.
///
/// A region host adds these to its own bounds (see `RegionHostBuilder::with_bounds`). They describe the boot program:
/// the kernels, predicates, memos, and counts of a later generation that the boot program does not have, and lists
/// that grow in later generations, are not accounted for.
///
/// An isolation slot's bounds ([`for_slot`][Self::for_slot]) also hold one step of a drain of its state owners'
/// time-driven output, which the region holds at once: at most [`DRAIN_STEP_EVENTS`][limits::values::DRAIN_STEP_EVENTS]
/// events, and one context's whole expansion beyond them; and its state owner's kernel is accounted at the region's
/// top level when the region is named after it ([`kernel_accounting_in`]). The bounds of a slot with keyed exits, into
/// a state owner on several lanes, hold the routed buffer each such exit keeps open for each target lane, with room for
/// the events it takes before it is sent.
#[derive(Clone, Debug)]
pub struct RegionBounds {
    /// Each kernel's node, operator, and the subcomponent it is accounted under, if not the region's own group.
    kernels: Vec<(NodeId, ConfiguredOperator, Option<String>)>,
    /// Whether the region drains state owners' time-driven output.
    drains: bool,
    predicates: Vec<PredicateBounds>,
    registers: usize,
    /// The bytes of the memos of the program's memoized decisions.
    memos: usize,
    /// The program's decisions.
    decisions: usize,
    /// The exits of its widest decision.
    widest_decision: usize,
    /// The routed buffers its keyed exits keep open, one per exit and target lane.
    routed_buffers: usize,
    /// The events those buffers take before they are sent.
    routed_events: usize,
}

impl RegionBounds {
    /// Returns the bounds of a driver running `program`, a program compiled from `plan`.
    ///
    /// # Errors
    ///
    /// Returns an error if a kernel's node is not in the plan, or its operator cannot be configured with the node's
    /// parameters.
    pub fn new(plan: &Plan, registry: &OperatorRegistry, program: &RegionProgram) -> Result<Self, RegionError> {
        Self::accounted(plan, registry, program, |node| Some(kernel_accounting_name(node)))
    }

    /// Returns the bounds of the region of `slot` running `program`, a program compiled from `plan`, on one of the
    /// slot's lanes: those of [`new`][Self::new], with each kernel accounted as [`kernel_accounting_in`] says, for an
    /// isolation slot one drain step, and the open routed buffers of its keyed exits.
    ///
    /// # Errors
    ///
    /// Returns an error if a kernel's node is not in the plan, or its operator cannot be configured with the node's
    /// parameters.
    pub fn for_slot(
        plan: &Plan, registry: &OperatorRegistry, program: &RegionProgram, slot: &Slot,
    ) -> Result<Self, RegionError> {
        let mut bounds = Self::accounted(plan, registry, program, |node| kernel_accounting_in(slot, node))?;
        bounds.drains = slot.is_isolation();
        for exit in &slot.exits {
            if let ExitRouting::Keyed(groups) = &exit.routing {
                let lanes = groups.lanes();
                bounds.routed_buffers += usize::from(lanes);
                bounds.routed_events += usize::from(lanes) * routed_buffer_events(lanes);
            }
        }
        Ok(bounds)
    }

    fn accounted<A>(
        plan: &Plan, registry: &OperatorRegistry, program: &RegionProgram, accounting: A,
    ) -> Result<Self, RegionError>
    where
        A: Fn(&NodeId) -> Option<String>,
    {
        let kernels = program
            .kernels()
            .iter()
            .map(|kernel| {
                let node = plan.node(&kernel.key.node).ok_or_else(|| RegionError::BoundsNode {
                    node: kernel.key.node.clone(),
                })?;
                let operator = registry.configure(node.op.as_str(), &node.params).map_err(|source| {
                    RegionError::BoundsOperator {
                        node: node.id.clone(),
                        source,
                    }
                })?;
                Ok((node.id.clone(), operator, accounting(&node.id)))
            })
            .collect::<Result<_, RegionError>>()?;
        let memos = program
            .decisions()
            .iter()
            .filter(|decision| decision.memoized)
            .map(|decision| {
                DecisionMemo::footprint(limits::values::DECISION_MEMO_ENTRIES as usize, decision.exits.len())
            })
            .sum();
        let predicates = program
            .decisions()
            .iter()
            .flat_map(|decision| decision.predicates.entries())
            .filter_map(|entry| {
                Some(PredicateBounds {
                    node: entry.nodes.first()?.clone(),
                    values: entry.predicate.value_bytes(),
                    matchers: entry.compiled.matcher_bytes(),
                })
            })
            .collect();
        Ok(Self {
            kernels,
            drains: false,
            predicates,
            registers: program.register_count(),
            memos,
            decisions: program.decisions().len(),
            widest_decision: program
                .decisions()
                .iter()
                .map(|decision| decision.exits.len())
                .max()
                .unwrap_or_default(),
            routed_buffers: 0,
            routed_events: 0,
        })
    }
}

impl MemoryBounds for RegionBounds {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        for (_, operator, accounted) in &self.kernels {
            // The boot program's state owners set the capacity of the state budget every later state owner draws on,
            // so their state, at their own limits, bounds what the region's state owners ever hold together.
            let specify = |builder: &mut MemoryBoundsBuilder| {
                operator.specify_bounds(builder);
                operator.specify_state_bounds(builder);
            };
            match accounted {
                Some(name) => specify(&mut builder.subcomponent(name)),
                None => specify(builder),
            }
        }
        if self.drains {
            // One drain step's output: what one call of a state owner's time-driven work emits, at most the step's
            // events and one context's whole expansion beyond them.
            builder.firm().with_expr(UsageExpr::product(
                "drain step",
                UsageExpr::struct_size::<Event>("event"),
                UsageExpr::constant(
                    "events per drain step",
                    limits::values::DRAIN_STEP_EVENTS as usize + MAX_HISTOGRAM_SERIES as usize,
                ),
            ));
        }
        for predicate in &self.predicates {
            builder
                .subcomponent(kernel_accounting_name(&predicate.node))
                .minimum()
                .with_single_value::<SharedPredicate>("predicate")
                .with_fixed_amount("predicate values", predicate.values)
                .with_fixed_amount("matcher entries", predicate.matchers);
        }
        builder.firm().with_expr(UsageExpr::product(
            "register buffers",
            UsageExpr::constant("registers", self.registers),
            UsageExpr::sum(
                "register buffer",
                UsageExpr::struct_size::<EventsBuffer>("events buffer"),
                UsageExpr::product(
                    "events",
                    UsageExpr::struct_size::<Event>("event"),
                    UsageExpr::constant("batch size", limits::values::BATCH_SIZE),
                ),
            ),
        ));
        if self.routed_buffers > 0 {
            // Each keyed exit's open buffer for each target lane, with room for the events it takes before it is sent.
            builder.firm().with_expr(UsageExpr::sum(
                "open routed buffers",
                UsageExpr::product(
                    "routed buffers",
                    UsageExpr::constant("buffers", self.routed_buffers),
                    UsageExpr::struct_size::<EventsBuffer>("events buffer"),
                ),
                UsageExpr::product(
                    "routed events",
                    UsageExpr::constant("events", self.routed_events),
                    UsageExpr::struct_size::<Event>("event"),
                ),
            ));
        }
        if self.memos > 0 {
            builder
                .firm()
                .with_expr(UsageExpr::constant("decision memos", self.memos));
        }
        if self.decisions > 0 {
            builder.firm().with_expr(UsageExpr::product(
                "decision counts",
                UsageExpr::constant("decisions", self.decisions),
                UsageExpr::constant("counts per decision", ExitSetCounts::footprint(COUNTED_EXIT_SETS)),
            ));
            builder.firm().with_expr(UsageExpr::sum(
                "decision scratch",
                UsageExpr::product(
                    "exit sets",
                    UsageExpr::struct_size::<ExitSet>("exit set"),
                    UsageExpr::constant("batch size", limits::values::BATCH_SIZE),
                ),
                UsageExpr::product(
                    "exit buffers",
                    UsageExpr::constant("exits of the widest decision", self.widest_decision),
                    UsageExpr::sum(
                        "exit buffer",
                        UsageExpr::struct_size::<u64>("events per exit"),
                        UsageExpr::struct_size::<Option<EventsBuffer>>("output buffer"),
                    ),
                ),
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_an_isolation_slot_named_after_its_state_owner_accounts_it_in_its_own_group() {
        // `dsd_agg` stands for the native transform of that name; `metrics_enrich` is a fused slot named after host
        // enrichment, whose kernel keeps its own subcomponent.
        let plan = Plan::from_json(crate::testing::DEFAULT_PROGRAM_JSON).unwrap();
        let frame = crate::compile::Frame::compute(
            &plan,
            &crate::operators::OperatorRegistry::native(),
            &crate::testing::default_program_slot_name,
        )
        .unwrap();
        let slot = |name: &str| {
            frame
                .slots()
                .iter()
                .find(|slot| slot.name.as_str() == name)
                .unwrap()
                .clone()
        };
        let node = |id: &str| id.parse::<NodeId>().unwrap();
        assert_eq!(kernel_accounting_in(&slot("dsd_agg"), &node("dsd_agg")), None);
        assert_eq!(
            kernel_accounting_in(&slot("dsd_agg"), &node("policy/dsd_agg")),
            Some("policy".to_string())
        );
        assert_eq!(
            kernel_accounting_in(&slot("metrics_enrich"), &node("metrics_enrich")),
            Some("metrics_enrich".to_string())
        );
    }

    #[test]
    fn a_kernel_is_accounted_in_its_region_only_when_the_region_is_named_after_its_reserved_node() {
        // (region, node, subcomponent)
        let cases = [
            ("dsd_agg", "dsd_agg", None),
            ("dsd_pre_aggregation", "dsd_enrich", Some("dsd_enrich")),
            ("dsd_agg", "policy/dsd_agg", Some("policy")),
            ("dsd_agg", "policy/agg", Some("policy")),
        ];
        for (region, node, expected) in cases {
            let node: NodeId = node.parse().unwrap();
            assert_eq!(
                kernel_accounting_path(region, &node).as_deref(),
                expected,
                "{} in {}",
                node,
                region
            );
        }
    }
}
