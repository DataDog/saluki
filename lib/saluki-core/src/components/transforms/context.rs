use memory_accounting::{ComponentRegistry, MemoryLimiter};
use saluki_health::{Health, HealthRegistry};

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
    health_handle: Option<Health>,
    health_registry: HealthRegistry,
    component_registry: ComponentRegistry,
}

impl TransformContext {
    /// Creates a new `TransformContext`.
    pub fn new(
        component_context: ComponentContext, forwarder: Forwarder, event_stream: EventStream,
        event_buffer_pool: FixedSizeObjectPool<EventBuffer>, memory_limiter: MemoryLimiter,
        component_registry: ComponentRegistry, health_handle: Health, health_registry: HealthRegistry,
    ) -> Self {
        Self {
            component_context,
            forwarder,
            event_stream,
            event_buffer_pool,
            memory_limiter,
            health_handle: Some(health_handle),
            health_registry,
            component_registry,
        }
    }

    /// Consumes the health handle of this transform context.
    ///
    /// ## Panics
    ///
    /// Panics if the health handle has already been taken.
    pub fn take_health_handle(&mut self) -> Health {
        self.health_handle.take().expect("health handle already taken")
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

    /// Gets a reference to the health registry.
    pub fn health_registry(&self) -> &HealthRegistry {
        &self.health_registry
    }

    /// Gets a mutable reference to the component registry.
    pub fn component_registry(&self) -> &ComponentRegistry {
        &self.component_registry
    }
}
