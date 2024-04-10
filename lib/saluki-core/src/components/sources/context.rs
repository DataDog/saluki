use std::sync::Arc;

use crate::{
    buffers::FixedSizeBufferPool,
    components::ComponentContext,
    topology::{
        interconnect::{EventBuffer, Forwarder},
        shutdown::ComponentShutdownHandle,
    },
};

struct SourceContextInner {
    component_context: ComponentContext,
    forwarder: Forwarder,
    event_buffer_pool: FixedSizeBufferPool<EventBuffer>,
}

pub struct SourceContext {
    shutdown_handle: Option<ComponentShutdownHandle>,
    inner: Arc<SourceContextInner>,
}

impl SourceContext {
    pub fn new(
        component_context: ComponentContext, shutdown_handle: ComponentShutdownHandle, forwarder: Forwarder,
        event_buffer_pool: FixedSizeBufferPool<EventBuffer>,
    ) -> Self {
        Self {
            shutdown_handle: Some(shutdown_handle),
            inner: Arc::new(SourceContextInner {
                component_context,
                forwarder,
                event_buffer_pool,
            }),
        }
    }

    pub fn take_shutdown_handle(&mut self) -> Option<ComponentShutdownHandle> {
        self.shutdown_handle.take()
    }

    #[allow(unused)]
    pub fn component_context(&self) -> ComponentContext {
        self.inner.component_context.clone()
    }

    pub fn forwarder(&self) -> &Forwarder {
        &self.inner.forwarder
    }

    pub fn event_buffer_pool(&self) -> &FixedSizeBufferPool<EventBuffer> {
        &self.inner.event_buffer_pool
    }
}

impl Clone for SourceContext {
    fn clone(&self) -> Self {
        Self {
            shutdown_handle: None,
            inner: self.inner.clone(),
        }
    }
}
