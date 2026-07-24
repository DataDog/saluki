use std::{borrow::Cow, time::Instant};

use saluki_common::collections::FastHashMap;
use saluki_error::{generic_error, GenericError};
use saluki_metrics::{static_metrics, Counter, Histogram};
use tokio::sync::mpsc;

use super::Dispatchable;
use crate::{components::ComponentContext, topology::OutputName};

// TODO: When we have support for additional static labels on a per-metric basis, add `discard_reason` to
// `events_discarded_total` metric to indicate that it's due to the destination component being disconnected.
#[static_metrics(prefix = "component", labels(component_id, component_type, output))]
#[derive(Clone)]
struct DispatcherMetrics {
    events_sent_total: Counter,
    #[metric(level = trace)]
    send_latency_seconds: Histogram,
    events_discarded_total: Counter,
}

impl DispatcherMetrics {
    fn default_output(context: ComponentContext) -> Self {
        Self::with_output_name(context, "_default")
    }

    fn named_output(context: ComponentContext, output_name: &str) -> Self {
        Self::with_output_name(context, output_name)
    }

    fn with_output_name(context: ComponentContext, output_name: &str) -> Self {
        Self::new(context.component_id(), context.component_type().as_str(), output_name)
    }
}

/// A type that can be used as a buffer for dispatching items.
pub trait DispatchBuffer: Dispatchable + Default {
    /// Type of item that can be pushed into the buffer.
    type Item;

    /// Returns the number of items currently in the buffer.
    fn len(&self) -> usize;

    /// Returns `true` if the buffer is full.
    fn is_full(&self) -> bool;

    /// Attempts to push an item into the buffer.
    ///
    /// Returns `Some(item)` if the buffer is full and the item couldn't be pushed.
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
            // Anchor the legitimate zero-sender discard. A wired edge never reaches this branch, so this stays a
            // disconnected-output signal — not a silent-loss-on-a-wired-edge violation.
            saluki_antithesis::sometimes!(
                true,
                "events discarded on a zero-sender output",
                { "items": item_count }
            );
            return Ok(());
        }

        let start = Instant::now();
        let item_count = item.item_count();

        // Send the item to all senders except the last one by cloning the item.
        saluki_antithesis::always_gt!(self.senders.len(), 0, "dispatcher fanout has at least one sender");
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
    /// If the output doesn't exist, an error is returned.
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
    /// If the default output isn't set, or there is an error sending to the default output, an error is returned.
    pub async fn dispatch(&self, item: T) -> Result<(), GenericError> {
        self.dispatch_inner(None, item).await
    }

    /// Dispatches the given items to the given named output.
    ///
    /// # Errors
    ///
    /// If a output of the given name isn't set, or there is an error sending to the output, an error is returned.
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
    /// This should generally be used if the items being dispatched aren't already collected in a container, or exposed
    /// via an iterable type. It allows for efficiently buffering items one-by-one before dispatching them to the
    /// underlying output.
    ///
    /// # Errors
    ///
    /// If the default output hasn't been configured, an error will be returned.
    pub fn buffered(&self) -> Result<BufferedDispatcher<'_, T>, GenericError> {
        self.get_default_output().map(BufferedDispatcher::new)
    }

    /// Creates a buffered dispatcher for the given named output.
    ///
    /// This should generally be used if the items being dispatched aren't already collected in a container, or exposed
    /// via an iterable type. It allows for efficiently buffering items one-by-one before dispatching them to the
    /// underlying output.
    ///
    /// # Errors
    ///
    /// If the given named output hasn't been configured, an error will be returned.
    pub fn buffered_named<N>(&self, output_name: N) -> Result<BufferedDispatcher<'_, T>, GenericError>
    where
        N: AsRef<str>,
    {
        self.get_named_output(output_name.as_ref()).map(BufferedDispatcher::new)
    }

    /// Dispatches a single item to the default output.
    ///
    /// # Errors
    ///
    /// If the default output isn't set, or there is an error sending to the default output, an error is returned.
    pub async fn dispatch_one(&self, item: T::Item) -> Result<(), GenericError> {
        self.dispatch_one_inner(None, item).await
    }

    /// Dispatches a single item to the given named output.
    ///
    /// # Errors
    ///
    /// If an output of the given name isn't set, or there is an error sending to the output, an error is returned.
    pub async fn dispatch_one_named<N>(&self, output_name: N, item: T::Item) -> Result<(), GenericError>
    where
        N: AsRef<str>,
    {
        self.dispatch_one_inner(Some(output_name.as_ref()), item).await
    }

    async fn dispatch_one_inner(&self, output_name: Option<&str>, item: T::Item) -> Result<(), GenericError> {
        let target = match output_name {
            None => self.get_default_output()?,
            Some(name) => self.get_named_output(name)?,
        };

        let mut buffer = T::default();
        if buffer.try_push(item).is_some() {
            return Err(generic_error!("Default-constructed buffer rejected a single item."));
        }
        target.send(buffer).await
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

    /// One output-kind case: the output to operate on, plus the `output` metric label it maps to.
    ///
    /// Every dispatcher operation must behave identically for the default output and for a named output, so the tests
    /// below iterate over these two cases rather than duplicating a default-vs-named function pair for each scenario.
    struct OutputCase {
        output: OutputName,
        metric_label: &'static str,
    }

    fn output_cases() -> [OutputCase; 2] {
        [
            OutputCase {
                output: OutputName::Default,
                metric_label: "_default",
            },
            OutputCase {
                output: OutputName::Given("special".into()),
                metric_label: "special",
            },
        ]
    }

    /// Declares `output` on the dispatcher and attaches the given senders to it.
    fn add_output_with_senders<T: Dispatchable, const N: usize>(
        dispatcher: &mut Dispatcher<T>, output: &OutputName, senders: [mpsc::Sender<T>; N],
    ) {
        dispatcher
            .add_output(output.clone())
            .expect("output should not be declared yet");
        for sender in senders {
            dispatcher
                .attach_sender_to_output(output, sender)
                .expect("output should exist after being declared");
        }
    }

    /// Dispatches `item` to `output`, choosing the default or named entry point as appropriate.
    async fn dispatch_to<T: Dispatchable>(
        dispatcher: &Dispatcher<T>, output: &OutputName, item: T,
    ) -> Result<(), GenericError> {
        match output {
            OutputName::Default => dispatcher.dispatch(item).await,
            OutputName::Given(name) => dispatcher.dispatch_named(name.as_ref(), item).await,
        }
    }

    /// Creates a buffered dispatcher for `output`, choosing the default or named entry point as appropriate.
    fn buffered_for<'a, T: DispatchBuffer>(
        dispatcher: &'a Dispatcher<T>, output: &OutputName,
    ) -> Result<BufferedDispatcher<'a, T>, GenericError> {
        match output {
            OutputName::Default => dispatcher.buffered(),
            OutputName::Given(name) => dispatcher.buffered_named(name.as_ref()),
        }
    }

    /// Dispatches a single item to `output`, choosing the default or named entry point as appropriate.
    async fn dispatch_one_to<T: DispatchBuffer>(
        dispatcher: &Dispatcher<T>, output: &OutputName, item: T::Item,
    ) -> Result<(), GenericError> {
        match output {
            OutputName::Default => dispatcher.dispatch_one(item).await,
            OutputName::Given(name) => dispatcher.dispatch_one_named(name.as_ref(), item).await,
        }
    }

    /// Returns whether `output` is connected, choosing the default or named query as appropriate.
    fn is_output_connected<T: Dispatchable>(dispatcher: &Dispatcher<T>, output: &OutputName) -> bool {
        match output {
            OutputName::Default => dispatcher.is_default_output_connected(),
            OutputName::Given(name) => dispatcher.is_named_output_connected(name.as_ref()),
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
    async fn dispatch_delivers_item_to_each_output_kind() {
        // Dispatching a single item to a wired output delivers it unchanged, for both the default and a named output.
        for case in output_cases() {
            let mut dispatcher = unbuffered_dispatcher::<SingleEvent<usize>>();
            let (tx, mut rx) = mpsc::channel(1);
            add_output_with_senders(&mut dispatcher, &case.output, [tx]);

            let input_item = 42.into();
            dispatch_to(&dispatcher, &case.output, input_item).await.unwrap();

            let output_item = rx.try_recv().expect("input item should have been dispatched");
            assert_eq!(output_item, input_item);
        }
    }

    #[tokio::test]
    async fn dispatch_fans_out_to_all_senders_for_each_output_kind() {
        // With multiple senders attached to an output, a dispatched item is delivered (cloned) to every sender.
        for case in output_cases() {
            let mut dispatcher = unbuffered_dispatcher::<SingleEvent<usize>>();
            let (tx1, mut rx1) = mpsc::channel(1);
            let (tx2, mut rx2) = mpsc::channel(1);
            add_output_with_senders(&mut dispatcher, &case.output, [tx1, tx2]);

            let input_item = 42.into();
            dispatch_to(&dispatcher, &case.output, input_item).await.unwrap();

            assert_eq!(rx1.try_recv().expect("first sender should receive"), input_item);
            assert_eq!(rx2.try_recv().expect("second sender should receive"), input_item);
        }
    }

    #[tokio::test]
    async fn dispatch_to_unconfigured_output_errors() {
        // Dispatching to an output that was never declared fails with the documented error for that output kind.
        for case in output_cases() {
            let dispatcher = unbuffered_dispatcher::<SingleEvent<()>>();
            let err = dispatch_to(&dispatcher, &case.output, ().into())
                .await
                .expect_err("dispatch to an unconfigured output must fail");
            let msg = err.to_string();
            match &case.output {
                OutputName::Default => assert_eq!(msg, "No default output declared."),
                OutputName::Given(_) => assert!(msg.contains("No output named"), "got: {msg}"),
            }
        }
    }

    #[tokio::test]
    async fn buffered_dispatch_flushes_partial_buffer() {
        // A single buffered push, once flushed, arrives as a one-element buffer on the wired output.
        for case in output_cases() {
            let mut dispatcher = buffered_dispatcher::<FixedUsizeVec<4>>();
            let (tx, mut rx) = mpsc::channel(1);
            add_output_with_senders(&mut dispatcher, &case.output, [tx]);

            let input_item = 42;
            let mut buffered = buffered_for(&dispatcher, &case.output).unwrap();
            buffered.push(input_item).await.unwrap();
            let flushed_len = buffered.flush().await.unwrap();
            assert_eq!(flushed_len, 1);

            let output_item = rx.try_recv().expect("input item should have been dispatched");
            assert_eq!(output_item.len(), 1);
            assert_eq!(output_item[0], input_item);
        }
    }

    #[tokio::test]
    async fn buffered_dispatch_flushes_full_buffer_during_push() {
        // Pushing more items than a single buffer can hold flushes the full buffer mid-push, so the items arrive as
        // successive buffers (four, then the remaining two).
        for case in output_cases() {
            let mut dispatcher = buffered_dispatcher::<FixedUsizeVec<4>>();
            let (tx, mut rx) = mpsc::channel(2);
            add_output_with_senders(&mut dispatcher, &case.output, [tx]);

            let input_items: Vec<usize> = vec![1, 2, 3, 4, 5, 6];
            let mut buffered = buffered_for(&dispatcher, &case.output).unwrap();
            for item in &input_items {
                buffered.push(*item).await.unwrap();
            }
            let flushed_len = buffered.flush().await.unwrap();
            assert_eq!(flushed_len, input_items.len());

            let first = rx.try_recv().expect("first buffer should have been dispatched");
            assert_eq!(first.len(), 4);
            assert_eq!(first[0..4], input_items[0..4]);

            let second = rx.try_recv().expect("second buffer should have been dispatched");
            assert_eq!(second.len(), 2);
            assert_eq!(second[0..2], input_items[4..6]);
        }
    }

    #[tokio::test]
    async fn buffered_dispatch_fans_out_partial_buffer() {
        // A buffered push flushed to an output with multiple senders is delivered to every sender.
        for case in output_cases() {
            let mut dispatcher = buffered_dispatcher::<FixedUsizeVec<4>>();
            let (tx1, mut rx1) = mpsc::channel(1);
            let (tx2, mut rx2) = mpsc::channel(1);
            add_output_with_senders(&mut dispatcher, &case.output, [tx1, tx2]);

            let input_item = 42;
            let mut buffered = buffered_for(&dispatcher, &case.output).unwrap();
            buffered.push(input_item).await.unwrap();
            let flushed_len = buffered.flush().await.unwrap();
            assert_eq!(flushed_len, 1);

            let out1 = rx1.try_recv().expect("first sender should receive");
            assert_eq!(out1.len(), 1);
            assert_eq!(out1[0], input_item);

            let out2 = rx2.try_recv().expect("second sender should receive");
            assert_eq!(out2.len(), 1);
            assert_eq!(out2[0], input_item);
        }
    }

    #[tokio::test]
    async fn dispatch_to_output_without_senders_succeeds() {
        // Dispatching to a declared output that has no senders attached succeeds (the item is discarded, not an error).
        for case in output_cases() {
            let mut dispatcher = unbuffered_dispatcher::<SingleEvent<u32>>();
            dispatcher
                .add_output(case.output.clone())
                .expect("should be able to declare the output");

            dispatch_to(&dispatcher, &case.output, 42.into())
                .await
                .expect("dispatch to a senderless output should succeed");
        }
    }

    #[tokio::test]
    async fn disconnected_output_records_discarded_events() {
        // Dispatching to a declared-but-senderless output records the item count as discarded (and none as sent, with
        // no send-latency samples), for both the default and a named output.
        for case in output_cases() {
            let recorder = DebuggingRecorder::new();
            let snapshotter = recorder.snapshotter();
            let output = case.output.clone();
            let dispatcher = metrics::with_local_recorder(&recorder, || {
                let mut dispatcher = buffered_dispatcher::<FixedUsizeVec<4>>();
                dispatcher
                    .add_output(output)
                    .expect("should not fail to declare the output");
                dispatcher
            });

            // Dispatch an item with a count of 1 to the senderless output; it is discarded rather than sent.
            let mut single_item = FixedUsizeVec::<4>::default();
            assert_eq!(None, single_item.try_push(42));
            let single_item_count = single_item.item_count() as u64;
            dispatch_to(&dispatcher, &case.output, single_item)
                .await
                .expect("should not fail to dispatch");

            let (events_sent, events_discarded, send_latencies) = get_output_metrics(&snapshotter, case.metric_label);
            assert_eq!(events_sent, 0);
            assert_eq!(events_discarded, single_item_count);
            assert!(send_latencies.is_empty());

            // Dispatch a second item, this time with a count of 3.
            let mut multiple_items = FixedUsizeVec::<4>::default();
            assert_eq!(None, multiple_items.try_push(42));
            assert_eq!(None, multiple_items.try_push(12345));
            assert_eq!(None, multiple_items.try_push(1337));
            let multiple_items_count = multiple_items.item_count() as u64;
            dispatch_to(&dispatcher, &case.output, multiple_items)
                .await
                .expect("should not fail to dispatch");

            let (events_sent, events_discarded, send_latencies) = get_output_metrics(&snapshotter, case.metric_label);
            assert_eq!(events_sent, 0);
            assert_eq!(events_discarded, multiple_items_count);
            assert!(send_latencies.is_empty());
        }
    }

    #[tokio::test]
    async fn output_connected_reflects_sender_attachment() {
        // The connected-query reports false before an output exists, false once declared without senders, and true
        // only after a sender is attached -- for both the default and a named output.
        for case in output_cases() {
            let mut dispatcher = unbuffered_dispatcher::<SingleEvent<u32>>();

            assert!(
                !is_output_connected(&dispatcher, &case.output),
                "an undeclared output must report as not connected"
            );

            dispatcher
                .add_output(case.output.clone())
                .expect("should be able to declare the output");
            assert!(
                !is_output_connected(&dispatcher, &case.output),
                "a declared output with no senders must report as not connected"
            );

            let (tx, _rx) = mpsc::channel(1);
            dispatcher
                .attach_sender_to_output(&case.output, tx)
                .expect("should be able to attach a sender");
            assert!(
                is_output_connected(&dispatcher, &case.output),
                "an output with a sender attached must report as connected"
            );

            // A query for a different, undeclared named output is always false.
            assert!(
                !dispatcher.is_named_output_connected("nonexistent_output"),
                "an unknown named output must report as not connected"
            );
        }
    }

    #[tokio::test]
    async fn dispatch_one_wraps_item_in_single_element_buffer() {
        // `dispatch_one`/`dispatch_one_named` wrap a single item into a one-element buffer on the wired output.
        for case in output_cases() {
            let mut dispatcher = buffered_dispatcher::<FixedUsizeVec<4>>();
            let (tx, mut rx) = mpsc::channel(1);
            add_output_with_senders(&mut dispatcher, &case.output, [tx]);

            let input_item = 42;
            dispatch_one_to(&dispatcher, &case.output, input_item).await.unwrap();

            let output_item = rx.try_recv().expect("input item should have been dispatched");
            assert_eq!(output_item.len(), 1);
            assert_eq!(output_item[0], input_item);
        }
    }

    #[tokio::test]
    async fn dispatch_one_to_unconfigured_output_errors() {
        // `dispatch_one`/`dispatch_one_named` to an output that was never declared fails with the documented error for
        // that output kind.
        for case in output_cases() {
            let dispatcher = buffered_dispatcher::<FixedUsizeVec<4>>();
            let err = dispatch_one_to(&dispatcher, &case.output, 42)
                .await
                .expect_err("dispatch_one to an unconfigured output must fail");
            let msg = err.to_string();
            match &case.output {
                OutputName::Default => assert_eq!(msg, "No default output declared."),
                OutputName::Given(_) => assert!(msg.contains("No output named"), "got: {msg}"),
            }
        }
    }

    #[tokio::test]
    async fn add_output_rejects_duplicate() {
        // `add_output`'s documented error: declaring the same output twice is rejected with an "already exists" error,
        // for both the default and a named output.
        for case in output_cases() {
            let mut dispatcher = unbuffered_dispatcher::<SingleEvent<u32>>();
            dispatcher
                .add_output(case.output.clone())
                .expect("first declaration should succeed");

            let err = dispatcher
                .add_output(case.output.clone())
                .expect_err("declaring the same output twice must fail");
            assert!(err.to_string().contains("already exists"), "got: {err}");
        }
    }

    #[tokio::test]
    async fn attach_sender_to_undeclared_output_errors() {
        // `attach_sender_to_output`'s documented error: attaching a sender to an output that hasn't been declared fails
        // with the documented error for that output kind.
        for case in output_cases() {
            let mut dispatcher = unbuffered_dispatcher::<SingleEvent<u32>>();
            let (tx, _rx) = mpsc::channel(1);
            let err = dispatcher
                .attach_sender_to_output(&case.output, tx)
                .expect_err("attaching to an undeclared output must fail");
            let msg = err.to_string();
            match &case.output {
                OutputName::Default => assert_eq!(msg, "No default output declared."),
                OutputName::Given(_) => assert!(msg.contains("does not exist"), "got: {msg}"),
            }
        }
    }

    #[tokio::test]
    async fn dispatch_one_errors_when_buffer_cannot_hold_a_single_item() {
        // Defensive guard in `dispatch_one_inner`: if the default-constructed buffer can't accept even one item, it
        // returns a documented error rather than silently dropping the item. This branch is only reachable via a
        // degenerate zero-capacity buffer type; every real buffer (capacity >= 1) accepts the first item.
        let mut dispatcher = buffered_dispatcher::<FixedUsizeVec<0>>();
        let (tx, _rx) = mpsc::channel(1);
        add_output_with_senders(&mut dispatcher, &OutputName::Default, [tx]);

        let err = dispatcher
            .dispatch_one(42)
            .await
            .expect_err("a zero-capacity buffer must be rejected, not silently drop the item");
        assert!(err.to_string().contains("rejected a single item"), "got: {err}");
    }
}
