use crate::{ComponentBounds, MemoryBounds, MemoryBoundsBuilder};

#[derive(Debug)]
pub struct BoundedComponent {
    minimum_required: Option<usize>,
    firm_limit: usize,
}

impl BoundedComponent {
    pub fn new(minimum_required: Option<usize>, firm_limit: usize) -> Self {
        Self {
            minimum_required,
            firm_limit,
        }
    }
}

impl MemoryBounds for BoundedComponent {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        builder.minimum().with_fixed_amount(self.minimum_required.unwrap_or(0));
        builder.firm().with_fixed_amount(self.firm_limit);
    }
}

pub fn get_component_bounds<C>(component: &C) -> ComponentBounds
where
    C: MemoryBounds,
{
    let mut builder = MemoryBoundsBuilder::new();
    {
        builder.bounded_component("component", component);
    }
    builder.finalize()
}
