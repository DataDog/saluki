use crate::MemoryBounds;

#[derive(Debug)]
pub struct BoundedComponent {
    minimum_required: Option<usize>,
    soft_limit: usize,
}

impl BoundedComponent {
    pub fn new(minimum_required: Option<usize>, soft_limit: usize) -> Self {
        Self {
            minimum_required,
            soft_limit,
        }
    }
}

impl MemoryBounds for BoundedComponent {
    fn minimum_required(&self) -> Option<usize> {
        self.minimum_required
    }

    fn soft_limit(&self) -> usize {
        self.soft_limit
    }
}
