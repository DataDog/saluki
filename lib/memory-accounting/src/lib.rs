#![allow(dead_code)]

mod partitioner;

#[cfg(test)]
pub mod test_util;

mod builder;

pub use self::builder::MemoryBoundsBuilder;

mod grant;
pub use self::grant::MemoryGrant;

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
    fn calculate_bounds(&self, builder: &mut MemoryBoundsBuilder);
}

pub struct CalculatedBounds {
    pub minimum_required: usize,
    pub firm_limit: usize,
}
