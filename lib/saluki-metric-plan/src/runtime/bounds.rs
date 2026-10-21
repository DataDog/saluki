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
        DecisionMemo, ExitRouting, ExitSet, ExitSetCounts, RegionKey, RegionProgram, SharedPredicate, Slot,
        COUNTED_EXIT_SETS,
    },
    ir::{NodeId, Plan},
    limits,
    operators::{ConfiguredOperator, OperatorRegistry, MAX_HISTOGRAM_SERIES},
};

/// Returns the name, under its region's component, of the subcomponent whose bounds and resource group a kernel's
/// node is accounted under: the node's ID.
pub fn kernel_accounting_name(node: &NodeId) -> String {
    node.local().to_string()
}

/// Where a kernel's memory and CPU time are accounted, and its bounds declared, relative to the component of the
/// region that runs it ([`kernel_accounting_in`]).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum KernelAccounting {
    /// In the region's own group, with its bounds at the region's top level: the state owner of an isolation slot
    /// named after it, as the slot of aggregation stands for the native `dsd_agg` transform.
    Region,

    /// Under a subcomponent of the region, named after the node ([`kernel_accounting_name`]).
    Under(String),

    /// As a component of its own beside the region, named after the node: a state owner fused with its neighbors in a
    /// slot's main region, which keeps the resource group and bounds path of the component it would have in a slot of
    /// its own, such as `dsd_agg.aggregate_context_limit`. The region's bounds leave it out
    /// ([`RegionBounds::beside`]).
    Beside(String),
}

/// Returns where the kernel of `node` is accounted in the region of a slot named `region` that runs it in `program`,
/// a program of the slot's main region or of a sub-region of the slot; `isolation` says whether the slot isolates a
/// state owner (see [`Slot::is_isolation`]).
///
/// A state owner accounts under its own node's name wherever it runs: in the region's own group when the slot isolates
/// it and is named after it, as the slot of aggregation is on lanes; beside the region, as a component of its own,
/// when it is fused with its neighbors in a slot's main region, as aggregation is without lanes, so that its resource
/// group and bounds stay those of its native component; and under the region otherwise, a sub-region included. Every
/// other kernel is accounted under [`kernel_accounting_name`].
pub fn kernel_accounting_path(
    region: &str, isolation: bool, program: &RegionProgram, node: &NodeId,
) -> KernelAccounting {
    let name = kernel_accounting_name(node);
    let owner = program
        .kernels()
        .iter()
        .any(|kernel| &kernel.key.node == node && kernel.lineage.is_some());
    match program.region() {
        RegionKey::Main if owner && isolation && name == region => KernelAccounting::Region,
        RegionKey::Main if owner && !isolation => KernelAccounting::Beside(name),
        RegionKey::Main | RegionKey::State(_) => KernelAccounting::Under(name),
    }
}

/// Returns where the kernel of `node` is accounted in the region of `slot` running `program`:
/// [`kernel_accounting_path`] for the slot.
pub fn kernel_accounting_in(slot: &Slot, program: &RegionProgram, node: &NodeId) -> KernelAccounting {
    kernel_accounting_path(slot.name.as_str(), slot.is_isolation(), program, node)
}

/// Returns `true` if a region program holds a state owner: it has a kernel with a lineage.
fn holds_state(program: &RegionProgram) -> bool {
    program.kernels().iter().any(|kernel| kernel.lineage.is_some())
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
/// The bounds of a region holding a state owner ([`for_slot`][Self::for_slot]) also hold one step of a drain of its
/// state owner's time-driven output, which the region holds at once: at most [`DRAIN_STEP_EVENTS`][limits::values::DRAIN_STEP_EVENTS]
/// events, and one context's whole expansion beyond them; and its state owner's kernel is accounted at the region's
/// top level when the region isolates it and is named after it, and beside the region when it is fused with its
/// neighbors ([`kernel_accounting_in`]). The bounds of a slot with keyed exits, into
/// a state owner on several lanes, hold the routed buffer each such exit keeps open for each target lane, with room for
/// the events it takes before it is sent.
#[derive(Clone, Debug)]
pub struct RegionBounds {
    /// Each kernel's node, operator, and where it is accounted.
    kernels: Vec<(NodeId, ConfiguredOperator, KernelAccounting)>,
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
        Self::accounted(plan, registry, program, |node| {
            KernelAccounting::Under(kernel_accounting_name(node))
        })
    }

    /// Returns the bounds of the region of `slot` running `program`, a program compiled from `plan`, on one of the
    /// slot's lanes: those of [`new`][Self::new], with each kernel accounted as [`kernel_accounting_in`] says, for a
    /// program that holds a state owner one drain step, and the open routed buffers of its keyed exits.
    ///
    /// # Errors
    ///
    /// Returns an error if a kernel's node is not in the plan, or its operator cannot be configured with the node's
    /// parameters.
    pub fn for_slot(
        plan: &Plan, registry: &OperatorRegistry, program: &RegionProgram, slot: &Slot,
    ) -> Result<Self, RegionError> {
        let mut bounds = Self::accounted(plan, registry, program, |node| {
            kernel_accounting_in(slot, program, node)
        })?;
        bounds.drains = holds_state(program);
        for exit in &slot.exits {
            if let ExitRouting::Keyed(groups) = &exit.routing {
                let lanes = groups.lanes();
                bounds.routed_buffers += usize::from(lanes);
                bounds.routed_events += usize::from(lanes) * routed_buffer_events(lanes);
            }
        }
        Ok(bounds)
    }

    /// Returns the bounds of each kernel accounted as a component of its own beside the region
    /// ([`KernelAccounting::Beside`]), which the region's own bounds leave out: the component's name, and its bounds,
    /// which its operator and its state declare at the component's top level.
    pub fn beside(&self) -> impl Iterator<Item = (&str, KernelBounds)> + '_ {
        self.kernels
            .iter()
            .filter_map(|(_, operator, accounted)| match accounted {
                KernelAccounting::Beside(name) => Some((name.as_str(), KernelBounds(operator.clone()))),
                KernelAccounting::Region | KernelAccounting::Under(_) => None,
            })
    }

    fn accounted<A>(
        plan: &Plan, registry: &OperatorRegistry, program: &RegionProgram, accounting: A,
    ) -> Result<Self, RegionError>
    where
        A: Fn(&NodeId) -> KernelAccounting,
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

/// The bounds of one kernel accounted beside its region ([`RegionBounds::beside`]): its operator's and its state's.
#[derive(Clone, Debug)]
pub struct KernelBounds(ConfiguredOperator);

impl MemoryBounds for KernelBounds {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        self.0.specify_bounds(builder);
        self.0.specify_state_bounds(builder);
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
                KernelAccounting::Region => specify(builder),
                KernelAccounting::Under(name) => specify(&mut builder.subcomponent(name)),
                // Declared by the component beside the region (`beside`).
                KernelAccounting::Beside(_) => {}
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
    fn a_state_owner_accounts_under_its_own_name_wherever_it_runs() {
        // Isolated, `dsd_agg` stands for the native transform of that name; `metrics_enrich` is a fused slot, in which
        // host enrichment's kernel keeps its own subcomponent.
        let plan = Plan::from_json(crate::testing::DEFAULT_PROGRAM_JSON).unwrap();
        let registry = crate::operators::OperatorRegistry::native();
        let node = |id: &str| id.parse::<NodeId>().unwrap();
        let under = |id: &str| KernelAccounting::Under(id.to_string());
        for isolate_state in [true, false] {
            let options = crate::compile::FrameOptions { isolate_state };
            let frame = crate::compile::Frame::compute_with(
                &plan,
                &registry,
                &crate::testing::default_program_slot_name,
                &crate::compile::FrameHosts::new(),
                &crate::compile::LanePolicy::ONE,
                options,
            )
            .unwrap();
            let derived = crate::compile::DerivedPoints::for_plan(&frame, &plan, &registry);
            let compiled = crate::compile::compile_unvalidated(&plan, &registry, &frame, &derived).unwrap();
            let slot = |name: &str| frame.slots().iter().find(|slot| slot.name.as_str() == name).unwrap();
            let accounting =
                |slot: &Slot, node: NodeId| kernel_accounting_in(slot, &compiled.programs[slot.id.get()], &node);
            if isolate_state {
                assert_eq!(accounting(slot("dsd_agg"), node("dsd_agg")), KernelAccounting::Region);
                // A node that is no state owner of the region's program accounts under its own name.
                assert_eq!(accounting(slot("dsd_agg"), node("second_agg")), under("second_agg"));
                assert_eq!(
                    accounting(slot("metrics_enrich"), node("metrics_host_enrichment")),
                    under("metrics_host_enrichment")
                );
                // So does a node named like the slot without a state owner it runs in.
                assert_eq!(
                    accounting(slot("metrics_enrich"), node("metrics_enrich")),
                    under("metrics_enrich")
                );
            } else {
                // Fused, aggregation keeps its native component's group beside the one region.
                let region = slot("dsd_pre_aggregation");
                assert_eq!(
                    accounting(region, node("dsd_agg")),
                    KernelAccounting::Beside("dsd_agg".to_string())
                );
                assert_eq!(accounting(region, node("dsd_enrich")), under("dsd_enrich"));
                assert_eq!(
                    accounting(region, node("metrics_host_enrichment")),
                    under("metrics_host_enrichment")
                );
                // The region's bounds leave aggregation out, which the component beside it declares.
                let bounds =
                    RegionBounds::for_slot(&plan, &registry, &compiled.programs[region.id.get()], region).unwrap();
                assert_eq!(bounds.beside().map(|(name, _)| name).collect::<Vec<_>>(), ["dsd_agg"]);
            }
        }
    }
}
