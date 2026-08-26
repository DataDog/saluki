use std::sync::Arc;

use saluki_common::sync::shutdown::ShutdownHandle;

use crate::accounting::ComponentRegistry;
use crate::health::Health;
use crate::{
    components::{ComponentContext, ComponentSpawner},
    topology::{PayloadsDispatcher, TopologyContext},
};

struct RelayContextInner {
    topology_context: TopologyContext,
    component_context: ComponentContext,
    component_registry: ComponentRegistry,
    dispatcher: PayloadsDispatcher,
    spawner: ComponentSpawner,
}

/// Relay context.
pub struct RelayContext {
    shutdown_handle: Option<ShutdownHandle>,
    health_handle: Option<Health>,
    inner: Arc<RelayContextInner>,
}

impl RelayContext {
    /// Creates a new `RelayContext`.
    pub fn new(
        topology_context: &TopologyContext, component_context: &ComponentContext,
        component_registry: ComponentRegistry, health_handle: Health, dispatcher: PayloadsDispatcher,
        spawner: ComponentSpawner,
    ) -> Self {
        Self {
            shutdown_handle: None,
            health_handle: Some(health_handle),
            inner: Arc::new(RelayContextInner {
                topology_context: topology_context.clone(),
                component_context: component_context.clone(),
                component_registry,
                dispatcher,
                spawner,
            }),
        }
    }

    /// Installs the shutdown handle for this relay context.
    ///
    /// Called once by the runtime, before the component runs, with the shutdown signal of the
    /// component's dedicated supervisor.
    pub(crate) fn set_shutdown_handle(&mut self, shutdown_handle: ShutdownHandle) {
        self.shutdown_handle = Some(shutdown_handle);
    }

    /// Installs the shutdown handle for this relay context, for tests.
    ///
    /// The topology runtime does this itself before a relay runs, using the shutdown signal of the component's
    /// dedicated supervisor. A test that drives a relay through a real shutdown has to stand in for it.
    #[cfg(any(test, feature = "test-util"))]
    pub fn set_shutdown_handle_for_test(&mut self, shutdown_handle: ShutdownHandle) {
        self.set_shutdown_handle(shutdown_handle);
    }

    /// Consumes the shutdown handle of this relay context.
    ///
    /// # Panics
    ///
    /// Panics if the shutdown handle has already been taken.
    pub fn take_shutdown_handle(&mut self) -> ShutdownHandle {
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

    /// Returns a reference to the topology context.
    pub fn topology_context(&self) -> &TopologyContext {
        &self.inner.topology_context
    }

    /// Returns a reference to the component context.
    pub fn component_context(&self) -> &ComponentContext {
        &self.inner.component_context
    }

    /// Returns a reference to the component registry.
    pub fn component_registry(&self) -> &ComponentRegistry {
        &self.inner.component_registry
    }

    /// Returns a reference to the payloads dispatcher.
    pub fn dispatcher(&self) -> &PayloadsDispatcher {
        &self.inner.dispatcher
    }

    /// Returns a spawner for supervised child tasks belonging to this component.
    ///
    /// All child tasks spawned through this mechanism are tied to the lifecycle of the component itself, such that
    /// they're automatically shutdown/stopped when the component is stopped during topology shutdown, etc.
    pub fn spawner(&self) -> &ComponentSpawner {
        &self.inner.spawner
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
