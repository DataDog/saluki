#![allow(dead_code)]

//mod partitioner;

#[cfg(test)]
pub mod test_util;

mod builder;

use std::collections::HashMap;

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
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder);
}

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
    pub fn total_firm_limit_bytes(&self) -> usize {
        self.self_firm_limit_bytes
            + self
                .subcomponents
                .values()
                .map(|cb| cb.total_firm_limit_bytes())
                .sum::<usize>()
    }

    /// Returns an iterator of all leaf components within this component.
    ///
    /// A leaf component is a component which has no subcomponents.
    pub fn leaf_components(&self) -> impl IntoIterator<Item = (String, &ComponentBounds)> {
        let mut leaf_components = Vec::new();

        self.leaf_components_inner("root", &mut leaf_components);

        leaf_components
    }

    fn leaf_components_inner<'a>(&'a self, prefix: &str, components: &mut Vec<(String, &'a ComponentBounds)>) {
        for (name, bounds) in &self.subcomponents {
            let new_name = format!("{}.{}", prefix, name);
            if bounds.subcomponents.is_empty() {
                components.push((new_name, bounds));
            } else {
                bounds.leaf_components_inner(&new_name, components);
            }
        }
    }
}
