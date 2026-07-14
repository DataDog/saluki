use crate::accounting::ComponentRegistry;
use crate::health::Health;
use crate::runtime::SupervisorHandle;
use crate::{
    components::ComponentContext,
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
    supervisor_handle: SupervisorHandle,
}

impl EncoderContext {
    /// Creates a new `EncoderContext`.
    pub fn new(
        topology_context: &TopologyContext, component_context: &ComponentContext,
        component_registry: ComponentRegistry, health_handle: Health, dispatcher: PayloadsDispatcher,
        consumer: EventsConsumer, supervisor_handle: SupervisorHandle,
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

    /// Consumes the health handle of this encoder context.
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

    /// Gets a reference to the payloads dispatcher.
    pub fn dispatcher(&self) -> &PayloadsDispatcher {
        &self.dispatcher
    }

    /// Gets a mutable reference to the events consumer.
    pub fn events(&mut self) -> &mut EventsConsumer {
        &mut self.consumer
    }

    /// Returns a handle to the supervisor that this component is spawned on.
    ///
    /// Dynamic child processes can be spawned via the supervisor handle and thus have their lifecycle
    /// coupled to the component itself: if the component restarts, or the component's supervisor dies,
    /// the dynamic child processes will also be terminated automatically as well.
    pub fn spawn_handle(&self) -> &SupervisorHandle {
        &self.supervisor_handle
    }
}
