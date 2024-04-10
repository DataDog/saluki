use crate::{
    buffers::FixedSizeBufferPool,
    components::ComponentContext,
    topology::interconnect::{EventBuffer, EventStream, Forwarder},
};

pub struct TransformContext {
    component_context: ComponentContext,
    forwarder: Forwarder,
    event_stream: EventStream,
    event_buffer_pool: FixedSizeBufferPool<EventBuffer>,
}

impl TransformContext {
    pub fn new(
        component_context: ComponentContext, forwarder: Forwarder, event_stream: EventStream,
        event_buffer_pool: FixedSizeBufferPool<EventBuffer>,
    ) -> Self {
        Self {
            component_context,
            forwarder,
            event_stream,
            event_buffer_pool,
        }
    }

    pub fn component_context(&self) -> ComponentContext {
        self.component_context.clone()
    }

    pub fn forwarder(&self) -> &Forwarder {
        &self.forwarder
    }

    pub fn event_stream(&mut self) -> &mut EventStream {
        &mut self.event_stream
    }

    #[allow(unused)]
    pub fn event_buffer_pool(&self) -> &FixedSizeBufferPool<EventBuffer> {
        &self.event_buffer_pool
    }
}
