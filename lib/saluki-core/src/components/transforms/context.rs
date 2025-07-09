use memory_accounting::{ComponentRegistry, MemoryLimiter};
use saluki_health::{Health, HealthRegistry};
use tokio::runtime::Handle;

use crate::{
    components::ComponentContext,
    topology::interconnect::{Dispatcher, EventStream, FixedSizeEventBuffer},
};

/// Transform context.
pub struct TransformContext {
    component_context: ComponentContext,
    dispatcher: Dispatcher<FixedSizeEventBuffer<1024>>,
    event_stream: EventStream,
    memory_limiter: MemoryLimiter,
    health_handle: Option<Health>,
    health_registry: HealthRegistry,
    component_registry: ComponentRegistry,
    thread_pool: Handle,
}

impl TransformContext {
    /// Creates a new `TransformContext`.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        component_context: ComponentContext, dispatcher: Dispatcher<FixedSizeEventBuffer<1024>>,
        event_stream: EventStream, memory_limiter: MemoryLimiter, component_registry: ComponentRegistry,
        health_handle: Health, health_registry: HealthRegistry, thread_pool: Handle,
    ) -> Self {
        Self {
            component_context,
            dispatcher,
            event_stream,
            memory_limiter,
            health_handle: Some(health_handle),
            health_registry,
            component_registry,
            thread_pool,
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

    /// Gets a reference to the dispatcher.
    pub fn dispatcher(&self) -> &Dispatcher<FixedSizeEventBuffer<1024>> {
        &self.dispatcher
    }

    /// Gets a mutable reference to the event stream.
    pub fn event_stream(&mut self) -> &mut EventStream {
        &mut self.event_stream
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

    /// Gets a reference to the global thread pool.
    pub fn global_thread_pool(&self) -> &Handle {
        &self.thread_pool
    }
}
