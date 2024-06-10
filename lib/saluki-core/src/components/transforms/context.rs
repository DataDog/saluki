use crate::{
    components::ComponentContext,
    pooling::FixedSizeObjectPool,
    topology::interconnect::{EventBuffer, EventStream, Forwarder},
};

/// Transform context.
pub struct TransformContext {
    component_context: ComponentContext,
    forwarder: Forwarder,
    event_stream: EventStream,
    event_buffer_pool: FixedSizeObjectPool<EventBuffer>,
}

impl TransformContext {
    /// Creates a new `TransformContext`.
    pub fn new(
        component_context: ComponentContext, forwarder: Forwarder, event_stream: EventStream,
        event_buffer_pool: FixedSizeObjectPool<EventBuffer>,
    ) -> Self {
        Self {
            component_context,
            forwarder,
            event_stream,
            event_buffer_pool,
        }
    }

    /// Returns the component context.
    pub fn component_context(&self) -> ComponentContext {
        self.component_context.clone()
    }

    /// Gets a reference to the forwarder.
    pub fn forwarder(&self) -> &Forwarder {
        &self.forwarder
    }

    /// Gets a mutable reference to the event stream.
    pub fn event_stream(&mut self) -> &mut EventStream {
        &mut self.event_stream
    }

    /// Gets a reference to the event buffer pool.
    pub fn event_buffer_pool(&self) -> &FixedSizeObjectPool<EventBuffer> {
        &self.event_buffer_pool
    }
}
