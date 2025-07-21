use memory_accounting::ComponentRegistry;
use saluki_health::Health;

use crate::{
    components::ComponentContext,
    topology::{PayloadsConsumer, TopologyContext},
};

/// Forwarder context.
pub struct ForwarderContext {
    topology_context: TopologyContext,
    component_context: ComponentContext,
    component_registry: ComponentRegistry,
    health_handle: Option<Health>,
    consumer: PayloadsConsumer,
}

impl ForwarderContext {
    /// Creates a new `ForwarderContext`.
    pub fn new(
        topology_context: &TopologyContext, component_context: &ComponentContext,
        component_registry: ComponentRegistry, health_handle: Health, consumer: PayloadsConsumer,
    ) -> Self {
        Self {
            topology_context: topology_context.clone(),
            component_context: component_context.clone(),
            component_registry,
            health_handle: Some(health_handle),
            consumer,
        }
    }

    /// Consumes the health handle of this forwarder context.
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

    /// Gets a mutable reference to the payloads consumer.
    pub fn payloads(&mut self) -> &mut PayloadsConsumer {
        &mut self.consumer
    }
}
