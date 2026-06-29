use resource_accounting::ComponentRegistry;

use crate::health::Health;
use crate::runtime::SupervisorHandle;
use crate::{
    components::ComponentContext,
    topology::{EventsConsumer, EventsDispatcher, TopologyContext},
};

/// Transform context.
pub struct TransformContext {
    topology_context: TopologyContext,
    component_context: ComponentContext,
    component_registry: ComponentRegistry,
    health_handle: Option<Health>,
    dispatcher: EventsDispatcher,
    consumer: EventsConsumer,
    supervisor_handle: SupervisorHandle,
}

impl TransformContext {
    /// Creates a new `TransformContext`.
    pub fn new(
        topology_context: &TopologyContext, component_context: &ComponentContext,
        component_registry: ComponentRegistry, health_handle: Health, dispatcher: EventsDispatcher,
        consumer: EventsConsumer, supervisor_handle: SupervisorHandle,
    ) -> Self {
        Self {
            topology_context: topology_context.clone(),
            component_context: component_context.clone(),
            component_registry,
            health_handle: Some(health_handle),
            dispatcher,
            consumer,
            supervisor_handle,
        }
    }

    /// Consumes the health handle of this transform context.
    ///
    /// # Panics
    ///
    /// Panics if the health handle has already been taken.
    pub fn take_health_handle(&mut self) -> Health {
        self.health_handle.take().expect("health handle already taken")
    }

    /// Gets a reference to the topology context.
    pub fn topology_context(&self) -> &TopologyContext {
        &self.topology_context
    }

    /// Gets a reference to the component context.
    pub fn component_context(&self) -> &ComponentContext {
        &self.component_context
    }

    /// Gets a reference to the events dispatcher.
    pub fn dispatcher(&self) -> &EventsDispatcher {
        &self.dispatcher
    }

    /// Gets a mutable reference to the events consumer.
    pub fn events(&mut self) -> &mut EventsConsumer {
        &mut self.consumer
    }

    /// Gets a mutable reference to the component registry.
    pub fn component_registry(&self) -> &ComponentRegistry {
        &self.component_registry
    }

    /// Returns a handle for spawning dynamic children under this component's dedicated supervisor.
    ///
    /// Spawned children are temporary -- they are never restarted, and they are torn down when the
    /// component (and thus its supervisor) stops -- which suits structured, on-demand background work.
    ///
    /// > **Note:** a child's name becomes a process name and a resource-group identifier. Child names
    /// > **MUST** be bounded and low-cardinality; never embed per-request or per-peer values, as doing
    /// > so leaks unbounded process/metric identity.
    pub fn spawn_handle(&self) -> &SupervisorHandle {
        &self.supervisor_handle
    }
}
