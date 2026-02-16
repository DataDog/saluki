//! AST evaluation functions
//!
//! Provides arena-based evaluation with zero-allocation argument passing.

use super::arena::AstArena;
use super::ast::*;
use super::ops::{compare, math_op};
use crate::{Args, BoxError, EvalContext, Result, Value};

// =====================================================================================================================
// Evaluation Helpers
// =====================================================================================================================

/// Apply indexes to a value. Used only for converter/editor return value indexing (e.g. Split(...)[0]).
#[inline]
pub(crate) fn apply_indexes(value: Value, indexes: &[IndexExpr]) -> Result<Value> {
    let mut current = value;
    for index in indexes {
        current = apply_index(&current, index)?;
    }
    Ok(current)
}

/// Apply an index to a value
fn apply_index(value: &Value, index: &IndexExpr) -> Result<Value> {
    match (value, index) {
        (Value::List(list), IndexExpr::Int(i)) => list
            .get(*i)
            .cloned()
            .ok_or_else(|| format!("Index {} out of bounds", i).into()),
        (Value::Map(map), IndexExpr::String(key)) => map
            .get(key)
            .cloned()
            .ok_or_else(|| format!("Key '{}' not found", key).into()),
        (Value::String(s), IndexExpr::Int(i)) => s
            .chars()
            .nth(*i)
            .map(|c| Value::string(c.to_string()))
            .ok_or_else(|| format!("Index {} out of bounds", i).into()),
        _ => Err(format!("Cannot index {:?} with {:?}", value, index).into()),
    }
}

// =====================================================================================================================
// Arena-based AST Evaluation
// =====================================================================================================================

/// Zero-allocation argument evaluator for function calls.
/// Implements lazy evaluation - arguments are only evaluated when requested.
struct ArenaArgs<'a> {
    arena: &'a AstArena,
    args: &'a [ArenaArgExpr],
    ctx: &'a mut EvalContext,
}

impl<'a> ArenaArgs<'a> {
    /// Builds an [`ArenaArgs`] for a call: borrows arena, argument list, and context.
    #[inline]
    fn new(arena: &'a AstArena, args: &'a [ArenaArgExpr], ctx: &'a mut EvalContext) -> Self {
        Self { arena, args, ctx }
    }
}

/// Implementation of [`Args`] for arena-based function/editor calls.
///
/// Arguments are stored as [`ArenaArgExpr`] refs; evaluation is lazy and uses the arena
/// and context to compute values on demand without allocating an argument vector.
impl<'a> Args for ArenaArgs<'a> {
    /// Returns exclusive access to the evaluation context for the duration of the call.
    #[inline]
    fn ctx(&mut self) -> &mut EvalContext {
        self.ctx
    }

    /// Returns the number of arguments (positional and named).
    #[inline]
    fn len(&self) -> usize {
        self.args.len()
    }

    /// Evaluates the argument at `index` via the arena and returns its [`Value`].
    /// Supports both positional and named args; named args are indexed by position.
    #[inline]
    fn get(&mut self, index: usize) -> Result<Value> {
        if index >= self.args.len() {
            return Err(format!("Argument index {} out of bounds", index).into());
        }

        match &self.args[index] {
            ArenaArgExpr::Positional(value_ref) => arena_evaluate_value_expr(*value_ref, self.arena, self.ctx),
            ArenaArgExpr::Named { value, .. } => arena_evaluate_value_expr(*value, self.arena, self.ctx),
        }
    }

    /// Returns the name of the argument at `index` if it is a named argument; `None` for positional.
    #[inline]
    fn name(&self, index: usize) -> Option<&str> {
        if index >= self.args.len() {
            return None;
        }
        match &self.args[index] {
            ArenaArgExpr::Positional(_) => None,
            ArenaArgExpr::Named { name, .. } => Some(name),
        }
    }

    /// Sets the value at the path referenced by the argument at `index`.
    /// Only supported when that argument is a path expression; otherwise returns an error.
    #[inline]
    fn set(&mut self, index: usize, value: &Value) -> Result<()> {
        if index >= self.args.len() {
            return Err(format!("Argument index {} out of bounds", index).into());
        }

        let value_ref = match &self.args[index] {
            ArenaArgExpr::Positional(r) => *r,
            ArenaArgExpr::Named { value, .. } => *value,
        };

        match self.arena.get_value(value_ref) {
            ArenaValueExpr::Path(resolved_path) => resolved_path
                .accessor
                .set(self.ctx, &resolved_path.full_path, &resolved_path.indexes, value),
            _ => Err("set: argument must be a path expression".into()),
        }
    }
}

/// Evaluate the arena-based root expression
#[inline]
pub fn arena_evaluate_root(root: &ArenaRootExpr, arena: &AstArena, ctx: &mut EvalContext) -> Result<Value> {
    match root {
        ArenaRootExpr::EditorStatement(stmt) => {
            let should_execute = if let Some(cond_ref) = stmt.condition {
                arena_evaluate_bool_expr(cond_ref, arena, ctx)?
            } else {
                true
            };

            if should_execute {
                arena_evaluate_function_call(stmt.editor, arena, ctx)?;
            }

            Ok(Value::Nil)
        }
        ArenaRootExpr::BooleanExpression(expr_ref) => {
            let result = arena_evaluate_bool_expr(*expr_ref, arena, ctx)?;
            Ok(Value::Bool(result))
        }
        ArenaRootExpr::MathExpression(expr_ref) => arena_evaluate_math_expr(*expr_ref, arena, ctx),
    }
}

/// Evaluate an arena-based boolean expression
#[inline]
fn arena_evaluate_bool_expr(expr_ref: BoolExprRef, arena: &AstArena, ctx: &mut EvalContext) -> Result<bool> {
    match arena.get_bool(expr_ref) {
        ArenaBoolExpr::Literal(b) => Ok(*b),
        ArenaBoolExpr::Comparison { left, op, right } => {
            let left_val = arena_evaluate_value_expr(*left, arena, ctx)?;
            let right_val = arena_evaluate_value_expr(*right, arena, ctx)?;
            compare(&left_val, op, &right_val)
        }
        ArenaBoolExpr::Converter(fc_ref) => {
            let result = arena_evaluate_function_call(*fc_ref, arena, ctx)?;
            match result {
                Value::Bool(b) => Ok(b),
                _ => Err("Converter did not return a boolean".into()),
            }
        }
        ArenaBoolExpr::Path(resolved_path) => {
            let value = resolved_path.accessor.get(ctx, &resolved_path.full_path, &resolved_path.indexes)?;
            match value {
                Value::Bool(b) => Ok(b),
                _ => Err("Path did not return a boolean".into()),
            }
        }
        ArenaBoolExpr::Not(inner_ref) => {
            let result = arena_evaluate_bool_expr(*inner_ref, arena, ctx)?;
            Ok(!result)
        }
        ArenaBoolExpr::And(left_ref, right_ref) => {
            let left_result = arena_evaluate_bool_expr(*left_ref, arena, ctx)?;
            if !left_result {
                return Ok(false);
            }
            arena_evaluate_bool_expr(*right_ref, arena, ctx)
        }
        ArenaBoolExpr::Or(left_ref, right_ref) => {
            let left_result = arena_evaluate_bool_expr(*left_ref, arena, ctx)?;
            if left_result {
                return Ok(true);
            }
            arena_evaluate_bool_expr(*right_ref, arena, ctx)
        }
    }
}

/// Evaluate an arena-based value expression
#[inline]
fn arena_evaluate_value_expr(expr_ref: ValueExprRef, arena: &AstArena, ctx: &mut EvalContext) -> Result<Value> {
    match arena.get_value(expr_ref) {
        ArenaValueExpr::Literal(v) => Ok(v.clone()),
        ArenaValueExpr::Path(resolved_path) => resolved_path.accessor.get(ctx, &resolved_path.full_path, &resolved_path.indexes),
        ArenaValueExpr::List(items) => {
            let values: Result<Vec<Value>> = items
                .iter()
                .map(|item_ref| arena_evaluate_value_expr(*item_ref, arena, ctx))
                .collect();
            Ok(Value::List(values?))
        }
        ArenaValueExpr::Map(entries) => {
            let mut map = std::collections::HashMap::new();
            for (key, value_ref) in entries {
                let value = arena_evaluate_value_expr(*value_ref, arena, ctx)?;
                map.insert(key.clone(), value);
            }
            Ok(Value::Map(map))
        }
        ArenaValueExpr::FunctionCall(fc_ref) => arena_evaluate_function_call(*fc_ref, arena, ctx),
        ArenaValueExpr::Math(math_ref) => arena_evaluate_math_expr(*math_ref, arena, ctx),
    }
}

/// Evaluate an arena-based function call (ZERO ALLOCATION)
#[inline]
fn arena_evaluate_function_call(fc_ref: FunctionCallRef, arena: &AstArena, ctx: &mut EvalContext) -> Result<Value> {
    let fc = arena.get_func(fc_ref);

    let callback = fc
        .callback
        .as_ref()
        .ok_or_else(|| -> BoxError { format!("Unknown function: {}", fc.name).into() })?;

    let mut args = ArenaArgs::new(arena, &fc.args, ctx);
    let result = callback(&mut args)?;
    apply_indexes(result, &fc.indexes)
}

/// Evaluate an arena-based math expression
#[inline]
fn arena_evaluate_math_expr(expr_ref: MathExprRef, arena: &AstArena, ctx: &mut EvalContext) -> Result<Value> {
    match arena.get_math(expr_ref) {
        ArenaMathExpr::Primary(value_ref) => arena_evaluate_value_expr(*value_ref, arena, ctx),
        ArenaMathExpr::Negate(inner_ref) => {
            let value = arena_evaluate_math_expr(*inner_ref, arena, ctx)?;
            match value {
                Value::Int(i) => Ok(Value::Int(-i)),
                Value::Float(f) => Ok(Value::Float(-f)),
                _ => Err("Cannot negate non-numeric value".into()),
            }
        }
        ArenaMathExpr::Binary {
            left: left_ref,
            op,
            right: right_ref,
        } => {
            let left_val = arena_evaluate_math_expr(*left_ref, arena, ctx)?;
            let right_val = arena_evaluate_math_expr(*right_ref, arena, ctx)?;
            math_op(&left_val, op, &right_val)
        }
    }
}
