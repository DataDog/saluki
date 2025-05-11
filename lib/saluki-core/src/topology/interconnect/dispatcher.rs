use std::time::Instant;

use metrics::{Counter, Histogram, SharedString};
use saluki_common::collections::FastHashMap;
use saluki_error::{generic_error, GenericError};
use saluki_metrics::MetricsBuilder;
use smallvec::SmallVec;
use tokio::sync::mpsc;

use super::FixedSizeEventBuffer;
use crate::{
    components::ComponentContext,
    data_model::event::Event,
    observability::ComponentMetricsExt as _,
    pooling::{ElasticObjectPool, ObjectPool},
    topology::OutputName,
};

type DispatcherBufferPool = ElasticObjectPool<FixedSizeEventBuffer>;

struct DispatcherMetrics {
    events_sent: Counter,
    send_latency: Histogram,
}

impl DispatcherMetrics {
    fn default_output(context: ComponentContext) -> Self {
        Self::with_output_name(context, "_default")
    }

    fn named_output(context: ComponentContext, output_name: &str) -> Self {
        Self::with_output_name(context, output_name.to_string())
    }

    fn with_output_name<N>(context: ComponentContext, output_name: N) -> Self
    where
        N: Into<SharedString>,
    {
        let metrics_builder = MetricsBuilder::from_component_context(context).add_default_tag(("output", output_name));

        Self {
            events_sent: metrics_builder.register_debug_counter("component_events_sent_total"),
            send_latency: metrics_builder.register_debug_histogram("component_send_latency_seconds"),
        }
    }
}

struct DispatchTarget {
    metrics: DispatcherMetrics,
    senders: Vec<mpsc::Sender<FixedSizeEventBuffer>>,
}

impl DispatchTarget {
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

    fn add_sender(&mut self, sender: mpsc::Sender<FixedSizeEventBuffer>) {
        self.senders.push(sender);
    }

    fn metrics(&self) -> &DispatcherMetrics {
        &self.metrics
    }

    fn senders(&self) -> &[mpsc::Sender<FixedSizeEventBuffer>] {
        &self.senders
    }
}

struct BufferedSender<'a> {
    buffer: Option<FixedSizeEventBuffer>,
    buffer_pool: &'a DispatcherBufferPool,
    metrics: &'a DispatcherMetrics,
    sender: &'a mpsc::Sender<FixedSizeEventBuffer>,
}

impl<'a> BufferedSender<'a> {
    fn new(
        buffer_pool: &'a DispatcherBufferPool, metrics: &'a DispatcherMetrics,
        sender: &'a mpsc::Sender<FixedSizeEventBuffer>,
    ) -> Self {
        Self {
            buffer: None,
            buffer_pool,
            metrics,
            sender,
        }
    }

    async fn send_buffer_direct(&self, buffer: FixedSizeEventBuffer) -> Result<(), GenericError> {
        let start = Instant::now();

        let buffer_len = buffer.len();
        let result = self
            .sender
            .send(buffer)
            .await
            .map_err(|_| generic_error!("Failed to send to output; {} events lost", buffer_len));

        let elapsed = start.elapsed();
        self.metrics.send_latency.record(elapsed);

        result
    }

    async fn try_flush_buffer(&mut self) -> Result<(), GenericError> {
        match self.buffer.take() {
            None => Ok(()),
            Some(buffer) => {
                let buffer_len = buffer.len();
                if buffer_len > 0 {
                    self.send_buffer_direct(buffer).await
                } else {
                    Ok(())
                }
            }
        }
    }

    async fn push(&mut self, event: Event) -> Result<(), GenericError> {
        // If our buffer is full, consume it and dispatch it, before acquiring a new one.
        let buffer_full = self.buffer.as_ref().is_some_and(|b| b.is_full());
        if buffer_full {
            self.try_flush_buffer().await?;
        }

        // Ensure we have a buffer to push into.
        if self.buffer.is_none() {
            self.buffer = Some(self.buffer_pool.acquire().await);
        }

        // Push the event into the buffer.
        let buffer = self.buffer.as_mut().expect("buffer should be present");
        if buffer.try_push(event).is_some() {
            panic!("buffer should never be full at this point");
        }

        Ok(())
    }

    async fn flush(mut self) -> Result<(), GenericError> {
        self.try_flush_buffer().await?;

        Ok(())
    }
}

enum SenderBackend<'a> {
    Single(BufferedSender<'a>),
    Multiple(SmallVec<[BufferedSender<'a>; 4]>),
}

/// A buffered dispatcher.
///
/// `BufferedDispatcher` provides an efficient and ergonomic interface to `Dispatcher` that allows for writing events
/// one-by-one into batches, which are then dispatched to the configured output as needed. This allows callers to focus
/// on the logic around what events to send, without needing to worry about the details of event buffer sizing or
/// flushing.
pub struct BufferedDispatcher<'a> {
    metrics: &'a DispatcherMetrics,
    flushed_len: usize,
    backend: SenderBackend<'a>,
}

impl<'a> BufferedDispatcher<'a> {
    fn new(buffer_pool: &'a DispatcherBufferPool, target: &'a DispatchTarget) -> Self {
        let metrics = target.metrics();
        let senders = target.senders();

        let backend = if senders.len() == 1 {
            SenderBackend::Single(BufferedSender::new(buffer_pool, metrics, &senders[0]))
        } else {
            let senders = senders
                .iter()
                .map(|sender| BufferedSender::new(buffer_pool, metrics, sender))
                .collect::<SmallVec<[_; 4]>>();
            SenderBackend::Multiple(senders)
        };

        Self {
            metrics,
            flushed_len: 0,
            backend,
        }
    }

    /// Pushes an event into the buffered dispatcher.
    ///
    /// # Errors
    ///
    /// If there is an error flushing events to the output, an error is returned.
    pub async fn push(&mut self, event: Event) -> Result<(), GenericError> {
        match &mut self.backend {
            SenderBackend::Single(sender) => sender.push(event).await?,
            SenderBackend::Multiple(senders) => {
                let mut event = Some(event);
                let last_sender_idx = senders.len();

                for (sender_idx, sender) in senders.iter_mut().enumerate() {
                    let event = if sender_idx == last_sender_idx {
                        event.take().unwrap()
                    } else {
                        event.clone().unwrap()
                    };

                    sender.push(event).await?;
                }
            }
        }

        self.flushed_len += 1;

        Ok(())
    }

    /// Consumes this buffered dispatcher and sends/flushes all input events to the underlying output.
    ///
    /// If flushing is successful, `Ok(flushed)` is returned, where `flushed` is the total number of events that
    /// have been flushed through this buffered dispatcher.
    ///
    /// # Errors
    ///
    /// If there is an error sending events to the output, an error is returned.
    pub async fn send_all<I>(mut self, events: I) -> Result<usize, GenericError>
    where
        I: IntoIterator<Item = Event>,
    {
        for event in events {
            self.push(event).await?;
        }

        self.flush().await
    }

    /// Consumes this buffered dispatcher, flushing any buffered events to the underlying output.
    ///
    /// If flushing is successful, `Ok(flushed)` is returned, where `flushed` is the total number of events that have
    /// been flushed through this buffered dispatcher.
    ///
    /// # Errors
    ///
    /// If there is an error sending events to the output, an error is returned.
    pub async fn flush(self) -> Result<usize, GenericError> {
        match self.backend {
            SenderBackend::Single(sender) => sender.flush().await?,
            SenderBackend::Multiple(senders) => {
                for sender in senders {
                    sender.flush().await?;
                }
            }
        }

        self.metrics.events_sent.increment(self.flushed_len as u64);

        Ok(self.flushed_len)
    }
}

/// Event dispatcher.
///
/// [`Dispatcher`] provides an ergonomic interface for sending inputs to a downstream component. It has support for
/// multiple outputs (a default output, and additional "named" outputs) and provides telemetry around the number of
/// dispatched events as well as the latency of sending events.
pub struct Dispatcher {
    context: ComponentContext,
    buffer_pool: DispatcherBufferPool,
    default: Option<DispatchTarget>,
    targets: FastHashMap<String, DispatchTarget>,
}

impl Dispatcher {
    /// Create a new `Dispatcher` for the given component context.
    pub fn new(context: ComponentContext, buffer_pool: DispatcherBufferPool) -> Self {
        Self {
            context,
            buffer_pool,
            default: None,
            targets: FastHashMap::default(),
        }
    }

    /// Adds an output to the dispatcher, attached to the given sender.
    pub fn add_output(&mut self, output_name: OutputName, sender: mpsc::Sender<FixedSizeEventBuffer>) {
        let target = match output_name {
            OutputName::Default => self
                .default
                .get_or_insert_with(|| DispatchTarget::default_output(self.context.clone())),
            OutputName::Given(name) => self
                .targets
                .entry(name.to_string())
                .or_insert_with(|| DispatchTarget::named_output(self.context.clone(), &name)),
        };
        target.add_sender(sender);
    }

    /// Creates a buffered dispatcher for the default output.
    ///
    /// This should generally be used if the events being dispatched are not already collected in a container, or exposed
    /// via an iterable type. It allows for efficiently buffering events one-by-one before dispatching them to the
    /// underlying output.
    ///
    /// # Errors
    ///
    /// If the default output has not been configured, an error will be returned.
    pub fn buffered(&self) -> Result<BufferedDispatcher<'_>, GenericError> {
        let target = self
            .default
            .as_ref()
            .ok_or_else(|| generic_error!("No default output declared."))?;
        Ok(BufferedDispatcher::new(&self.buffer_pool, target))
    }

    /// Creates a buffered dispatcher for the given named output.
    ///
    /// This should generally be used if the events being dispatched are not already collected in a container, or exposed
    /// via an iterable type. It allows for efficiently buffering events one-by-one before dispatching them to the
    /// underlying output.
    ///
    /// # Errors
    ///
    /// If the given named output has not been configured, an error will be returned.
    pub fn buffered_named<N>(&self, output_name: N) -> Result<BufferedDispatcher<'_>, GenericError>
    where
        N: AsRef<str>,
    {
        let target = self
            .targets
            .get(output_name.as_ref())
            .ok_or_else(|| generic_error!("No output named '{}' declared.", output_name.as_ref()))?;
        Ok(BufferedDispatcher::new(&self.buffer_pool, target))
    }

    /// Dispatches the given events to the default output.
    ///
    /// If dispatching is successful, `Ok(flushed)` is returned, where `flushed` is the total number of events that have
    /// been dispatched.
    ///
    /// # Errors
    ///
    /// If the default output is not set, or there is an error sending to the default output, an error is returned.
    pub async fn dispatch<I>(&self, events: I) -> Result<usize, GenericError>
    where
        I: IntoIterator<Item = Event>,
    {
        self.dispatch_inner::<String, _>(None, events).await
    }

    /// Dispatches the given events to the given named output.
    ///
    /// If dispatching is successful, `Ok(flushed)` is returned, where `flushed` is the total number of events that have
    /// been dispatched.
    ///
    /// # Errors
    ///
    /// If a output of the given name is not set, or there is an error sending to the output, an error is returned.
    pub async fn dispatch_named<N, I>(&self, output_name: N, events: I) -> Result<usize, GenericError>
    where
        N: AsRef<str>,
        I: IntoIterator<Item = Event>,
    {
        self.dispatch_inner(Some(output_name), events).await
    }

    async fn dispatch_inner<N, I>(&self, output_name: Option<N>, events: I) -> Result<usize, GenericError>
    where
        N: AsRef<str>,
        I: IntoIterator<Item = Event>,
    {
        let mut buffered_dispatcher = match output_name {
            None => self.buffered()?,
            Some(name) => self.buffered_named(name)?,
        };

        for event in events {
            buffered_dispatcher.push(event).await?;
        }

        buffered_dispatcher.flush().await
    }

    /// Dispatches the given event buffer to the default output.
    ///
    /// This is provided for special cases where an event buffer is already held and should be dispatched directly to an
    /// output, rather than needing to submit events one-by-one such as when using [`buffered`][Self::buffered].
    ///
    /// # Errors
    ///
    /// If the default output is not set, or there is an error sending to the default output, an error is returned.
    pub async fn dispatch_buffer(&self, buffer: FixedSizeEventBuffer) -> Result<(), GenericError> {
        self.dispatch_buffer_inner::<String>(None, buffer).await
    }

    /// Dispatches the given event buffer to the given named output.
    ///
    /// This is provided for special cases where an event buffer is already held and should be dispatched directly to an
    /// output, rather than needing to submit events one-by-one such as when using [`buffered_named`][Self::buffered_named].
    ///
    /// # Errors
    ///
    /// If a output of the given name is not set, or there is an error sending to the output, an error is returned.
    pub async fn dispatch_buffer_named<N>(
        &self, output_name: N, buffer: FixedSizeEventBuffer,
    ) -> Result<(), GenericError>
    where
        N: AsRef<str>,
    {
        self.dispatch_buffer_inner(Some(output_name), buffer).await
    }

    async fn dispatch_buffer_inner<N>(
        &self, output_name: Option<N>, buffer: FixedSizeEventBuffer,
    ) -> Result<(), GenericError>
    where
        N: AsRef<str>,
    {
        let target = match output_name {
            None => self
                .default
                .as_ref()
                .ok_or_else(|| generic_error!("No default output declared."))?,
            Some(name) => self
                .targets
                .get(name.as_ref())
                .ok_or_else(|| generic_error!("No output named '{}' declared.", name.as_ref()))?,
        };

        let metrics = target.metrics();
        let senders = target.senders();

        // Nothing to do if the buffer is empty.
        let buffer_len = buffer.len();
        if buffer_len == 0 {
            return Ok(());
        }

        // TODO: Ideally, we should be tracking the send latency for each downstream component that we're sending to
        // here instead of for the overall dispatching operation.

        // Write all of the input events to each buffered sender, except for the last, which will be sent the original
        // event buffer directly. This lets us avoid an additional clone of the event buffer.
        let mut buffered_senders = senders
            .iter()
            .take(senders.len() - 1)
            .map(|sender| BufferedSender::new(&self.buffer_pool, metrics, sender))
            .collect::<Vec<_>>();

        for event in &buffer {
            for buffered_sender in &mut buffered_senders {
                buffered_sender.push(event.clone()).await?;
            }
        }

        // Flush the buffered senders to ensure all events are dispatched.
        for buffered_sender in buffered_senders {
            buffered_sender.flush().await?;
        }

        // Finally, send the original event buffer to the last sender.
        let last_sender = target.senders().last().expect("last sender should be present");
        last_sender
            .send(buffer)
            .await
            .map_err(|_| generic_error!("Failed to send to output; {} events lost", buffer_len))?;

        metrics.events_sent.increment(buffer_len as u64);

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    // TODO: Tests asserting we emit metrics, and the right metrics.

    use tokio_test::{task::spawn as test_spawn, *};

    use super::*;
    use crate::{
        data_model::event::{metric::Metric, Event},
        topology::interconnect::FixedSizeEventBufferInner,
    };

    fn create_dispatcher(event_buffers: usize, buffer_size: usize) -> (Dispatcher, DispatcherBufferPool) {
        let component_context = ComponentContext::test_source("dispatcher_test");
        let (buffer_pool, _) = ElasticObjectPool::with_builder("test", 1, event_buffers, move || {
            FixedSizeEventBufferInner::with_capacity(buffer_size)
        });

        (Dispatcher::new(component_context, buffer_pool.clone()), buffer_pool)
    }

    #[tokio::test]
    async fn default_output() {
        // Create the dispatcher and wire up a sender to the default output so that we can receive what gets dispatched.
        let (mut dispatcher, _) = create_dispatcher(1, 1);

        let (tx, mut rx) = mpsc::channel(1);
        dispatcher.add_output(OutputName::Default, tx);

        // Create a basic metric event and then dispatch it.
        let metric = Metric::counter("basic_metric", 42.0);

        let mut buffered = dispatcher.buffered().unwrap();
        buffered.push(Event::Metric(metric.clone())).await.unwrap();
        let flushed_len = buffered.flush().await.unwrap();
        assert_eq!(flushed_len, 1);

        // Make sure there's an event buffer waiting for us and that it has one event: the one we sent.
        let dispatched_ebuf = rx.try_recv().expect("event buffer should have been dispatched");
        assert_eq!(dispatched_ebuf.len(), 1);

        let dispatched_metric = dispatched_ebuf
            .into_iter()
            .next()
            .and_then(|event| event.try_into_metric())
            .expect("should be single metric in the buffer");
        assert_eq!(dispatched_metric.context(), metric.context());
    }

    #[tokio::test]
    async fn named_output() {
        // Create the dispatcher and wire up a sender to the named output so that we can receive what gets dispatched.
        let (mut dispatcher, _) = create_dispatcher(1, 1);

        let (tx, mut rx) = mpsc::channel(1);
        dispatcher.add_output(OutputName::Given("metrics".into()), tx);

        // Create a basic metric event and then dispatch it.
        let metric = Metric::counter("basic_metric", 42.0);

        let mut buffered = dispatcher.buffered_named("metrics").unwrap();
        buffered.push(Event::Metric(metric.clone())).await.unwrap();
        let flushed_len = buffered.flush().await.unwrap();
        assert_eq!(flushed_len, 1);

        // Make sure there's an event buffer waiting for us and that it has one event: the one we sent.
        let dispatched_ebuf = rx.try_recv().expect("event buffer should have been dispatched");
        assert_eq!(dispatched_ebuf.len(), 1);

        let dispatched_metric = dispatched_ebuf
            .into_iter()
            .next()
            .and_then(|event| event.try_into_metric())
            .expect("should be single metric in the buffer");
        assert_eq!(dispatched_metric.context(), metric.context());
    }

    #[tokio::test]
    async fn multiple_senders_default_output() {
        // Create the dispatcher and wire up two senders to the default output so that we can receive what gets dispatched.
        let (mut dispatcher, _) = create_dispatcher(2, 1);

        let (tx1, mut rx1) = mpsc::channel(1);
        let (tx2, mut rx2) = mpsc::channel(1);
        dispatcher.add_output(OutputName::Default, tx1);
        dispatcher.add_output(OutputName::Default, tx2);

        // Create a basic metric event and then dispatch it.
        let metric = Metric::counter("basic_metric", 42.0);

        let mut buffered = dispatcher.buffered().unwrap();
        buffered.push(Event::Metric(metric.clone())).await.unwrap();
        let flushed_len = buffered.flush().await.unwrap();
        assert_eq!(flushed_len, 1);

        // Make sure there's an event buffer waiting for us on each received and that it has one event: the one we sent.
        let dispatched_ebuf1 = rx1.try_recv().expect("event buffer should have been dispatched");
        let dispatched_ebuf2 = rx2.try_recv().expect("event buffer should have been dispatched");
        assert_eq!(dispatched_ebuf1.len(), 1);
        assert_eq!(dispatched_ebuf2.len(), 1);

        let dispatched_metric1 = dispatched_ebuf1
            .into_iter()
            .next()
            .and_then(|event| event.try_into_metric())
            .expect("should be single metric in the buffer");
        assert_eq!(dispatched_metric1.context(), metric.context());

        let dispatched_metric2 = dispatched_ebuf2
            .into_iter()
            .next()
            .and_then(|event| event.try_into_metric())
            .expect("should be single metric in the buffer");
        assert_eq!(dispatched_metric2.context(), metric.context());
    }

    #[tokio::test]
    async fn multiple_senders_named_output() {
        // Create the dispatcher and wire up two senders to the default output so that we can receive what gets dispatched.
        let (mut dispatcher, _) = create_dispatcher(2, 1);

        let (tx1, mut rx1) = mpsc::channel(1);
        let (tx2, mut rx2) = mpsc::channel(1);
        dispatcher.add_output(OutputName::Given("metrics".into()), tx1);
        dispatcher.add_output(OutputName::Given("metrics".into()), tx2);

        // Create a basic metric event and then dispatch it.
        let metric = Metric::counter("basic_metric", 42.0);

        let mut buffered = dispatcher.buffered_named("metrics").unwrap();
        buffered.push(Event::Metric(metric.clone())).await.unwrap();
        let flushed_len = buffered.flush().await.unwrap();
        assert_eq!(flushed_len, 1);

        // Make sure there's an event buffer waiting for us on each received and that it has one event: the one we sent.
        let dispatched_ebuf1 = rx1.try_recv().expect("event buffer should have been dispatched");
        let dispatched_ebuf2 = rx2.try_recv().expect("event buffer should have been dispatched");
        assert_eq!(dispatched_ebuf1.len(), 1);
        assert_eq!(dispatched_ebuf2.len(), 1);

        let dispatched_metric1 = dispatched_ebuf1
            .into_iter()
            .next()
            .and_then(|event| event.try_into_metric())
            .expect("should be single metric in the buffer");
        assert_eq!(dispatched_metric1.context(), metric.context());

        let dispatched_metric2 = dispatched_ebuf2
            .into_iter()
            .next()
            .and_then(|event| event.try_into_metric())
            .expect("should be single metric in the buffer");
        assert_eq!(dispatched_metric2.context(), metric.context());
    }

    #[tokio::test]
    async fn default_output_not_set() {
        // Create the dispatcher and try to dispatch an event without setting up a default output.
        let (dispatcher, ebuf_pool) = create_dispatcher(1, 1);

        let result = dispatcher.buffered();
        assert!(result.is_err());

        let ebuf = ebuf_pool.acquire().await;
        let result = dispatcher.dispatch_buffer(ebuf).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn named_output_not_set() {
        // Create the dispatcher and try to dispatch an event without setting up a named output.
        let (dispatcher, ebuf_pool) = create_dispatcher(1, 1);

        let result = dispatcher.buffered_named("metrics");
        assert!(result.is_err());

        let ebuf = ebuf_pool.acquire().await;
        let result = dispatcher.dispatch_buffer_named("metrics", ebuf).await;
        assert!(result.is_err());
    }

    #[test]
    fn multiple_senders_blocks_when_event_buffer_pool_empty() {
        // Create the dispatcher and wire up two senders to the default output so that we can receive what gets
        // dispatched.
        //
        // Crucially, our event buffer pool is set at a fixed size of two, and we'll use this to control if the event
        // buffer pool is empty or not when calling `dispatch`, to ensure that dispatching blocks when the pool is empty
        // and completes when it can acquire the necessary additional buffer.
        let (mut dispatcher, ebuf_pool) = create_dispatcher(2, 1);

        let (tx1, mut rx1) = mpsc::channel(1);
        let (tx2, mut rx2) = mpsc::channel(1);
        dispatcher.add_output(OutputName::Default, tx1);
        dispatcher.add_output(OutputName::Default, tx2);

        // Create a basic metric event and then attempt to dispatch it.
        //
        // Before dispatching, we'll acquire an event buffer in the pool to ensure that the pool only has one event
        // buffer present, since dispatching to two outputs will require two event buffers overall.
        let metric = Metric::counter("basic_metric", 42.0);

        let mut receive1 = test_spawn(rx1.recv());
        let mut receive2 = test_spawn(rx2.recv());
        assert_pending!(receive1.poll());
        assert_pending!(receive2.poll());

        // Grab an event buffer from the pool and hold on to it before trying to dispatch:
        let mut acquire_ebuf = test_spawn(ebuf_pool.acquire());
        let ebuf = assert_ready!(acquire_ebuf.poll());

        let mut buffered = dispatcher.buffered().unwrap();
        let mut buffered_push = test_spawn(buffered.push(Event::Metric(metric.clone())));
        assert_pending!(buffered_push.poll());

        // We know our push call is pending, and neither receiver should have gotten anything yet because we have
        // to acquire all event buffers for each output before we start pushing any events into the output buffers:
        assert_pending!(receive1.poll());
        assert_pending!(receive2.poll());

        // Now drop the spare event buffer, which returns it to the pool and should unblock the push:
        drop(ebuf);
        assert_ready_ok!(buffered_push.poll());
        drop(buffered_push);

        // Finally, flush the buffered dispatcher to ensure all events are dispatched:
        let mut buffered_flush = test_spawn(buffered.flush());
        assert_ready_ok!(buffered_flush.poll());

        let dispatched_ebuf1 = assert_ready!(receive1.poll()).expect("event buffer should have been dispatched");
        assert_eq!(dispatched_ebuf1.len(), 1);

        let dispatched_ebuf2 = assert_ready!(receive2.poll()).expect("event buffer should have been dispatched");
        assert_eq!(dispatched_ebuf2.len(), 1);
    }
}
