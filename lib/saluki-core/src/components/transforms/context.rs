use memory_accounting::{ComponentRegistry, MemoryLimiter};

use crate::{
    components::ComponentContext,
    pooling::FixedSizeObjectPool,
    topology::interconnect::{EventBuffer, EventStream, Forwarder},
};

/// Transform context.
pub struct TransformContext {
    component_context: ComponentContext,
    forwarder: Forwarder,
    event_stream: EventStream,
    event_buffer_pool: FixedSizeObjectPool<EventBuffer>,
    memory_limiter: MemoryLimiter,
    component_registry: ComponentRegistry,
}

impl TransformContext {
    /// Creates a new `TransformContext`.
    pub fn new(
        component_context: ComponentContext, forwarder: Forwarder, event_stream: EventStream,
        event_buffer_pool: FixedSizeObjectPool<EventBuffer>, memory_limiter: MemoryLimiter,
        component_registry: ComponentRegistry,
    ) -> Self {
        Self {
            component_context,
            forwarder,
            event_stream,
            event_buffer_pool,
            memory_limiter,
            component_registry,
        }
    }

    /// Returns the component context.
    pub fn component_context(&self) -> ComponentContext {
        self.component_context.clone()
    }

    /// Gets a reference to the forwarder.
    pub fn forwarder(&self) -> &Forwarder {
        &self.forwarder
    }

    /// Gets a mutable reference to the event stream.
    pub fn event_stream(&mut self) -> &mut EventStream {
        &mut self.event_stream
    }

    /// Gets a reference to the event buffer pool.
    pub fn event_buffer_pool(&self) -> &FixedSizeObjectPool<EventBuffer> {
        &self.event_buffer_pool
    }

    /// Gets a reference to the memory limiter.
    pub fn memory_limiter(&self) -> &MemoryLimiter {
        &self.memory_limiter
    }

    /// Gets a mutable reference to the component registry.
    pub fn component_registry_mut(&mut self) -> &mut ComponentRegistry {
        &mut self.component_registry
    }
}
