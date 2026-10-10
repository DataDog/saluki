//! The memory a region driver declares: its kernels', by their operators, its decisions' predicates, the buffers it
//! holds between steps, and what its decisions keep: their memos, their counts per exit set, and the scratch a
//! decision step reuses.

use saluki_core::{
    accounting::{MemoryBounds, MemoryBoundsBuilder, UsageExpr},
    data_model::event::Event,
    topology::EventsBuffer,
};

use super::RegionError;
use crate::{
    compile::{DecisionMemo, ExitSet, ExitSetCounts, RegionProgram, SharedPredicate, COUNTED_EXIT_SETS},
    ir::{Namespace, NodeId, Plan},
    limits,
    operators::{ConfiguredOperator, OperatorRegistry},
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

/// The memory bounds of a region driver running a program: each kernel's, as its operator declares them, under the
/// subcomponent its node is accounted under ([`kernel_accounting_name`]); each predicate of its decisions, its values
/// and its compiled matchers' entries, under the subcomponent of the first `split` using it; one full events buffer
/// per register of the program, which the driver may hold between steps; and what its decisions keep. The driver
/// allocates, with its generation, the memo of each memoized decision (R4) and the counts per exit set of every
/// decision ([`COUNTED_EXIT_SETS`] sets each); and once, for any generation, the scratch a decision step reuses: an
/// exit set per event of a full batch, and per exit of the widest decision a count and an output buffer.
///
/// A region host adds these to its own bounds (see `RegionHostBuilder::with_bounds`). They describe the boot program:
/// the kernels, predicates, memos, and counts of a later generation that the boot program does not have, and lists
/// that grow in later generations, are not accounted for.
#[derive(Clone, Debug)]
pub struct RegionBounds {
    kernels: Vec<(NodeId, ConfiguredOperator)>,
    predicates: Vec<PredicateBounds>,
    registers: usize,
    /// The bytes of the memos of the program's memoized decisions.
    memos: usize,
    /// The program's decisions.
    decisions: usize,
    /// The exits of its widest decision.
    widest_decision: usize,
}

impl RegionBounds {
    /// Returns the bounds of a driver running `program`, a program compiled from `plan`.
    ///
    /// # Errors
    ///
    /// Returns an error if a kernel's node is not in the plan, or its operator cannot be configured with the node's
    /// parameters.
    pub fn new(plan: &Plan, registry: &OperatorRegistry, program: &RegionProgram) -> Result<Self, RegionError> {
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
                Ok((node.id.clone(), operator))
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
        })
    }
}

impl MemoryBounds for RegionBounds {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        for (node, operator) in &self.kernels {
            operator.specify_bounds(&mut builder.subcomponent(kernel_accounting_name(node)));
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
