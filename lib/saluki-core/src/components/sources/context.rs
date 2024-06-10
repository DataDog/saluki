use std::sync::Arc;

use crate::{
    components::ComponentContext,
    pooling::FixedSizeObjectPool,
    topology::{
        interconnect::{EventBuffer, Forwarder},
        shutdown::ComponentShutdownHandle,
    },
};

struct SourceContextInner {
    component_context: ComponentContext,
    forwarder: Forwarder,
    event_buffer_pool: FixedSizeObjectPool<EventBuffer>,
}

/// Source context.
pub struct SourceContext {
    shutdown_handle: Option<ComponentShutdownHandle>,
    inner: Arc<SourceContextInner>,
}

impl SourceContext {
    /// Creates a new `SourceContext`.
    pub fn new(
        component_context: ComponentContext, shutdown_handle: ComponentShutdownHandle, forwarder: Forwarder,
        event_buffer_pool: FixedSizeObjectPool<EventBuffer>,
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

    /// Consumes the shutdown handle of this source context.
    ///
    /// If the shutdown handle has already been taken, `None` is returned.
    pub fn take_shutdown_handle(&mut self) -> Option<ComponentShutdownHandle> {
        self.shutdown_handle.take()
    }

    /// Returns the component context.
    pub fn component_context(&self) -> ComponentContext {
        self.inner.component_context.clone()
    }

    /// Gets a reference to the forwarder.
    pub fn forwarder(&self) -> &Forwarder {
        &self.inner.forwarder
    }

    /// Gets a reference to the event buffer pool.
    pub fn event_buffer_pool(&self) -> &FixedSizeObjectPool<EventBuffer> {
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
