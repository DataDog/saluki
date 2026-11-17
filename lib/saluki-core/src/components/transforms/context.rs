use crate::accounting::ComponentRegistry;
use crate::data_model::event::Event;
use crate::health::Health;
use crate::{
    components::ComponentContext,
    topology::{BufferItem, EventsConsumer, EventsConsumerFor, EventsDispatcher, EventsDispatcherFor, TopologyContext},
};

/// Transform context.
///
/// `I` is the type of item the transform consumes, and `O` is the type of item it dispatches. Both default to [`Event`],
/// in which case the transform consumes and dispatches [`EventsBuffer`][crate::topology::EventsBuffer]s holding any
/// event type. A transform that works with a single event type, such as `TransformContext<Trace, TraceStats>`, consumes
/// and dispatches [`TypedEventsBuffer`][crate::topology::TypedEventsBuffer]s of that type instead.
pub struct TransformContext<I: BufferItem = Event, O: BufferItem = Event> {
    topology_context: TopologyContext,
    component_context: ComponentContext,
    component_registry: ComponentRegistry,
    health_handle: Option<Health>,
    dispatcher: EventsDispatcherFor<O>,
    consumer: EventsConsumerFor<I>,
}

impl TransformContext {
    /// Creates a new `TransformContext`.
    pub fn new(
        topology_context: &TopologyContext, component_context: &ComponentContext,
        component_registry: ComponentRegistry, health_handle: Health, dispatcher: EventsDispatcher,
        consumer: EventsConsumer,
    ) -> Self {
        Self {
            topology_context: topology_context.clone(),
            component_context: component_context.clone(),
            component_registry,
            health_handle: Some(health_handle),
            dispatcher,
            consumer,
        }
    }

    /// Converts this context into one for a transform that consumes items of type `I` and dispatches items of type `O`.
    ///
    /// The interconnects are unchanged: only the type of item that the consumer hands to the transform, and that the
    /// dispatcher accepts from it, changes.
    pub fn into_typed<I: BufferItem, O: BufferItem>(self) -> TransformContext<I, O> {
        TransformContext {
            topology_context: self.topology_context,
            component_context: self.component_context,
            component_registry: self.component_registry,
            health_handle: self.health_handle,
            dispatcher: self.dispatcher.with_item_type(),
            consumer: self.consumer.with_item_type(),
        }
    }
}

impl<I: BufferItem, O: BufferItem> TransformContext<I, O> {
    /// Consumes the health handle of this transform context.
    ///
    /// # Panics
    ///
    /// Panics if the health handle has already been taken.
    pub fn take_health_handle(&mut self) -> Health {
        self.health_handle.take().expect("health handle already taken")
    }

    /// Returns a reference to the topology context.
    pub fn topology_context(&self) -> &TopologyContext {
        &self.topology_context
    }

    /// Returns a reference to the component context.
    pub fn component_context(&self) -> &ComponentContext {
        &self.component_context
    }

    /// Returns a reference to the events dispatcher.
    pub fn dispatcher(&self) -> &EventsDispatcherFor<O> {
        &self.dispatcher
    }

    /// Returns a mutable reference to the events consumer.
    pub fn events(&mut self) -> &mut EventsConsumerFor<I> {
        &mut self.consumer
    }

    /// Returns a mutable reference to the component registry.
    pub fn component_registry(&self) -> &ComponentRegistry {
        &self.component_registry
    }
}
