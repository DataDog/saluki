//! OTTL Parser Implementation
//!
//! This module is split into submodules for maintainability:
//! - `ast`: AST type definitions (arena-based and boxed)
//! - `arena`: Arena storage and AST conversion with constant folding
//! - `grammar`: Chumsky parser
//! - `eval`: AST evaluation functions

mod arena;
mod ast;
mod eval;
mod grammar;
mod ops;

use arena::{convert_to_arena, AstArena};
// Re-export AST types that may be needed externally
pub use ast::*;
use eval::arena_evaluate_root;
use grammar::build_parser;

use crate::lexer::Token;
use crate::{BoxError, CallbackMap, EnumMap, OttlParser, PathResolverMap, Result, Value};

// =====================================================================================================================
// Parser Implementation
// =====================================================================================================================

/// OTTL Parser that parses input strings and produces executable objects.
/// Paths are resolved at parse time (not at execution time) for maximum performance.
/// Generic over context type `C` so no type erasure or `'static` is required.
pub struct Parser<C> {
    /// Arena-based AST for cache-friendly execution
    arena: AstArena<C>,
    /// Arena-based root expression
    arena_root: Option<ArenaRootExpr<C>>,
    /// Parsing errors
    errors: Vec<String>,
}

impl<C> Parser<C> {
    /// Creates a new parser with the given configuration.
    /// Each path that appears in the expression must have a corresponding entry in `path_resolvers`;
    /// otherwise parsing fails with an error.
    pub fn new(
        editors_map: &CallbackMap<C>,
        converters_map: &CallbackMap<C>,
        enums_map: &EnumMap,
        path_resolvers: &PathResolverMap<C>,
        expression: &str,
    ) -> Self {
        let mut parser = Parser::<C> {
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
impl<C> OttlParser<C> for Parser<C> {
    fn is_error(&self) -> Result<()> {
        if self.errors.is_empty() {
            Ok(())
        } else {
            Err(self.errors.join("; ").into())
        }
    }

    fn execute(&self, ctx: &mut C) -> Result<Value> {
        let arena_root = self
            .arena_root
            .as_ref()
            .ok_or_else(|| -> BoxError { "No AST available (parsing failed)".into() })?;

        // No runtime path resolution - paths are pre-resolved at parse time!
        arena_evaluate_root(arena_root, &self.arena, ctx)
    }
}
