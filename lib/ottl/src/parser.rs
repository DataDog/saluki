//! OTTL Parser Implementation
//!
//! This module contains the Parser struct and its implementation.

use super::{CallbackMap, EnumMap, EvalContext, OttlParser, PathResolver, Result, Value};

// =====================================================================================================================
/// OTTL Parser that parses input strings and produces executable objects.
///
/// Create a parser using [`Parser::new`], then use the [`OttlParser`] trait methods
/// to check for errors and execute the statement.
pub struct Parser<'a> {
    /// Map of editor function names to their implementations
    editors: &'a mut CallbackMap,
    /// Map of converter function names to their implementations
    converters: &'a mut CallbackMap,
    /// Map of enum names to their integer values
    enums: &'a mut EnumMap,
    /// Function to resolve paths to PathAccessor implementations
    path_resolver: &'a mut PathResolver,
}

impl<'a> Parser<'a> {
    /// Creates a new parser with the given configuration.
    ///
    /// # Arguments
    ///   * `editors_map` - Map of editor function names to their callback implementations.
    ///     Editors are functions that modify the context (e.g., `set`, `delete`).
    ///   * `converters_map` - Map of converter function names to their callback implementations.
    ///     Converters are functions that transform values (e.g., `Concat`, `Int`).
    ///   * `enums_map` - Map of enum names to their integer values (e.g., `SEVERITY_INFO` -> 9).
    ///   * `path_resolver_cb` - Function to resolve path strings to PathAccessor implementations.
    ///   * `expression` - The OTTL expression string to parse (e.g., `"set(attributes[\"key\"], \"value\")"`)
    ///
    /// # Returns
    ///
    /// A new `Parser` instance configured with the provided callbacks and ready to execute.
    pub fn new(
        editors_map: &'a mut CallbackMap, converters_map: &'a mut CallbackMap, enums_map: &'a mut EnumMap,
        path_resolver_cb: &'a mut PathResolver, _expression: &str,
    ) -> Self {
        Self {
            editors: editors_map,
            converters: converters_map,
            enums: enums_map,
            path_resolver: path_resolver_cb,
        }
    }
}

/// Implementation of the [`OttlParser`] trait for [`Parser`].
impl<'a> OttlParser for Parser<'a> {
    fn is_error(&self) -> Result<()> {
        // TODO: implement actual error checking
        Ok(())
    }

    fn execute(&self, _ctx: &mut EvalContext) -> Result<Value> {
        // TODO: implement actual execution logic
        Ok(Value::Nil)
    }
}
