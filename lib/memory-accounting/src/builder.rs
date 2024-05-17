use std::marker::PhantomData;

use crate::ComponentBounds;

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
    fn add_usage(bounds: &mut ComponentBounds, amount: usize);
}

impl BoundsMutator for Minimum {
    fn add_usage(bounds: &mut ComponentBounds, amount: usize) {
        bounds.self_minimum_required_bytes = bounds.self_minimum_required_bytes.saturating_add(amount);
    }
}

impl BoundsMutator for Firm {
    fn add_usage(bounds: &mut ComponentBounds, amount: usize) {
        bounds.self_firm_limit_bytes = bounds.self_firm_limit_bytes.saturating_add(amount);
    }
}

enum MutablePointer<'a, T> {
    Borrowed(&'a mut T),
    Owned(T),
}

impl<'a, T> MutablePointer<'a, T> {
    fn as_mut(&mut self) -> &mut T {
        match self {
            MutablePointer::Borrowed(inner) => inner,
            MutablePointer::Owned(inner) => inner,
        }
    }
}

pub struct MemoryBoundsBuilder<'bounds> {
    inner: MutablePointer<'bounds, ComponentBounds>,
}

impl MemoryBoundsBuilder<'static> {
    pub fn new() -> Self {
        Self {
            inner: MutablePointer::Owned(ComponentBounds::default()),
        }
    }

    /// Returns the calculated component bounds.
    pub fn finalize(self) -> ComponentBounds {
        match self.inner {
            // TODO: This should be unreachable if we're 'static, since we only have an owned version for 'static... but
            // I'm trying to think through if the compiler is going to infer 'static for the calls to `component`. :think:
            MutablePointer::Borrowed(_) => unreachable!(),
            MutablePointer::Owned(inner) => inner,
        }
    }
}

impl<'a> MemoryBoundsBuilder<'a> {
    /// Gets a builder object that can be used to define the miniumum required memory for this component to operate.
    pub fn minimum(&mut self) -> BoundsBuilder<'_, Minimum> {
        BoundsBuilder::<'_, Minimum>::new(self.inner.as_mut())
    }

    /// Gets a builder object that can be used to define the firm memory limit for this component.
    pub fn firm(&mut self) -> BoundsBuilder<'_, Firm> {
        BoundsBuilder::<'_, Firm>::new(self.inner.as_mut())
    }

    /// Creates a nested subcomponent and gets a builder object for it.
    ///
    /// This allows for defining the bounds of various subcomponents within a larger component, which are then rolled up
    /// into the calculated bounds for the parent component.
    pub fn component<S>(&mut self, name: S) -> MemoryBoundsBuilder<'_>
    where
        S: Into<String>,
    {
        let subcomponents = &mut self.inner.as_mut().subcomponents;
        let inner = subcomponents.entry(name.into()).or_default();

        MemoryBoundsBuilder {
            inner: MutablePointer::Borrowed(inner),
        }
    }
}

impl Default for MemoryBoundsBuilder<'static> {
    fn default() -> Self {
        Self::new()
    }
}

pub struct BoundsBuilder<'a, S> {
    bounds: &'a mut ComponentBounds,
    _state: PhantomData<S>,
}

impl<'a, S: BoundsMutator> BoundsBuilder<'a, S> {
    fn new(bounds: &'a mut ComponentBounds) -> Self {
        Self {
            bounds,
            _state: PhantomData,
        }
    }

    /// Accounts for a fixed amount of memory usage.
    ///
    /// This is a catch-all for directly accounting for a specific number of bytes.
    pub fn with_fixed_amount(&mut self, chunk_size: usize) -> &mut Self {
        S::add_usage(self.bounds, chunk_size);
        self
    }

    /// Accounts for an item container of the given length.
    ///
    /// This can be used to track the expected memory usage of generalized containers like `Vec<T>`, where items are
    /// homogenous and allocated contiguously.
    pub fn with_array<T>(&mut self, len: usize) -> &mut Self {
        S::add_usage(self.bounds, len * std::mem::size_of::<T>());
        self
    }

    /// Accounts for a map container of the given length.
    ///
    /// This can be used to track the expected memory usage of generalized maps like `HashMap<K, V>`, where keys and
    /// values are
    pub fn with_map<K, V>(&mut self, len: usize) -> &mut Self {
        S::add_usage(self.bounds, len * (std::mem::size_of::<K>() + std::mem::size_of::<V>()));
        self
    }
}
