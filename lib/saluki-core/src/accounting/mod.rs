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

    /// Evaluates this expression to a byte count.
    ///
    /// Every leaf is ultimately operator-controlled, so an expression can describe a size larger
    /// than `usize` can hold. Arithmetic saturates at [`usize::MAX`] rather than overflowing: a bound
    /// too large to represent is reported as the largest one that can be, which no memory grant can
    /// satisfy, so bounds verification rejects it. Wrapping would instead report a small bound and
    /// let verification pass.
    fn evaluate(&self) -> usize {
        match self {
            Self::Config { value, .. } | Self::StructSize { value, .. } | Self::Constant { value, .. } => *value,
            Self::Product { values } => values.iter().map(UsageExpr::evaluate).fold(1, usize::saturating_mul),
            Self::Sum { values } => values.iter().map(UsageExpr::evaluate).fold(0, usize::saturating_add),
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
            .chain(self.subcomponents.values().map(|cb| cb.total_minimum_required_bytes()))
            .fold(0, usize::saturating_add)
    }

    /// Gets the total firm limit bytes for this component and all subcomponents.
    ///
    /// The firm limit includes the minimum required bytes.
    pub fn total_firm_limit_bytes(&self) -> usize {
        self.self_minimum_required_bytes
            .iter()
            .chain(self.self_firm_limit_bytes.iter())
            .map(UsageExpr::evaluate)
            .chain(self.subcomponents.values().map(|cb| cb.total_firm_limit_bytes()))
            .fold(0, usize::saturating_add)
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

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::{ComponentBounds, UsageExpr};

    #[test]
    fn leaf_expressions_evaluate_to_their_value() {
        assert_eq!(UsageExpr::config("cfg", 7).evaluate(), 7);
        assert_eq!(UsageExpr::constant("const", 11).evaluate(), 11);
        assert_eq!(
            UsageExpr::struct_size::<u64>("u64").evaluate(),
            std::mem::size_of::<u64>()
        );
    }

    #[test]
    fn product_evaluates_to_the_product_of_its_subexpressions() {
        let expr = UsageExpr::product(
            "area",
            UsageExpr::constant("width", 4),
            UsageExpr::constant("height", 8),
        );
        assert_eq!(expr.evaluate(), 32);
    }

    #[test]
    fn sum_evaluates_to_the_sum_of_its_subexpressions() {
        let expr = UsageExpr::sum("total", UsageExpr::constant("a", 4), UsageExpr::constant("b", 8));
        assert_eq!(expr.evaluate(), 12);
    }

    #[test]
    fn products_and_sums_compose_recursively() {
        // (2 + 3) * 4 = 20
        let expr = UsageExpr::product(
            "scaled",
            UsageExpr::sum("base", UsageExpr::constant("a", 2), UsageExpr::constant("b", 3)),
            UsageExpr::constant("factor", 4),
        );
        assert_eq!(expr.evaluate(), 20);
    }

    #[test]
    fn empty_products_and_sums_keep_their_identities() {
        // Folding by hand has to reproduce what `Iterator::product` and `Iterator::sum` return for an
        // empty sequence, or an expression with no subexpressions changes meaning.
        assert_eq!(UsageExpr::Product { values: Vec::new() }.evaluate(), 1);
        assert_eq!(UsageExpr::Sum { values: Vec::new() }.evaluate(), 0);
    }

    #[test]
    fn a_sum_too_large_to_represent_saturates() {
        // An operator-supplied byte budget can be large enough that the total does not fit in a
        // `usize`. Wrapping would report a tiny bound that verification happily accepts.
        let expr = UsageExpr::sum(
            "total",
            UsageExpr::config("queue budget", usize::MAX),
            UsageExpr::constant("overhead", 4096),
        );

        assert_eq!(expr.evaluate(), usize::MAX);
    }

    #[test]
    fn a_product_too_large_to_represent_saturates() {
        let expr = UsageExpr::product(
            "scaled",
            UsageExpr::config("count", usize::MAX),
            UsageExpr::constant("size", 2),
        );

        assert_eq!(expr.evaluate(), usize::MAX);
    }

    #[test]
    fn saturation_survives_aggregation_across_subcomponents() {
        // Saturating inside `evaluate` is not enough on its own: the per-component totals add those
        // results together, so an already-saturated expression must not overflow one level up.
        let saturated = ComponentBounds {
            self_minimum_required_bytes: vec![UsageExpr::config("queue budget", usize::MAX)],
            self_firm_limit_bytes: vec![UsageExpr::constant("overhead", 4096)],
            subcomponents: HashMap::new(),
        };
        let mut subcomponents = HashMap::new();
        subcomponents.insert("forwarder".to_string(), saturated);

        let bounds = ComponentBounds {
            self_minimum_required_bytes: vec![UsageExpr::constant("root min", 1024)],
            self_firm_limit_bytes: vec![UsageExpr::constant("root firm", 2048)],
            subcomponents,
        };

        assert_eq!(bounds.total_minimum_required_bytes(), usize::MAX);
        assert_eq!(bounds.total_firm_limit_bytes(), usize::MAX);
    }
}
