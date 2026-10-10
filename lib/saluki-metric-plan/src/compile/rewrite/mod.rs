//! Logical rewrites: the plan-to-plan rewrites the compiler applies to a candidate before placing it.
//!
//! The specification's rewrite set is closed. Each rewrite has proof obligations stated in terms of effect contracts,
//! and a property test against the reference interpreter. They live in three places:
//!
//! | ID | Rewrite | Where it runs |
//! |---|---|---|
//! | R1 | Fuse adjacent operators into one region. | At boot, in [`Frame::compute`]: it decides the frame's slots. |
//! | R2 | Share predicate results. | In [decision compilation](super::decision): each canonical-form-equal predicate of a decision is evaluated once per event. |
//! | R3 | Fold constant `split`s and remove unreachable subgraphs. | Here ([`r3`]) for `split`s that literal constants, or `kind_in` leaves for every kind their input carries, make constant, with the subgraphs they make unreachable; in [decision compilation](super::decision) for constants that configuration bindings give (`flag`, `is_empty`). |
//! | R4 | Memoize a decision per interned context plus kind. | In [decision compilation](super::decision), which decides whether a decision may be memoized, and the region driver, which keeps the memo. |
//! | R5a | Push a filter upstream across a stateless operator. | Here ([`r5a`]). |
//! | R5b | Push a filter upstream across a stateful operator. | Stage 6. |
//! | R6 | Hoist a drop that every path below a `tee` applies. | Here ([`r6`]). |
//! | R7 | Reorder operands of pure `all` / `any`. | Here ([`r7`]). |
//!
//! # The pipeline
//!
//! [`optimize`] runs the logical rewrites in a fixed order, [`PIPELINE`], to a fixed point: a **pass** applies each
//! rewrite in turn, as often as it applies, and passes repeat until one changes nothing. Every **application** is
//! checked before it is accepted: the rewritten plan must validate (without the catalog's references, which no rewrite
//! changes, but with the ports its scopes name) and must place into the frame, with every kernel in the slot it ran in
//! before, whose name its resource group carries. An application that fails a check is **skipped**: the plan stays as
//! it was, and the skip is recorded, so that `explain` shows it. A rewrite never offers a site it was already skipped
//! at again ([`RewriteContext::is_rejected`]). At most [`MAX_PASSES`] passes and [`MAX_APPLICATIONS`] applications
//! run; a pipeline that reaches either bound stops with the plan it has, every step of which was checked, and reports
//! that it did not converge.
//!
//! A plan whose validation report has errors, such as a default program that runs degraded, is compiled without
//! logical rewrites: their obligations are checked against validated effect contracts and port types.
//!
//! # Rules every rewrite follows
//!
//! - **The frame is fixed.** Rewrites take the frame computed at boot as input and never place work outside it: no new
//!   slot, exit, fixed component, or output. The pipeline's placement check enforces this.
//! - **Exits stay.** A rewrite may remove nodes and `emit`s inside programs (R3), but never removes one of the frame's
//!   exits or a declared output: an exit nothing uses stays connected and idle.
//! - **Structure never depends on configuration values.** `flag` and `is_empty` are constant given the resolved values
//!   of their bindings, but a logical rewrite treats them as opaque whatever those values are, so that structure, and
//!   with it placement and the plan diff, never changes with a live binding's value. Only compiled programs fold them.
//! - **Dynamic operators stay where they are.** Operators that read signals are never moved, merged, or memoized, and
//!   nothing moves across them.
//! - **Routing, lossy operators, operator parameters, and delivery never change.** A rewrite can change internal
//!   telemetry, which its record lists ([`TelemetryEffect`]).
//! - **Capacity.** On every trace where neither plan hits a capacity limit, the rewritten plan delivers what the
//!   original delivers. Under capacity pressure, a rewrite may only reduce capacity loss: it never delivers a
//!   contribution the policy excludes, and never increases capacity loss for a policy-included contribution. A
//!   rewrite whose equivalence holds only under that rule says so ([`RewriteRecord::capacity_rule`]), and `explain`
//!   flags it.
//!
//! Each rewrite has a `property_test_*` that runs random valid plans and traces through the reference interpreter,
//! rewritten and not, and checks them equivalent, with a variant under capacity pressure checked with
//! [`compare_under_capacity`][crate::equivalence::compare_under_capacity]; and the whole pipeline has its own, over
//! both families of random plans the tests draw from: `harness`'s, built around filters, tag projections, operators
//! that shed to capacity, and scopes (R5a and R6 sites), and `random_plans`'s, whose `split`s draw their predicates
//! from a strategy each test chooses, such as constant and reorderable ones (R3 and R7 sites). A `capacity(output, n)` bound counts
//! records in the interpreter's [delivery order][crate::interpret#delivery-order], which follows from the plan's
//! dataflow alone, never from path lengths: a rewrite that keeps every remaining port's records in order, as R3 and R7
//! do, delivers every output's records in the same order, which their property tests check, and so delivers and
//! refuses exactly the same records under any bound, including bounds that cut inside a batch.

use std::{collections::BTreeSet, fmt};

use super::{
    frame::{FrameTarget, Site, SlotId},
    placement::{place_unvalidated, Placement},
    Frame,
};
use crate::{
    ir::{NodeId, Plan},
    operators::OperatorRegistry,
    validate::{revalidate, ValidationReport},
};

mod filters;
pub mod r3;
pub mod r5a;
pub mod r6;
pub mod r7;

#[cfg(test)]
pub(crate) mod harness;

/// The most passes [`optimize`] runs before it stops without a fixed point.
pub const MAX_PASSES: usize = 8;

/// The most applications [`optimize`] accepts or skips, over every pass, before it stops without a fixed point.
///
/// Four per node of the largest artifact: every logical rewrite applies at most a few times per node.
pub const MAX_APPLICATIONS: usize = 4 * crate::limits::values::ARTIFACT_NODES as usize;

/// The logical rewrites, in the order each pass of [`optimize`] runs them.
///
/// - R3 runs first, so that the rewrites after it see neither constant `split`s nor unreachable nodes, and never move
///   a filter onto a path that carries nothing.
/// - R5a runs before R6: it brings filters up, through merges and the maps below a `tee`, until they are the `tee`'s
///   direct targets, which is the only form R6 hoists.
/// - R7, which only reorders operands within one predicate, runs last, so that it also orders the predicates R5a's
///   partial push-downs restrict (`all(kind_in, ..)`) and the copies R6 hoists.
///
/// Passes repeat until one changes nothing, so a site one rewrite opens for an earlier one in the order (an R6 hoist
/// that leaves a filter below a map R5a can cross, say) is taken in the next pass.
pub const PIPELINE: [&'static dyn Rewrite; 4] = [&r3::R3, &r5a::R5a, &r6::R6, &r7::R7];

/// The identifier of a rewrite of the specification's closed set, other than R1, which the frame records.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum RewriteId {
    /// Share predicate results.
    R2,

    /// Fold constant `split`s and remove unreachable subgraphs.
    R3,

    /// Memoize a decision per interned context plus kind.
    R4,

    /// Push a filter upstream across a stateless operator.
    R5a,

    /// Hoist a drop that every path below a `tee` applies.
    R6,

    /// Reorder operands of pure `all` / `any`.
    R7,
}

impl RewriteId {
    /// Every identifier, in the specification's order: what a host registers one series per rewrite for.
    pub const ALL: [Self; 6] = [Self::R2, Self::R3, Self::R4, Self::R5a, Self::R6, Self::R7];

    /// Returns the identifier as the specification writes it, such as `R5a`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::R2 => "R2",
            Self::R3 => "R3",
            Self::R4 => "R4",
            Self::R5a => "R5a",
            Self::R6 => "R6",
            Self::R7 => "R7",
        }
    }
}

impl fmt::Display for RewriteId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// How a rewrite changes a series of internal telemetry.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum TelemetryChange {
    /// The series is no longer reported, such as the per-port counts of a node R3 removes.
    Removed,

    /// A new series is reported, such as the per-port counts of a filter copy R5a inserts.
    Added,

    /// The series is reported with different values, such as a `drop` node's policy loss after R6 hoists it.
    Changed,
}

impl TelemetryChange {
    /// Returns the change's name: `removed`, `added`, or `changed`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Removed => "removed",
            Self::Added => "added",
            Self::Changed => "changed",
        }
    }
}

/// One change a rewrite makes to internal telemetry.
///
/// Telemetry is not part of equivalence, so rewrites may change it; `explain` lists every change.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TelemetryEffect {
    /// The node whose series change.
    pub node: NodeId,

    /// How they change.
    pub change: TelemetryChange,

    /// What changes, such as ``plan_node_events_total{component_id:<node>}``, and why.
    pub detail: String,
}

/// What happened to one application of a rewrite.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RewriteOutcome {
    /// The rewrite was applied.
    Applied,

    /// The rewrite was not applied: the plan stays as it was.
    Skipped {
        /// Why, such as the placement problem that a rewritten plan would have in the frame.
        reason: String,
    },
}

/// One application of a rewrite, applied or skipped, as `explain` lists it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RewriteRecord {
    /// The rewrite.
    pub id: RewriteId,

    /// Whether it was applied.
    pub outcome: RewriteOutcome,

    /// The nodes it changes, removes, or inserts, in plan order of the plan it was applied to, then inserted nodes.
    ///
    /// The nodes identify the application's site: the pipeline never accepts an application at a site it skipped.
    pub nodes: Vec<NodeId>,

    /// Why the rewrite is sound here: the proof obligations it checked, by name, and how each holds.
    pub justification: String,

    /// What it changes in internal telemetry.
    pub telemetry: Vec<TelemetryEffect>,

    /// Whether its equivalence relies on the specification's capacity rule: it holds on every trace without capacity
    /// pressure, and under pressure it only reduces capacity loss.
    pub capacity_rule: bool,
}

impl RewriteRecord {
    /// Returns `true` if the rewrite was applied.
    pub fn is_applied(&self) -> bool {
        matches!(self.outcome, RewriteOutcome::Applied)
    }
}

/// One application a rewrite proposes: the rewritten plan, and what its record says.
#[derive(Clone, Debug)]
pub struct Rewritten {
    /// The whole plan, rewritten at one site.
    pub plan: Plan,

    /// The nodes it changes, removes, or inserts; see [`RewriteRecord::nodes`].
    pub nodes: Vec<NodeId>,

    /// Why the rewrite is sound here: the proof obligations it checked, by name, and how each holds.
    pub justification: String,

    /// What it changes in internal telemetry.
    pub telemetry: Vec<TelemetryEffect>,

    /// Whether its equivalence relies on the capacity rule; see [`RewriteRecord::capacity_rule`].
    pub capacity_rule: bool,
}

impl Rewritten {
    fn into_record(self, id: RewriteId, outcome: RewriteOutcome) -> (Plan, RewriteRecord) {
        let record = RewriteRecord {
            id,
            outcome,
            nodes: self.nodes,
            justification: self.justification,
            telemetry: self.telemetry,
            capacity_rule: self.capacity_rule,
        };
        (self.plan, record)
    }
}

/// What a rewrite sees of the plan it rewrites, and of where it runs.
#[derive(Clone, Copy)]
pub struct RewriteContext<'a> {
    registry: &'a OperatorRegistry,
    frame: &'a Frame,
    report: &'a ValidationReport,
    profile: Option<&'a r7::OperandProfile>,
    /// The records of the applications considered so far, whose skipped sites a rewrite must not propose again.
    records: &'a [RewriteRecord],
}

impl<'a> RewriteContext<'a> {
    /// Creates the context of a plan whose validation report is `report`, to be placed into `frame`.
    pub fn new(registry: &'a OperatorRegistry, frame: &'a Frame, report: &'a ValidationReport) -> Self {
        Self {
            registry,
            frame,
            report,
            profile: None,
            records: &[],
        }
    }

    /// Orders R7's operands by `profile` instead of by its static cost order.
    ///
    /// The profile is an explicit input of compilation, never a runtime flag: two compilations with the same profile
    /// produce the same programs.
    pub fn with_profile(mut self, profile: &'a r7::OperandProfile) -> Self {
        self.profile = Some(profile);
        self
    }

    /// Returns the operator registry.
    pub fn registry(&self) -> &'a OperatorRegistry {
        self.registry
    }

    /// Returns the frame computed at boot, which a rewritten plan must place into.
    pub fn frame(&self) -> &'a Frame {
        self.frame
    }

    /// Returns the validation report of the plan being rewritten, with its port types.
    pub fn report(&self) -> &'a ValidationReport {
        self.report
    }

    /// Returns the profile R7 orders operands by, if compilation was given one.
    pub fn profile(&self) -> Option<&'a r7::OperandProfile> {
        self.profile
    }

    /// Returns `true` if an application of `id` at the site `nodes` was skipped already: a rewrite must not propose it
    /// again.
    pub fn is_rejected(&self, id: RewriteId, nodes: &[NodeId]) -> bool {
        self.records
            .iter()
            .any(|record| !record.is_applied() && record.id == id && record.nodes == nodes)
    }

    fn with(self, report: &'a ValidationReport, records: &'a [RewriteRecord]) -> Self {
        Self {
            report,
            records,
            ..self
        }
    }
}

impl fmt::Debug for RewriteContext<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RewriteContext")
            .field("profile", &self.profile)
            .field("records", &self.records.len())
            .finish_non_exhaustive()
    }
}

/// A logical rewrite.
///
/// A rewrite proposes one application at a time: the first site, in a deterministic order (plan order of the nodes
/// involved), where its proof obligations hold and that [`RewriteContext::is_rejected`] does not reject. It never
/// checks validation or placement of the rewritten plan itself: [`optimize`] does, for every application.
pub trait Rewrite: Sync {
    /// Returns the rewrite's identifier.
    fn id(&self) -> RewriteId;

    /// Returns one application of the rewrite to `plan`, or `None` if it applies nowhere.
    fn apply(&self, ctx: &RewriteContext<'_>, plan: &Plan) -> Option<Rewritten>;
}

/// A plan after the logical rewrites, with what they did.
#[derive(Clone, Debug)]
pub struct Optimized {
    /// The rewritten plan: what placement and program compilation run on.
    pub plan: Plan,

    /// Every application, applied or skipped, in the order the pipeline considered them.
    pub records: Vec<RewriteRecord>,

    /// Whether the pipeline reached a fixed point within [`MAX_PASSES`] and [`MAX_APPLICATIONS`].
    pub converged: bool,
}

impl Optimized {
    /// Returns `true` if a rewrite was applied, so that the plan differs from the one optimized.
    pub fn changed(&self) -> bool {
        self.records.iter().any(RewriteRecord::is_applied)
    }
}

/// Runs the logical rewrites over a plan whose validation report is the context's, to a fixed point.
///
/// See the [module documentation](self) for the order, the checks every application passes, and the bounds.
pub fn optimize(plan: &Plan, ctx: &RewriteContext<'_>) -> Optimized {
    run_pipeline(plan, ctx, &PIPELINE)
}

/// Runs `rewrites`, in order, as [`optimize`] runs [`PIPELINE`]: to a fixed point, checking every application.
///
/// Tests run one rewrite alone this way, with the same checks as the pipeline.
fn run_pipeline(plan: &Plan, ctx: &RewriteContext<'_>, rewrites: &[&dyn Rewrite]) -> Optimized {
    let mut optimized = Optimized {
        plan: plan.clone(),
        records: Vec::new(),
        converged: true,
    };
    if !ctx.report.is_valid() {
        return optimized;
    }

    // A plan that does not place is not compiled, so nothing is rewritten; the placement of the plan so far is what
    // each application's is compared with.
    let Ok(mut placement) = place_unvalidated(plan, ctx.registry, ctx.frame) else {
        return optimized;
    };
    let mut report = ctx.report.clone();
    let mut applications = 0;
    for _ in 0..MAX_PASSES {
        let mut changed = false;
        for rewrite in rewrites {
            let id = rewrite.id();
            loop {
                if applications == MAX_APPLICATIONS {
                    optimized.converged = false;
                    return optimized;
                }
                let step = ctx.with(&report, &optimized.records);
                let Some(rewritten) = rewrite.apply(&step, &optimized.plan) else {
                    break;
                };
                applications += 1;
                if step.is_rejected(id, &rewritten.nodes) {
                    // A rewrite that proposes a site it was skipped at would propose it forever.
                    break;
                }
                match check(ctx, &placement, &rewritten.plan) {
                    Ok((checked, placed)) => {
                        let (plan, record) = rewritten.into_record(id, RewriteOutcome::Applied);
                        optimized.plan = plan;
                        optimized.records.push(record);
                        report = checked;
                        placement = placed;
                        changed = true;
                    }
                    Err(reason) => {
                        let (_, record) = rewritten.into_record(id, RewriteOutcome::Skipped { reason });
                        optimized.records.push(record);
                    }
                }
            }
        }
        if !changed {
            return optimized;
        }
    }
    optimized.converged = false;
    optimized
}

/// Checks a rewritten plan: it must validate and place into the frame, with every kernel in the slot `placement`, the
/// placement of the plan before the rewrite, gives it. Returns its validation report and its placement.
fn check(
    ctx: &RewriteContext<'_>, placement: &Placement, plan: &Plan,
) -> Result<(ValidationReport, Placement), String> {
    let report = revalidate(ctx.registry, plan);
    if !report.is_valid() {
        let errors = report.errors().map(ToString::to_string).collect::<BTreeSet<_>>();
        return Err(format!(
            "the rewritten plan would not validate: {}",
            errors.into_iter().collect::<Vec<_>>().join("; ")
        ));
    }
    let placed = place_unvalidated(plan, ctx.registry, ctx.frame)
        .map_err(|error| format!("the rewritten plan would not fit the frame built at boot: {}", error))?;
    if let Some(moved) = moved_kernel(ctx, placement, &placed, plan) {
        return Err(moved);
    }
    Ok((report, placed))
}

/// Returns why the rewritten plan `plan`, placed as `placed`, moves a kernel to another slot than `placement` runs it
/// in, if it does.
///
/// A kernel is built in a resource group named for its slot and node, so moving it would change its memory accounting
/// and the frame's cost cuts, which no rewrite's record lists. This happens when a filter moves above a kernel across
/// a cut R1 made between them: the kernel would then be fed from the filter's slot, which has no exit back.
fn moved_kernel(ctx: &RewriteContext<'_>, placement: &Placement, placed: &Placement, plan: &Plan) -> Option<String> {
    let graph = super::resolve(plan, ctx.registry).ok()?;
    let slot = |slot: SlotId| format!("`{}`", ctx.frame.describe_target(&FrameTarget::Slot(slot)));
    (0..graph.nodes.len()).find_map(|node| {
        graph.op(node)?.kernel()?;
        let id = graph.id(node);
        match (placement.site(id), placed.site(id)) {
            (Some(Site::Slot(before)), Some(Site::Slot(after))) if before != after => Some(format!(
                "the rewritten plan would move the kernel `{}` from {} to {}, which changes the resource group it is \
                 built in",
                id,
                slot(before),
                slot(after)
            )),
            _ => None,
        }
    })
}

#[cfg(test)]
mod random_plans;
#[cfg(test)]
mod tests;
