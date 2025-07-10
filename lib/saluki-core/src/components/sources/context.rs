use std::{marker::PhantomData, sync::Arc};

use memory_accounting::{ComponentRegistry, MemoryLimiter};
use saluki_health::{Health, HealthRegistry};
use tokio::runtime::Handle;

use crate::{
    components::ComponentContext,
    topology::{interconnect::Dispatcher, shutdown::ComponentShutdownHandle, TopologyConfiguration},
};

struct SourceContextInner<T>
where
    T: TopologyConfiguration,
{
    component_context: ComponentContext,
    dispatcher: Dispatcher<T::Events>,
    memory_limiter: MemoryLimiter,
    health_registry: HealthRegistry,
    component_registry: ComponentRegistry,
    thread_pool: Handle,
}

/// Source context.
pub struct SourceContext<T>
where
    T: TopologyConfiguration,
{
    shutdown_handle: Option<ComponentShutdownHandle>,
    health_handle: Option<Health>,
    inner: Arc<SourceContextInner<T>>,
    _config: PhantomData<T>,
}

impl<T> SourceContext<T>
where
    T: TopologyConfiguration,
{
    /// Creates a new `SourceContext`.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        component_context: ComponentContext, shutdown_handle: ComponentShutdownHandle,
        dispatcher: Dispatcher<T::Events>, memory_limiter: MemoryLimiter, component_registry: ComponentRegistry,
        health_handle: Health, health_registry: HealthRegistry, thread_pool: Handle,
    ) -> Self {
        Self {
            shutdown_handle: Some(shutdown_handle),
            health_handle: Some(health_handle),
            inner: Arc::new(SourceContextInner {
                component_context,
                dispatcher,
                memory_limiter,
                health_registry,
                component_registry,
                thread_pool,
            }),
            _config: PhantomData,
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

    /// Gets a reference to the dispatcher.
    pub fn dispatcher(&self) -> &Dispatcher<T::Events> {
        &self.inner.dispatcher
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

    /// Gets a reference to the global thread pool.
    pub fn global_thread_pool(&self) -> &Handle {
        &self.inner.thread_pool
    }
}

impl<T> Clone for SourceContext<T>
where
    T: TopologyConfiguration,
{
    fn clone(&self) -> Self {
        Self {
            shutdown_handle: None,
            health_handle: None,
            inner: self.inner.clone(),
            _config: PhantomData,
        }
    }
}
