//! Test-specific helpers.

use crate::{ComponentBounds, MemoryBounds, MemoryBoundsBuilder};

/// A generic component mock that can have its bounds configured.
#[derive(Debug)]
pub struct BoundedComponent {
    minimum_required: Option<usize>,
    firm_limit: usize,
}

impl BoundedComponent {
    /// Creates a new `BoundedComponent` with the given bounds.
    pub fn new(minimum_required: Option<usize>, firm_limit: usize) -> Self {
        Self {
            minimum_required,
            firm_limit,
        }
    }
}

impl MemoryBounds for BoundedComponent {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        builder
            .minimum()
            .with_fixed_amount("min amount", self.minimum_required.unwrap_or(0));
        builder.firm().with_fixed_amount("firm limit", self.firm_limit);
    }
}

/// Captures the bounds for a given component.
pub fn get_component_bounds<C>(component: &C) -> ComponentBounds
where
    C: MemoryBounds,
{
    let mut builder = MemoryBoundsBuilder::for_test();
    {
        let mut component_builder = builder.subcomponent("component");
        component.specify_bounds(&mut component_builder);
    }
    builder.as_bounds()
}
