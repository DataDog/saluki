//! The memory a region driver declares: its kernels', by their operators, and the buffers it holds between steps.

use saluki_core::{
    accounting::{MemoryBounds, MemoryBoundsBuilder, UsageExpr},
    data_model::event::Event,
    topology::EventsBuffer,
};

use super::RegionError;
use crate::{
    compile::RegionProgram,
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

/// The memory bounds of a region driver running a program: each kernel's, as its operator declares them, under the
/// subcomponent its node is accounted under ([`kernel_accounting_name`]), and one full events buffer per register of
/// the program, which the driver may hold between steps.
///
/// A region host adds these to its own bounds (see `RegionHostBuilder::with_bounds`). They describe the boot program:
/// the kernels of a later generation that the boot program does not have are not accounted for.
#[derive(Clone, Debug)]
pub struct RegionBounds {
    kernels: Vec<(NodeId, ConfiguredOperator)>,
    registers: usize,
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
        Ok(Self {
            kernels,
            registers: program.register_count(),
        })
    }
}

impl MemoryBounds for RegionBounds {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        for (node, operator) in &self.kernels {
            operator.specify_bounds(&mut builder.subcomponent(kernel_accounting_name(node)));
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
    }
}
