//! Process-level memory bounds and limiting for components.
//!
//! This module lets components declare their expected memory usage and supports enforcing
//! process-wide memory limits:
//!
//! - **memory bounds**: components declare their _expected_ memory usage (a minimum required
//!   amount and a firm limit) via the [`MemoryBounds`] trait and [`MemoryBoundsBuilder`],
//! - **bounds verification**: [`BoundsVerifier`] checks that the combined bounds of all components
//!   fit within a [`MemoryGrant`],
//! - **memory limiting**: [`MemoryLimiter`] applies cooperative backpressure as the process
//!   approaches a configured memory limit,
//! - **component registry**: [`ComponentRegistry`] ties these together, providing a nestable
//!   structure of components that can each declare bounds and register a resource group for
//!   runtime usage tracking.
//!
//! Actual (as opposed to expected) memory usage and CPU time are tracked separately via the
//! resource-tracking primitives in [`saluki_common::resource_tracking`]; the tracking allocator
//! found there must be installed as the global allocator for runtime usage to be attributed to
//! registered components.

use std::collections::HashMap;

use serde::Serialize;

mod api;
pub use self::api::ResourceAPIHandler;

mod grant;
pub use self::grant::MemoryGrant;

mod limiter;
pub use self::limiter::MemoryLimiter;

mod registry;
pub use self::registry::{ComponentRegistry, ComponentRegistryHandle, MemoryBoundsBuilder};

mod verifier;
pub use self::verifier::{BoundsVerifier, VerifiedBounds, VerifierError};

#[cfg(test)]
pub(crate) mod test_util;

/// Memory bounds for a component.
///
/// Components will naturally allocate memory in many phases, from initialization to normal operation. In some cases,
/// these allocations can be unbounded, leading to potential memory exhaustion.
///
/// When a component has a way to bound its memory usage, it can implement this trait to provide that accounting. A
/// bounds builder exposes a simple interface for tallying up the memory usage of individual pieces of a component, such
/// as buffers and buffer pools, containers, and more.
pub trait MemoryBounds {
    /// Specifies the minimum and firm memory bounds for this component and its subcomponents.
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder);
}

impl<T> MemoryBounds for &T
where
    T: MemoryBounds,
{
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        T::specify_bounds(self, builder);
    }
}

impl<T> MemoryBounds for Box<T>
where
    T: MemoryBounds + ?Sized,
{
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        T::specify_bounds(self, builder);
    }
}

/// Represents a memory usage expression for a component.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type")]
pub enum UsageExpr {
    /// A config value
    Config {
        /// The name
        name: String,
        /// The value
        value: usize,
    },

    /// A struct size
    StructSize {
        /// The value
        name: String,
        /// The value
        value: usize,
    },

    /// A constant value
    Constant {
        /// The name
        name: String,
        /// The value
        value: usize,
    },

    /// A product of subexpressions
    Product {
        /// Values to multiply
        values: Vec<UsageExpr>,
    },

    /// A sum of subexpressions
    Sum {
        /// Values to add
        values: Vec<UsageExpr>,
    },
}

impl UsageExpr {
    /// Creates a new usage expression that's a config value.
    pub fn config(s: impl Into<String>, value: usize) -> Self {
        Self::Config { name: s.into(), value }
    }

    /// Creates a new usage expression that's a constant value.
    pub fn constant(s: impl Into<String>, value: usize) -> Self {
        Self::Constant { name: s.into(), value }
    }

    /// Creates a new usage expression that's a struct size.
    pub fn struct_size<T>(s: impl Into<String>) -> Self {
        Self::StructSize {
            name: s.into(),
            value: std::mem::size_of::<T>(),
        }
    }

    /// Creates a new usage expression that's the product of two subexpressions.
    pub fn product(_s: impl Into<String>, lhs: UsageExpr, rhs: UsageExpr) -> Self {
        Self::Product { values: vec![lhs, rhs] }
    }

    /// Creates a new usage expression that's the sum of two subexpressions.
    pub fn sum(_s: impl Into<String>, lhs: UsageExpr, rhs: UsageExpr) -> Self {
        Self::Sum { values: vec![lhs, rhs] }
    }

    fn evaluate(&self) -> usize {
        match self {
            Self::Config { value, .. } | Self::StructSize { value, .. } | Self::Constant { value, .. } => *value,
            Self::Product { values } => values.iter().map(UsageExpr::evaluate).product(),
            Self::Sum { values } => values.iter().map(UsageExpr::evaluate).sum(),
        }
    }
}

/// Memory bounds for a component.
#[derive(Clone, Debug, Default)]
pub struct ComponentBounds {
    self_minimum_required_bytes: Vec<UsageExpr>,
    self_firm_limit_bytes: Vec<UsageExpr>,
    subcomponents: HashMap<String, ComponentBounds>,
}

impl ComponentBounds {
    /// Gets the total minimum required bytes for this component and all subcomponents.
    pub fn total_minimum_required_bytes(&self) -> usize {
        self.self_minimum_required_bytes
            .iter()
            .map(UsageExpr::evaluate)
            .sum::<usize>()
            + self
                .subcomponents
                .values()
                .map(|cb| cb.total_minimum_required_bytes())
                .sum::<usize>()
    }

    /// Gets the total firm limit bytes for this component and all subcomponents.
    ///
    /// The firm limit includes the minimum required bytes.
    pub fn total_firm_limit_bytes(&self) -> usize {
        self.self_minimum_required_bytes
            .iter()
            .map(UsageExpr::evaluate)
            .sum::<usize>()
            + self
                .self_firm_limit_bytes
                .iter()
                .map(UsageExpr::evaluate)
                .sum::<usize>()
            + self
                .subcomponents
                .values()
                .map(|cb| cb.total_firm_limit_bytes())
                .sum::<usize>()
    }

    /// Returns an iterator of all subcomponents within this component.
    ///
    /// Only iterates over direct subcomponents, not the subcomponents of those subcomponents, and so on.
    pub fn subcomponents(&self) -> impl IntoIterator<Item = (&String, &ComponentBounds)> {
        self.subcomponents.iter()
    }

    /// Returns a tree of all bound expressions for this component and its subcomponents as JSON.
    pub fn to_exprs(&self) -> Vec<serde_json::Value> {
        let path = vec!["root".to_string()];
        let mut stack = vec![(path, self)];
        let mut output = Vec::new();

        while let Some((path, cb)) = stack.pop() {
            for expr in &cb.self_minimum_required_bytes {
                output.push(serde_json::json!({
                    "name": format!("{}.min", path.join(".")),
                    "expr": expr,
                }));
            }
            for expr in &cb.self_firm_limit_bytes {
                output.push(serde_json::json!({
                    "name": format!("{}.firm", path.join(".")),
                    "expr": expr,
                }));
            }

            for (name, subcomponent) in cb.subcomponents() {
                let mut path = path.clone();
                path.push(name.clone());
                stack.push((path, subcomponent));
            }
        }

        output
    }
}
