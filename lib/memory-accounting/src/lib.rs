//! Helpers for defining memory bounds and accounting for process-wide memory usage.
#![deny(warnings)]
#![deny(missing_docs)]

use std::collections::HashMap;

//mod partitioner;

#[cfg(test)]
pub mod test_util;

mod builder;
pub use self::builder::MemoryBoundsBuilder;

mod grant;
pub use self::grant::MemoryGrant;

mod limiter;
pub use self::limiter::MemoryLimiter;

mod verifier;
pub use self::verifier::{BoundsVerifier, VerifiedBounds, VerifierError};

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

/// Memory bounds for a component.
#[derive(Default)]
pub struct ComponentBounds {
    self_minimum_required_bytes: usize,
    self_firm_limit_bytes: usize,
    subcomponents: HashMap<String, ComponentBounds>,
}

impl ComponentBounds {
    /// Gets the total minimum required bytes for this component and all subcomponents.
    pub fn total_minimum_required_bytes(&self) -> usize {
        self.self_minimum_required_bytes
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
            + self.self_firm_limit_bytes
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
}
