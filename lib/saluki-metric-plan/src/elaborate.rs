//! Elaboration: the one pass from a program as written to the core program that installs.
//!
//! A program as written refers to configuration by binding, leaves parameters at their defaults, writes sets in any
//! order, and names sources, outputs, and recipients. Elaboration resolves all of it and writes the **core program**:
//! every binding written as its value, every parameter in its operator's encoded normal form, every name resolved to a
//! plan ID or port. The core plan is what installs, what [`Plan::hash`] hashes (the one hash), what the plan diff,
//! kernel keys, and adoption compare, and what explain shows. Which binding each literal came from is kept beside the
//! plan, as [`BindingUses`], and is not hashed. The default program goes through the same pass
//! ([`elaborate_default`]).
//!
//! # Missing
//!
//! The types are fixed for V4, which writes composition against them; the passes are V6's.

use std::collections::BTreeMap;

use crate::{
    canonical::{CanonicalError, PlanHash},
    compose::{Approval, ComposeError, ComposeOutcome},
    ir::{ArtifactNodeId, BindingName, NodeId, Plan, ReceivedArtifact},
    operators::OperatorRegistry,
    validate::{Catalog, ValidationReport},
};

/// A program elaborated into its core form.
#[derive(Clone, Debug)]
pub struct Elaborated {
    /// The core plan: every binding written as its value, every parameter in normal form.
    pub plan: Plan,

    /// The core plan's validation report. A program whose report has errors is rejected.
    pub report: ValidationReport,

    /// Which plan node each artifact node became. Empty for the default program.
    pub ids: BTreeMap<ArtifactNodeId, NodeId>,

    /// Which literal of the core plan came from which binding: an annotation explain prints, not part of the hash.
    pub bindings: BindingUses,

    /// Whether the artifact was approved against an active plan other than the one it is elaborated against.
    pub stale_review: bool,

    /// Whether the artifact's `emit`s were composed as written, or, for the artifact in effect, some were dropped.
    pub outcome: ComposeOutcome,
}

impl Elaborated {
    /// Returns the core plan's hash: the one hash of what the program does.
    ///
    /// # Errors
    ///
    /// Returns an error if the plan has no canonical form.
    pub fn hash(&self) -> Result<PlanHash, CanonicalError> {
        let _ = &self.plan;
        todo!("V6: the core plan's hash, `self.plan.hash()` once `Plan::hash` takes no argument")
    }
}

/// A path to one value inside a node's parameters: an RFC 6901 JSON pointer relative to the node's `params` object,
/// such as `/predicate/intake_name/matches/values`.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ParamPath(pub String);

/// Which literal of a core plan came from which configuration binding, by node.
///
/// The core plan writes every binding as its value, so that a binding and the same literal elaborate to one plan, with
/// one hash. This keeps the provenance for explain, which prints `<- config.x` beside the literal.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct BindingUses(pub BTreeMap<NodeId, Vec<(ParamPath, BindingName)>>);

/// Elaborates a received artifact, a whole program, with the host's sources and outputs into its core form.
///
/// This is V4's [`compose`](crate::compose::compose), followed by the core form's passes.
///
/// # Errors
///
/// Returns an error if the artifact cannot be composed at all; see [`ComposeError`].
pub fn elaborate(
    default_plan: &Plan, artifact: &ReceivedArtifact, approval: &Approval, catalog: &dyn Catalog,
    registry: &OperatorRegistry,
) -> Result<Elaborated, ComposeError> {
    let _ = (default_plan, artifact, approval, catalog, registry);
    todo!("V6: elaboration")
}

/// Elaborates a lowered default program into its core form, with the same passes [`elaborate`] runs after resolving
/// an artifact's names.
pub fn elaborate_default(lowered: &Plan, catalog: &dyn Catalog, registry: &OperatorRegistry) -> Elaborated {
    let _ = (lowered, catalog, registry);
    todo!("V6: elaboration of the default program")
}
