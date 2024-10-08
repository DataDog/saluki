use std::sync::Arc;

use memory_accounting::{ComponentRegistry, MemoryLimiter};
use saluki_health::{Health, HealthRegistry};

use crate::{
    components::ComponentContext,
    pooling::ElasticObjectPool,
    topology::{
        interconnect::{FixedSizeEventBuffer, Forwarder},
        shutdown::ComponentShutdownHandle,
    },
};

struct SourceContextInner {
    component_context: ComponentContext,
    forwarder: Forwarder,
    event_buffer_pool: ElasticObjectPool<FixedSizeEventBuffer>,
    memory_limiter: MemoryLimiter,
    health_registry: HealthRegistry,
    component_registry: ComponentRegistry,
}

/// Source context.
pub struct SourceContext {
    shutdown_handle: Option<ComponentShutdownHandle>,
    health_handle: Option<Health>,
    inner: Arc<SourceContextInner>,
}

impl SourceContext {
    /// Creates a new `SourceContext`.
    pub fn new(
        component_context: ComponentContext, shutdown_handle: ComponentShutdownHandle, forwarder: Forwarder,
        event_buffer_pool: ElasticObjectPool<FixedSizeEventBuffer>, memory_limiter: MemoryLimiter,
        component_registry: ComponentRegistry, health_handle: Health, health_registry: HealthRegistry,
    ) -> Self {
        Self {
            shutdown_handle: Some(shutdown_handle),
            health_handle: Some(health_handle),
            inner: Arc::new(SourceContextInner {
                component_context,
                forwarder,
                event_buffer_pool,
                memory_limiter,
                health_registry,
                component_registry,
            }),
        }
    }

    /// Consumes the shutdown handle of this source context.
    ///
    /// ## Panics
    ///
    /// Panics if the shutdown handle has already been taken.
    pub fn take_shutdown_handle(&mut self) -> ComponentShutdownHandle {
        self.shutdown_handle.take().expect("shutdown handle already taken")
    }

    /// Consumes the health handle of this source context.
    ///
    /// ## Panics
    ///
    /// Panics if the health handle has already been taken.
    pub fn take_health_handle(&mut self) -> Health {
        self.health_handle.take().expect("health handle already taken")
    }

    /// Returns the component context.
    pub fn component_context(&self) -> ComponentContext {
        self.inner.component_context.clone()
    }

    /// Gets a reference to the forwarder.
    pub fn forwarder(&self) -> &Forwarder {
        &self.inner.forwarder
    }

    /// Gets a reference to the event buffer pool.
    pub fn event_buffer_pool(&self) -> &ElasticObjectPool<FixedSizeEventBuffer> {
        &self.inner.event_buffer_pool
    }

    /// Gets a reference to the memory limiter.
    pub fn memory_limiter(&self) -> &MemoryLimiter {
        &self.inner.memory_limiter
    }

    /// Gets a reference to the health registry.
    pub fn health_registry(&self) -> &HealthRegistry {
        &self.inner.health_registry
    }

    /// Gets a reference to the component registry.
    pub fn component_registry(&self) -> &ComponentRegistry {
        &self.inner.component_registry
    }
}

impl Clone for SourceContext {
    fn clone(&self) -> Self {
        Self {
            shutdown_handle: None,
            health_handle: None,
            inner: self.inner.clone(),
        }
    }
}
