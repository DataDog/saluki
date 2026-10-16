//! Formatting: a [syntax tree](crate::ast) to text, breaking values and predicates to fit the line width.
//!
//! Part D of the [pipeline syntax](../../../docs/design/programmable-metrics-pipeline-syntax.md) work replaces this
//! stub.

use crate::ast::Program;

/// Writes `program` as text, breaking lines beyond `width` columns. `usize::MAX` writes every value on one line.
///
/// The text depends only on the tree, never on its spans.
pub(crate) fn format(_program: &Program, _width: usize) -> String {
    todo!("part D: formatting")
}
