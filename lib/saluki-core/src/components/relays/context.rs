use std::sync::Arc;

use memory_accounting::ComponentRegistry;
use saluki_health::Health;

use crate::{
    components::ComponentContext,
    topology::{shutdown::ComponentShutdownHandle, PayloadsDispatcher, TopologyContext},
};

struct RelayContextInner {
    topology_context: TopologyContext,
    component_context: ComponentContext,
    component_registry: ComponentRegistry,
    dispatcher: PayloadsDispatcher,
}

/// Relay context.
pub struct RelayContext {
    shutdown_handle: Option<ComponentShutdownHandle>,
    health_handle: Option<Health>,
    inner: Arc<RelayContextInner>,
}

impl RelayContext {
    /// Creates a new `RelayContext`.
    pub fn new(
        topology_context: &TopologyContext, component_context: &ComponentContext,
        component_registry: ComponentRegistry, shutdown_handle: ComponentShutdownHandle, health_handle: Health,
        dispatcher: PayloadsDispatcher,
    ) -> Self {
        Self {
            shutdown_handle: Some(shutdown_handle),
            health_handle: Some(health_handle),
            inner: Arc::new(RelayContextInner {
                topology_context: topology_context.clone(),
                component_context: component_context.clone(),
                component_registry,
                dispatcher,
            }),
        }
    }

    /// Consumes the shutdown handle of this relay context.
    ///
    /// # Panics
    ///
    /// Panics if the shutdown handle has already been taken.
    pub fn take_shutdown_handle(&mut self) -> ComponentShutdownHandle {
        self.shutdown_handle.take().expect("shutdown handle already taken")
    }

    /// Consumes the health handle of this relay context.
    ///
    /// # Panics
    ///
    /// Panics if the health handle has already been taken.
    pub fn take_health_handle(&mut self) -> Health {
        self.health_handle.take().expect("health handle already taken")
    }

    /// Gets a reference to the topology context.
    pub fn topology_context(&self) -> &TopologyContext {
        &self.inner.topology_context
    }

    /// Gets a reference to the component context.
    pub fn component_context(&self) -> &ComponentContext {
        &self.inner.component_context
    }

    /// Gets a reference to the component registry.
    pub fn component_registry(&self) -> &ComponentRegistry {
        &self.inner.component_registry
    }

    /// Gets a reference to the payloads dispatcher.
    pub fn dispatcher(&self) -> &PayloadsDispatcher {
        &self.inner.dispatcher
    }
}

impl Clone for RelayContext {
    fn clone(&self) -> Self {
        Self {
            shutdown_handle: None,
            health_handle: None,
            inner: self.inner.clone(),
        }
    }
}
