/// OTTL Parser Library
///
/// This library provides a Rust implementation of the OpenTelemetry Transformation Language (OTTL)
/// parser with callback binding at parse time.
///
/// # Example
///
/// ```ignore
/// use ottl::{Parser, OttlParser, CallbackMap, EnumMap, PathResolverMap, EvalContextFamily};
///
/// struct MyFamily;
/// impl EvalContextFamily for MyFamily {
///     type Context<'a> = MyContext<'a>;
/// }
///
/// let editors = CallbackMap::new();
/// let converters = CallbackMap::new();
/// let enums = EnumMap::new();
/// let path_resolvers = PathResolverMap::<MyFamily>::new();
///
/// let parser = Parser::<MyFamily>::new(&editors, &converters, &enums, &path_resolvers, "set(my.attr, 1) where 1 > 0");
/// let mut ctx = MyContext { /* ... */ };
/// let result = parser.execute(&mut ctx);
/// ```
use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::sync::Arc;

pub mod helpers;
pub(crate) mod lexer;
mod parser;

#[cfg(test)]
mod tests;

// Re-export from submodules
pub use parser::IndexExpr;
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

/// A "family" of evaluation context types, parameterized by lifetime.
///
/// This trait uses Generic Associated Types (GATs) to separate the context family
/// (known at parse time, carries no lifetime) from the concrete context type
/// (instantiated with a lifetime at execution time).
///
/// # Why a family?
///
/// The parser is long-lived and created once, but contexts are short-lived and often
/// borrow data. We can't write `Parser<MyContext<'a>>` because `'a` doesn't exist
/// when the parser is created. Instead, the parser stores the *family* `F`, and
/// `F::Context<'a>` is only materialized when `execute` is called.
///
/// # Example
///
/// ```ignore
/// struct SpanFamily;
///
/// impl EvalContextFamily for SpanFamily {
///     type Context<'a> = SpanContext<'a>;
/// }
///
/// struct SpanContext<'a> {
///     span: &'a mut Span,
///     resource: &'a Resource,
/// }
/// ```
pub trait EvalContextFamily: 'static {
    /// The concrete context type for a given borrow lifetime.
    type Context<'a>;
}

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
///
/// The evaluator calls [`get`](PathAccessor::get) and [`set`](PathAccessor::set) with the path
/// and index list; path interpretation and indexing (e.g. `["key"]`, `[0]`) are the integrator's
/// responsibility.
///
/// The type parameter `F` is the [`EvalContextFamily`] that determines the concrete context type.
/// The lifetime on `F::Context<'a>` is introduced per method call, so `PathAccessor<F>` itself
/// carries no lifetime and can be stored in `Arc<dyn PathAccessor<F>>`.
pub trait PathAccessor<F: EvalContextFamily>: fmt::Debug + Send + Sync {
    /// Get the value at this path with the given indexes applied.
    ///
    /// Path interpretation, including indexing (e.g. `["key"]`, `[0]`), is **not** implemented by
    /// OTTL; the integrator must implement this method. For a typical "get base value then apply
    /// indexes" implementation, use [`crate::helpers::apply_indexes`].
    fn get<'a>(&self, ctx: &F::Context<'a>, path: &str, indexes: &[IndexExpr]) -> Result<Value>;

    /// Set the value at this path with the given indexes applied.
    ///
    /// When `indexes` is empty, the implementor sets the value at the path. When `indexes` is
    /// non-empty (e.g. `my.list[0] = x`), the implementor may support updating at that index
    /// or return an error.
    fn set<'a>(&self, ctx: &mut F::Context<'a>, path: &str, indexes: &[IndexExpr], value: &Value) -> Result<()>;
}

/// Type alias for the path resolver function.
/// Returns a PathAccessor for the given context family.
pub type PathResolver<F> = Arc<dyn Fn() -> Result<Arc<dyn PathAccessor<F>>> + Send + Sync>;

/// Map from path string to its resolver. Parser looks up each path in the expression
/// in this map; if a path is missing, parsing fails with an error.
pub type PathResolverMap<F> = HashMap<String, PathResolver<F>>;

// =====================================================================================================================
// Callback Types
// =====================================================================================================================

/// Trait for lazy argument evaluation - ZERO ALLOCATION at runtime!
/// Arguments are evaluated only when requested by the callback.
pub trait Args {
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
/// The type parameter `F` is the [`EvalContextFamily`] that determines the
/// concrete evaluation context type.
///
/// # Example
///
/// ```ignore
/// use ottl::{OttlParser, Parser, EvalContextFamily};
///
/// let parser = Parser::<MyFamily>::new(...);
///
/// // Check for parsing errors
/// parser.is_error()?;
///
/// // Execute the statement
/// let mut ctx = MyContext { /* ... */ };
/// let result = parser.execute(&mut ctx)?;
/// ```
pub trait OttlParser<F: EvalContextFamily> {
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
    fn execute<'a>(&self, ctx: &mut F::Context<'a>) -> Result<Value>;
}
