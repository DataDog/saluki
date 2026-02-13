//! Common operations for comparison and math evaluation
//!
//! Used by both constant folding (arena.rs) and runtime evaluation (eval.rs)

use super::ast::{CompOp, MathOp};
use crate::{Result, Value};

/// Evaluate a comparison operation between two values
#[inline]
pub fn compare(left: &Value, op: &CompOp, right: &Value) -> Result<bool> {
    match (left, right) {
        (Value::Int(l), Value::Int(r)) => Ok(cmp_ord(l, r, op)),
        (Value::Float(l), Value::Float(r)) => Ok(cmp_ord(l, r, op)),
        (Value::Int(l), Value::Float(r)) => Ok(cmp_ord(&(*l as f64), r, op)),
        (Value::Float(l), Value::Int(r)) => Ok(cmp_ord(l, &(*r as f64), op)),
        (Value::String(l), Value::String(r)) => Ok(cmp_ord(l, r, op)),
        (Value::Bool(l), Value::Bool(r)) => cmp_eq_only(l, r, op, "Boolean"),
        (Value::Nil, Value::Nil) => cmp_eq_only(&(), &(), op, "Nil"),
        (Value::Nil, _) | (_, Value::Nil) => Ok(matches!(op, CompOp::NotEq)),
        (Value::Bytes(l), Value::Bytes(r)) => cmp_eq_only(l, r, op, "Bytes"),
        (Value::List(l), Value::List(r)) => cmp_eq_only(l, r, op, "List"),
        (Value::Map(l), Value::Map(r)) => cmp_eq_only(l, r, op, "Map"),
        _ => match op {
            CompOp::Eq => Ok(false),
            CompOp::NotEq => Ok(true),
            _ => Err(format!("Cannot compare different types with {:?}", op).into()),
        },
    }
}

/// Compare values that support full ordering
#[inline]
fn cmp_ord<T: PartialOrd + PartialEq>(l: &T, r: &T, op: &CompOp) -> bool {
    match op {
        CompOp::Eq => l == r,
        CompOp::NotEq => l != r,
        CompOp::Less => l < r,
        CompOp::Greater => l > r,
        CompOp::LessEq => l <= r,
        CompOp::GreaterEq => l >= r,
    }
}

/// Compare values that only support equality
#[inline]
fn cmp_eq_only<T: PartialEq>(l: &T, r: &T, op: &CompOp, type_name: &str) -> Result<bool> {
    match op {
        CompOp::Eq => Ok(l == r),
        CompOp::NotEq => Ok(l != r),
        _ => Err(format!("{} comparison only supports == and !=", type_name).into()),
    }
}

/// Evaluate a math operation between two values
#[inline]
pub fn math_op(left: &Value, op: &MathOp, right: &Value) -> Result<Value> {
    match (left, right) {
        (Value::Int(l), Value::Int(r)) => int_op(*l, *r, op),
        (Value::Float(l), Value::Float(r)) => float_op(*l, *r, op),
        (Value::Int(l), Value::Float(r)) => float_op(*l as f64, *r, op),
        (Value::Float(l), Value::Int(r)) => float_op(*l, *r as f64, op),
        (Value::String(l), Value::String(r)) if matches!(op, MathOp::Add) => {
            Ok(Value::string(format!("{}{}", l, r)))
        }
        _ => Err(format!(
            "Cannot perform math operation on {:?} and {:?}",
            std::mem::discriminant(left),
            std::mem::discriminant(right)
        )
        .into()),
    }
}

#[inline]
fn int_op(l: i64, r: i64, op: &MathOp) -> Result<Value> {
    Ok(Value::Int(match op {
        MathOp::Add => l + r,
        MathOp::Sub => l - r,
        MathOp::Mul => l * r,
        MathOp::Div if r == 0 => return Err("Division by zero".into()),
        MathOp::Div => l / r,
    }))
}

#[inline]
fn float_op(l: f64, r: f64, op: &MathOp) -> Result<Value> {
    Ok(Value::Float(match op {
        MathOp::Add => l + r,
        MathOp::Sub => l - r,
        MathOp::Mul => l * r,
        MathOp::Div if r == 0.0 => return Err("Division by zero".into()),
        MathOp::Div => l / r,
    }))
}

/// Try math operation for constant folding (returns None on error)
#[inline]
pub fn try_math_op(left: &Value, op: &MathOp, right: &Value) -> Option<Value> {
    match (left, right) {
        (Value::Int(l), Value::Int(r)) => try_int_op(*l, *r, op),
        (Value::Float(l), Value::Float(r)) => try_float_op(*l, *r, op),
        (Value::Int(l), Value::Float(r)) => try_float_op(*l as f64, *r, op),
        (Value::Float(l), Value::Int(r)) => try_float_op(*l, *r as f64, op),
        _ => None,
    }
}

#[inline]
fn try_int_op(l: i64, r: i64, op: &MathOp) -> Option<Value> {
    Some(Value::Int(match op {
        MathOp::Add => l + r,
        MathOp::Sub => l - r,
        MathOp::Mul => l * r,
        MathOp::Div if r == 0 => return None,
        MathOp::Div => l / r,
    }))
}

#[inline]
fn try_float_op(l: f64, r: f64, op: &MathOp) -> Option<Value> {
    Some(Value::Float(match op {
        MathOp::Add => l + r,
        MathOp::Sub => l - r,
        MathOp::Mul => l * r,
        MathOp::Div if r == 0.0 => return None,
        MathOp::Div => l / r,
    }))
}
