use std::marker::PhantomData;

use crate::CalculatedBounds;

pub struct Minimum;
pub struct Firm;

pub(crate) mod private {
    pub trait Sealed {}

    impl Sealed for super::Minimum {}
    impl Sealed for super::Firm {}
}

// Simple trait-based builder state approach so we can use a single builder view to modify either the minimum required
// or firm limit amounts.
pub trait BoundsMutator: private::Sealed {
    fn add_usage(builder: &mut MemoryBoundsBuilder, amount: usize);
}

impl BoundsMutator for Minimum {
    fn add_usage(builder: &mut MemoryBoundsBuilder, amount: usize) {
        builder.minimum_required = builder.minimum_required.saturating_add(amount);
    }
}

impl BoundsMutator for Firm {
    fn add_usage(builder: &mut MemoryBoundsBuilder, amount: usize) {
        builder.firm_limit = builder.firm_limit.saturating_add(amount);
    }
}

#[derive(Default)]
pub struct MemoryBoundsBuilder {
    minimum_required: usize,
    firm_limit: usize,
}

impl MemoryBoundsBuilder {
    /// Gets a builder object that can be used to define the miniumum required memory for this component to operate.
    pub fn minimum(&mut self) -> BoundsBuilder<'_, Minimum> {
        BoundsBuilder::<'_, Minimum>::new(self)
    }

    /// Gets a builder object that can be used to define the firm memory limit for this component.
    pub fn firm(&mut self) -> BoundsBuilder<'_, Firm> {
        BoundsBuilder::<'_, Firm>::new(self)
    }

    /// Returns the calculated bounds.
    pub fn calculated_bounds(&self) -> CalculatedBounds {
        CalculatedBounds {
            minimum_required: self.minimum_required,
            firm_limit: self.firm_limit,
        }
    }
}

pub struct BoundsBuilder<'a, S> {
    builder: &'a mut MemoryBoundsBuilder,
    _state: PhantomData<S>,
}

impl<'a, S: BoundsMutator> BoundsBuilder<'a, S> {
    fn new(builder: &'a mut MemoryBoundsBuilder) -> Self {
        Self {
            builder,
            _state: PhantomData,
        }
    }

    /// Accounts for a fixed amount of memory usage.
    ///
    /// This is a catch-all for directly accounting for a specific number of bytes.
    pub fn with_fixed_amount(&mut self, chunk_size: usize) -> &mut Self {
        S::add_usage(self.builder, chunk_size);
        self
    }

    /// Accounts for an item container of the given length.
    ///
    /// This can be used to track the expected memory usage of generalized containers like `Vec<T>`, where items are
    /// homogenous and allocated contiguously.
    pub fn with_array<T>(&mut self, len: usize) -> &mut Self {
        S::add_usage(self.builder, len * std::mem::size_of::<T>());
        self
    }

    /// Accounts for a map container of the given length.
    ///
    /// This can be used to track the expected memory usage of generalized maps like `HashMap<K, V>`, where keys and
    /// values are
    pub fn with_map<K, V>(&mut self, len: usize) -> &mut Self {
        S::add_usage(
            self.builder,
            len * (std::mem::size_of::<K>() + std::mem::size_of::<V>()),
        );
        self
    }
}
