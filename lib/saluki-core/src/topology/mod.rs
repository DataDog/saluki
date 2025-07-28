//! Topology building.

use std::num::NonZeroUsize;

use memory_accounting::ComponentRegistry;

use crate::data_model::payload::Payload;
use crate::topology::interconnect::FixedSizeEventBuffer;

mod blueprint;
pub use self::blueprint::{BlueprintError, TopologyBlueprint};

mod built;
pub use self::built::BuiltTopology;

mod context;
pub use self::context::TopologyContext;

mod graph;

mod ids;
pub use self::ids::*;

pub mod interconnect;
use self::interconnect::{Consumer, Dispatcher};

mod running;
pub use self::running::RunningTopology;

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
