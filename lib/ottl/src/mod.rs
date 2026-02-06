//AZH: temporary, since this is just an API review we don't have yet code which is using it.
#![allow(dead_code)]

/// OTTL Parser Library
///
/// This library provides a Rust implementation of the OpenTelemetry Transformation Language (OTTL)
/// parser with callback binding at parse time.
///
/// # Example
///
/// ```ignore
/// use rottl::{Parser, OttlParser, CallbackMap, EnumMap, PathResolver};
///
/// let mut editors = CallbackMap::new();
/// let mut converters = CallbackMap::new();
/// let mut enums = EnumMap::new();
/// let mut resolver = ...;
///
/// let parser = Parser::new(&mut editors, &mut converters, &mut enums, &mut resolver, "set(\"my.attr\", 1) where 1 > 0");
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
    /// String value
    /// AZH: TODO: consider to use Arc for reference counting in case of cloning and not making full copy with going to heap, locks, etc.
    String(String),
    /// Bytes literal (e.g., 0xC0FFEE)
    Bytes(Vec<u8>),
    /// List of values
    List(Vec<Value>),
    /// Map of string keys to values
    Map(HashMap<String, Value>),
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

/// Methods for extracting data from [`Argument`] values.
///
/// These methods provide a unified interface for accessing argument values
/// regardless of whether the argument is positional or named.
///
/// # Examples
///
/// ```ignore
/// let pos_arg = Argument::Positional(Value::Int(42));
/// let named_arg = Argument::Named { name: String::from("count"), value: Value::Int(10) };
///
/// // Both return the inner value
/// assert_eq!(pos_arg.value(), &Value::Int(42));
/// assert_eq!(named_arg.value(), &Value::Int(10));
///
/// // Only named arguments have a name
/// assert_eq!(pos_arg.name(), None);
/// assert_eq!(named_arg.name(), Some("count"));
/// ```
impl Argument {
    /// Returns a reference to the value of this argument.
    pub fn value(&self) -> &Value {
        match self {
            Argument::Positional(v) => v,
            Argument::Named { value, .. } => value,
        }
    }

    /// Get the name if this is a named argument
    pub fn name(&self) -> Option<&str> {
        match self {
            Argument::Positional(_) => None,
            Argument::Named { name, .. } => Some(name),
        }
    }
}

// =====================================================================================================================
// Path Accessor Types
// =====================================================================================================================

/// Trait for accessing (reading and writing) path values in the context.
pub trait PathAccessor: fmt::Debug {
    /// Get the value at this path from the context
    fn get(&self, ctx: &EvalContext, path: &str) -> Result<&Value>;

    /// Set the value at this path in the context
    fn set(&self, ctx: &mut EvalContext, path: &str, value: &Value) -> Result<()>;
}

/// Type alias for the path resolver function.
/// Takes a path string (e.g., "body.attributes.key") and returns a PathAccessor.
pub type PathResolver = Arc<dyn Fn(&str) -> Result<Arc<dyn PathAccessor + Send + Sync>> + Send + Sync>;

// =====================================================================================================================
// Callback Types
// =====================================================================================================================

/// Callback function type for editors and converters.
/// Takes a mutable context and a list of arguments, returns a Value or error.
pub type CallbackFn = Arc<dyn Fn(&mut EvalContext, Vec<Argument>) -> Result<Value> + Send + Sync>;

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
/// use rottl::{OttlParser, Parser};
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
