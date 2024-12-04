//! Component basics.

pub mod destinations;

use std::fmt;

pub mod sources;
pub mod transforms;

use crate::topology::ComponentId;

/// A component context.
///
/// Component contexts uniquely identify a component within a topology by coupling the component identifier (name) and
/// component type (source, transform, or destination).
///
/// Practically speaking, all components are required to have a unique identifier. However, identifiers may be opaque
/// enough that without knowing the _type_ of component, the identifier doesn't provide enough information.
#[derive(Clone)]
pub struct ComponentContext {
    component_id: ComponentId,
    component_type: &'static str,
}

impl ComponentContext {
    /// Creates a new `ComponentContext` for a source component with the given identifier.
    pub fn source(component_id: ComponentId) -> Self {
        Self {
            component_id,
            component_type: "source",
        }
    }

    /// Creates a new `ComponentContext` for a transform component with the given identifier.
    pub fn transform(component_id: ComponentId) -> Self {
        Self {
            component_id,
            component_type: "transform",
        }
    }

    /// Creates a new `ComponentContext` for a destination component with the given identifier.
    pub fn destination(component_id: ComponentId) -> Self {
        Self {
            component_id,
            component_type: "destination",
        }
    }

    /// Creates a new `ComponentContext` for a source component with the given identifier.
    #[cfg(test)]
    pub fn test_source<S: AsRef<str>>(component_id: S) -> Self {
        Self {
            component_id: ComponentId::try_from(component_id.as_ref()).expect("invalid component ID"),
            component_type: "source",
        }
    }

    /// Creates a new `ComponentContext` for a transform component with the given identifier.
    #[cfg(test)]
    pub fn test_transform<S: AsRef<str>>(component_id: S) -> Self {
        Self {
            component_id: ComponentId::try_from(component_id.as_ref()).expect("invalid component ID"),
            component_type: "transform",
        }
    }

    /// Creates a new `ComponentContext` for a destination component with the given identifier.
    #[cfg(test)]
    pub fn test_destination<S: AsRef<str>>(component_id: S) -> Self {
        Self {
            component_id: ComponentId::try_from(component_id.as_ref()).expect("invalid component ID"),
            component_type: "destination",
        }
    }

    /// Returns the component identifier.
    pub fn component_id(&self) -> &ComponentId {
        &self.component_id
    }

    /// Returns the component type.
    pub fn component_type(&self) -> &'static str {
        self.component_type
    }
}

impl fmt::Display for ComponentContext {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}[{}]", self.component_type, self.component_id)
    }
}
