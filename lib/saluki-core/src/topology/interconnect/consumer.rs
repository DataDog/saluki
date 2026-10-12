use saluki_metrics::{static_metrics, Counter, Histogram};
use tokio::sync::mpsc;

use super::Dispatchable;
use crate::components::ComponentContext;

#[static_metrics(prefix = component, labels(component_id, component_type))]
#[derive(Clone)]
struct ConsumerMetrics {
    events_received_total: Counter,
    #[metric(level = trace)]
    events_received_size: Histogram,
}

impl ConsumerMetrics {
    fn from_component_context(context: ComponentContext) -> Self {
        Self::new(context.component_id(), context.component_type().as_str())
    }

    fn record(&self, item_count: usize) {
        self.events_received_total().increment(item_count as u64);
        self.events_received_size().record(item_count as f64);
    }
}

/// The counters of what a component received, which a dispatcher writes for what it skips sending to a component
/// whose [input gate][super::InputGate] is closed, as the component's consumer would have on receiving it.
#[derive(Clone)]
pub(crate) struct ReceivedMetrics {
    metrics: ConsumerMetrics,
}

impl ReceivedMetrics {
    /// Records one item of `item_count` items as received.
    pub(crate) fn record(&self, item_count: usize) {
        self.metrics.record(item_count);
    }
}

/// Sees each item a consumer yields (see [`Consumer::observe`]).
#[cfg(any(test, feature = "test-util"))]
type Observer<T> = Box<dyn Fn(&T) + Send + Sync>;

/// A stream of items sent to a component.
///
/// This represents the receiving end of a component interconnect, where the sending end is [`Dispatcher<T>`][super::Dispatcher].
pub struct Consumer<T> {
    inner: mpsc::Receiver<T>,
    metrics: ConsumerMetrics,
    #[cfg(any(test, feature = "test-util"))]
    observer: Option<Observer<T>>,
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
            #[cfg(any(test, feature = "test-util"))]
            observer: None,
        }
    }

    /// Calls `observer` with every item this consumer yields from now on, before the component receives it, in place
    /// of any observer set earlier.
    ///
    /// This is a test helper, with the `test-util` feature: it lets a test or a measurement see what a component
    /// receives, such as when a marker reaches an encoder, without a hop in front of the component. The observer runs
    /// on the component's task, so it must be cheap.
    #[cfg(any(test, feature = "test-util"))]
    pub fn observe<F>(&mut self, observer: F)
    where
        F: Fn(&T) + Send + Sync + 'static,
    {
        self.observer = Some(Box::new(observer));
    }

    /// Gets the next item in the stream.
    ///
    /// If the component (or components) connected to this consumer have stopped, `None` is returned.
    pub async fn next(&mut self) -> Option<T> {
        match self.inner.recv().await {
            Some(item) => {
                self.metrics.record(item.item_count());
                #[cfg(any(test, feature = "test-util"))]
                if let Some(observer) = &self.observer {
                    observer(&item);
                }
                Some(item)
            }
            None => None,
        }
    }

    /// Returns the number of items sent to this consumer and not received yet.
    ///
    /// Items are received in the order they were sent, so once this consumer has received as many more items as this
    /// returns, it has received every item that was sent before the call. A component uses that to order a request it
    /// receives on another channel after everything already sent to it.
    pub fn queued(&self) -> usize {
        self.inner.len()
    }

    /// Returns the counters this consumer records what its component receives in.
    pub(crate) fn received_metrics(&self) -> ReceivedMetrics {
        ReceivedMetrics {
            metrics: self.metrics.clone(),
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
        let component_context = ComponentContext::test_source("consumer_test");

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
    async fn queued_counts_what_was_sent_and_not_received_yet() {
        let (mut consumer, tx) = create_consumer(4);
        assert_eq!(consumer.queued(), 0);

        for data in ["a", "b", "c"] {
            tx.send(DispatchableEvent::new(data))
                .await
                .expect("should not fail to send item");
        }
        assert_eq!(consumer.queued(), 3);

        // Items are received in order: once as many more as were queued have been received, every one sent before is.
        consumer.next().await.expect("should receive item");
        assert_eq!(consumer.queued(), 2);

        // Items still queued when every sender is gone are still received and counted.
        drop(tx);
        assert_eq!(consumer.queued(), 2);
        consumer.next().await.expect("should receive item");
        consumer.next().await.expect("should receive item");
        assert_eq!(consumer.queued(), 0);
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

    #[tokio::test]
    async fn an_observer_sees_every_item_before_the_component_does() {
        use std::sync::{Arc, Mutex};

        let (mut consumer, tx) = create_consumer(2);
        let seen = Arc::new(Mutex::new(Vec::new()));
        let observed = Arc::clone(&seen);
        consumer.observe(move |item: &DispatchableEvent<&'static str>| observed.lock().unwrap().push(item.data));

        tx.send(DispatchableEvent::new("first")).await.unwrap();
        tx.send(DispatchableEvent::new("second")).await.unwrap();
        assert_eq!(consumer.next().await.unwrap().data, "first");
        assert_eq!(*seen.lock().unwrap(), ["first"]);
        assert_eq!(consumer.next().await.unwrap().data, "second");
        assert_eq!(*seen.lock().unwrap(), ["first", "second"]);
    }
}
