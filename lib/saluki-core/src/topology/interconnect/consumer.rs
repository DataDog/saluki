use saluki_metrics::static_metrics;
use tokio::sync::mpsc;

use super::Dispatchable;
use crate::components::ComponentContext;

static_metrics!(
    name => ConsumerMetrics,
    prefix => component,
    labels => [component_id: String, component_type: &'static str],
    metrics => [
        counter(events_received_total),
        trace_histogram(events_received_size),
    ],
);

impl ConsumerMetrics {
    fn from_component_context(context: ComponentContext) -> Self {
        Self::new(context.component_id().to_string(), context.component_type().as_str())
    }
}

/// A stream of items sent to a component.
///
/// This represents the receiving end of a component interconnect, where the sending end is [`Dispatcher<T>`].
pub struct Consumer<T> {
    inner: mpsc::Receiver<T>,
    metrics: ConsumerMetrics,
}

impl<T> Consumer<T>
where
    T: Dispatchable,
{
    /// Create a new `Consumer` for the given component context and inner receiver.
    pub fn new(context: ComponentContext, inner: mpsc::Receiver<T>) -> Self {
        Self {
            inner,
            metrics: ConsumerMetrics::from_component_context(context),
        }
    }

    /// Gets the next item in the stream.
    ///
    /// If the component (or components) connected to this consumer have stopped, `None` is returned.
    pub async fn next(&mut self) -> Option<T> {
        match self.inner.recv().await {
            Some(item) => {
                self.metrics.events_received_total().increment(item.item_count() as u64);
                self.metrics.events_received_size().record(item.item_count() as f64);
                Some(item)
            }
            None => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use metrics::{Key, Label};
    use metrics_util::{
        debugging::{DebugValue, DebuggingRecorder},
        CompositeKey, MetricKind,
    };
    use ordered_float::OrderedFloat;

    use super::*;
    use crate::topology::ComponentId;

    #[derive(Clone, Debug, Eq, PartialEq)]
    struct DispatchableEvent<T> {
        item_count: usize,
        data: T,
    }

    impl<T: Clone> DispatchableEvent<T> {
        fn new(data: T) -> Self {
            Self { item_count: 1, data }
        }

        fn with_item_count(item_count: usize, data: T) -> Self {
            Self { item_count, data }
        }
    }

    impl<T: Clone> Dispatchable for DispatchableEvent<T> {
        fn item_count(&self) -> usize {
            self.item_count
        }
    }

    fn create_consumer<T: Clone>(
        channel_size: usize,
    ) -> (Consumer<DispatchableEvent<T>>, mpsc::Sender<DispatchableEvent<T>>) {
        let component_context = ComponentId::try_from("consumer_test")
            .map(ComponentContext::source)
            .expect("component ID should never be invalid");

        let (tx, rx) = mpsc::channel(channel_size);
        let consumer = Consumer::new(component_context, rx);

        (consumer, tx)
    }

    fn get_consumer_metric_composite_key(kind: MetricKind, name: &'static str) -> CompositeKey {
        // We build the labels according to what we'll generate when calling `create_consumer`:
        static LABELS: &[Label] = &[
            Label::from_static_parts("component_id", "consumer_test"),
            Label::from_static_parts("component_type", "source"),
        ];
        let key = Key::from_static_parts(name, LABELS);
        CompositeKey::new(kind, key)
    }

    #[tokio::test]
    async fn next() {
        let (mut consumer, tx) = create_consumer(1);

        // Send an item, and make sure we can receive it:
        let input_item = DispatchableEvent::new("hello world");
        tx.send(input_item.clone()).await.expect("should not fail to send item");

        let output_item = consumer.next().await.expect("should receive item");
        assert_eq!(output_item, input_item);

        // Now drop the sender, which should close the consumer:
        drop(tx);

        assert!(consumer.next().await.is_none());
    }

    #[tokio::test]
    async fn metrics() {
        let events_received_key =
            get_consumer_metric_composite_key(MetricKind::Counter, ConsumerMetrics::events_received_total_name());
        let events_received_size_key =
            get_consumer_metric_composite_key(MetricKind::Histogram, ConsumerMetrics::events_received_size_name());

        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let (mut consumer, tx) = metrics::with_local_recorder(&recorder, || create_consumer(1));

        // Send an item with an item count of 1, and make sure we can receive it, and that we update our metrics accordingly:
        let single_item = DispatchableEvent::new("single item");
        tx.send(single_item.clone())
            .await
            .expect("should not fail to send item");

        let output_item = consumer.next().await.expect("should receive item");
        assert_eq!(output_item, single_item);

        // TODO: This API for querying the metrics really sucks... and we need something better.
        let current_metrics = snapshotter.snapshot().into_hashmap();
        let (_, _, events_received) = current_metrics
            .get(&events_received_key)
            .expect("should have events received metric");
        let (_, _, events_received_size) = current_metrics
            .get(&events_received_size_key)
            .expect("should have events received size metric");
        assert_eq!(events_received, &DebugValue::Counter(1));
        let expected_sizes = vec![OrderedFloat(1.0)];
        assert_eq!(events_received_size, &DebugValue::Histogram(expected_sizes));

        // Now send an item with an item count of 42, and make sure we can receive it, and that we update our metrics accordingly:
        let multiple_items = DispatchableEvent::with_item_count(42, "multiple_items");
        tx.send(multiple_items.clone())
            .await
            .expect("should not fail to send item");

        let output_item = consumer.next().await.expect("should receive item");
        assert_eq!(output_item, multiple_items);

        // TODO: This API for querying the metrics really sucks... and we need something better.
        let current_metrics = snapshotter.snapshot().into_hashmap();
        let (_, _, events_received) = current_metrics
            .get(&events_received_key)
            .expect("should have events received metric");
        let (_, _, events_received_size) = current_metrics
            .get(&events_received_size_key)
            .expect("should have events received size metric");
        assert_eq!(events_received, &DebugValue::Counter(42));

        let expected_sizes = vec![OrderedFloat(42.0)];
        assert_eq!(events_received_size, &DebugValue::Histogram(expected_sizes));
    }
}
