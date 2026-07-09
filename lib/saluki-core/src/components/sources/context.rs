use std::sync::Arc;

use saluki_common::sync::shutdown::ShutdownHandle;

use crate::accounting::ComponentRegistry;
use crate::health::Health;
use crate::runtime::SupervisorHandle;
use crate::{
    components::ComponentContext,
    topology::{EventsDispatcher, TopologyContext},
};

struct SourceContextInner {
    topology_context: TopologyContext,
    component_context: ComponentContext,
    component_registry: ComponentRegistry,
    dispatcher: EventsDispatcher,
    supervisor_handle: SupervisorHandle,
}

/// Source context.
pub struct SourceContext {
    shutdown_handle: Option<ShutdownHandle>,
    health_handle: Option<Health>,
    inner: Arc<SourceContextInner>,
}

impl SourceContext {
    /// Creates a new `SourceContext`.
    pub fn new(
        topology_context: &TopologyContext, component_context: &ComponentContext,
        component_registry: ComponentRegistry, health_handle: Health, dispatcher: EventsDispatcher,
        supervisor_handle: SupervisorHandle,
    ) -> Self {
        Self {
            shutdown_handle: None,
            health_handle: Some(health_handle),
            inner: Arc::new(SourceContextInner {
                topology_context: topology_context.clone(),
                component_context: component_context.clone(),
                component_registry,
                dispatcher,
                supervisor_handle,
            }),
        }
    }

    /// Installs the shutdown handle for this source context.
    ///
    /// Called once by the runtime, before the component runs, with the shutdown signal of the
    /// component's dedicated supervisor.
    pub(crate) fn set_shutdown_handle(&mut self, shutdown_handle: ShutdownHandle) {
        self.shutdown_handle = Some(shutdown_handle);
    }

    /// Consumes the shutdown handle of this source context.
    ///
    /// # Panics
    ///
    /// Panics if the shutdown handle has already been taken.
    pub fn take_shutdown_handle(&mut self) -> ShutdownHandle {
        self.shutdown_handle.take().expect("shutdown handle already taken")
    }

    /// Consumes the health handle of this source context.
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

    /// Gets a reference to the events dispatcher.
    pub fn dispatcher(&self) -> &EventsDispatcher {
        &self.inner.dispatcher
    }

    /// Returns a handle to the supervisor that this component is spawned on.
    ///
    /// Dynamic child processes can be spawned via the supervisor handle and thus have their lifecycle
    /// coupled to the component itself: if the component restarts, or the component's supervisor dies,
    /// the dynamic child processes will also be terminated automatically as well.
    pub fn spawn_handle(&self) -> &SupervisorHandle {
        &self.inner.supervisor_handle
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
