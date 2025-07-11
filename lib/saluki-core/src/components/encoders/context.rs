use memory_accounting::{ComponentRegistry, MemoryLimiter};
use saluki_health::{Health, HealthRegistry};
use tokio::runtime::Handle;

use crate::{
    components::ComponentContext,
    topology::{EventsConsumer, PayloadsDispatcher},
};

/// Encoder context.
pub struct EncoderContext {
    component_context: ComponentContext,
    dispatcher: PayloadsDispatcher,
    consumer: EventsConsumer,
    memory_limiter: MemoryLimiter,
    health_handle: Option<Health>,
    health_registry: HealthRegistry,
    component_registry: ComponentRegistry,
    thread_pool: Handle,
}

impl EncoderContext {
    /// Creates a new `EncoderContext`.
    pub fn new(
        component_context: ComponentContext, dispatcher: PayloadsDispatcher, consumer: EventsConsumer,
        memory_limiter: MemoryLimiter, component_registry: ComponentRegistry, health_handle: Health,
        health_registry: HealthRegistry, thread_pool: Handle,
    ) -> Self {
        Self {
            component_context,
            dispatcher,
            consumer,
            memory_limiter,
            health_handle: Some(health_handle),
            health_registry,
            component_registry,
            thread_pool,
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

    /// Returns the component context.
    pub fn component_context(&self) -> ComponentContext {
        self.component_context.clone()
    }

    /// Gets a reference to the payloads dispatcher.
    pub fn dispatcher(&self) -> &PayloadsDispatcher {
        &self.dispatcher
    }

    /// Gets a mutable reference to the events consumer.
    pub fn events(&mut self) -> &mut EventsConsumer {
        &mut self.consumer
    }

    /// Gets a reference to the memory limiter.
    pub fn memory_limiter(&self) -> &MemoryLimiter {
        &self.memory_limiter
    }

    /// Gets a reference to the health registry.
    pub fn health_registry(&mut self) -> &HealthRegistry {
        &self.health_registry
    }

    /// Gets a reference to the component registry.
    pub fn component_registry(&mut self) -> &ComponentRegistry {
        &self.component_registry
    }

    /// Gets a reference to the global thread pool.
    pub fn global_thread_pool(&self) -> &Handle {
        &self.thread_pool
    }
}
