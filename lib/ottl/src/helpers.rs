//! Helper utilities for OTTL integrators.
//!
//! This module provides functions that implement common behavior expected when
//! integrating OTTL (e.g. implementing [`crate::PathAccessor`]). Index resolution
//! is the integrator's responsibility; this helper is provided for convenience
//! in tests and reference implementations.

use crate::{IndexExpr, Value};

/// Applies a sequence of index expressions to a value and returns the result.
///
/// Supports indexing into [`Value::List`] and [`Value::Map`] with [`IndexExpr::Int`]
/// and [`IndexExpr::String`] respectively, and into [`Value::String`] with
/// [`IndexExpr::Int`] (character index). Any other combination returns an error.
///
/// # Errors
///
/// Returns an error if an index is out of bounds, a map key is missing, or
/// the value type does not support the given index type.
///
/// # Example
///
/// ```ignore
/// use ottl::helpers::apply_indexes;
/// use ottl::{Value, IndexExpr};
///
/// let list = Value::List(vec![Value::Int(1), Value::Int(2)]);
/// let indexes = [IndexExpr::Int(0)];
/// let v = apply_indexes(list, &indexes)?;
/// assert!(matches!(v, Value::Int(1)));
/// ```
pub fn apply_indexes(value: Value, indexes: &[IndexExpr]) -> crate::Result<Value> {
    let mut current = value;
    for index in indexes {
        current = match (&current, index) {
            (Value::List(list), IndexExpr::Int(i)) => list
                .get(*i)
                .cloned()
                .ok_or_else(|| -> crate::BoxError { format!("Index {} out of bounds", i).into() })?,
            (Value::Map(map), IndexExpr::String(key)) => map
                .get(key)
                .cloned()
                .ok_or_else(|| -> crate::BoxError { format!("Key '{}' not found", key).into() })?,
            (Value::String(s), IndexExpr::Int(i)) => s
                .chars()
                .nth(*i)
                .map(|c| Value::string(c.to_string()))
                .ok_or_else(|| -> crate::BoxError { format!("Index {} out of bounds", i).into() })?,
            _ => return Err(format!("Cannot index {:?} with {:?}", current, index).into()),
        };
    }
    Ok(current)
}
