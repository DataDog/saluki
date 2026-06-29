use std::sync::Arc;

use resource_accounting::ComponentRegistry;
use saluki_common::sync::shutdown::ShutdownHandle;

use crate::health::Health;
use crate::runtime::SupervisorHandle;
use crate::{
    components::ComponentContext,
    topology::{PayloadsDispatcher, TopologyContext},
};

struct RelayContextInner {
    topology_context: TopologyContext,
    component_context: ComponentContext,
    component_registry: ComponentRegistry,
    dispatcher: PayloadsDispatcher,
    supervisor_handle: SupervisorHandle,
}

/// Relay context.
pub struct RelayContext {
    shutdown_handle: Option<ShutdownHandle>,
    health_handle: Option<Health>,
    inner: Arc<RelayContextInner>,
}

impl RelayContext {
    /// Creates a new `RelayContext`.
    ///
    /// The context is created without a shutdown handle; the runtime installs it via
    /// `set_shutdown_handle` immediately before the component runs.
    pub fn new(
        topology_context: &TopologyContext, component_context: &ComponentContext,
        component_registry: ComponentRegistry, health_handle: Health, dispatcher: PayloadsDispatcher,
        supervisor_handle: SupervisorHandle,
    ) -> Self {
        Self {
            shutdown_handle: None,
            health_handle: Some(health_handle),
            inner: Arc::new(RelayContextInner {
                topology_context: topology_context.clone(),
                component_context: component_context.clone(),
                component_registry,
                dispatcher,
                supervisor_handle,
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

    /// Returns a handle for spawning dynamic children under this component's dedicated supervisor.
    ///
    /// Spawned children are temporary -- they are never restarted, and they are torn down when the
    /// component (and thus its supervisor) stops -- which suits structured, on-demand work such as one
    /// task per network connection.
    ///
    /// > **Note:** a child's name becomes a process name and a resource-group identifier. Child names
    /// > **MUST** be bounded and low-cardinality; never embed per-request or per-peer values (such as a
    /// > remote address), as doing so leaks unbounded process/metric identity.
    pub fn spawn_handle(&self) -> &SupervisorHandle {
        &self.inner.supervisor_handle
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
