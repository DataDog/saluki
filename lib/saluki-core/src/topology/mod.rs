//! Topology building.

use std::num::NonZeroUsize;

use resource_accounting::ComponentRegistry;

use crate::data_model::payload::Payload;
use crate::topology::interconnect::FixedSizeEventBuffer;

mod blueprint;
pub use self::blueprint::{BlueprintError, TopologyBlueprint, TopologyReady};

mod built;

mod context;
pub use self::context::TopologyContext;

mod graph;

pub mod ids;
pub use self::ids::{
    ComponentId, ComponentOutputId, OutputDefinition, OutputName, TypedComponentId, TypedComponentOutputId,
};

pub mod interconnect;
use self::interconnect::{Consumer, Dispatcher};

mod running;

pub mod shutdown;

#[cfg(test)]
pub(super) mod test_util;

// SAFETY: These are obviously non-zero.
const DEFAULT_EVENTS_BUFFER_CAPACITY: usize = 1024;
const DEFAULT_INTERCONNECT_CAPACITY: NonZeroUsize = NonZeroUsize::new(128).unwrap();

// Topology-wide defaults.

/// Default type for dispatching/consuming items from event-based components.
pub type EventsBuffer = FixedSizeEventBuffer<DEFAULT_EVENTS_BUFFER_CAPACITY>;

/// Default dispatcher for event-based components.
pub type EventsDispatcher = Dispatcher<EventsBuffer>;

/// Default consumer for event-based components.
pub type EventsConsumer = Consumer<EventsBuffer>;

/// Default type for dispatching/consuming items from payload-based components.
pub type PayloadsBuffer = Payload;

/// Default dispatcher for payload-based components.
pub type PayloadsDispatcher = Dispatcher<PayloadsBuffer>;

/// Default consumer for payload-based components.
pub type PayloadsConsumer = Consumer<PayloadsBuffer>;

/// Returns the health registry component-name root for a topology with the given name.
///
/// Every component in a topology registers in the health registry under this dotted root (for example,
/// `topology.primary.sources.dsd_in`). Centralizing it here keeps component registration (when a topology is spawned)
/// and topology readiness waiting (`TopologyReady`) in sync.
fn health_component_root(name: &str) -> String {
    format!("topology.{}", name)
}

pub(super) struct RegisteredComponent<T> {
    component: T,
    component_registry: ComponentRegistry,
}

impl<T> RegisteredComponent<T> {
    fn new(component: T, component_registry: ComponentRegistry) -> Self {
        Self {
            component,
            component_registry,
        }
    }
    fn into_parts(self) -> (T, ComponentRegistry) {
        (self.component, self.component_registry)
    }
}
