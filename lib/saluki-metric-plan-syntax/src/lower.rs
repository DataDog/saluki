//! Lowering: a [syntax tree](crate::ast) to the artifact it describes, with the spans of its parts.
//!
//! Part B of the [pipeline syntax](../../../docs/design/programmable-metrics-pipeline-syntax.md) work replaces this
//! stub.

use saluki_metric_plan::ir::{ArtifactBase, PolicyArtifact};

use crate::{ast::Program, diagnostic::Error, spans::SpanMap};

/// Lowers a program to its artifact, in canonical order, with the span of every node, parameter value, and edge.
///
/// A program without a `base` statement records `default_base`, if given.
///
/// # Errors
///
/// Returns every problem found: a missing `base` without a default, a name defined twice or used before it is
/// defined, an unused stream or port, a node with several output ports used without a port, a derived ID that collides
/// with another node's, and any reference to a port a node does not have. If there are none, returns an error if the
/// artifact's JSON is longer than the artifact size limit.
pub(crate) fn lower(
    _source: &str, _program: Program, _default_base: Option<&ArtifactBase>,
) -> Result<(PolicyArtifact, SpanMap), Vec<Error>> {
    todo!("part B: lowering")
}
