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
    #[cfg(any(test, feature = "test-util"))]
    pub fn test_source<S: AsRef<str>>(component_id: S) -> Self {
        Self::source(
            &SubsystemIdentifier::from_segments(["topology", "test"]),
            ComponentId::try_from(component_id.as_ref()).expect("invalid component ID"),
        )
    }

    /// Creates a new `ComponentContext` for a relay component with the given identifier, in a test topology.
    #[cfg(any(test, feature = "test-util"))]
    pub fn test_relay<S: AsRef<str>>(component_id: S) -> Self {
        Self::relay(
            &SubsystemIdentifier::from_segments(["topology", "test"]),
            ComponentId::try_from(component_id.as_ref()).expect("invalid component ID"),
        )
    }

    /// Creates a new `ComponentContext` for a decoder component with the given identifier, in a test topology.
    #[cfg(any(test, feature = "test-util"))]
    pub fn test_decoder<S: AsRef<str>>(component_id: S) -> Self {
        Self::decoder(
            &SubsystemIdentifier::from_segments(["topology", "test"]),
            ComponentId::try_from(component_id.as_ref()).expect("invalid component ID"),
        )
    }

    /// Creates a new `ComponentContext` for a transform component with the given identifier, in a test topology.
    #[cfg(any(test, feature = "test-util"))]
    pub fn test_transform<S: AsRef<str>>(component_id: S) -> Self {
        Self::transform(
            &SubsystemIdentifier::from_segments(["topology", "test"]),
            ComponentId::try_from(component_id.as_ref()).expect("invalid component ID"),
        )
    }

    /// Creates a new `ComponentContext` for an encoder component with the given identifier, in a test topology.
    #[cfg(any(test, feature = "test-util"))]
    pub fn test_encoder<S: AsRef<str>>(component_id: S) -> Self {
        Self::encoder(
            &SubsystemIdentifier::from_segments(["topology", "test"]),
            ComponentId::try_from(component_id.as_ref()).expect("invalid component ID"),
        )
    }

    /// Creates a new `ComponentContext` for a forwarder component with the given identifier, in a test topology.
    #[cfg(any(test, feature = "test-util"))]
    pub fn test_forwarder<S: AsRef<str>>(component_id: S) -> Self {
        Self::forwarder(
            &SubsystemIdentifier::from_segments(["topology", "test"]),
            ComponentId::try_from(component_id.as_ref()).expect("invalid component ID"),
        )
    }

    /// Creates a new `ComponentContext` for a destination component with the given identifier, in a test topology.
    #[cfg(any(test, feature = "test-util"))]
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

    /// Returns the fully qualified identity of this component.
    ///
    /// The returned identifier uniquely identifies the component within the process, inclusive of the topology to
    /// which it belongs.
    pub fn identity(&self) -> SubsystemIdentifier {
        self.topology_root
            .clone()
            .child(self.component_type.as_category_str())
            .child(&*self.component_id)
    }
}

impl fmt::Display for ComponentContext {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.identity())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use saluki_common::sync::shutdown::ShutdownHandle;
    use tokio::runtime::Handle;
    use tokio::sync::mpsc;

    use super::decoders::DecoderContext;
    use super::destinations::DestinationContext;
    use super::encoders::EncoderContext;
    use super::forwarders::ForwarderContext;
    use super::relays::RelayContext;
    use super::sources::SourceContext;
    use super::transforms::TransformContext;
    use super::ComponentContext;
    use crate::accounting::{ComponentRegistry, MemoryLimiter};
    use crate::health::{Health, HealthRegistry};
    use crate::runtime::state::DataspaceRegistry;
    use crate::runtime::{Supervisor, SupervisorHandle};
    use crate::support::SubsystemIdentifier;
    use crate::topology::interconnect::{Consumer, Dispatcher};
    use crate::topology::{
        EventsBuffer, EventsConsumer, EventsDispatcher, PayloadsBuffer, PayloadsConsumer, PayloadsDispatcher,
        TopologyContext,
    };

    #[test]
    fn identity_dotted_form() {
        let context = ComponentContext::test_source("dsd_in");
        assert_eq!(context.identity().to_string(), "topology.test.sources.dsd_in");
    }

    #[test]
    fn context_display_equals_identity_to_string() {
        let context = ComponentContext::test_transform("dsd_mapper");
        assert_eq!(context.to_string(), context.identity().to_string());
    }

    // Double-take handle tests.
    //
    // Every `*Context` documents that `take_health_handle` (and, for sources/relays, `take_shutdown_handle`) panics
    // if the handle has already been taken. Each test below builds a real context, takes the handle once (which must
    // succeed), then takes it a second time and asserts the documented panic fires with its documented message.
    //
    // The shared builders below collapse the otherwise-identical dependency construction across the seven context
    // types; each context's `new` only differs in which dispatcher/consumer it accepts.

    fn topology_context() -> TopologyContext {
        TopologyContext::new(
            Arc::from("test"),
            MemoryLimiter::noop(),
            HealthRegistry::new(),
            Handle::current(),
            DataspaceRegistry::new(),
        )
    }

    fn health_handle() -> Health {
        HealthRegistry::new()
            .register_component(&SubsystemIdentifier::from_dotted("test"))
            .expect("component was not previously registered")
    }

    fn supervisor_handle() -> SupervisorHandle {
        Supervisor::new("test").expect("valid supervisor name").handle()
    }

    fn events_dispatcher(component_context: &ComponentContext) -> EventsDispatcher {
        Dispatcher::new(component_context.clone())
    }

    fn payloads_dispatcher(component_context: &ComponentContext) -> PayloadsDispatcher {
        Dispatcher::new(component_context.clone())
    }

    fn events_consumer(component_context: &ComponentContext) -> EventsConsumer {
        let (_tx, rx) = mpsc::channel::<EventsBuffer>(1);
        Consumer::new(component_context.clone(), rx)
    }

    fn payloads_consumer(component_context: &ComponentContext) -> PayloadsConsumer {
        let (_tx, rx) = mpsc::channel::<PayloadsBuffer>(1);
        Consumer::new(component_context.clone(), rx)
    }

    fn source_context() -> SourceContext {
        let cc = ComponentContext::test_source("test");
        SourceContext::new(
            &topology_context(),
            &cc,
            ComponentRegistry::default(),
            health_handle(),
            events_dispatcher(&cc),
            supervisor_handle(),
        )
    }

    fn relay_context() -> RelayContext {
        let cc = ComponentContext::test_relay("test");
        RelayContext::new(
            &topology_context(),
            &cc,
            ComponentRegistry::default(),
            health_handle(),
            payloads_dispatcher(&cc),
            supervisor_handle(),
        )
    }

    fn decoder_context() -> DecoderContext {
        let cc = ComponentContext::test_decoder("test");
        DecoderContext::new(
            &topology_context(),
            &cc,
            ComponentRegistry::default(),
            health_handle(),
            events_dispatcher(&cc),
            payloads_consumer(&cc),
            supervisor_handle(),
        )
    }

    fn transform_context() -> TransformContext {
        let cc = ComponentContext::test_transform("test");
        TransformContext::new(
            &topology_context(),
            &cc,
            ComponentRegistry::default(),
            health_handle(),
            events_dispatcher(&cc),
            events_consumer(&cc),
            supervisor_handle(),
        )
    }

    fn destination_context() -> DestinationContext {
        let cc = ComponentContext::test_destination("test");
        DestinationContext::new(
            &topology_context(),
            &cc,
            ComponentRegistry::default(),
            health_handle(),
            events_consumer(&cc),
            supervisor_handle(),
        )
    }

    fn encoder_context() -> EncoderContext {
        let cc = ComponentContext::test_encoder("test");
        EncoderContext::new(
            &topology_context(),
            &cc,
            ComponentRegistry::default(),
            health_handle(),
            payloads_dispatcher(&cc),
            events_consumer(&cc),
            supervisor_handle(),
        )
    }

    fn forwarder_context() -> ForwarderContext {
        let cc = ComponentContext::test_forwarder("test");
        ForwarderContext::new(
            &topology_context(),
            &cc,
            ComponentRegistry::default(),
            health_handle(),
            payloads_consumer(&cc),
            supervisor_handle(),
        )
    }

    // Health-handle double-take: applies to all seven context types.

    #[tokio::test]
    #[should_panic(expected = "health handle already taken")]
    async fn source_context_panics_on_double_take_of_health_handle() {
        let mut ctx = source_context();
        let _first = ctx.take_health_handle();
        let _second = ctx.take_health_handle();
    }

    #[tokio::test]
    #[should_panic(expected = "health handle already taken")]
    async fn relay_context_panics_on_double_take_of_health_handle() {
        let mut ctx = relay_context();
        let _first = ctx.take_health_handle();
        let _second = ctx.take_health_handle();
    }

    #[tokio::test]
    #[should_panic(expected = "health handle already taken")]
    async fn decoder_context_panics_on_double_take_of_health_handle() {
        let mut ctx = decoder_context();
        let _first = ctx.take_health_handle();
        let _second = ctx.take_health_handle();
    }

    #[tokio::test]
    #[should_panic(expected = "health handle already taken")]
    async fn transform_context_panics_on_double_take_of_health_handle() {
        let mut ctx = transform_context();
        let _first = ctx.take_health_handle();
        let _second = ctx.take_health_handle();
    }

    #[tokio::test]
    #[should_panic(expected = "health handle already taken")]
    async fn destination_context_panics_on_double_take_of_health_handle() {
        let mut ctx = destination_context();
        let _first = ctx.take_health_handle();
        let _second = ctx.take_health_handle();
    }

    #[tokio::test]
    #[should_panic(expected = "health handle already taken")]
    async fn encoder_context_panics_on_double_take_of_health_handle() {
        let mut ctx = encoder_context();
        let _first = ctx.take_health_handle();
        let _second = ctx.take_health_handle();
    }

    #[tokio::test]
    #[should_panic(expected = "health handle already taken")]
    async fn forwarder_context_panics_on_double_take_of_health_handle() {
        let mut ctx = forwarder_context();
        let _first = ctx.take_health_handle();
        let _second = ctx.take_health_handle();
    }

    // Shutdown-handle double-take: only sources and relays expose a shutdown handle. The runtime installs it via the
    // crate-private `set_shutdown_handle` before the component runs, so we do the same here before taking it twice.

    #[tokio::test]
    #[should_panic(expected = "shutdown handle already taken")]
    async fn source_context_panics_on_double_take_of_shutdown_handle() {
        let mut ctx = source_context();
        ctx.set_shutdown_handle(ShutdownHandle::noop());
        let _first = ctx.take_shutdown_handle();
        let _second = ctx.take_shutdown_handle();
    }

    #[tokio::test]
    #[should_panic(expected = "shutdown handle already taken")]
    async fn relay_context_panics_on_double_take_of_shutdown_handle() {
        let mut ctx = relay_context();
        ctx.set_shutdown_handle(ShutdownHandle::noop());
        let _first = ctx.take_shutdown_handle();
        let _second = ctx.take_shutdown_handle();
    }
}
