use std::sync::Arc;

use crate::accounting::ComponentRegistry;
use crate::health::Health;
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
    dispatcher: Arc<EventsDispatcher>,

    // One consumer per input lane, in lane order, until the transform takes them as lanes.
    consumers: Vec<EventsConsumer>,
    lanes_taken: bool,
}

impl TransformContext {
    /// Creates a new `TransformContext` for a transform with one input lane, read by `consumer`.
    pub fn new(
        topology_context: &TopologyContext, component_context: &ComponentContext,
        component_registry: ComponentRegistry, health_handle: Health, dispatcher: EventsDispatcher,
        consumer: EventsConsumer,
    ) -> Self {
        Self::with_lane_inputs(
            topology_context,
            component_context,
            component_registry,
            health_handle,
            dispatcher,
            vec![consumer],
        )
    }

    /// Creates a new `TransformContext` for a transform with one input lane per consumer in `consumers`, in lane order:
    /// lane `i` reads the `i`-th (see [`take_lane_inputs`][Self::take_lane_inputs]).
    pub fn with_lane_inputs(
        topology_context: &TopologyContext, component_context: &ComponentContext,
        component_registry: ComponentRegistry, health_handle: Health, dispatcher: EventsDispatcher,
        consumers: Vec<EventsConsumer>,
    ) -> Self {
        Self {
            topology_context: topology_context.clone(),
            component_context: component_context.clone(),
            component_registry,
            health_handle: Some(health_handle),
            dispatcher: Arc::new(dispatcher),
            consumers,
            lanes_taken: false,
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

    /// Returns a reference to the topology context.
    pub fn topology_context(&self) -> &TopologyContext {
        &self.topology_context
    }

    /// Returns a reference to the component context.
    pub fn component_context(&self) -> &ComponentContext {
        &self.component_context
    }

    /// Returns a reference to the events dispatcher.
    pub fn dispatcher(&self) -> &EventsDispatcher {
        &self.dispatcher
    }

    /// Returns the events dispatcher, for tasks of the transform that send on its outputs beside it, such as the tasks
    /// that run its lanes.
    ///
    /// Every task sends through the one dispatcher of the transform. Each lane of a downstream component receives the
    /// sends of one task in the order that task made them.
    pub fn shared_dispatcher(&self) -> Arc<EventsDispatcher> {
        Arc::clone(&self.dispatcher)
    }

    /// Returns a mutable reference to the events consumer of a transform with one input lane.
    ///
    /// # Panics
    ///
    /// Panics if the transform has more than one input lane, which only
    /// [`take_lane_inputs`][Self::take_lane_inputs] can read, or if it took its inputs as lanes.
    pub fn events(&mut self) -> &mut EventsConsumer {
        assert!(!self.lanes_taken, "the transform's inputs were taken as lanes");
        match self.consumers.as_mut_slice() {
            [consumer] => consumer,
            consumers => panic!(
                "the transform has {} input lanes, which it must take with `take_lane_inputs`",
                consumers.len()
            ),
        }
    }

    /// Takes the transform's input consumers, one per input lane, in lane order: lane `i` reads the `i`-th.
    ///
    /// A transform with lanes ([`TransformBuilder::input_lanes`][super::TransformBuilder::input_lanes]) hands each
    /// consumer to the task that runs its lane. A transform with one lane gets its one consumer. Afterwards,
    /// [`events`][Self::events] has no consumer to return, and taking the inputs again returns none.
    pub fn take_lane_inputs(&mut self) -> Vec<EventsConsumer> {
        self.lanes_taken = true;
        std::mem::take(&mut self.consumers)
    }

    /// Returns a mutable reference to the component registry.
    pub fn component_registry(&self) -> &ComponentRegistry {
        &self.component_registry
    }
}

#[cfg(test)]
mod tests {
    use tokio::sync::mpsc;

    use super::*;
    use crate::accounting::MemoryLimiter;
    use crate::health::HealthRegistry;
    use crate::runtime::state::DataspaceRegistry;
    use crate::topology::EventsBuffer;

    /// Returns the context of a transform with `lanes` input lanes, and the senders to them, in lane order.
    fn context_with_lanes(lanes: usize) -> (TransformContext, Vec<mpsc::Sender<EventsBuffer>>) {
        let component_context = ComponentContext::test_transform("laned");
        let health_registry = HealthRegistry::new();
        let health = health_registry
            .register_component(&component_context.identity())
            .expect("the component registers once");
        let topology_context = TopologyContext::new(
            Arc::from("test"),
            MemoryLimiter::noop(),
            health_registry,
            tokio::runtime::Handle::current(),
            DataspaceRegistry::new(),
        );
        let (senders, consumers) = (0..lanes)
            .map(|_| {
                let (sender, receiver) = mpsc::channel(1);
                (sender, EventsConsumer::new(component_context.clone(), receiver))
            })
            .unzip();
        let context = TransformContext::with_lane_inputs(
            &topology_context,
            &component_context,
            ComponentRegistry::default(),
            health,
            EventsDispatcher::new(component_context.clone()),
            consumers,
        );
        (context, senders)
    }

    #[tokio::test]
    async fn a_transform_takes_one_consumer_per_lane_in_lane_order() {
        // Lane 0 is sent nothing, and lanes 1 and 2 one buffer each.
        let (mut context, senders) = context_with_lanes(3);
        for sender in &senders[1..] {
            sender.send(EventsBuffer::default()).await.unwrap();
        }
        let mut lanes = context.take_lane_inputs();
        let queued = |lanes: &[EventsConsumer]| lanes.iter().map(EventsConsumer::queued).collect::<Vec<_>>();
        assert_eq!(queued(&lanes), vec![0, 1, 1]);
        assert!(lanes[2].next().await.is_some());
        assert_eq!(queued(&lanes), vec![0, 1, 0]);
        assert!(context.take_lane_inputs().is_empty(), "the inputs are taken once");

        // Every task of the transform sends through its one dispatcher.
        let shared = context.shared_dispatcher();
        assert!(std::ptr::eq(shared.as_ref(), context.dispatcher()));
    }

    #[tokio::test]
    async fn a_transform_with_one_lane_reads_it_as_its_events() {
        let (mut context, senders) = context_with_lanes(1);
        senders[0].send(EventsBuffer::default()).await.unwrap();
        assert!(context.events().next().await.is_some());
    }

    #[tokio::test]
    #[should_panic(expected = "the transform has 2 input lanes, which it must take with `take_lane_inputs`")]
    async fn a_transform_with_lanes_cannot_read_them_as_one_input() {
        let (mut context, _senders) = context_with_lanes(2);
        let _ = context.events();
    }

    #[tokio::test]
    #[should_panic(expected = "the transform's inputs were taken as lanes")]
    async fn a_transform_that_took_its_lanes_has_no_events_left() {
        let (mut context, _senders) = context_with_lanes(1);
        let _lanes = context.take_lane_inputs();
        let _ = context.events();
    }
}
