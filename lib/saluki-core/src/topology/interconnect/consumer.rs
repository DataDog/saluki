use metrics::{Counter, Histogram};
use saluki_metrics::MetricsBuilder;
use tokio::sync::mpsc;

use super::Dispatchable;
use crate::{components::ComponentContext, observability::ComponentMetricsExt as _};

/// A stream of items sent to a component.
///
/// This represents the receiving end of a component interconnect, where the sending end is [`Dispatcher<T>`].
pub struct Consumer<T> {
    inner: mpsc::Receiver<T>,
    events_received: Counter,
    events_received_size: Histogram,
}

impl<T> Consumer<T>
where
    T: Dispatchable,
{
    /// Create a new `Consumer` for the given component context and inner receiver.
    pub fn new(context: ComponentContext, inner: mpsc::Receiver<T>) -> Self {
        let metrics_builder = MetricsBuilder::from_component_context(&context);

        Self {
            inner,
            events_received: metrics_builder.register_debug_counter("component_events_received_total"),
            events_received_size: metrics_builder.register_debug_histogram("component_events_received_size"),
        }
    }

    /// Gets the next item in the stream.
    ///
    /// If the component (or components) connected to this consumer have stopped, `None` is returned.
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

    fn create_consumer(channel_size: usize) -> (Consumer<String>, mpsc::Sender<String>) {
        let component_context = ComponentId::try_from("consumer_test")
            .map(ComponentContext::source)
            .expect("component ID should never be invalid");

        let (tx, rx) = mpsc::channel(channel_size);
        let consumer = Consumer::new(component_context, rx);

        (consumer, tx)
    }

    #[tokio::test]
    async fn next() {
        let (mut consumer, tx) = create_consumer(1);

        // Send an item, and make sure we can receive it:
        let input_item = "hello world";
        tx.send(input_item.to_string())
            .await
            .expect("should not fail to send item");

        let output_item = consumer.next().await.expect("should receive item");
        assert_eq!(output_item, input_item);

        // Now drop the sender, which should close the consumer:
        drop(tx);

        assert!(consumer.next().await.is_none());
    }
}
