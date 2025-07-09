use metrics::{Counter, Histogram};
use saluki_metrics::MetricsBuilder;
use tokio::sync::mpsc;

use super::FixedSizeEventBuffer;
use crate::{components::ComponentContext, observability::ComponentMetricsExt as _};

/// A stream of events sent to a component.
///
/// For transforms and destinations, their events can only come from other components that have forwarded them onwards.
/// `EventStream` is the receiving end of the interconnection between two components, where
/// [`Dispatcher`][crate::topology::interconnect::Dispatcher] is the sending end.
pub struct EventStream {
    inner: mpsc::Receiver<FixedSizeEventBuffer<1024>>,
    events_received: Counter,
    events_received_size: Histogram,
}

impl EventStream {
    /// Create a new `EventStream` for the given component context and inner receiver.
    pub fn new(context: ComponentContext, inner: mpsc::Receiver<FixedSizeEventBuffer<1024>>) -> Self {
        let metrics_builder = MetricsBuilder::from_component_context(&context);

        Self {
            inner,
            events_received: metrics_builder.register_debug_counter("component_events_received_total"),
            events_received_size: metrics_builder.register_debug_histogram("component_events_received_size"),
        }
    }

    /// Gets the next event buffer in the stream.
    ///
    /// If the component (or components) connected to this event stream have stopped, `None` is returned.
    pub async fn next(&mut self) -> Option<FixedSizeEventBuffer<1024>> {
        match self.inner.recv().await {
            Some(buffer) => {
                self.events_received.increment(buffer.len() as u64);
                self.events_received_size.record(buffer.len() as f64);
                Some(buffer)
            }
            None => None,
        }
    }
}

#[cfg(test)]
mod tests {
    // TODO: Tests asserting we emit metrics, and the right metrics.

    use super::*;
    use crate::topology::ComponentId;

    fn create_event_stream(channel_size: usize) -> (EventStream, mpsc::Sender<FixedSizeEventBuffer<1024>>) {
        let component_context = ComponentId::try_from("event_stream_test")
            .map(ComponentContext::source)
            .expect("component ID should never be invalid");

        let (tx, rx) = mpsc::channel(channel_size);
        let event_stream = EventStream::new(component_context, rx);

        (event_stream, tx)
    }

    #[tokio::test]
    async fn next() {
        let (mut event_stream, tx) = create_event_stream(1);

        // Send an event buffer, and make sure we can receive it:
        let ebuf = FixedSizeEventBuffer::<1024>::default();
        assert!(ebuf.is_empty());

        tx.send(ebuf).await.expect("should not fail to send event buffer");

        let received_ebuf = event_stream.next().await.expect("should receive event buffer");
        assert!(received_ebuf.is_empty());

        // Now drop the sender, which should close the event stream:
        drop(tx);

        assert!(event_stream.next().await.is_none());
    }
}
