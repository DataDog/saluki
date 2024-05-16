use crate::MemoryBounds;

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
    fn calculate_bounds(&self, builder: &mut crate::MemoryBoundsBuilder) {
        builder.minimum().with_fixed_amount(self.minimum_required.unwrap_or(0));
        builder.firm().with_fixed_amount(self.firm_limit);
    }
}
