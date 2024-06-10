//! Component basics.

pub mod destinations;

mod metrics;
pub use self::metrics::MetricsBuilder;

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

    /// Returns the component identifier.
    pub fn component_id(&self) -> &ComponentId {
        &self.component_id
    }

    /// Returns the component type.
    pub fn component_type(&self) -> &'static str {
        self.component_type
    }
}
