use std::{borrow::Cow, time::Instant};

use saluki_common::collections::FastHashMap;
use saluki_error::{generic_error, GenericError};
use saluki_metrics::static_metrics;
use tokio::sync::mpsc;

use super::Dispatchable;
use crate::{components::ComponentContext, topology::OutputName};

// TODO: When we have support for additional static labels on a per-metric basis, add `discard_reason` to
// `events_discarded_total` metric to indicate that it's due to the destination component being disconnected.
static_metrics!(
    name => DispatcherMetrics,
    prefix => component,
    labels => [component_id: String, component_type: &'static str, output: String],
    metrics => [
        counter(events_sent_total),
        trace_histogram(send_latency_seconds),
        counter(events_discarded_total),
    ],
);

impl DispatcherMetrics {
    fn default_output(context: ComponentContext) -> Self {
        Self::with_output_name(context, "_default")
    }

    fn named_output(context: ComponentContext, output_name: &str) -> Self {
        Self::with_output_name(context, output_name)
    }

    fn with_output_name(context: ComponentContext, output_name: &str) -> Self {
        Self::new(
            context.component_id().to_string(),
            context.component_type().as_str(),
            output_name.to_string(),
        )
    }
}

/// A type that can be used as a buffer for dispatching items.
pub trait DispatchBuffer: Dispatchable + Default {
    /// Type of item that can be pushed into the buffer.
    type Item;

    /// Returns the number of items currently in the buffer.
    fn len(&self) -> usize;

    /// Returns the maximum number of items the buffer can hold.
    fn capacity(&self) -> usize;

    /// Returns `true` if the buffer is full.
    fn is_full(&self) -> bool;

    /// Attempts to push an item into the buffer.
    ///
    /// Returns `Some(item)` if the buffer is full and the item could not be pushed.
    fn try_push(&mut self, item: Self::Item) -> Option<Self::Item>;
}

struct DispatchTarget<T> {
    metrics: DispatcherMetrics,
    senders: Vec<mpsc::Sender<T>>,
}

impl<T> DispatchTarget<T>
where
    T: Dispatchable,
{
    fn default_output(context: ComponentContext) -> Self {
        Self {
            metrics: DispatcherMetrics::default_output(context),
            senders: Vec::new(),
        }
    }

    fn named_output(context: ComponentContext, output_name: &str) -> Self {
        Self {
            metrics: DispatcherMetrics::named_output(context, output_name),
            senders: Vec::new(),
        }
    }

    fn add_sender(&mut self, sender: mpsc::Sender<T>) {
        self.senders.push(sender);
    }

    async fn send(&self, item: T) -> Result<(), GenericError> {
        if self.senders.is_empty() {
            // Track discarded events when no senders are attached to this output
            let item_count = item.item_count() as u64;
            self.metrics.events_discarded_total().increment(item_count);
            return Ok(());
        }

        let start = Instant::now();
        let item_count = item.item_count();

        // Send the item to all senders except the last one by cloning the item.
        let cloned_sends = self.senders.len() - 1;
        for sender in &self.senders[0..cloned_sends] {
            sender
                .send(item.clone())
                .await
                .map_err(|_| generic_error!("Failed to send to output."))?;
        }

        // Send the item to the last sender without cloning.
        let last_sender = &self.senders[cloned_sends];
        last_sender
            .send(item)
            .await
            .map_err(|_| generic_error!("Failed to send to output."))?;

        let elapsed = start.elapsed();

        // TODO: We should consider splitting this out per-sender somehow. We would need to carry around the
        // destination component's ID, though, to properly associate it.
        self.metrics.send_latency_seconds().record(elapsed);

        let total_events_sent = (self.senders.len() * item_count) as u64;
        self.metrics.events_sent_total().increment(total_events_sent);

        Ok(())
    }
}

/// A buffered dispatcher.
///
/// `BufferedDispatcher` provides an efficient and ergonomic interface to `Dispatcher` that allows for writing events
/// one-by-one into batches, which are then dispatched to the configured output as needed. This allows callers to focus
/// on the logic around what items to send, without needing to worry about the details of event buffer sizing or
/// flushing.
pub struct BufferedDispatcher<'a, T> {
    metrics: &'a DispatcherMetrics,
    flushed_len: usize,
    buffer: Option<T>,
    target: &'a DispatchTarget<T>,
}

impl<'a, T> BufferedDispatcher<'a, T> {
    fn new(target: &'a DispatchTarget<T>) -> Self {
        Self {
            metrics: &target.metrics,
            flushed_len: 0,
            buffer: None,
            target,
        }
    }
}

impl<T> BufferedDispatcher<'_, T>
where
    T: DispatchBuffer,
{
    async fn try_flush_buffer(&self, buffer: T) -> Result<(), GenericError> {
        let buffer_len = buffer.len();
        if buffer_len > 0 {
            self.target.send(buffer).await
        } else {
            Ok(())
        }
    }

    /// Pushes an item into the buffered dispatcher.
    ///
    /// # Errors
    ///
    /// If there is an error flushing items to the output, or if there is an error acquiring a new buffer, an error
    /// is returned.
    pub async fn push(&mut self, item: T::Item) -> Result<(), GenericError> {
        // If our current buffer is full, flush it before acquiring a new one.
        if let Some(old_buffer) = self.buffer.take_if(|b| b.is_full()) {
            self.try_flush_buffer(old_buffer).await?;
        }

        // Add the item to our current buffer.
        //
        // If our current buffer is empty, create a new one first. If the current buffer is full, return an error
        // because it should be impossible to get a new buffer that is full.
        let buffer = self.buffer.get_or_insert_default();
        if buffer.try_push(item).is_some() {
            return Err(generic_error!("Dispatch buffer already full after acquisition."));
        }

        self.flushed_len += 1;

        Ok(())
    }

    /// Consumes this buffered dispatcher and sends/flushes all input items to the underlying output.
    ///
    /// If flushing is successful, `Ok(flushed)` is returned, where `flushed` is the total number of items that
    /// have been flushed through this buffered dispatcher.
    ///
    /// # Errors
    ///
    /// If there is an error sending items to the output, an error is returned.
    pub async fn send_all<I>(mut self, items: I) -> Result<usize, GenericError>
    where
        I: IntoIterator<Item = T::Item>,
    {
        for item in items {
            self.push(item).await?;
        }

        self.flush().await
    }

    /// Consumes this buffered dispatcher, flushing any buffered items to the underlying output.
    ///
    /// If flushing is successful, `Ok(flushed)` is returned, where `flushed` is the total number of items that have
    /// been flushed through this buffered dispatcher.
    ///
    /// # Errors
    ///
    /// If there is an error sending items to the output, an error is returned.
    pub async fn flush(mut self) -> Result<usize, GenericError> {
        if let Some(old_buffer) = self.buffer.take() {
            self.try_flush_buffer(old_buffer).await?;
        }

        // We increment the "events sent" metric here because we want to count the number of buffered items, vs doing it in
        // `DispatchTarget::send` where all it knows is that it sent one item.
        self.metrics.events_sent_total().increment(self.flushed_len as u64);

        Ok(self.flushed_len)
    }
}

/// Dispatches items from one component to another.
///
/// [`Dispatcher`] provides an ergonomic interface for sending items to a downstream component. It has support for
/// multiple outputs (a default output, and additional "named" outputs) and provides telemetry around the number of
/// dispatched items as well as the latency of sending them.
pub struct Dispatcher<T>
where
    T: Dispatchable,
{
    context: ComponentContext,
    default: Option<DispatchTarget<T>>,
    targets: FastHashMap<Cow<'static, str>, DispatchTarget<T>>,
}

impl<T> Dispatcher<T>
where
    T: Dispatchable,
{
    /// Create a new `Dispatcher` for the given component context.
    pub fn new(context: ComponentContext) -> Self {
        Self {
            context,
            default: None,
            targets: FastHashMap::default(),
        }
    }

    /// Adds an output to the dispatcher.
    ///
    /// # Errors
    ///
    /// If the output already exists, an error is returned.
    pub fn add_output(&mut self, output_name: OutputName) -> Result<(), GenericError> {
        match output_name {
            OutputName::Default => {
                if self.default.is_some() {
                    return Err(generic_error!("Default output already exists."));
                }

                self.default = Some(DispatchTarget::default_output(self.context.clone()));
            }
            OutputName::Given(name) => {
                if self.targets.contains_key(&name) {
                    return Err(generic_error!("Output '{}' already exists.", name));
                }
                let target = DispatchTarget::named_output(self.context.clone(), &name);
                self.targets.insert(name, target);
            }
        }

        Ok(())
    }

    /// Attaches a sender to the given output.
    ///
    /// # Errors
    ///
    /// If the output does not exist, an error is returned.
    pub fn attach_sender_to_output(
        &mut self, output_name: &OutputName, sender: mpsc::Sender<T>,
    ) -> Result<(), GenericError> {
        let target = match output_name {
            OutputName::Default => self
                .default
                .as_mut()
                .ok_or_else(|| generic_error!("No default output declared."))?,
            OutputName::Given(name) => self
                .targets
                .get_mut(name)
                .ok_or_else(|| generic_error!("Output '{}' does not exist.", name))?,
        };
        target.add_sender(sender);

        Ok(())
    }

    fn get_default_output(&self) -> Result<&DispatchTarget<T>, GenericError> {
        self.default
            .as_ref()
            .ok_or_else(|| generic_error!("No default output declared."))
    }

    fn get_named_output(&self, name: &str) -> Result<&DispatchTarget<T>, GenericError> {
        self.targets
            .get(name)
            .ok_or_else(|| generic_error!("No output named '{}' declared.", name))
    }

    /// Returns `true` if the default output is connected to downstream components.
    pub fn is_default_output_connected(&self) -> bool {
        self.default.as_ref().is_some_and(|target| !target.senders.is_empty())
    }

    /// Returns `true` if the named output is connected to downstream components.
    pub fn is_named_output_connected(&self, name: &str) -> bool {
        self.targets.get(name).is_some_and(|target| !target.senders.is_empty())
    }

    /// Dispatches the given item to the default output.
    ///
    /// # Errors
    ///
    /// If the default output is not set, or there is an error sending to the default output, an error is returned.
    pub async fn dispatch(&self, item: T) -> Result<(), GenericError> {
        self.dispatch_inner(None, item).await
    }

    /// Dispatches the given items to the given named output.
    ///
    /// # Errors
    ///
    /// If a output of the given name is not set, or there is an error sending to the output, an error is returned.
    pub async fn dispatch_named<N>(&self, output_name: N, item: T) -> Result<(), GenericError>
    where
        N: AsRef<str>,
    {
        self.dispatch_inner(Some(output_name.as_ref()), item).await
    }

    async fn dispatch_inner(&self, output_name: Option<&str>, item: T) -> Result<(), GenericError> {
        let target = match output_name {
            None => self.get_default_output()?,
            Some(name) => self.get_named_output(name)?,
        };

        target.send(item).await?;

        Ok(())
    }
}

impl<T> Dispatcher<T>
where
    T: DispatchBuffer,
{
    /// Creates a buffered dispatcher for the default output.
    ///
    /// This should generally be used if the items being dispatched are not already collected in a container, or exposed
    /// via an iterable type. It allows for efficiently buffering items one-by-one before dispatching them to the
    /// underlying output.
    ///
    /// # Errors
    ///
    /// If the default output has not been configured, an error will be returned.
    pub fn buffered(&self) -> Result<BufferedDispatcher<'_, T>, GenericError> {
        self.get_default_output().map(BufferedDispatcher::new)
    }

    /// Creates a buffered dispatcher for the given named output.
    ///
    /// This should generally be used if the items being dispatched are not already collected in a container, or exposed
    /// via an iterable type. It allows for efficiently buffering items one-by-one before dispatching them to the
    /// underlying output.
    ///
    /// # Errors
    ///
    /// If the given named output has not been configured, an error will be returned.
    pub fn buffered_named<N>(&self, output_name: N) -> Result<BufferedDispatcher<'_, T>, GenericError>
    where
        N: AsRef<str>,
    {
        self.get_named_output(output_name.as_ref()).map(BufferedDispatcher::new)
    }
}

#[cfg(test)]
mod tests {
    // TODO: Tests asserting we emit metrics, and the right metrics.

    use std::ops::Deref;

    use metrics::{Key, Label};
    use metrics_util::{
        debugging::{DebugValue, DebuggingRecorder, Snapshotter},
        CompositeKey, MetricKind,
    };
    use ordered_float::OrderedFloat;

    use super::*;

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    struct SingleEvent<T>(T);

    impl<T: Clone + Copy> Dispatchable for SingleEvent<T> {
        fn item_count(&self) -> usize {
            1
        }
    }

    impl<T: Clone + Copy> From<T> for SingleEvent<T> {
        fn from(value: T) -> Self {
            Self(value)
        }
    }

    #[derive(Clone, Debug, Eq, PartialEq)]
    struct FixedUsizeVec<const N: usize> {
        data: [usize; N],
        len: usize,
    }

    impl<const N: usize> Default for FixedUsizeVec<N> {
        fn default() -> Self {
            Self { data: [0; N], len: 0 }
        }
    }

    impl<const N: usize> Deref for FixedUsizeVec<N> {
        type Target = [usize];

        fn deref(&self) -> &Self::Target {
            &self.data
        }
    }

    impl<const N: usize> Dispatchable for FixedUsizeVec<N> {
        fn item_count(&self) -> usize {
            self.len
        }
    }

    impl<const N: usize> DispatchBuffer for FixedUsizeVec<N> {
        type Item = usize;

        fn len(&self) -> usize {
            self.len
        }

        fn capacity(&self) -> usize {
            N
        }

        fn is_full(&self) -> bool {
            self.len == N
        }

        fn try_push(&mut self, item: Self::Item) -> Option<Self::Item> {
            if self.is_full() {
                Some(item)
            } else {
                self.data[self.len] = item;
                self.len += 1;
                None
            }
        }
    }

    fn unbuffered_dispatcher<T: Dispatchable>() -> Dispatcher<T> {
        let component_context = ComponentContext::test_source("dispatcher_test");
        Dispatcher::new(component_context)
    }

    fn buffered_dispatcher<T: DispatchBuffer>() -> Dispatcher<T> {
        unbuffered_dispatcher()
    }

    fn add_dispatcher_default_output<T: Dispatchable, const N: usize>(
        dispatcher: &mut Dispatcher<T>, senders: [mpsc::Sender<T>; N],
    ) {
        dispatcher
            .add_output(OutputName::Default)
            .expect("default output should not be added yet");
        for sender in senders {
            dispatcher
                .attach_sender_to_output(&OutputName::Default, sender)
                .expect("default output should be added");
        }
    }

    fn add_dispatcher_named_output<T: Dispatchable, const N: usize>(
        dispatcher: &mut Dispatcher<T>, output_name: &'static str, senders: [mpsc::Sender<T>; N],
    ) {
        dispatcher
            .add_output(OutputName::Given(output_name.into()))
            .expect("named output should not be added yet");
        for sender in senders {
            dispatcher
                .attach_sender_to_output(&OutputName::Given(output_name.into()), sender)
                .expect("named output should be added");
        }
    }

    fn get_dispatcher_metric_ckey(
        kind: MetricKind, name: &'static str, output_name: &'static str, tags: &[(&'static str, &'static str)],
    ) -> CompositeKey {
        let mut labels = vec![
            Label::from_static_parts("component_id", "dispatcher_test"),
            Label::from_static_parts("component_type", "source"),
            Label::from_static_parts("output", output_name),
        ];

        for tag in tags {
            labels.push(Label::from_static_parts(tag.0, tag.1));
        }

        let key = Key::from_parts(name, labels);
        CompositeKey::new(kind, key)
    }

    fn get_output_metrics(snapshotter: &Snapshotter, output_name: &'static str) -> (u64, u64, Vec<OrderedFloat<f64>>) {
        let events_sent_key = get_dispatcher_metric_ckey(
            MetricKind::Counter,
            DispatcherMetrics::events_sent_total_name(),
            output_name,
            &[],
        );
        let events_discarded_key = get_dispatcher_metric_ckey(
            MetricKind::Counter,
            DispatcherMetrics::events_discarded_total_name(),
            output_name,
            &[],
        );
        let send_latency_key = get_dispatcher_metric_ckey(
            MetricKind::Histogram,
            DispatcherMetrics::send_latency_seconds_name(),
            output_name,
            &[],
        );

        // TODO: This API for querying the metrics really sucks... and we need something better.
        let current_metrics = snapshotter.snapshot().into_hashmap();
        let (_, _, events_sent) = current_metrics
            .get(&events_sent_key)
            .expect("should have events sent metric");
        let (_, _, events_discarded) = current_metrics
            .get(&events_discarded_key)
            .expect("should have events discarded metric");
        let (_, _, send_latency) = current_metrics
            .get(&send_latency_key)
            .expect("should have send latency metric");

        let events_sent = match events_sent {
            DebugValue::Counter(value) => *value,
            _ => panic!("unexpected metric type for events sent"),
        };

        let events_discarded = match events_discarded {
            DebugValue::Counter(value) => *value,
            _ => panic!("unexpected metric type for events discarded"),
        };

        let send_latency = match send_latency {
            DebugValue::Histogram(value) => value.clone(),
            _ => panic!("unexpected metric type for send latency"),
        };

        (events_sent, events_discarded, send_latency)
    }

    #[tokio::test]
    async fn default_output() {
        // Create the dispatcher and wire up a sender to the default output.
        let mut dispatcher = unbuffered_dispatcher::<SingleEvent<usize>>();

        let (tx, mut rx) = mpsc::channel(1);
        add_dispatcher_default_output(&mut dispatcher, [tx]);

        // Create an item and roundtrip it through the dispatcher.
        let input_item = 42.into();

        dispatcher.dispatch(input_item).await.unwrap();

        let output_item = rx.try_recv().expect("input item should have been dispatched");
        assert_eq!(output_item, input_item);
    }

    #[tokio::test]
    async fn named_output() {
        // Create the dispatcher and wire up a sender to a named output.
        let mut dispatcher = unbuffered_dispatcher::<SingleEvent<usize>>();

        let output_name = "special";
        let (tx, mut rx) = mpsc::channel(1);
        add_dispatcher_named_output(&mut dispatcher, output_name, [tx]);

        // Create an item and roundtrip it through the dispatcher.
        let input_item = 42.into();

        dispatcher.dispatch_named(output_name, input_item).await.unwrap();

        let output_item = rx.try_recv().expect("input item should have been dispatched");
        assert_eq!(output_item, input_item);
    }

    #[tokio::test]
    async fn default_output_multiple_senders() {
        // Create the dispatcher and wire up two senders to the default output.
        let mut dispatcher = unbuffered_dispatcher::<SingleEvent<usize>>();

        let (tx1, mut rx1) = mpsc::channel(1);
        let (tx2, mut rx2) = mpsc::channel(1);
        add_dispatcher_default_output(&mut dispatcher, [tx1, tx2]);

        // Create an item and roundtrip it through the dispatcher.
        let input_item = 42.into();

        dispatcher.dispatch(input_item).await.unwrap();

        let output_item1 = rx1.try_recv().expect("input item should have been dispatched");
        let output_item2 = rx2.try_recv().expect("input item should have been dispatched");
        assert_eq!(output_item1, input_item);
        assert_eq!(output_item2, input_item);
    }

    #[tokio::test]
    async fn named_output_multiple_senders() {
        // Create the dispatcher and wire up two senders to a named output.
        let mut dispatcher = unbuffered_dispatcher::<SingleEvent<usize>>();

        let output_name = "special";
        let (tx1, mut rx1) = mpsc::channel(1);
        let (tx2, mut rx2) = mpsc::channel(1);
        add_dispatcher_named_output(&mut dispatcher, output_name, [tx1, tx2]);

        // Create an item and roundtrip it through the dispatcher.
        let input_item = 42.into();

        dispatcher.dispatch_named(output_name, input_item).await.unwrap();

        let output_item1 = rx1.try_recv().expect("input item should have been dispatched");
        let output_item2 = rx2.try_recv().expect("input item should have been dispatched");
        assert_eq!(output_item1, input_item);
        assert_eq!(output_item2, input_item);
    }

    #[tokio::test]
    async fn default_output_not_set() {
        // Create the dispatcher and try to dispatch an item without setting up a default output.
        let dispatcher = unbuffered_dispatcher::<SingleEvent<()>>();

        let result = dispatcher.dispatch(().into()).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn named_output_not_set() {
        // Create the dispatcher and try to dispatch an event without setting up a named output.
        let dispatcher = unbuffered_dispatcher::<SingleEvent<()>>();

        let result = dispatcher.dispatch_named("non_existent", ().into()).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn default_output_buffered_partial() {
        // Create the dispatcher and wire up a sender to the default output, using a bufferable type.
        let mut dispatcher = buffered_dispatcher::<FixedUsizeVec<4>>();

        let (tx, mut rx) = mpsc::channel(1);
        add_dispatcher_default_output(&mut dispatcher, [tx]);

        // Create an item and roundtrip it through the dispatcher.
        let input_item = 42;

        let mut buffered = dispatcher.buffered().unwrap();
        buffered.push(input_item).await.unwrap();

        let flushed_len = buffered.flush().await.unwrap();
        assert_eq!(flushed_len, 1);

        let output_item = rx.try_recv().expect("input item should have been dispatched");
        assert_eq!(output_item.len(), 1);
        assert_eq!(output_item[0], input_item);
    }

    #[tokio::test]
    async fn named_output_buffered_partial() {
        // Create the dispatcher and wire up a sender to a named output, using a bufferable type.
        let mut dispatcher = buffered_dispatcher::<FixedUsizeVec<4>>();

        let output_name = "buffered_partial";
        let (tx, mut rx) = mpsc::channel(1);
        add_dispatcher_named_output(&mut dispatcher, output_name, [tx]);

        // Create an item and roundtrip it through the dispatcher.
        let input_item = 42;

        let mut buffered = dispatcher.buffered_named(output_name).unwrap();
        buffered.push(input_item).await.unwrap();

        let flushed_len = buffered.flush().await.unwrap();
        assert_eq!(flushed_len, 1);

        let output_item = rx.try_recv().expect("input item should have been dispatched");
        assert_eq!(output_item.len(), 1);
        assert_eq!(output_item[0], input_item);
    }

    #[tokio::test]
    async fn default_output_buffered_overflow() {
        // Create the dispatcher and wire up a sender to the default output, using a bufferable type.
        let mut dispatcher = buffered_dispatcher::<FixedUsizeVec<4>>();

        let (tx, mut rx) = mpsc::channel(2);
        add_dispatcher_default_output(&mut dispatcher, [tx]);

        // Create multiple items and roundtrip them through the dispatcher.
        //
        // We explicitly create more items than a single buffer can hold to exercise full buffers
        // being flushed during push.
        let input_items: Vec<usize> = vec![1, 2, 3, 4, 5, 6];

        let mut buffered = dispatcher.buffered().unwrap();

        for item in &input_items {
            buffered.push(*item).await.unwrap();
        }

        let flushed_len = buffered.flush().await.unwrap();
        assert_eq!(flushed_len, input_items.len());

        let output_item1 = rx.try_recv().expect("input item should have been dispatched");
        assert_eq!(output_item1.len(), 4);
        assert_eq!(output_item1[0..4], input_items[0..4]);

        let output_item2 = rx.try_recv().expect("input item should have been dispatched");
        assert_eq!(output_item2.len(), 2);
        assert_eq!(output_item2[0..2], input_items[4..6]);
    }

    #[tokio::test]
    async fn named_output_buffered_overflow() {
        // Create the dispatcher and wire up a sender to a named output, using a bufferable type.
        let mut dispatcher = buffered_dispatcher::<FixedUsizeVec<4>>();

        let output_name = "buffered_overflow";
        let (tx, mut rx) = mpsc::channel(2);
        add_dispatcher_named_output(&mut dispatcher, output_name, [tx]);

        // Create multiple items and roundtrip them through the dispatcher.
        //
        // We explicitly create more items than a single buffer can hold to exercise full buffers
        // being flushed during push.
        let input_items: Vec<usize> = vec![1, 2, 3, 4, 5, 6];

        let mut buffered = dispatcher.buffered_named(output_name).unwrap();

        for item in &input_items {
            buffered.push(*item).await.unwrap();
        }

        let flushed_len = buffered.flush().await.unwrap();
        assert_eq!(flushed_len, input_items.len());

        let output_item1 = rx.try_recv().expect("input item should have been dispatched");
        assert_eq!(output_item1.len(), 4);
        assert_eq!(output_item1[0..4], input_items[0..4]);

        let output_item2 = rx.try_recv().expect("input item should have been dispatched");
        assert_eq!(output_item2.len(), 2);
        assert_eq!(output_item2[0..2], input_items[4..6]);
    }

    #[tokio::test]
    async fn default_output_buffered_partial_multiple_senders() {
        // Create the dispatcher and wire up two senders to the default output, using a bufferable type.
        let mut dispatcher = buffered_dispatcher::<FixedUsizeVec<4>>();

        let (tx1, mut rx1) = mpsc::channel(1);
        let (tx2, mut rx2) = mpsc::channel(1);
        add_dispatcher_default_output(&mut dispatcher, [tx1, tx2]);

        // Create an item and roundtrip it through the dispatcher.
        let input_item = 42;

        let mut buffered = dispatcher.buffered().unwrap();
        buffered.push(input_item).await.unwrap();

        let flushed_len = buffered.flush().await.unwrap();
        assert_eq!(flushed_len, 1);

        let output_item1 = rx1.try_recv().expect("input item should have been dispatched");
        assert_eq!(output_item1.len(), 1);
        assert_eq!(output_item1[0], input_item);

        let output_item2 = rx2.try_recv().expect("input item should have been dispatched");
        assert_eq!(output_item2.len(), 1);
        assert_eq!(output_item2[0], input_item);
    }

    #[tokio::test]
    async fn named_output_buffered_partial_multiple_senders() {
        // Create the dispatcher and wire up two senders to a named output, using a bufferable type.
        let mut dispatcher = buffered_dispatcher::<FixedUsizeVec<4>>();

        let output_name = "buffered_partial";
        let (tx1, mut rx1) = mpsc::channel(1);
        let (tx2, mut rx2) = mpsc::channel(1);
        add_dispatcher_named_output(&mut dispatcher, output_name, [tx1, tx2]);

        // Create an item and roundtrip it through the dispatcher.
        let input_item = 42;

        let mut buffered = dispatcher.buffered_named(output_name).unwrap();
        buffered.push(input_item).await.unwrap();

        let flushed_len = buffered.flush().await.unwrap();
        assert_eq!(flushed_len, 1);

        let output_item1 = rx1.try_recv().expect("input item should have been dispatched");
        assert_eq!(output_item1.len(), 1);
        assert_eq!(output_item1[0], input_item);

        let output_item2 = rx2.try_recv().expect("input item should have been dispatched");
        assert_eq!(output_item2.len(), 1);
        assert_eq!(output_item2[0], input_item);
    }

    #[tokio::test]
    async fn default_output_no_senders() {
        // Test that we can add a default output and dispatch to it even with no senders attached
        let mut dispatcher = unbuffered_dispatcher::<SingleEvent<u32>>();

        // Add default output but don't attach any senders
        dispatcher
            .add_output(OutputName::Default)
            .expect("should be able to add default output");

        // Should not panic when dispatching to output with no senders
        let test_event = 42.into();
        let result = dispatcher.dispatch(test_event).await;
        assert!(
            result.is_ok(),
            "dispatch to default output with no senders should succeed"
        );
    }

    #[tokio::test]
    async fn named_output_no_senders() {
        // Test that we can add a named output and dispatch to it even with no senders attached
        let mut dispatcher = unbuffered_dispatcher::<SingleEvent<u32>>();

        // Add named output but don't attach any senders
        dispatcher
            .add_output(OutputName::Given("errors".into()))
            .expect("should be able to add named output");

        // Should not panic when dispatching to output with no senders
        let test_event = 42.into();
        let result = dispatcher.dispatch_named("errors", test_event).await;
        assert!(
            result.is_ok(),
            "dispatch to named output with no senders should succeed"
        );
    }

    #[tokio::test]
    async fn metrics_default_output_disconnected() {
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let dispatcher = metrics::with_local_recorder(&recorder, || {
            let mut dispatcher = buffered_dispatcher::<FixedUsizeVec<4>>();
            dispatcher
                .add_output(OutputName::Default)
                .expect("should not fail to add default output");
            dispatcher
        });

        // Send an item with an item count of 1, and make sure we can receive it, and that we update our metrics accordingly:
        let mut single_item = FixedUsizeVec::<4>::default();
        assert_eq!(None, single_item.try_push(42));
        let single_item_item_count = single_item.item_count() as u64;

        dispatcher
            .dispatch(single_item.clone())
            .await
            .expect("should not fail to dispatch");

        let (events_sent, events_discarded, send_latencies) = get_output_metrics(&snapshotter, "_default");
        assert_eq!(events_sent, 0);
        assert_eq!(events_discarded, single_item_item_count);
        assert!(send_latencies.is_empty());

        // Now send an item with an item count of 3, and make sure we can receive it, and that we update our metrics accordingly:
        let mut multiple_items = FixedUsizeVec::<4>::default();
        assert_eq!(None, multiple_items.try_push(42));
        assert_eq!(None, multiple_items.try_push(12345));
        assert_eq!(None, multiple_items.try_push(1337));
        let multiple_items_item_count = multiple_items.item_count() as u64;

        dispatcher
            .dispatch(multiple_items.clone())
            .await
            .expect("should not fail to dispatch");

        let (events_sent, events_discarded, send_latencies) = get_output_metrics(&snapshotter, "_default");
        assert_eq!(events_sent, 0);
        assert_eq!(events_discarded, multiple_items_item_count);
        assert!(send_latencies.is_empty());
    }

    #[tokio::test]
    async fn metrics_named_output_disconnected() {
        let output_name = "some_output";

        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let dispatcher = metrics::with_local_recorder(&recorder, || {
            let mut dispatcher = buffered_dispatcher::<FixedUsizeVec<4>>();
            dispatcher
                .add_output(OutputName::Given(output_name.into()))
                .expect("should not fail to add named output");
            dispatcher
        });

        // Send an item with an item count of 1, and make sure we can receive it, and that we update our metrics accordingly:
        let mut single_item = FixedUsizeVec::<4>::default();
        assert_eq!(None, single_item.try_push(42));
        let single_item_item_count = single_item.item_count() as u64;

        dispatcher
            .dispatch_named(output_name, single_item.clone())
            .await
            .expect("should not fail to dispatch");

        let (events_sent, events_discarded, send_latencies) = get_output_metrics(&snapshotter, output_name);
        assert_eq!(events_sent, 0);
        assert_eq!(events_discarded, single_item_item_count);
        assert!(send_latencies.is_empty());

        // Now send an item with an item count of 3, and make sure we can receive it, and that we update our metrics accordingly:
        let mut multiple_items = FixedUsizeVec::<4>::default();
        assert_eq!(None, multiple_items.try_push(42));
        assert_eq!(None, multiple_items.try_push(12345));
        assert_eq!(None, multiple_items.try_push(1337));
        let multiple_items_item_count = multiple_items.item_count() as u64;

        dispatcher
            .dispatch_named(output_name, multiple_items.clone())
            .await
            .expect("should not fail to dispatch");

        let (events_sent, events_discarded, send_latencies) = get_output_metrics(&snapshotter, output_name);
        assert_eq!(events_sent, 0);
        assert_eq!(events_discarded, multiple_items_item_count);
        assert!(send_latencies.is_empty());
    }

    #[tokio::test]
    async fn is_default_output_connected_behavior() {
        let mut dispatcher = unbuffered_dispatcher::<SingleEvent<u32>>();

        // Initially, no default output exists - should return false
        assert!(
            !dispatcher.is_default_output_connected(),
            "should return false when no default output exists"
        );

        // Add default output but no senders - should return false
        dispatcher
            .add_output(OutputName::Default)
            .expect("should be able to add default output");
        assert!(
            !dispatcher.is_default_output_connected(),
            "should return false when default output exists but has no senders"
        );

        // Add a sender to the default output - should return true
        let (tx, _rx) = mpsc::channel(1);
        dispatcher
            .attach_sender_to_output(&OutputName::Default, tx)
            .expect("should be able to attach sender");
        assert!(
            dispatcher.is_default_output_connected(),
            "should return true when default output has senders attached"
        );
    }

    #[tokio::test]
    async fn is_named_output_connected_behavior() {
        let mut dispatcher = unbuffered_dispatcher::<SingleEvent<u32>>();
        let output_name = "test_output";

        // Initially, no named output exists - should return false
        assert!(
            !dispatcher.is_named_output_connected(output_name),
            "should return false when named output doesn't exist"
        );

        // Add named output but no senders - should return false
        dispatcher
            .add_output(OutputName::Given(output_name.into()))
            .expect("should be able to add named output");
        assert!(
            !dispatcher.is_named_output_connected(output_name),
            "should return false when named output exists but has no senders"
        );

        // Add a sender to the named output - should return true
        let (tx, _rx) = mpsc::channel(1);
        dispatcher
            .attach_sender_to_output(&OutputName::Given(output_name.into()), tx)
            .expect("should be able to attach sender");
        assert!(
            dispatcher.is_named_output_connected(output_name),
            "should return true when named output has senders attached"
        );

        // Test with a different output name that doesn't exist - should return false
        assert!(
            !dispatcher.is_named_output_connected("nonexistent_output"),
            "should return false for nonexistent output"
        );
    }
}
