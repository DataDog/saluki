//! Topology building.

use std::num::NonZeroUsize;

use crate::data_model::event::{Event, EventKind, EventType};
use crate::data_model::payload::Payload;
use crate::support::SubsystemIdentifier;
use crate::topology::interconnect::{DispatchBuffer, FixedSizeEventBuffer, FromInterconnect, TypedEventBuffer};

mod blueprint;
pub use self::blueprint::{BlueprintError, TopologyBlueprint, TopologyReady};

mod built;

pub(crate) mod condition;
pub use self::condition::EdgeCondition;

pub(crate) mod component_worker;

mod context;
pub use self::context::TopologyContext;

mod graph;

pub mod ids;
pub use self::ids::{
    ComponentId, ComponentOutputId, OutputDefinition, OutputName, TypedComponentId, TypedComponentOutputId,
};

pub mod interconnect;
use self::interconnect::{Consumer, Dispatcher};

#[cfg(test)]
pub(super) mod test_util;

// SAFETY: These are obviously non-zero.
const DEFAULT_EVENTS_BUFFER_CAPACITY: usize = 1024;
pub(crate) const DEFAULT_INTERCONNECT_CAPACITY: NonZeroUsize = NonZeroUsize::new(128).unwrap();

// Topology-wide defaults.

/// Default type for dispatching/consuming items from event-based components.
pub type EventsBuffer = FixedSizeEventBuffer<DEFAULT_EVENTS_BUFFER_CAPACITY>;

/// Default dispatcher for event-based components.
pub type EventsDispatcher = Dispatcher<EventsBuffer>;

/// Default consumer for event-based components.
pub type EventsConsumer = Consumer<EventsBuffer>;

/// Buffer of events of a single type, for event-based components that consume or dispatch that type.
pub type TypedEventsBuffer<T> = TypedEventBuffer<T, DEFAULT_EVENTS_BUFFER_CAPACITY>;

/// An item that event-based components consume or dispatch.
///
/// Event interconnects always carry [`EventsBuffer`]. A component that works with [`Event`] consumes and dispatches
/// `EventsBuffer` as is. A component that works with a single event type, such as [`Metric`][crate::data_model::event::metric::Metric],
/// consumes and dispatches a [`TypedEventsBuffer`] of that type instead, which converts to and from `EventsBuffer`
/// without touching the events.
pub trait BufferItem: Send + 'static {
    /// Buffer that holds items of this type.
    type Buffer: DispatchBuffer<Item = Self> + FromInterconnect<EventsBuffer> + Into<EventsBuffer> + Send;

    /// Event types a buffer of this item can hold.
    const EVENT_TYPES: EventType;
}

impl BufferItem for Event {
    type Buffer = EventsBuffer;

    const EVENT_TYPES: EventType = EventType::all_bits();
}

impl<T> BufferItem for T
where
    T: EventKind + Clone + Send,
{
    type Buffer = TypedEventsBuffer<T>;

    const EVENT_TYPES: EventType = T::EVENT_TYPE;
}

/// Dispatcher for event-based components that dispatch items of type `T`.
pub type EventsDispatcherFor<T> = Dispatcher<<T as BufferItem>::Buffer, EventsBuffer>;

/// Consumer for event-based components that consume items of type `T`.
pub type EventsConsumerFor<T> = Consumer<<T as BufferItem>::Buffer, EventsBuffer>;

/// Default type for dispatching/consuming items from payload-based components.
pub type PayloadsBuffer = Payload;

/// Default dispatcher for payload-based components.
pub type PayloadsDispatcher = Dispatcher<PayloadsBuffer>;

/// Default consumer for payload-based components.
pub type PayloadsConsumer = Consumer<PayloadsBuffer>;

pub(crate) fn topology_identifier(name: &str) -> SubsystemIdentifier {
    SubsystemIdentifier::from_segments(["topology", name])
}
