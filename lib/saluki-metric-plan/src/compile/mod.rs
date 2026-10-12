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
//! - [`rewrite`]: the **logical rewrites** (R3, R5a, R6, R7) that [`compile`] applies to a candidate before placing
//!   it, each checked against the frame, with a record of every application for `explain`.
//! - [`placement`] then places the rewritten candidate.
//! - [`program`]: the **region programs** of one generation, one per slot: plain data, with the specifications of the
//!   kernels they run, that a region host executes batch at a time. Each program's `split`s and `tee`s compile to
//!   [decisions](decision), where the decision-time rewrites (R2, R3's folding of bound constants, R4) apply.
//!
//! [`compile`] runs rewrites, placement, and program compilation together; it is the one path from a candidate plan to
//! its programs, which the installer core takes for the boot generation and every later one.
//!
//! Through stage 8, regions replace only chains of synchronous work: every task boundary that exists natively stays,
//! as a fixed component. Sources and opaque hosts are fences; nothing fuses across them.

use std::sync::Arc;

use saluki_metric_kernels::aggregate::SiteIndex;

use self::program::DecisionRewrites;
use crate::{
    ir::Plan,
    operators::{names, Boundary, Cardinality, OperatorRegistry},
    predicate::NoBindings,
    validate::{graph::Graph, ValidationReport},
};

/// Defines a `u16` index type, for the tables of programs and decisions.
macro_rules! index {
    ($(#[$meta:meta])* $ty:ident) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $ty(u16);

        impl $ty {
            /// Creates an index.
            pub const fn new(index: u16) -> Self {
                Self(index)
            }

            /// Returns this index as a `usize`, for indexing.
            pub const fn get(self) -> usize {
                self.0 as usize
            }
        }

        impl ::std::fmt::Display for $ty {
            fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                self.0.fmt(f)
            }
        }
    };
}

pub mod decision;
pub mod frame;
pub mod placement;
pub mod program;
pub mod rewrite;

pub use self::decision::{
    Branch, Decision, DecisionExit, DecisionFunction, DecisionKind, DecisionMemo, DecisionNode, DecisionScratch,
    ExitSet, ExitSetCounts, ExitTarget, MalformedDecision, MemoKey, PredicateIndex, PredicateTable, SharedPredicate,
    COUNTED_EXIT_SETS, EXIT_SET_CAPACITY, MEMO_COST_FLOOR,
};
pub use self::frame::{
    AdmissionPointInfo, Exit, ExitIndex, FixedComponent, Frame, FrameEdge, FrameError, FrameHost, FrameHosts,
    FrameSource, FrameTarget, FusionDecision, FusionVerdict, Site, Slot, SlotId, SlotKind, SlotNamer,
};
pub use self::placement::{
    place, place_unvalidated, EntryKey, MissingCapability, PlacedEntry, Placement, PlacementError, PlacementProblem,
    SlotPlacement,
};
pub use self::program::{
    CompileError, DecisionIndex, EntryProgram, EntrySource, KernelIndex, KernelKey, LeaseTable, LineageInfo,
    LineageKey, ProgramKernel, RegionProgram, RegisterId, Route, StateParamsEntry, StateParamsIndex, Step,
};
pub use self::rewrite::{
    optimize, r5b::PushedFilterSite, Optimized, Rewrite, RewriteContext, RewriteId, RewriteOutcome, RewriteRecord,
    Rewritten, TelemetryChange, TelemetryEffect,
};

/// A candidate plan compiled for one frame: the plan the rewrites made of it, where each of its nodes runs, the
/// program of every slot, and every rewrite considered.
#[derive(Clone, Debug)]
pub struct CompiledPlan {
    /// The plan placed and compiled: the candidate after the logical rewrites. It equals the candidate when no
    /// rewrite applied.
    pub plan: Arc<Plan>,

    /// Where each node of the rewritten plan runs.
    pub placement: Placement,

    /// The program of every slot of the frame, in slot order. A slot the plan places nothing in has an empty program.
    pub programs: Vec<Arc<RegionProgram>>,

    /// Every rewrite considered, applied or skipped: first the logical rewrites, in the order the pipeline considered
    /// them, then R5b's sites, in plan order of their state owners, then the decision-time rewrites of each program's
    /// decisions, in slot and decision order.
    pub rewrites: Vec<RewriteRecord>,

    /// The filters R5b pushed into state owners, as emission suppression, in plan order of their state owners: the
    /// sites whose state owner's kernel is given the filter to judge its records with. A site found without one is
    /// recorded as skipped, and is not listed.
    pub pushed_filters: Vec<PushedFilterSite>,

    /// Whether the logical rewrites reached a fixed point within their bounds.
    pub converged: bool,
}

impl CompiledPlan {
    /// Returns the records of the logical rewrites, and of R5b, which the logical plan decides: those considered
    /// before placement, in the order they were considered.
    pub fn logical_rewrites(&self) -> &[RewriteRecord] {
        &self.rewrites[..self.rewrites.len() - self.decision_rewrite_count()]
    }

    /// Returns the records of the decision-time rewrites (R2, R3's folding of bound constants, and R4), which change
    /// only the compiled programs: those of each program's decisions, in slot and decision order.
    pub fn decision_rewrites(&self) -> &[RewriteRecord] {
        &self.rewrites[self.rewrites.len() - self.decision_rewrite_count()..]
    }

    fn decision_rewrite_count(&self) -> usize {
        let count = self
            .programs
            .iter()
            .flat_map(|program| program.decisions())
            .map(|decision| decision.rewrites.len())
            .sum::<usize>();
        count.min(self.rewrites.len())
    }
}

/// Places a validated candidate plan into a frame and compiles the program of every slot.
///
/// The logical rewrites run first ([`optimize`]), against the frame; the rewritten plan is placed and compiled.
///
/// # Errors
///
/// Returns an error if the plan is not valid, if it cannot be placed into the frame (see [`place`]), or if a program
/// cannot be compiled, for example because a predicate has a configuration binding without a resolved value.
pub fn compile(
    plan: &Plan, report: &ValidationReport, registry: &OperatorRegistry, frame: &Frame,
) -> Result<CompiledPlan, CompileError> {
    let placement = place(plan, report, registry, frame).map_err(|source| CompileError::Placement { source })?;
    compile_placed(plan, Some(report), registry, frame, placement, DecisionRewrites::ALL)
}

/// Compiles a validated candidate plan as [`compile`] does, but without the decision-time rewrites (R2, R3's folding of
/// bound constants, R4): the original that property tests compare their effect with.
#[cfg(test)]
pub(crate) fn compile_without_decision_rewrites(
    plan: &Plan, report: &ValidationReport, registry: &OperatorRegistry, frame: &Frame,
) -> Result<CompiledPlan, CompileError> {
    let placement = place(plan, report, registry, frame).map_err(|source| CompileError::Placement { source })?;
    compile_placed(plan, Some(report), registry, frame, placement, DecisionRewrites::NONE)
}

/// Compiles a validated candidate plan as [`compile`] does, but without R5b: no filter is pushed into a state owner.
#[cfg(test)]
pub(crate) fn compile_without_pushed_filters(
    plan: &Plan, report: &ValidationReport, registry: &OperatorRegistry, frame: &Frame,
) -> Result<CompiledPlan, CompileError> {
    let placement = place(plan, report, registry, frame).map_err(|source| CompileError::Placement { source })?;
    compile_placed(
        plan,
        Some(report),
        registry,
        frame,
        placement,
        DecisionRewrites::WITHOUT_PUSHED_FILTERS,
    )
}

/// Places a plan into a frame and compiles the program of every slot, whatever its validation report says, as long as
/// the plan is structurally sound.
///
/// This is for the default program, which runs, reported degraded, even when validation reports errors in it (see
/// [`place_unvalidated`]). Every other candidate is compiled with [`compile`]. No logical rewrite applies to a plan
/// compiled this way: their obligations are checked against a valid plan's effect contracts and port types.
///
/// # Errors
///
/// Returns an error if the plan has structural problems, if it cannot be placed into the frame, or if a program cannot
/// be compiled.
pub fn compile_unvalidated(
    plan: &Plan, registry: &OperatorRegistry, frame: &Frame,
) -> Result<CompiledPlan, CompileError> {
    let placement = place_unvalidated(plan, registry, frame).map_err(|source| CompileError::Placement { source })?;
    compile_placed(plan, None, registry, frame, placement, DecisionRewrites::ALL)
}

/// Rewrites a placed candidate, places the result, and compiles its programs: the one path from a candidate to its
/// programs. `report` is the candidate's validation report, if it is valid.
fn compile_placed(
    plan: &Plan, report: Option<&ValidationReport>, registry: &OperatorRegistry, frame: &Frame, placement: Placement,
    decision_rewrites: DecisionRewrites,
) -> Result<CompiledPlan, CompileError> {
    let optimized = match report {
        Some(report) => optimize(plan, &RewriteContext::new(registry, frame, report)),
        None => Optimized {
            plan: plan.clone(),
            records: Vec::new(),
            converged: true,
        },
    };
    let placement = if optimized.changed() {
        // The pipeline placed every application it accepted, so this fails only on a bug.
        place_unvalidated(&optimized.plan, registry, frame).map_err(|error| CompileError::Internal {
            message: format!("the rewritten plan does not place into the frame: {}", error),
        })?
    } else {
        placement
    };
    // R5b's sites are found on the logical plan the rewrites left; only a valid candidate has the port types they need.
    let (sites, records) = match report {
        Some(report) if decision_rewrites.pushes_filters() => rewrite::r5b::analyze(&optimized.plan, report, registry),
        _ => (Vec::new(), Vec::new()),
    };
    let pushed = site_filters(&sites)?;
    let (pushed_filters, pushed_records) = reconcile_pushed(sites, records, &pushed);
    let programs =
        program::compile_programs_with(&optimized.plan, registry, frame, &placement, &pushed, decision_rewrites)?
            .into_iter()
            .map(Arc::new)
            .collect::<Vec<_>>();
    let mut rewrites = optimized.records;
    rewrites.extend(pushed_records);
    rewrites.extend(
        programs
            .iter()
            .flat_map(|program| program.decisions())
            .flat_map(|decision| decision.rewrites.iter().cloned()),
    );
    Ok(CompiledPlan {
        plan: Arc::new(optimized.plan),
        placement,
        programs,
        rewrites,
        pushed_filters,
        converged: optimized.converged,
    })
}

/// Builds the filter each of R5b's sites hands its state owner, which judges the records it emits with it: the site's
/// predicate compiled as the programs compile their predicates, from the plan's resolved values, and numbered by the
/// site's position among the plan's sites, which the counts of what it suppresses are reported by.
///
/// # Errors
///
/// Returns an error if a binding the predicate reads has no value, as compiling the filter's own `split` would.
fn site_filters(sites: &[PushedFilterSite]) -> Result<Vec<program::SiteFilter>, CompileError> {
    sites
        .iter()
        .enumerate()
        .map(|(index, site)| {
            let index = u32::try_from(index).map_err(|_| CompileError::Internal {
                message: "R5b found more filter sites than it can number".to_string(),
            })?;
            let filter =
                site.compile(SiteIndex::new(index), &NoBindings)
                    .map_err(|source| CompileError::UnresolvedBinding {
                        node: site.filter.clone(),
                        source,
                    })?;
            Ok(program::SiteFilter {
                site: site.clone(),
                filter: Arc::new(filter),
            })
        })
        .collect()
}

/// The reason a site of R5b's is recorded as skipped when its state owner is given no filter for it.
const NO_SITE_FILTER: &str = "the state owner's kernel is given no filter for this site, so it suppresses nothing";

/// Keeps R5b's sites whose filter reaches their state owner, and records every other applied site as skipped.
///
/// A site takes effect only through the filter its state owner judges records with, so a site without one must not be
/// reported applied, counted in `plan_rewrites_applied`, or hashed into a program's fingerprint: it would claim a
/// suppression no kernel performs.
fn reconcile_pushed(
    sites: Vec<PushedFilterSite>, records: Vec<RewriteRecord>, pushed: &[program::SiteFilter],
) -> (Vec<PushedFilterSite>, Vec<RewriteRecord>) {
    let filtered = |site: &PushedFilterSite| pushed.iter().any(|pushed| &pushed.site == site);
    let records = records
        .into_iter()
        .map(|mut record| {
            let unfiltered = record.id == RewriteId::R5b
                && record.is_applied()
                && sites
                    .iter()
                    .any(|site| !filtered(site) && record.nodes == [site.owner.clone(), site.filter.clone()]);
            if unfiltered {
                record.outcome = RewriteOutcome::Skipped {
                    reason: NO_SITE_FILTER.to_string(),
                };
                // A skipped rewrite changes no telemetry.
                record.telemetry.clear();
            }
            record
        })
        .collect();
    let sites = sites.into_iter().filter(|site| filtered(site)).collect();
    (sites, records)
}

/// How one node of a plan relates to the physical structure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Role {
    /// A host: a source or an opaque host, run by a component of its own.
    Fixed,

    /// An `emit` or `drop` whose only input comes straight from a host. It lowers to a direct edge from the host to
    /// the output, or to nothing, never to a region of its own.
    Terminal,

    /// A synchronous operator, run in a slot.
    Synchronous,

    /// A state owner behind a declared isolation fence ([`Boundary::Isolated`][crate::operators::Boundary]), run in an
    /// isolation slot of its own ([`SlotKind::Isolation`]).
    Isolated,
}

/// Returns the role of every node of a resolved plan, by position.
///
/// Every node must have a configured operator, as in a plan without structural findings.
pub(crate) fn roles(graph: &Graph<'_>) -> Vec<Role> {
    let fixed = (0..graph.nodes.len())
        .map(|node| graph.op(node).is_some_and(|op| op.contract().boundary.is_host()))
        .collect::<Vec<_>>();
    (0..graph.nodes.len())
        .map(|node| {
            if fixed[node] {
                return Role::Fixed;
            }
            let Some(op) = graph.op(node) else {
                return Role::Synchronous;
            };
            if op.contract().boundary == Boundary::Isolated {
                return Role::Isolated;
            }
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
mod host_tests;
#[cfg(test)]
mod isolation_tests;
#[cfg(test)]
mod tests;
