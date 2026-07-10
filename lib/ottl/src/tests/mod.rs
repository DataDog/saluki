//! Tests for the OTTL library, split by concern:
//!
//! - [`lexer`]: tokenization of OTTL source into [`crate::lexer::Token`]s.
//! - [`parser`]: parsing and evaluating expressions, including path/index resolution.
//! - [`editors`]: editor/converter statements, indexed writes, and named arguments.
//! - [`errors`]: lexer, parser, syntax, and runtime error handling.
//!
//! Shared fixtures live here; concern-specific accessors and helpers live in their respective submodule.

use crate::{EvalContextFamily, PathResolverMap};

mod editors;
mod errors;
mod lexer;
mod parser;

/// Context family for tests that don't need any context data.
struct UnitFamily;

impl EvalContextFamily for UnitFamily {
    type Context<'a> = ();
}

/// Creates an empty path resolver map (for expressions with no paths).
fn empty_path_resolver_map() -> PathResolverMap<UnitFamily> {
    PathResolverMap::new()
}

/// Builds a `Parser` over `expression` with all editor/converter/enum/path maps empty.
///
/// The overwhelming majority of parser and error-handling tests register no editors, converters, enums, or path
/// resolvers, and previously repeated the same four-line setup inline. `Parser` owns its parsed state (it holds no
/// borrows of the argument maps), so it can be returned after the local maps drop.
fn parse_with_empty_maps(expression: &str) -> crate::parser::Parser<UnitFamily> {
    use crate::{CallbackMap, EnumMap};

    let editors = CallbackMap::new();
    let converters = CallbackMap::new();
    let enums = EnumMap::new();
    let path_resolvers = empty_path_resolver_map();
    crate::parser::Parser::new(&editors, &converters, &enums, &path_resolvers, expression)
}
