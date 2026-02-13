//! OTTL Parser Implementation
//!
//! This module is split into submodules for maintainability:
//! - `ast`: AST type definitions (arena-based and boxed)
//! - `arena`: Arena storage and AST conversion with constant folding
//! - `parser`: Chumsky parser
//! - `eval`: AST evaluation functions

mod arena;
mod ast;
mod eval;
mod ops;
mod parser;

use crate::lexer::Token;
use crate::{
    BoxError, CallbackMap, EnumMap, EvalContext, OttlParser, PathResolverMap, Result, Value,
};

// Re-export AST types that may be needed externally
pub use ast::*;

use arena::{convert_to_arena, AstArena};
use eval::arena_evaluate_root;
use parser::build_parser;

// =====================================================================================================================
// Parser Implementation
// =====================================================================================================================

/// OTTL Parser that parses input strings and produces executable objects.
/// Paths are resolved at parse time (not at execution time) for maximum performance.
pub struct Parser {
    /// Arena-based AST for cache-friendly execution
    arena: AstArena,
    /// Arena-based root expression
    arena_root: Option<ArenaRootExpr>,
    /// Parsing errors
    errors: Vec<String>,
}

impl Parser {
    /// Creates a new parser with the given configuration.
    /// Each path that appears in the expression must have a corresponding entry in `path_resolvers`;
    /// otherwise parsing fails with an error.
    pub fn new(
        editors_map: &CallbackMap,
        converters_map: &CallbackMap,
        enums_map: &EnumMap,
        path_resolvers: &PathResolverMap,
        expression: &str,
    ) -> Self {
        let mut parser = Parser {
            arena: AstArena::new(),
            arena_root: None,
            errors: Vec::new(),
        };

        // Tokenize the input
        let tokens_with_spans = match crate::lexer::Lexer::collect_with_spans(expression) {
            Ok(tokens) => tokens,
            Err(e) => {
                parser.errors.push(format!("Lexer error: {}", e));
                return parser;
            }
        };

        if tokens_with_spans.is_empty() && !expression.trim().is_empty() {
            parser.errors.push("Lexer failed to tokenize input".into());
            return parser;
        }

        // Extract just the tokens for parsing
        let tokens: Vec<Token> = tokens_with_spans.into_iter().map(|(t, _)| t).collect();

        // Build the chumsky parser
        let chumsky_parser = build_parser(editors_map, converters_map, enums_map);

        // Parse using slice input
        use chumsky::Parser as ChumskyParser;
        let result = chumsky_parser.parse(&tokens[..]);

        match result.into_result() {
            Ok(ast) => {
                // Convert to arena-based AST for cache-friendly execution
                // Path resolution happens HERE (once), not at each execution!
                match convert_to_arena(&ast, &mut parser.arena, path_resolvers) {
                    Ok(arena_root) => {
                        parser.arena_root = Some(arena_root);
                    }
                    Err(e) => {
                        parser.errors.push(format!("Path resolution error: {}", e));
                    }
                }
            }
            Err(errs) => {
                for err in errs {
                    parser.errors.push(format!("Parse error: {:?}", err));
                }
            }
        }

        parser
    }
}

/// Implementation of the [`OttlParser`] trait for [`Parser`].
impl OttlParser for Parser {
    fn is_error(&self) -> Result<()> {
        if self.errors.is_empty() {
            Ok(())
        } else {
            Err(self.errors.join("; ").into())
        }
    }

    fn execute(&self, ctx: &mut EvalContext) -> Result<Value> {
        let arena_root = self
            .arena_root
            .as_ref()
            .ok_or_else(|| -> BoxError { "No AST available (parsing failed)".into() })?;

        // No runtime path resolution - paths are pre-resolved at parse time!
        arena_evaluate_root(arena_root, &self.arena, ctx)
    }
}
