//! AST type definitions for OTTL parser
//!
//! Contains both arena-based types (for efficient evaluation) and
//! original boxed types (used during parsing).

use std::sync::Arc;

use crate::{CallbackFn, PathAccessor, Value};

// =====================================================================================================================
// Shared Types
// =====================================================================================================================

/// Comparison operators
#[derive(Debug, Clone, PartialEq, Copy)]
pub enum CompOp {
    Eq,
    NotEq,
    Less,
    Greater,
    LessEq,
    GreaterEq,
}

/// Math operators
#[derive(Debug, Clone, Copy)]
pub enum MathOp {
    Add,
    Sub,
    Mul,
    Div,
}

/// Index for path or converter result
#[derive(Debug, Clone)]
pub enum IndexExpr {
    /// String index like ["key"]
    String(String),
    /// Integer index like [0]
    Int(usize),
}

// =====================================================================================================================
// Arena-based AST Types (used during evaluation - cache-friendly)
// we are using u32 as index on purpose: usize is considered as too fat
// and cache locality will be better for 32 bits.
// 16 bits are considered as too small for generated AST (test purposes)
// =====================================================================================================================

/// Index into the arena for BoolExpr nodes
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BoolExprRef(pub(crate) u32);

/// Index into the arena for MathExpr nodes
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MathExprRef(pub(crate) u32);

/// Index into the arena for ValueExpr nodes
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ValueExprRef(pub(crate) u32);

/// Index into the arena for FunctionCall nodes
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FunctionCallRef(pub(crate) u32);

/// Resolved path with pre-computed path string and accessor (resolved at parse time).
/// Generic over context type `C` so the accessor operates on that context.
#[derive(Clone)]
pub struct ResolvedPath<C> {
    /// Pre-computed full path string (e.g., "my.int.value")
    pub full_path: String,
    /// Pre-resolved accessor (resolved once at parse time, not at each execution)
    pub accessor: Arc<dyn PathAccessor<C> + Send + Sync>,
    /// Optional indexes for indexing into the result
    pub indexes: Vec<IndexExpr>,
}

impl<C> std::fmt::Debug for ResolvedPath<C> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResolvedPath")
            .field("full_path", &self.full_path)
            .field("indexes", &self.indexes)
            .finish()
    }
}

/// Arena-based BoolExpr using indices instead of Box. Generic over context type `C`.
#[derive(Debug, Clone)]
pub enum ArenaBoolExpr<C> {
    Literal(bool),
    Comparison {
        left: ValueExprRef,
        op: CompOp,
        right: ValueExprRef,
    },
    Converter(FunctionCallRef),
    /// Path with pre-resolved accessor
    Path(ResolvedPath<C>),
    Not(BoolExprRef),
    And(BoolExprRef, BoolExprRef),
    Or(BoolExprRef, BoolExprRef),
}

/// Arena-based MathExpr using indices instead of Box
#[derive(Debug, Clone)]
pub enum ArenaMathExpr {
    Primary(ValueExprRef),
    Negate(MathExprRef),
    Binary {
        left: MathExprRef,
        op: MathOp,
        right: MathExprRef,
    },
}

/// Arena-based ValueExpr using indices instead of Box. Generic over context type `C`.
#[derive(Debug, Clone)]
pub enum ArenaValueExpr<C> {
    Literal(Value),
    /// Path with pre-resolved accessor (no runtime lookup!)
    Path(ResolvedPath<C>),
    List(Vec<ValueExprRef>),
    Map(Vec<(String, ValueExprRef)>),
    FunctionCall(FunctionCallRef),
    Math(MathExprRef),
}

/// Arena-based FunctionCall using indices for args. Generic over context type `C`.
#[derive(Clone)]
pub struct ArenaFunctionCall<C> {
    pub name: String,
    pub is_editor: bool,
    pub args: Vec<ArenaArgExpr>,
    pub indexes: Vec<IndexExpr>,
    pub callback: Option<CallbackFn<C>>,
}

impl<C> std::fmt::Debug for ArenaFunctionCall<C> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ArenaFunctionCall")
            .field("name", &self.name)
            .field("is_editor", &self.is_editor)
            .field("args", &self.args)
            .field("indexes", &self.indexes)
            .field("callback", &self.callback.is_some())
            .finish()
    }
}

/// Arena-based ArgExpr
#[derive(Debug, Clone)]
pub enum ArenaArgExpr {
    Positional(ValueExprRef),
    Named { name: String, value: ValueExprRef },
}

/// Arena-based EditorStatement. Holds [`PhantomData<C>`] so [`ArenaRootExpr<C>`] is well-formed.
#[derive(Debug, Clone)]
pub struct ArenaEditorStatement<C> {
    pub editor: FunctionCallRef,
    pub condition: Option<BoolExprRef>,
    pub(crate) _marker: std::marker::PhantomData<C>,
}

/// Arena-based root expression. Generic over context type `C`.
#[derive(Debug, Clone)]
pub enum ArenaRootExpr<C> {
    EditorStatement(ArenaEditorStatement<C>),
    BooleanExpression(BoolExprRef),
    MathExpression(MathExprRef),
}

// =====================================================================================================================
// Original AST Node Types (used during parsing, then converted to arena)
// =====================================================================================================================

/// Path expression (e.g., resource.attributes["key"])
#[derive(Debug, Clone)]
pub struct PathExpr {
    /// Segments of the path (e.g., ["resource", "attributes"])
    pub segments: Vec<String>,
    /// Optional indexes
    pub indexes: Vec<IndexExpr>,
}

/// Function invocation (Editor or Converter). Generic over context type `C`.
#[derive(Clone)]
pub struct FunctionCall<C> {
    /// Function name
    pub name: String,
    /// Whether this is an editor (lowercase) or converter (uppercase)
    pub is_editor: bool,
    /// Arguments
    pub args: Vec<ArgExpr<C>>,
    /// Optional indexes (for converters)
    pub indexes: Vec<IndexExpr>,
    /// Callback reference (resolved at parse time)
    pub callback: Option<CallbackFn<C>>,
}

impl<C: std::fmt::Debug> std::fmt::Debug for FunctionCall<C> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FunctionCall")
            .field("name", &self.name)
            .field("is_editor", &self.is_editor)
            .field("args", &self.args)
            .field("indexes", &self.indexes)
            .field("callback", &self.callback.is_some())
            .finish()
    }
}

/// Argument expression. Generic over context type `C`.
#[derive(Debug, Clone)]
pub enum ArgExpr<C> {
    /// Positional argument
    Positional(ValueExpr<C>),
    /// Named argument
    Named { name: String, value: ValueExpr<C> },
}

/// Value expression - any value in OTTL. Generic over context type `C`.
#[derive(Debug, Clone)]
pub enum ValueExpr<C> {
    /// Literal value
    Literal(Value),
    /// Path expression
    Path(PathExpr),
    /// List literal
    List(Vec<ValueExpr<C>>),
    /// Map literal
    Map(Vec<(String, ValueExpr<C>)>),
    /// Function call (converter or editor)
    FunctionCall(Box<FunctionCall<C>>),
    /// Math expression
    Math(Box<MathExpr<C>>),
}

/// Math expression with operator precedence. Generic over context type `C`.
#[derive(Debug, Clone)]
pub enum MathExpr<C> {
    /// Primary value (literal, path, converter, or grouped expression)
    Primary(ValueExpr<C>),
    /// Unary negation
    Negate(Box<MathExpr<C>>),
    /// Binary operation: term (+/-) or factor (*/)
    Binary {
        left: Box<MathExpr<C>>,
        op: MathOp,
        right: Box<MathExpr<C>>,
    },
}

/// Boolean expression with operator precedence. Generic over context type `C`.
#[derive(Debug, Clone)]
pub enum BoolExpr<C> {
    /// Literal boolean
    Literal(bool),
    /// Comparison expression
    Comparison {
        left: ValueExpr<C>,
        op: CompOp,
        right: ValueExpr<C>,
    },
    /// Converter call returning boolean
    Converter(Box<FunctionCall<C>>),
    /// Path that evaluates to boolean
    Path(PathExpr),
    /// Logical NOT
    Not(Box<BoolExpr<C>>),
    /// Logical AND
    And(Box<BoolExpr<C>>, Box<BoolExpr<C>>),
    /// Logical OR
    Or(Box<BoolExpr<C>>, Box<BoolExpr<C>>),
}

/// Editor invocation statement. Generic over context type `C`.
#[derive(Debug, Clone)]
pub struct EditorStatement<C> {
    /// The editor function call
    pub editor: FunctionCall<C>,
    /// Optional WHERE clause condition
    pub condition: Option<BoolExpr<C>>,
}

/// Root AST node - either an editor statement, a boolean expression, or a math expression.
/// Generic over context type `C`.
#[derive(Debug, Clone)]
pub enum RootExpr<C> {
    /// Editor invocation with optional WHERE clause
    EditorStatement(EditorStatement<C>),
    /// Standalone boolean expression
    BooleanExpression(BoolExpr<C>),
    /// Standalone math expression
    MathExpression(MathExpr<C>),
}
