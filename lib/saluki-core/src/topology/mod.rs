//! Topology building.

use std::num::NonZeroUsize;

use crate::data_model::payload::Payload;
use crate::support::SubsystemIdentifier;
use crate::topology::interconnect::FixedSizeEventBuffer;

mod blueprint;
pub use self::blueprint::{BlueprintError, TopologyBlueprint, TopologyReady};

mod built;

mod component_worker;

mod context;
pub use self::context::TopologyContext;

mod graph;

pub mod ids;
pub use self::ids::{
    ComponentId, ComponentOutputId, OutputDefinition, OutputName, TypedComponentId, TypedComponentOutputId,
};

pub mod interconnect;
use self::interconnect::{Consumer, Dispatcher, EventBufferManager};

#[cfg(test)]
pub(super) mod test_util;

// SAFETY: These are obviously non-zero.
const DEFAULT_EVENTS_BUFFER_CAPACITY: usize = 1024;
const DEFAULT_INTERCONNECT_CAPACITY: NonZeroUsize = NonZeroUsize::new(128).unwrap();

// Topology-wide defaults.

/// Default type for dispatching/consuming items from event-based components.
pub type EventsBuffer = FixedSizeEventBuffer<DEFAULT_EVENTS_BUFFER_CAPACITY>;

/// Default manager for event buffers.
pub type EventsBufferManager = EventBufferManager<DEFAULT_EVENTS_BUFFER_CAPACITY>;

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

pub(crate) fn topology_identifier(name: &str) -> SubsystemIdentifier {
    SubsystemIdentifier::from_segments(["topology", name])
}
