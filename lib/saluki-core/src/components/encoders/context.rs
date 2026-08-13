use crate::accounting::ComponentRegistry;
use crate::health::Health;
use crate::{
    components::{ComponentContext, ComponentSpawner},
    topology::{EventsConsumer, PayloadsDispatcher, TopologyContext},
};

/// Encoder context.
pub struct EncoderContext {
    topology_context: TopologyContext,
    component_context: ComponentContext,
    component_registry: ComponentRegistry,
    health_handle: Option<Health>,
    dispatcher: PayloadsDispatcher,
    consumer: EventsConsumer,
    spawner: ComponentSpawner,
}

impl EncoderContext {
    /// Creates a new `EncoderContext`.
    pub fn new(
        topology_context: &TopologyContext, component_context: &ComponentContext,
        component_registry: ComponentRegistry, health_handle: Health, dispatcher: PayloadsDispatcher,
        consumer: EventsConsumer, spawner: ComponentSpawner,
    ) -> Self {
        Self {
            topology_context: topology_context.clone(),
            component_context: component_context.clone(),
            component_registry,
            health_handle: Some(health_handle),
            dispatcher,
            consumer,
            spawner,
        }
    }

    /// Consumes the health handle of this encoder context.
    ///
    /// # Panics
    ///
    /// Panics if the health handle has already been taken.
    pub fn take_health_handle(&mut self) -> Health {
        self.health_handle.take().expect("health handle already taken")
    }

    /// Returns a reference to the topology context.
    pub fn topology_context(&self) -> &TopologyContext {
        &self.topology_context
    }

    /// Returns a reference to the component context.
    pub fn component_context(&self) -> &ComponentContext {
        &self.component_context
    }

    /// Returns a reference to the component registry.
    pub fn component_registry(&mut self) -> &ComponentRegistry {
        &self.component_registry
    }

    /// Returns a reference to the payloads dispatcher.
    pub fn dispatcher(&self) -> &PayloadsDispatcher {
        &self.dispatcher
    }

    /// Returns a mutable reference to the events consumer.
    pub fn events(&mut self) -> &mut EventsConsumer {
        &mut self.consumer
    }

    /// Returns a spawner for supervised child tasks belonging to this component.
    ///
    /// All child tasks spawned through this mechanism are tied to the lifecycle of the component itself, such that
    /// they're automatically shutdown/stopped when the component is stopped during topology shutdown, etc.
    pub fn spawner(&self) -> &ComponentSpawner {
        &self.spawner
    }
}
