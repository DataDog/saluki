//! The parser: tokens to a [syntax tree](crate::ast), whose parameter values and predicates are already in the JSON
//! shapes of the IR.
//!
//! Part A of the [pipeline syntax](../../../docs/design/programmable-metrics-pipeline-syntax.md) work replaces this
//! stub.

use crate::{ast::Program, diagnostic::Error, lexer::Token};

/// The deepest that lists, records, predicates, and parenthesized expressions may nest.
pub(crate) const MAX_DEPTH: usize = 128;

/// Parses a program from its tokens, which end with [`TokenKind::Eof`](crate::lexer::TokenKind::Eof).
///
/// # Errors
///
/// Returns the first syntax error, after any notes that explain it.
pub(crate) fn parse_program(_source: &str, _tokens: &[Token]) -> Result<Program, Vec<Error>> {
    todo!("part A: the pipeline parser")
}
