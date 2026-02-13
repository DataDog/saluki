/// OTTL Parser Library
///
/// This library provides a Rust implementation of the OpenTelemetry Transformation Language (OTTL)
/// parser with callback binding at parse time.
///
/// # Example
///
/// ```ignore
/// use ottl::{Parser, OttlParser, CallbackMap, EnumMap, PathResolverMap};
///
/// let editors = CallbackMap::new();
/// let converters = CallbackMap::new();
/// let enums = EnumMap::new();
/// let path_resolvers = PathResolverMap::new(); // or insert path -> PathResolver for each path in expression
///
/// let parser = Parser::new(&editors, &converters, &enums, &path_resolvers, "set(my.attr, 1) where 1 > 0");
/// let result = parser.execute(&mut ctx);
/// ```
use std::any::Any;
use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::sync::Arc;

pub(crate) mod lexer;
mod parser;

#[cfg(test)]
mod tests;

// Re-export from submodules
pub use parser::Parser;

// =====================================================================================================================
// Error and Result Types
// =====================================================================================================================

/// Standard error type for the library
pub type BoxError = Box<dyn Error + Send + Sync>;

/// Standard result type for the library
pub type Result<T> = std::result::Result<T, BoxError>;

// =====================================================================================================================
// Context Types
// =====================================================================================================================

/// User-provided context passed to callbacks during evaluation.
/// This is a placeholder type that will be refined later.
pub type EvalContext = Box<dyn Any>;

// =====================================================================================================================
// Value Types
// =====================================================================================================================

/// Represents all possible values in OTTL expressions and function arguments.
/// Uses Arc<str> for strings and Arc<[u8]> for bytes to enable cheap cloning.
#[derive(Clone, Default, Debug, PartialEq /* , Eq, PartialOrd - not applicable it seems */)]
pub enum Value {
    /// Nil/null value
    #[default]
    Nil,
    /// Boolean value (true/false)
    Bool(bool),
    /// 64-bit signed integer
    Int(i64),
    /// 64-bit floating point
    Float(f64),
    /// String value (Arc for cheap clone)
    String(Arc<str>),
    /// Bytes literal (e.g., 0xC0FFEE) - Arc for cheap clone
    Bytes(Arc<[u8]>),
    /// List of values
    List(Vec<Value>),
    /// Map of string keys to values  
    Map(HashMap<String, Value>),
}

///Static methods of Value
impl Value {
    ///Static method: create a string value from any string-like type
    #[inline]
    pub fn string(s: impl Into<Arc<str>>) -> Self {
        Value::String(s.into())
    }

    ///Static method:  create a bytes value from any bytes-like type
    #[inline]
    pub fn bytes(b: impl Into<Arc<[u8]>>) -> Self {
        Value::Bytes(b.into())
    }
}

// =====================================================================================================================
// Argument Types
// =====================================================================================================================

/// Argument passed to callback functions.
/// Can be either a positional argument or a named argument.
#[derive(Debug, Clone)]
pub enum Argument {
    /// Positional argument with just a value
    Positional(Value),
    /// Named argument with name and value
    Named { name: String, value: Value },
}

// =====================================================================================================================
// Path Accessor Types
// =====================================================================================================================

/// Trait for accessing (reading and writing) path values in the context.
pub trait PathAccessor: fmt::Debug {
    /// Get the value at this path from the context.
    /// Returns owned Value - for primitive types this is cheap (Copy-like).
    /// For strings/bytes, use Arc internally for cheap cloning.
    fn get(&self, ctx: &EvalContext, path: &str) -> Result<Value>;

    /// Set the value at this path in the context
    fn set(&self, ctx: &mut EvalContext, path: &str, value: &Value) -> Result<()>;
}

/// Type alias for the path resolver function.
/// Returns a PathAccessor.
pub type PathResolver = Arc<dyn Fn() -> Result<Arc<dyn PathAccessor + Send + Sync>> + Send + Sync>;

/// Map from path string to its resolver. Parser looks up each path in the expression
/// in this map; if a path is missing, parsing fails with an error.
pub type PathResolverMap = HashMap<String, PathResolver>;

// =====================================================================================================================
// Callback Types
// =====================================================================================================================

/// Trait for lazy argument evaluation - ZERO ALLOCATION at runtime!
/// Arguments are evaluated only when requested by the callback.
/// Contains both the evaluation context and lazy access to arguments.
pub trait Args {
    /// Access to evaluation context
    fn ctx(&mut self) -> &mut EvalContext;

    /// Number of arguments
    fn len(&self) -> usize;

    /// Check if empty
    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Get argument value by index (lazy evaluation - NO ALLOCATION)
    fn get(&mut self, index: usize) -> Result<Value>;

    /// Get argument name by index (for named arguments)
    fn name(&self, index: usize) -> Option<&str>;

    /// Get named argument value (searches by name, lazy evaluation)
    fn get_named(&mut self, name: &str) -> Option<Result<Value>> {
        for i in 0..self.len() {
            if self.name(i) == Some(name) {
                return Some(self.get(i));
            }
        }
        None
    }

    /// Set value at argument path by index.
    /// The argument at `index` must be a path expression.
    /// This calls PathAccessor::set on the resolved path.
    fn set(&mut self, index: usize, value: &Value) -> Result<()>;
}

/// Callback function type for editors and converters.
/// Uses lazy Args trait for ZERO-ALLOCATION argument evaluation.
/// Args provides both context access and lazy argument retrieval.
pub type CallbackFn = Arc<dyn Fn(&mut dyn Args) -> Result<Value> + Send + Sync>;

/// Map of function names to their callback implementations.
pub type CallbackMap = HashMap<String, CallbackFn>;

/// Map of enum names to their integer values.
pub type EnumMap = HashMap<String, i64>;

// =====================================================================================================================
// Parser API Trait
// =====================================================================================================================

/// Public API trait for the OTTL Parser.
///
/// This trait defines the interface for executing parsed OTTL statements.
/// Implement this trait to create custom parser implementations or use the
/// default [`Parser`] implementation.
///
/// # Example
///
/// ```ignore
/// use ottl::{OttlParser, Parser};
///
/// let parser = Parser::new(...);
///
/// // Check for parsing errors
/// parser.is_error()?;
///
/// // Execute the statement
/// let result = parser.execute(&mut ctx)?;
/// ```
pub trait OttlParser {
    /// Checks if the parser encountered any errors during creation.
    ///
    /// Call this method after creating a parser to verify that the OTTL expression
    /// was parsed successfully.
    ///
    /// # Returns
    ///   * `Ok(())` - if no errors occurred during parsing
    ///   * `Err(BoxError)` - if parsing failed with error details
    fn is_error(&self) -> Result<()>;

    /// Executes this OTTL statement with the given context.
    ///
    /// This method evaluates the parsed OTTL expression, invoking any bound
    /// editor or converter callbacks as needed and resolving path references.
    ///
    /// # Arguments
    ///   * `ctx` - The mutable evaluation context that provides access to telemetry data
    ///     and can be modified by editor functions.
    ///
    /// # Returns
    ///   * `Ok(Value)` - The result of evaluating the expression. If expression has no
    ///     return value, returns `Value::Nil`.
    ///   * `Err(BoxError)` - An error if evaluation fails (e.g., type mismatch,
    ///     missing path, callback error).
    fn execute(&self, ctx: &mut EvalContext) -> Result<Value>;
}
