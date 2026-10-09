//! Compiling logical plans into physical structure and region programs.
//!
//! A logical [plan](crate::ir::Plan) says what happens to metrics. Running it takes three steps, each in its own
//! module:
//!
//! - [`frame`]: the **physical frame**, computed once at boot from the boot plan (the default program). It is the
//!   fixed structure of the topology: the **fixed components** (sources, opaque hosts, and the outputs' encoders),
//!   the **slots** between them, each run by one region host component, and the physical edges between them. Rewrite
//!   R1, fusion, decides which synchronous operators share a slot; the frame records each decision.
//! - [`placement`]: **placing** a candidate plan into the frame, which assigns every logical node to a fixed component,
//!   a slot, or a direct edge, and checks that every edge between them is one the frame has. A candidate that needs
//!   structure the frame lacks is rejected with an error naming the missing capability.
//! - [`program`]: the **region programs** of one generation, one per slot: plain data, with the specifications of the
//!   kernels they run, that a region host executes batch at a time.
//!
//! [`compile`] runs placement and program compilation together.
//!
//! Through stage 8, regions replace only chains of synchronous work: every task boundary that exists natively stays,
//! as a fixed component. Sources and opaque hosts are fences; nothing fuses across them.

use std::sync::Arc;

use crate::{
    ir::Plan,
    operators::{names, Cardinality, OperatorRegistry},
    validate::{graph::Graph, ValidationReport},
};

pub mod frame;
pub mod placement;
pub mod program;

pub use self::frame::{
    AdmissionPointInfo, Exit, ExitIndex, FixedComponent, Frame, FrameEdge, FrameError, FrameSource, FrameTarget,
    FusionDecision, FusionVerdict, Site, Slot, SlotId, SlotNamer,
};
pub use self::placement::{
    place, place_unvalidated, EntryKey, MissingCapability, PlacedEntry, Placement, PlacementError, PlacementProblem,
    SlotPlacement,
};
pub use self::program::{
    CompileError, Decision, DecisionIndex, DecisionKind, EntryProgram, KernelIndex, KernelKey, ProgramKernel,
    RegionProgram, RegisterId, Route, Step,
};

/// A candidate plan compiled for one frame: where each node runs, and the program of every slot.
#[derive(Clone, Debug)]
pub struct CompiledPlan {
    /// Where each node of the plan runs.
    pub placement: Placement,

    /// The program of every slot of the frame, in slot order. A slot the plan places nothing in has an empty program.
    pub programs: Vec<Arc<RegionProgram>>,
}

/// Places a validated candidate plan into a frame and compiles the program of every slot.
///
/// # Errors
///
/// Returns an error if the plan is not valid, if it cannot be placed into the frame (see [`place`]), or if a program
/// cannot be compiled, for example because a predicate has a configuration binding without a resolved value.
pub fn compile(
    plan: &Plan, report: &ValidationReport, registry: &OperatorRegistry, frame: &Frame,
) -> Result<CompiledPlan, CompileError> {
    let placement = place(plan, report, registry, frame).map_err(|source| CompileError::Placement { source })?;
    compile_placed(plan, registry, frame, placement)
}

/// Places a plan into a frame and compiles the program of every slot, whatever its validation report says, as long as
/// the plan is structurally sound.
///
/// This is for the default program, which runs, reported degraded, even when validation reports errors in it (see
/// [`place_unvalidated`]). Every other candidate is compiled with [`compile`].
///
/// # Errors
///
/// Returns an error if the plan has structural problems, if it cannot be placed into the frame, or if a program cannot
/// be compiled.
pub fn compile_unvalidated(
    plan: &Plan, registry: &OperatorRegistry, frame: &Frame,
) -> Result<CompiledPlan, CompileError> {
    let placement = place_unvalidated(plan, registry, frame).map_err(|source| CompileError::Placement { source })?;
    compile_placed(plan, registry, frame, placement)
}

fn compile_placed(
    plan: &Plan, registry: &OperatorRegistry, frame: &Frame, placement: Placement,
) -> Result<CompiledPlan, CompileError> {
    let programs = program::compile_programs(plan, registry, frame, &placement)?
        .into_iter()
        .map(Arc::new)
        .collect();
    Ok(CompiledPlan { placement, programs })
}

/// How one node of a plan relates to the physical structure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Role {
    /// A fence: a source or an opaque host, run by a component of its own.
    Fixed,

    /// An `emit` or `drop` whose only input comes straight from a fence. It lowers to a direct edge from the fence to
    /// the output, or to nothing, never to a region of its own.
    Terminal,

    /// A synchronous operator, run in a slot.
    Synchronous,
}

/// Returns the role of every node of a resolved plan, by position.
///
/// Every node must have a configured operator, as in a plan without structural findings.
pub(crate) fn roles(graph: &Graph<'_>) -> Vec<Role> {
    let fixed = (0..graph.nodes.len())
        .map(|node| graph.op(node).is_some_and(|op| op.contract().boundary.is_fence()))
        .collect::<Vec<_>>();
    (0..graph.nodes.len())
        .map(|node| {
            if fixed[node] {
                return Role::Fixed;
            }
            let Some(op) = graph.op(node) else {
                return Role::Synchronous;
            };
            let terminal = matches!(op.name(), names::EMIT | names::DROP)
                && op.contract().cardinality == Cardinality::Sink
                && op.ports().inputs.len() == 1;
            if terminal && graph.source(node, 0).is_some_and(|(from, _)| fixed[from]) {
                Role::Terminal
            } else {
                Role::Synchronous
            }
        })
        .collect()
}

/// Builds the graph of a plan, returning the structural problems validation would report if there are any.
pub(crate) fn resolve<'p>(plan: &'p Plan, registry: &OperatorRegistry) -> Result<Graph<'p>, Vec<String>> {
    let mut report = ValidationReport::default();
    let graph = Graph::build(plan, registry, &mut report);
    let mut problems = report.findings().iter().map(ToString::to_string).collect::<Vec<_>>();
    if graph.cyclic && problems.is_empty() {
        problems.push("the plan has a cycle".to_string());
    }
    if problems.is_empty() {
        Ok(graph)
    } else {
        Err(problems)
    }
}

#[cfg(test)]
mod execution_tests;
#[cfg(test)]
mod tests;
