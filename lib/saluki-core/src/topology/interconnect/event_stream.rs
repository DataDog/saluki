use std::{future::poll_fn, num::NonZeroUsize};

use metrics::{Counter, Histogram};
use tokio::sync::mpsc;

use crate::components::{metrics::MetricsBuilder, ComponentContext};

use super::EventBuffer;

// Since we're dealing with event _buffers_, this becomes a multiplicative factor, so we might be receiving 128 (or
// whatever the number is) event buffers of 128 events each. This is good for batching/efficiency but we don't want
// wildly large batches, so this number is sized conservatively for now.
const NEXT_READY_RECV_LIMIT: NonZeroUsize = unsafe { NonZeroUsize::new_unchecked(128) };

pub struct EventStream {
    inner: mpsc::Receiver<EventBuffer>,
    events_received: Counter,
    events_received_size: Histogram,
}

impl EventStream {
    pub fn new(context: ComponentContext, inner: mpsc::Receiver<EventBuffer>) -> Self {
        let metrics_builder = MetricsBuilder::from_component_context(context);

        Self {
            inner,
            events_received: metrics_builder.register_counter("component_events_received_total"),
            events_received_size: metrics_builder.register_histogram("component_events_received_size"),
        }
    }

    pub async fn next(&mut self) -> Option<EventBuffer> {
        match self.inner.recv().await {
            Some(buffer) => {
                self.events_received.increment(buffer.len() as u64);
                self.events_received_size.record(buffer.len() as f64);
                Some(buffer)
            }
            None => None,
        }
    }

    pub async fn next_ready(&mut self) -> Option<Vec<EventBuffer>> {
        let mut buffers = Vec::new();
        poll_fn(|cx| self.inner.poll_recv_many(cx, &mut buffers, NEXT_READY_RECV_LIMIT.get())).await;

        if buffers.is_empty() {
            None
        } else {
            let mut total_events_received = 0;
            for buffer in &buffers {
                total_events_received += buffer.len() as u64;
                self.events_received_size.record(buffer.len() as f64);
            }
            self.events_received.increment(total_events_received);

            Some(buffers)
        }
    }
}
