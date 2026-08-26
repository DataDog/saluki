//! Context provided to component builders.

use std::fmt;

use super::{ComponentContext, ComponentType};
use crate::{
    runtime::state::{AcquireError, ResourceLease, ResourceRegistry, ResourceSpecification},
    support::SubsystemIdentifier,
    topology::ComponentId,
};

/// Build-time component context.
///
/// Carries information about the component being built, as well as access to the resource registry in order to acquire
/// external resources if necessary.
#[derive(Clone)]
pub struct BuildContext {
    component_context: ComponentContext,
    resource_registry: ResourceRegistry,
}

impl BuildContext {
    /// Creates a new `BuildContext` for the given component.
    pub fn new(component_context: ComponentContext, resource_registry: ResourceRegistry) -> Self {
        Self {
            component_context,
            resource_registry,
        }
    }

    /// Returns the context of the component being built.
    pub fn component_context(&self) -> &ComponentContext {
        &self.component_context
    }

    /// Returns the component identifier.
    ///
    /// This is the relative identifier of the component, unique within its topology but not guaranteed to be globally
    /// unique. See [`BuildContext::identity`] for the fully qualified identity.
    pub fn component_id(&self) -> &ComponentId {
        self.component_context.component_id()
    }

    /// Returns the component type.
    pub fn component_type(&self) -> ComponentType {
        self.component_context.component_type()
    }

    /// Returns the fully qualified identity of this component.
    ///
    /// The returned identifier uniquely identifies the component within the process, inclusive of the topology to
    /// which it belongs.
    pub fn identity(&self) -> SubsystemIdentifier {
        self.component_context.identity()
    }

    /// Acquires a resource on behalf of the component being built.
    ///
    /// # Errors
    ///
    /// If the resource is already held elsewhere in the process, or can't be created, an error is returned.
    pub async fn acquire_resource<S: ResourceSpecification>(
        &self, spec: S,
    ) -> Result<ResourceLease<S::Resource>, AcquireError> {
        self.resource_registry.acquire(&self.identity(), spec).await
    }
}

impl fmt::Display for BuildContext {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.component_context)
    }
}

macro_rules! test_build_contexts {
    ($($name:ident => $component_context:ident, $doc:literal;)*) => {
        $(
            #[doc = $doc]
            ///
            /// The returned context carries its own empty [`ResourceRegistry`], so resources acquired through it are
            /// isolated to this context.
            #[cfg(any(test, feature = "test-util"))]
            pub fn $name<S: AsRef<str>>(component_id: S) -> Self {
                Self::new(ComponentContext::$component_context(component_id), ResourceRegistry::new())
            }
        )*
    };
}

impl BuildContext {
    test_build_contexts! {
        test_source => test_source, "Creates a new `BuildContext` for a source component with the given identifier, in a test topology.";
        test_relay => test_relay, "Creates a new `BuildContext` for a relay component with the given identifier, in a test topology.";
        test_decoder => test_decoder, "Creates a new `BuildContext` for a decoder component with the given identifier, in a test topology.";
        test_transform => test_transform, "Creates a new `BuildContext` for a transform component with the given identifier, in a test topology.";
        test_encoder => test_encoder, "Creates a new `BuildContext` for an encoder component with the given identifier, in a test topology.";
        test_forwarder => test_forwarder, "Creates a new `BuildContext` for a forwarder component with the given identifier, in a test topology.";
        test_destination => test_destination, "Creates a new `BuildContext` for a destination component with the given identifier, in a test topology.";
    }
}
