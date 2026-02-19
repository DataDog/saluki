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

/// Resolved path with pre-computed path string and accessor (resolved at parse time)
#[derive(Clone)]
pub struct ResolvedPath {
    /// Pre-computed full path string (e.g., "my.int.value")
    pub full_path: String,
    /// Pre-resolved accessor (resolved once at parse time, not at each execution)
    pub accessor: Arc<dyn PathAccessor + Send + Sync>,
    /// Optional indexes for indexing into the result
    pub indexes: Vec<IndexExpr>,
}

impl std::fmt::Debug for ResolvedPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResolvedPath")
            .field("full_path", &self.full_path)
            .field("indexes", &self.indexes)
            .finish()
    }
}

/// Arena-based BoolExpr using indices instead of Box
#[derive(Debug, Clone)]
pub enum ArenaBoolExpr {
    Literal(bool),
    Comparison {
        left: ValueExprRef,
        op: CompOp,
        right: ValueExprRef,
    },
    Converter(FunctionCallRef),
    /// Path with pre-resolved accessor
    Path(ResolvedPath),
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

/// Arena-based ValueExpr using indices instead of Box
#[derive(Debug, Clone)]
pub enum ArenaValueExpr {
    Literal(Value),
    /// Path with pre-resolved accessor (no runtime lookup!)
    Path(ResolvedPath),
    List(Vec<ValueExprRef>),
    Map(Vec<(String, ValueExprRef)>),
    FunctionCall(FunctionCallRef),
    Math(MathExprRef),
}

/// Arena-based FunctionCall using indices for args
#[derive(Clone)]
pub struct ArenaFunctionCall {
    pub name: String,
    pub is_editor: bool,
    pub args: Vec<ArenaArgExpr>,
    pub indexes: Vec<IndexExpr>,
    pub callback: Option<CallbackFn>,
}

impl std::fmt::Debug for ArenaFunctionCall {
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

/// Arena-based EditorStatement
#[derive(Debug, Clone)]
pub struct ArenaEditorStatement {
    pub editor: FunctionCallRef,
    pub condition: Option<BoolExprRef>,
}

/// Arena-based root expression
#[derive(Debug, Clone)]
pub enum ArenaRootExpr {
    EditorStatement(ArenaEditorStatement),
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

/// Function invocation (Editor or Converter)
#[derive(Clone)]
pub struct FunctionCall {
    /// Function name
    pub name: String,
    /// Whether this is an editor (lowercase) or converter (uppercase)
    pub is_editor: bool,
    /// Arguments
    pub args: Vec<ArgExpr>,
    /// Optional indexes (for converters)
    pub indexes: Vec<IndexExpr>,
    /// Callback reference (resolved at parse time)
    pub callback: Option<CallbackFn>,
}

impl std::fmt::Debug for FunctionCall {
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

/// Argument expression
#[derive(Debug, Clone)]
pub enum ArgExpr {
    /// Positional argument
    Positional(ValueExpr),
    /// Named argument
    Named { name: String, value: ValueExpr },
}

/// Value expression - any value in OTTL
#[derive(Debug, Clone)]
pub enum ValueExpr {
    /// Literal value
    Literal(Value),
    /// Path expression
    Path(PathExpr),
    /// List literal
    List(Vec<ValueExpr>),
    /// Map literal
    Map(Vec<(String, ValueExpr)>),
    /// Function call (converter or editor)
    FunctionCall(Box<FunctionCall>),
    /// Math expression
    Math(Box<MathExpr>),
}

/// Math expression with operator precedence
#[derive(Debug, Clone)]
pub enum MathExpr {
    /// Primary value (literal, path, converter, or grouped expression)
    Primary(ValueExpr),
    /// Unary negation
    Negate(Box<MathExpr>),
    /// Binary operation: term (+/-) or factor (*/)
    Binary {
        left: Box<MathExpr>,
        op: MathOp,
        right: Box<MathExpr>,
    },
}

/// Boolean expression with operator precedence
#[derive(Debug, Clone)]
pub enum BoolExpr {
    /// Literal boolean
    Literal(bool),
    /// Comparison expression
    Comparison {
        left: ValueExpr,
        op: CompOp,
        right: ValueExpr,
    },
    /// Converter call returning boolean
    Converter(Box<FunctionCall>),
    /// Path that evaluates to boolean
    Path(PathExpr),
    /// Logical NOT
    Not(Box<BoolExpr>),
    /// Logical AND
    And(Box<BoolExpr>, Box<BoolExpr>),
    /// Logical OR
    Or(Box<BoolExpr>, Box<BoolExpr>),
}

/// Editor invocation statement
#[derive(Debug, Clone)]
pub struct EditorStatement {
    /// The editor function call
    pub editor: FunctionCall,
    /// Optional WHERE clause condition
    pub condition: Option<BoolExpr>,
}

/// Root AST node - either an editor statement, a boolean expression, or a math expression
#[derive(Debug, Clone)]
pub enum RootExpr {
    /// Editor invocation with optional WHERE clause
    EditorStatement(EditorStatement),
    /// Standalone boolean expression
    BooleanExpression(BoolExpr),
    /// Standalone math expression
    MathExpression(MathExpr),
}
