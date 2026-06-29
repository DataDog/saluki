use resource_accounting::ComponentRegistry;

use crate::health::Health;
use crate::runtime::SupervisorHandle;
use crate::{
    components::ComponentContext,
    topology::{EventsDispatcher, PayloadsConsumer, TopologyContext},
};

/// Decoder context.
pub struct DecoderContext {
    topology_context: TopologyContext,
    component_context: ComponentContext,
    component_registry: ComponentRegistry,
    health_handle: Option<Health>,
    dispatcher: EventsDispatcher,
    consumer: PayloadsConsumer,
    supervisor_handle: SupervisorHandle,
}

impl DecoderContext {
    /// Creates a new `DecoderContext`.
    pub fn new(
        topology_context: &TopologyContext, component_context: &ComponentContext,
        component_registry: ComponentRegistry, health_handle: Health, dispatcher: EventsDispatcher,
        consumer: PayloadsConsumer, supervisor_handle: SupervisorHandle,
    ) -> Self {
        Self {
            topology_context: topology_context.clone(),
            component_context: component_context.clone(),
            component_registry,
            health_handle: Some(health_handle),
            dispatcher,
            consumer,
            supervisor_handle,
        }
    }

    /// Consumes the health handle of this decoder context.
    ///
    /// # Panics
    ///
    /// Panics if the health handle has already been taken.
    pub fn take_health_handle(&mut self) -> Health {
        self.health_handle.take().expect("health handle already taken")
    }

    /// Gets a reference to the topology context.
    pub fn topology_context(&self) -> &TopologyContext {
        &self.topology_context
    }

    /// Gets a reference to the component context.
    pub fn component_context(&self) -> &ComponentContext {
        &self.component_context
    }

    /// Gets a reference to the component registry.
    pub fn component_registry(&mut self) -> &ComponentRegistry {
        &self.component_registry
    }

    /// Gets a reference to the events dispatcher.
    pub fn dispatcher(&self) -> &EventsDispatcher {
        &self.dispatcher
    }

    /// Gets a mutable reference to the payloads consumer.
    pub fn payloads(&mut self) -> &mut PayloadsConsumer {
        &mut self.consumer
    }

    /// Returns a handle to the supervisor that this component is spawned on.
    ///
    /// Dynamic child processes can be spawned via the supervisor handle and thus have their lifecycle
    /// coupled to the component itself: if the component restarts, or the component's supervisor die,
    /// the dynamic child processes will also be terminated automatically as well.
    pub fn spawn_handle(&self) -> &SupervisorHandle {
        &self.supervisor_handle
    }
}
