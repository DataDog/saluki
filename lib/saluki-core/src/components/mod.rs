//! Component basics.

use std::fmt;

use crate::{support::SubsystemIdentifier, topology::ComponentId};

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

    /// Returns the categorical string representation of the component type.
    ///
    /// This is a plural form of the value returned by [`as_str`][Self::as_str]. For example, if [`as_str`][Self::as_str]
    /// returns `source`, this returns `sources`.
    pub fn as_category_str(&self) -> &'static str {
        match self {
            Self::Source => "sources",
            Self::Relay => "relays",
            Self::Decoder => "decoders",
            Self::Transform => "transforms",
            Self::Encoder => "encoders",
            Self::Forwarder => "forwarders",
            Self::Destination => "destinations",
        }
    }
}

/// A component context.
///
/// Holds the identifiers (absolute and relative) for a component, as well as its type.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ComponentContext {
    topology_root: SubsystemIdentifier,
    component_id: ComponentId,
    component_type: ComponentType,
}

impl ComponentContext {
    /// Creates a new `ComponentContext` rooted at the given topology, with the given identifier and type.
    pub fn new(topology_root: &SubsystemIdentifier, component_id: ComponentId, component_type: ComponentType) -> Self {
        Self {
            topology_root: topology_root.clone(),
            component_id,
            component_type,
        }
    }

    /// Creates a new `ComponentContext` for a source component with the given identifier, within the named topology.
    pub fn source(topology_root: &SubsystemIdentifier, component_id: ComponentId) -> Self {
        Self::new(topology_root, component_id, ComponentType::Source)
    }

    /// Creates a new `ComponentContext` for a relay component with the given identifier, within the named topology.
    pub fn relay(topology_root: &SubsystemIdentifier, component_id: ComponentId) -> Self {
        Self::new(topology_root, component_id, ComponentType::Relay)
    }

    /// Creates a new `ComponentContext` for a decoder component with the given identifier, within the named topology.
    pub fn decoder(topology_root: &SubsystemIdentifier, component_id: ComponentId) -> Self {
        Self::new(topology_root, component_id, ComponentType::Decoder)
    }

    /// Creates a new `ComponentContext` for a transform component with the given identifier, within the named topology.
    pub fn transform(topology_root: &SubsystemIdentifier, component_id: ComponentId) -> Self {
        Self::new(topology_root, component_id, ComponentType::Transform)
    }

    /// Creates a new `ComponentContext` for an encoder component with the given identifier, within the named topology.
    pub fn encoder(topology_root: &SubsystemIdentifier, component_id: ComponentId) -> Self {
        Self::new(topology_root, component_id, ComponentType::Encoder)
    }

    /// Creates a new `ComponentContext` for a forwarder component with the given identifier, within the named topology.
    pub fn forwarder(topology_root: &SubsystemIdentifier, component_id: ComponentId) -> Self {
        Self::new(topology_root, component_id, ComponentType::Forwarder)
    }

    /// Creates a new `ComponentContext` for a destination component with the given identifier, within the named topology.
    pub fn destination(topology_root: &SubsystemIdentifier, component_id: ComponentId) -> Self {
        Self::new(topology_root, component_id, ComponentType::Destination)
    }

    /// Creates a new `ComponentContext` for a source component with the given identifier, in a test topology.
    #[cfg(test)]
    pub fn test_source<S: AsRef<str>>(component_id: S) -> Self {
        Self::source(
            &SubsystemIdentifier::from_segments(["topology", "test"]),
            ComponentId::try_from(component_id.as_ref()).expect("invalid component ID"),
        )
    }

    /// Creates a new `ComponentContext` for a relay component with the given identifier, in a test topology.
    #[cfg(test)]
    pub fn test_relay<S: AsRef<str>>(component_id: S) -> Self {
        Self::relay(
            &SubsystemIdentifier::from_segments(["topology", "test"]),
            ComponentId::try_from(component_id.as_ref()).expect("invalid component ID"),
        )
    }

    /// Creates a new `ComponentContext` for a decoder component with the given identifier, in a test topology.
    #[cfg(test)]
    pub fn test_decoder<S: AsRef<str>>(component_id: S) -> Self {
        Self::decoder(
            &SubsystemIdentifier::from_segments(["topology", "test"]),
            ComponentId::try_from(component_id.as_ref()).expect("invalid component ID"),
        )
    }

    /// Creates a new `ComponentContext` for a transform component with the given identifier, in a test topology.
    #[cfg(test)]
    pub fn test_transform<S: AsRef<str>>(component_id: S) -> Self {
        Self::transform(
            &SubsystemIdentifier::from_segments(["topology", "test"]),
            ComponentId::try_from(component_id.as_ref()).expect("invalid component ID"),
        )
    }

    /// Creates a new `ComponentContext` for an encoder component with the given identifier, in a test topology.
    #[cfg(test)]
    pub fn test_encoder<S: AsRef<str>>(component_id: S) -> Self {
        Self::encoder(
            &SubsystemIdentifier::from_segments(["topology", "test"]),
            ComponentId::try_from(component_id.as_ref()).expect("invalid component ID"),
        )
    }

    /// Creates a new `ComponentContext` for a forwarder component with the given identifier, in a test topology.
    #[cfg(test)]
    pub fn test_forwarder<S: AsRef<str>>(component_id: S) -> Self {
        Self::forwarder(
            &SubsystemIdentifier::from_segments(["topology", "test"]),
            ComponentId::try_from(component_id.as_ref()).expect("invalid component ID"),
        )
    }

    /// Creates a new `ComponentContext` for a destination component with the given identifier, in a test topology.
    #[cfg(test)]
    pub fn test_destination<S: AsRef<str>>(component_id: S) -> Self {
        Self::destination(
            &SubsystemIdentifier::from_segments(["topology", "test"]),
            ComponentId::try_from(component_id.as_ref()).expect("invalid component ID"),
        )
    }

    /// Returns the component identifier.
    ///
    /// This is the relative identifier of the component, unique within its topology but not guaranteed to be globally
    /// unique. See [`ComponentContext::identity`] for the fully qualified identity.
    pub fn component_id(&self) -> &ComponentId {
        &self.component_id
    }

    /// Returns the component type.
    pub fn component_type(&self) -> ComponentType {
        self.component_type
    }

    /// Returns the fully qualified, canonical identity of this component.
    ///
    /// The returned [`SubsystemIdentifier`] is the single source of truth for how this component is named across
    /// subsystems (the health registry, resource accounting, and the supervision/process tree). It extends the
    /// topology root with the component's category and identifier, yielding the form `topology.<name>.<category>.<id>`
    /// (where `<category>` is the plural component type, [`ComponentType::as_category`]).
    pub fn identity(&self) -> SubsystemIdentifier {
        self.topology_root
            .clone()
            .child(self.component_type.as_category_str())
            .child(&*self.component_id)
    }
}

impl fmt::Display for ComponentContext {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}[{}]", self.component_type.as_str(), self.component_id)
    }
}

#[cfg(test)]
mod tests {
    use super::ComponentContext;

    #[test]
    fn identity_dotted_form() {
        let context = ComponentContext::test_source("dsd_in");
        assert_eq!(context.identity().to_string(), "topology.test.sources.dsd_in");
    }

    #[test]
    fn identity_sanitizes_hyphenated_id() {
        // Hyphens are allowed in component IDs but are sanitized to underscores in the canonical identity, so it is a
        // valid process name and matches across every subsystem.
        let context = ComponentContext::test_transform("dsd-mapper");
        assert_eq!(context.identity().to_string(), "topology.test.transforms.dsd_mapper");
    }
}
