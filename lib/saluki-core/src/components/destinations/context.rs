use memory_accounting::{ComponentRegistry, MemoryLimiter};

use crate::{components::ComponentContext, topology::interconnect::EventStream};

/// Destination context.
pub struct DestinationContext {
    component_context: ComponentContext,
    events: EventStream,
    memory_limiter: MemoryLimiter,
    component_registry: ComponentRegistry,
}

impl DestinationContext {
    /// Creates a new `DestinationContext`.
    pub fn new(
        component_context: ComponentContext, events: EventStream, memory_limiter: MemoryLimiter,
        component_registry: ComponentRegistry,
    ) -> Self {
        Self {
            component_context,
            events,
            memory_limiter,
            component_registry,
        }
    }

    /// Returns the component context.
    pub fn component_context(&self) -> ComponentContext {
        self.component_context.clone()
    }

    /// Gets a mutable reference to the event stream.
    pub fn events(&mut self) -> &mut EventStream {
        &mut self.events
    }

    /// Gets a reference to the memory limiter.
    pub fn memory_limiter(&self) -> &MemoryLimiter {
        &self.memory_limiter
    }

    /// Gets a mutable reference to the component registry.
    pub fn component_registry_mut(&mut self) -> &ComponentRegistry {
        &self.component_registry
    }
}
