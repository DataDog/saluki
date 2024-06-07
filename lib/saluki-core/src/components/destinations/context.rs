use memory_accounting::limiter::MemoryLimiter;

use crate::{components::ComponentContext, topology::interconnect::EventStream};

/// Destination context.
pub struct DestinationContext {
    component_context: ComponentContext,
    events: EventStream,
    memory_limiter: MemoryLimiter,
}

impl DestinationContext {
    /// Creates a new `DestinationContext`.
    pub fn new(component_context: ComponentContext, events: EventStream, memory_limiter: MemoryLimiter) -> Self {
        Self {
            component_context,
            events,
            memory_limiter,
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

    #[allow(unused)]
    pub fn memory_limiter(&self) -> &MemoryLimiter {
        &self.memory_limiter
    }
}
