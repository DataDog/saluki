use metrics::{Counter, Histogram};
use saluki_metrics::MetricsBuilder;
use tokio::sync::mpsc;

use crate::{components::ComponentContext, observability::ComponentMetricsExt as _};

use super::Dispatchable;

/// A stream of events sent to a component.
///
/// This represents the receiving end of a component interconnect, where the sending end is [`Dispatcher<T>`].
pub struct EventStream<T> {
    inner: mpsc::Receiver<T>,
    events_received: Counter,
    events_received_size: Histogram,
}

impl<T> EventStream<T>
where
    T: Dispatchable,
{
    /// Create a new `EventStream` for the given component context and inner receiver.
    pub fn new(context: ComponentContext, inner: mpsc::Receiver<T>) -> Self {
        let metrics_builder = MetricsBuilder::from_component_context(&context);

        Self {
            inner,
            events_received: metrics_builder.register_debug_counter("component_events_received_total"),
            events_received_size: metrics_builder.register_debug_histogram("component_events_received_size"),
        }
    }

    /// Gets the next event in the stream.
    ///
    /// If the component (or components) connected to this event stream have stopped, `None` is returned.
    pub async fn next(&mut self) -> Option<T> {
        match self.inner.recv().await {
            Some(item) => {
                self.events_received.increment(item.item_count() as u64);
                self.events_received_size.record(item.item_count() as f64);
                Some(item)
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

    fn create_event_stream(channel_size: usize) -> (EventStream<String>, mpsc::Sender<String>) {
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

        // Send an event, and make sure we can receive it:
        let input_event = "hello world";
        tx.send(input_event.to_string())
            .await
            .expect("should not fail to send event");

        let output_event = event_stream.next().await.expect("should receive event");
        assert_eq!(output_event, input_event);

        // Now drop the sender, which should close the event stream:
        drop(tx);

        assert!(event_stream.next().await.is_none());
    }
}
