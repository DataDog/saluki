//! Component basics.

use std::fmt;

use crate::topology::ComponentId;

pub mod decoders;
pub mod destinations;
pub mod encoders;
pub mod forwarders;
pub mod relays;
pub mod sources;
pub mod transforms;

/// Component type.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum ComponentType {
    /// Source.
    Source,

    /// Relay.
    Relay,

    /// Decoder.
    Decoder,

    /// Transform.
    Transform,

    /// Encoder.
    Encoder,

    /// Forwarder.
    Forwarder,

    /// Destination.
    Destination,
}

impl ComponentType {
    /// Returns the string representation of the component type.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Source => "source",
            Self::Relay => "relay",
            Self::Decoder => "decoder",
            Self::Transform => "transform",
            Self::Encoder => "encoder",
            Self::Forwarder => "forwarder",
            Self::Destination => "destination",
        }
    }
}

/// A component context.
///
/// Component contexts uniquely identify a component within a topology by coupling the component identifier (name) and
/// component type (source, relay, decoder, transform, encoder, forwarder, or destination).
///
/// Practically speaking, all components are required to have a unique identifier. However, identifiers may be opaque
/// enough that without knowing the _type_ of component, the identifier doesn't provide enough information.
#[derive(Clone)]
pub struct ComponentContext {
    component_id: ComponentId,
    component_type: ComponentType,
}

impl ComponentContext {
    /// Creates a new `ComponentContext` for a source component with the given identifier.
    pub fn source(component_id: ComponentId) -> Self {
        Self {
            component_id,
            component_type: ComponentType::Source,
        }
    }

    /// Creates a new `ComponentContext` for a relay component with the given identifier.
    pub fn relay(component_id: ComponentId) -> Self {
        Self {
            component_id,
            component_type: ComponentType::Relay,
        }
    }

    /// Creates a new `ComponentContext` for a decoder component with the given identifier.
    pub fn decoder(component_id: ComponentId) -> Self {
        Self {
            component_id,
            component_type: ComponentType::Decoder,
        }
    }

    /// Creates a new `ComponentContext` for a transform component with the given identifier.
    pub fn transform(component_id: ComponentId) -> Self {
        Self {
            component_id,
            component_type: ComponentType::Transform,
        }
    }

    /// Creates a new `ComponentContext` for an encoder component with the given identifier.
    pub fn encoder(component_id: ComponentId) -> Self {
        Self {
            component_id,
            component_type: ComponentType::Encoder,
        }
    }

    /// Creates a new `ComponentContext` for a forwarder component with the given identifier.
    pub fn forwarder(component_id: ComponentId) -> Self {
        Self {
            component_id,
            component_type: ComponentType::Forwarder,
        }
    }

    /// Creates a new `ComponentContext` for a destination component with the given identifier.
    pub fn destination(component_id: ComponentId) -> Self {
        Self {
            component_id,
            component_type: ComponentType::Destination,
        }
    }

    /// Creates a new `ComponentContext` for a source component with the given identifier.
    #[cfg(test)]
    pub fn test_source<S: AsRef<str>>(component_id: S) -> Self {
        Self {
            component_id: ComponentId::try_from(component_id.as_ref()).expect("invalid component ID"),
            component_type: ComponentType::Source,
        }
    }

    /// Creates a new `ComponentContext` for a relay component with the given identifier.
    #[cfg(test)]
    pub fn test_relay<S: AsRef<str>>(component_id: S) -> Self {
        Self {
            component_id: ComponentId::try_from(component_id.as_ref()).expect("invalid component ID"),
            component_type: ComponentType::Relay,
        }
    }

    /// Creates a new `ComponentContext` for a decoder component with the given identifier.
    #[cfg(test)]
    pub fn test_decoder<S: AsRef<str>>(component_id: S) -> Self {
        Self {
            component_id: ComponentId::try_from(component_id.as_ref()).expect("invalid component ID"),
            component_type: ComponentType::Decoder,
        }
    }

    /// Creates a new `ComponentContext` for a transform component with the given identifier.
    #[cfg(test)]
    pub fn test_transform<S: AsRef<str>>(component_id: S) -> Self {
        Self {
            component_id: ComponentId::try_from(component_id.as_ref()).expect("invalid component ID"),
            component_type: ComponentType::Transform,
        }
    }

    /// Creates a new `ComponentContext` for a encoder component with the given identifier.
    #[cfg(test)]
    pub fn test_encoder<S: AsRef<str>>(component_id: S) -> Self {
        Self {
            component_id: ComponentId::try_from(component_id.as_ref()).expect("invalid component ID"),
            component_type: ComponentType::Encoder,
        }
    }

    /// Creates a new `ComponentContext` for a forwarder component with the given identifier.
    #[cfg(test)]
    pub fn test_forwarder<S: AsRef<str>>(component_id: S) -> Self {
        Self {
            component_id: ComponentId::try_from(component_id.as_ref()).expect("invalid component ID"),
            component_type: ComponentType::Forwarder,
        }
    }

    /// Creates a new `ComponentContext` for a destination component with the given identifier.
    #[cfg(test)]
    pub fn test_destination<S: AsRef<str>>(component_id: S) -> Self {
        Self {
            component_id: ComponentId::try_from(component_id.as_ref()).expect("invalid component ID"),
            component_type: ComponentType::Destination,
        }
    }

    /// Returns the component identifier.
    pub fn component_id(&self) -> &ComponentId {
        &self.component_id
    }

    /// Returns the component type.
    pub fn component_type(&self) -> ComponentType {
        self.component_type
    }
}

impl fmt::Display for ComponentContext {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}[{}]", self.component_type.as_str(), self.component_id)
    }
}
