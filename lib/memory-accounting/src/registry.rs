use std::marker::PhantomData;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, MutexGuard},
};

use crate::{
    allocator::{AllocationGroupRegistry, AllocationGroupToken},
    BoundsVerifier, ComponentBounds, MemoryBounds, MemoryGrant, VerifiedBounds, VerifierError,
};

struct ComponentMetadata {
    full_name: Option<String>,
    bounds: ComponentBounds,
    token: Option<AllocationGroupToken>,
    subcomponents: HashMap<String, Arc<Mutex<ComponentMetadata>>>,
}

impl ComponentMetadata {
    fn from_full_name(full_name: Option<String>) -> Self {
        Self {
            full_name,
            bounds: ComponentBounds::default(),
            token: None,
            subcomponents: HashMap::new(),
        }
    }

    fn get_or_create<S>(&mut self, name: S) -> Arc<Mutex<Self>>
    where
        S: AsRef<str>,
    {
        // Split the name into the current level name and the remaining name.
        //
        // This lets us handle names which refer to a target nested component instead of having to chain a ton of calls
        // together.
        let name = name.as_ref();
        let (current_level_name, remaining_name) = match name.split_once('/') {
            Some((current_level_name, remaining_name)) => (current_level_name, Some(remaining_name)),
            None => (name, None),
        };

        // Now we need to see if we have an existing component here or if we need to create a new one.
        match self.subcomponents.get(current_level_name) {
            Some(existing) => match remaining_name {
                Some(remaining_name) => {
                    // We found an intermediate subcomponent, so keep recursing.
                    existing.lock().unwrap().get_or_create(remaining_name)
                }
                None => {
                    // We've found the leaf subcomponent.
                    Arc::clone(existing)
                }
            },
            None => {
                // We couldn't find the component at this level, so we need to create it.
                //
                // We do all of our name calculation and so on, but we also leave the token empty for now. We do this to
                // avoid registering intermediate components that aren't actually used by the code, but are simply a
                // consequence of wanting to having a nicely nested structure.
                //
                // We'll register a token for the component the first time it's requested.
                let full_name = match self.full_name.as_ref() {
                    Some(parent_full_name) => format!("{}.{}", parent_full_name, current_level_name),
                    None => current_level_name.to_string(),
                };

                let inner = self
                    .subcomponents
                    .entry(current_level_name.to_string())
                    .or_insert_with(|| Arc::new(Mutex::new(Self::from_full_name(Some(full_name)))));

                // If we still need to recurse further, do so here.. otherwise, return the subcomponent we just created
                // as-is.
                match remaining_name {
                    Some(remaining_name) => inner.lock().unwrap().get_or_create(remaining_name),
                    None => Arc::clone(inner),
                }
            }
        }
    }

    fn token(&mut self) -> AllocationGroupToken {
        match self.token {
            Some(token) => token,
            None => match self.full_name.as_deref() {
                Some(full_name) => {
                    let allocator_component_registry = AllocationGroupRegistry::global();
                    let token = allocator_component_registry.register_allocation_group(full_name);
                    self.token = Some(token);

                    token
                }
                None => AllocationGroupToken::root(),
            },
        }
    }

    fn as_bounds(&self) -> ComponentBounds {
        let mut bounds = ComponentBounds::default();
        bounds.self_firm_limit_bytes = self.bounds.self_firm_limit_bytes;
        bounds.self_minimum_required_bytes = self.bounds.self_minimum_required_bytes;

        for (name, subcomponent) in self.subcomponents.iter() {
            let subcomponent = subcomponent.lock().unwrap();
            let subcomponent_bounds = subcomponent.as_bounds();
            bounds.subcomponents.insert(name.clone(), subcomponent_bounds);
        }

        bounds
    }
}

/// A registry for components for tracking memory bounds and runtime memory usage.
pub struct ComponentRegistry {
    inner: Arc<Mutex<ComponentMetadata>>,
}

impl ComponentRegistry {
    /// Gets a component by name, or creates it if it doesn't exist.
    ///
    /// The name provided can be given in a direct (`component_name``) or nested (`path/to/component_name`) form. If the
    /// nested form is given, each component in the path will be created if it doesn't exist.
    ///
    /// Returns a `ComponentRegistry` scoped to the component.
    pub fn get_or_create<S>(&mut self, name: S) -> Self
    where
        S: AsRef<str>,
    {
        let mut inner = self.inner.lock().unwrap();
        Self {
            inner: inner.get_or_create(name),
        }
    }

    /// Gets a bounds builder attached to the root component.
    pub fn bounds_builder(&mut self) -> MemoryBoundsBuilder<'_> {
        MemoryBoundsBuilder {
            inner: Self {
                inner: Arc::clone(&self.inner),
            },
            _lt: PhantomData,
        }
    }

    /// Gets the tracking token for the component scoped to this registry.
    ///
    /// If the component is the root component (has no name), the root allocation token is returned.  Otherwise, the
    /// component is registered (using its full name) if it hasn't already been, and that token is returned.
    pub fn token(&mut self) -> AllocationGroupToken {
        let mut inner = self.inner.lock().unwrap();
        inner.token()
    }

    /// Validates that all components are able to respect the calculated effective limit.
    ///
    /// If validation succeeds, `VerifiedBounds`` is returned, which provides information about the effective limit that
    /// can be used for allocating memory.
    ///
    /// ## Errors
    ///
    /// A number of invalid conditions are checked and will cause an error to be returned:
    ///
    /// - when a component has invalid bounds (e.g. minimum required bytes higher than firm limit)
    /// - when the combined total of the firm limit for all components exceeds the effective limit
    pub fn verify_bounds(&mut self, initial_grant: MemoryGrant) -> Result<VerifiedBounds, VerifierError> {
        let bounds = self.inner.lock().unwrap().as_bounds();
        BoundsVerifier::new(initial_grant, bounds).verify()
    }
}

impl Default for ComponentRegistry {
    fn default() -> Self {
        Self {
            inner: Arc::new(Mutex::new(ComponentMetadata::from_full_name(None))),
        }
    }
}

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

/// Builder for defining the memory bounds of a component and its subcomponents.
///
/// This builder provides a simple interface for defining the minimum and firm bounds of a component, as well as
/// declaring subcomponents. For example, a topology can contain its own "self" memory bounds, and then define the
/// individual bounds for each component in the topology.
pub struct MemoryBoundsBuilder<'a> {
    inner: ComponentRegistry,
    _lt: PhantomData<&'a ()>,
}

impl MemoryBoundsBuilder<'static> {
    #[cfg(test)]
    pub(crate) fn for_test() -> Self {
        Self {
            inner: ComponentRegistry::default(),
            _lt: PhantomData,
        }
    }
}

impl<'a> MemoryBoundsBuilder<'a> {
    /// Gets a builder object for defining the minimum bounds of the current component.
    pub fn minimum(&mut self) -> BoundsBuilder<'_, Minimum> {
        let bounds = self.inner.inner.lock().unwrap();
        BoundsBuilder::<'_, Minimum>::new(bounds)
    }

    /// Gets a builder object for defining the firm bounds of the current component.
    ///
    /// The firm limit is additive with the minimum required memory, so entries that are added via `minimum` do not need
    /// to be added again here.
    pub fn firm(&mut self) -> BoundsBuilder<'_, Firm> {
        let bounds = self.inner.inner.lock().unwrap();
        BoundsBuilder::<'_, Firm>::new(bounds)
    }

    /// Creates a nested subcomponent and gets a builder object for it.
    ///
    /// This allows for defining the bounds of various subcomponents within a larger component, which are then rolled up
    /// into the calculated bounds for the parent component.
    pub fn subcomponent<S>(&mut self, name: S) -> MemoryBoundsBuilder<'_>
    where
        S: AsRef<str>,
    {
        let component = self.inner.get_or_create(name);
        MemoryBoundsBuilder {
            inner: component,
            _lt: PhantomData,
        }
    }

    /// Creates a nested subcomponent based on the given component.
    ///
    /// This allows for defining a subcomponent whose bounds come from an object that implements `MemoryBounds` directly.
    pub fn with_subcomponent<S, C>(&mut self, name: S, component: &C) -> &mut Self
    where
        S: AsRef<str>,
        C: MemoryBounds,
    {
        let mut builder = self.subcomponent(name);
        component.specify_bounds(&mut builder);

        self
    }

    /// Merges a set of existing `ComponentBounds` into the current builder.
    pub fn merge_existing(&mut self, existing: &ComponentBounds) -> &mut Self {
        let mut bounds_builder = self.inner.bounds_builder();
        bounds_builder
            .minimum()
            .with_fixed_amount(existing.self_minimum_required_bytes);
        bounds_builder.firm().with_fixed_amount(existing.self_firm_limit_bytes);

        for (name, existing_subcomponent) in &existing.subcomponents {
            let subcomponent = self.inner.get_or_create(name);
            let mut builder = MemoryBoundsBuilder {
                inner: subcomponent,
                _lt: PhantomData,
            };

            builder.merge_existing(existing_subcomponent);
        }

        self
    }

    #[cfg(test)]
    pub(crate) fn as_bounds(&self) -> ComponentBounds {
        self.inner.inner.lock().unwrap().as_bounds()
    }
}

/// Bounds builder.
///
/// Helper type for defining the bounds of a component in a field-driven manner.
pub struct BoundsBuilder<'a, S> {
    inner: MutexGuard<'a, ComponentMetadata>,
    _state: PhantomData<S>,
}

impl<'a, S: BoundsMutator> BoundsBuilder<'a, S> {
    fn new(inner: MutexGuard<'a, ComponentMetadata>) -> Self {
        Self {
            inner,
            _state: PhantomData,
        }
    }

    /// Accounts for a fixed amount of memory usage.
    ///
    /// This is a catch-all for directly accounting for a specific number of bytes.
    pub fn with_fixed_amount(&mut self, chunk_size: usize) -> &mut Self {
        S::add_usage(&mut self.inner.bounds, chunk_size);
        self
    }

    /// Accounts for an item container of the given length.
    ///
    /// This can be used to track the expected memory usage of generalized containers like `Vec<T>`, where items are
    /// homogenous and allocated contiguously.
    pub fn with_array<T>(&mut self, len: usize) -> &mut Self {
        S::add_usage(&mut self.inner.bounds, len * std::mem::size_of::<T>());
        self
    }

    /// Accounts for a map container of the given length.
    ///
    /// This can be used to track the expected memory usage of generalized maps like `HashMap<K, V>`, where keys and
    /// values are
    pub fn with_map<K, V>(&mut self, len: usize) -> &mut Self {
        S::add_usage(
            &mut self.inner.bounds,
            len * (std::mem::size_of::<K>() + std::mem::size_of::<V>()),
        );
        self
    }
}
