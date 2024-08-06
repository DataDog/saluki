//! Building blocks for declaring and enforcing memory bounds for components.
//!
//! ## Overview
//!
//! This crate provides a three-pronged approach to memory accounting:
//!
//! - memory bounds (components declare their _expected_ memory usage)
//! - allocation tracking (tracking _actual_ memory usage)
//! - memory limiting (enforcing _maximum_ memory usage)
//!
//! Through this approach, data planes can be vastly more resilient to memory exhaustion or
//! exceeding externally-applied memory limits.
//!
//! ## Memory bounds
//!
//! One major problem with resource planning is predicting memory usage. For many applications,
//! there are a number of factors that can influence memory usage, such as:
//!
//! - the workload itself (amount of data coming in)
//! - application configuration (buffer sizes)
//! - application changes (new features, bug fixes)
//!
//! This requires additional effort by operators, potentially on an ongoing basis, to empirically
//! determine the right amount of memory to dedicate. What if instead, an application could
//! determine a reasonable upper bound on its memory usage based on its configuration and report
//! that to the operator? This is the goal of memory bounds.
//!
//! Memory bounds are a way for components to declare their expected memory usage, categorized into
//! both a minimum required amount and a firm limit. The minimum required amount is the amount of
//! memory that is required for the component to function correctly, which generally encompasses
//! things like pre-allocated buffers. The firm limit is meant to indicate the maximum amount of
//! memory that the component should use, regardless of the workload.
//!
//! Providing firm limits does require some additional thought and care, as a component needs to be
//! able to actually limit itself in order to adhere to those limits. While determining the the
//! bounds themselves is out of scope for this crate, our other two prongs are meant to pick up the
//! slack where memory bounds fall off.
//!
//! ## Allocation tracking
//!
//! As memory bounds are inherently lossy, and not everything can be fully bounded, we need a way to
//! track the actual memory used against the expected memory usage. This is where allocation
//! tracking comes into play and offers a very precise view into per-component memory usage.
//!
//! A custom allocator is provided that tracks all memory allocations, and more specifically,
//! attributes them to a set of registered components. Components register with the allocator and
//! receive a "token" that can be used to scope allocations to that component.
//!
//! By tracking allocations in this way, we end up with the actual usage of each component, which
//! can then be compared against the memory bounds to determine if a component is exceeding its
//! bounds or not. In cases where a component is exceeding its bounds, or the application as a whole
//! is exceeding its configured limit, we need a way to attempt to enforce those limits.
//!
//! ## Memory limiting
//!
//! Memory limiting is the final prong in our approach to memory accounting.
//!
//! When the application is approaching its configured memory limit, or is exceeding the limit, a
//! mechanism is needed to slow down the rate of memory growth. The global memory limiter is a
//! mechanism for cooperatively applying backpressure in order to limit the rate of work, and
//! thereby limit the rate of allocations. Components participate by utilizing the global memory
//! limiter, which conditionally applies small delays in order to artificially generate backpressure.
#![deny(warnings)]
#![deny(missing_docs)]

use std::collections::HashMap;

//mod partitioner;

#[cfg(test)]
pub mod test_util;

pub mod allocator;
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

impl<'a, T> MemoryBounds for &'a T
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

/// Memory bounds for a component.
#[derive(Clone, Debug, Default)]
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
