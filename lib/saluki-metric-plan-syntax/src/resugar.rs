//! Resugaring: an artifact to the [syntax tree](crate::ast) a person would write for it.
//!
//! Part C of the [pipeline syntax](../../../docs/design/programmable-metrics-pipeline-syntax.md) work replaces this
//! stub.

use saluki_metric_plan::ir::PolicyArtifact;

use crate::{ast::Program, PrintOptions};

/// Returns the tree that describes `artifact`, with sugar wherever lowering it gives back exactly the same graph.
///
/// Total: any artifact gives a tree. For a well-formed artifact in canonical order, lowering the tree gives back the
/// artifact. `options` says whether the tree has a `base` statement, and whether it lists output fingerprints.
pub(crate) fn resugar(_artifact: &PolicyArtifact, _options: &PrintOptions) -> Program {
    todo!("part C: resugaring")
}
