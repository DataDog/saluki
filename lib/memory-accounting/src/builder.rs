use std::marker::PhantomData;

use crate::{ComponentBounds, MemoryBounds};

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

/// Builder for defining the memory bounds of a component and its subcomponents.
///
/// This builder provides a simple interface for defining the minimum and firm bounds of a component, as well as
/// declaring subcomponents. For example, a topology can contain its own "self" memory bounds, and then define the
/// individual bounds for each component in the topology.
pub struct MemoryBoundsBuilder<'bounds> {
    inner: MutablePointer<'bounds, ComponentBounds>,
}

impl MemoryBoundsBuilder<'static> {
    /// Create an empty `MemoryBoundsBuilder`.
    pub fn new() -> Self {
        Self {
            inner: MutablePointer::Owned(ComponentBounds::default()),
        }
    }

    /// Returns the calculated component bounds.
    pub fn finalize(self) -> ComponentBounds {
        match self.inner {
            MutablePointer::Borrowed(_) => unreachable!(),
            MutablePointer::Owned(inner) => inner,
        }
    }
}

impl<'a> MemoryBoundsBuilder<'a> {
    /// Gets a builder object for defining the minimum bounds of the current component.
    pub fn minimum(&mut self) -> BoundsBuilder<'_, Minimum> {
        BoundsBuilder::<'_, Minimum>::new(self.inner.as_mut())
    }

    /// Gets a builder object for defining the firm bounds of the current component.
    ///
    /// The firm limit is additive with the minimum required memory, so entries that are added via `minimum` do not need
    /// to be added again here.
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

    /// Creates a nested subcomponent based on the given component.
    ///
    /// This allows for defining a subcomponent whose bounds come from an object that implements `MemoryBounds` directly.
    pub fn bounded_component<S, C>(&mut self, name: S, component: &C) -> &mut Self
    where
        S: Into<String>,
        C: MemoryBounds,
    {
        let mut builder = self.component(name);
        component.specify_bounds(&mut builder);

        self
    }

    /// Merges a set of existing `ComponentBounds` into the current builder.
    pub fn merge_existing(&mut self, existing: &ComponentBounds) -> &mut Self {
        let inner = self.inner.as_mut();
        inner.self_minimum_required_bytes += existing.self_minimum_required_bytes;
        inner.self_firm_limit_bytes += existing.self_firm_limit_bytes;

        for (name, existing_subcomponent) in &existing.subcomponents {
            let subcomponent = inner.subcomponents.entry(name.clone()).or_default();
            let mut builder = MemoryBoundsBuilder {
                inner: MutablePointer::Borrowed(subcomponent),
            };

            builder.merge_existing(existing_subcomponent);
        }

        self
    }
}

impl Default for MemoryBoundsBuilder<'static> {
    fn default() -> Self {
        Self::new()
    }
}

/// A bounds builder.
///
/// `BoundsBuilder` provides helper methods for more directly describing the memory usage of a component by matching
/// common types and usages. For example, methods are provided for accounting for the memory used by an array (e.g.
/// `Vec<T>`) or a map (e.g. `HashMap<K, V>`). While the caller is able to do this math themselves, the builder provides
/// these methods to make it as easy as reasonably possible and to allow for implementations of `MemoryBounds` to be as
/// self-describing, in terms of the code, as possible.
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
    /// values are typically allocated contiguously.
    pub fn with_map<K, V>(&mut self, len: usize) -> &mut Self {
        S::add_usage(self.bounds, len * (std::mem::size_of::<K>() + std::mem::size_of::<V>()));
        self
    }
}
