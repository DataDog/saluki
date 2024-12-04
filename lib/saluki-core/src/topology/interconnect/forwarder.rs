use std::time::Instant;

use ahash::AHashMap;
use metrics::{Counter, Histogram, SharedString};
use saluki_error::{generic_error, GenericError};
use saluki_event::Event;
use saluki_metrics::MetricsBuilder;
use smallvec::SmallVec;
use tokio::sync::mpsc;

use super::FixedSizeEventBuffer;
use crate::{
    components::ComponentContext,
    observability::ComponentMetricsExt as _,
    pooling::{ElasticObjectPool, ObjectPool},
    topology::OutputName,
};

struct ForwarderMetrics {
    events_sent: Counter,
    forwarding_latency: Histogram,
}

impl ForwarderMetrics {
    pub fn default_output(context: ComponentContext) -> Self {
        Self::with_output_name(context, "_default")
    }

    pub fn named_output(context: ComponentContext, output_name: &str) -> Self {
        Self::with_output_name(context, output_name.to_string())
    }

    fn with_output_name<N>(context: ComponentContext, output_name: N) -> Self
    where
        N: Into<SharedString>,
    {
        let metrics_builder = MetricsBuilder::from_component_context(context).add_default_tag(("output", output_name));

        Self {
            events_sent: metrics_builder.register_debug_counter("component_events_sent_total"),
            forwarding_latency: metrics_builder.register_debug_histogram("component_send_latency_seconds"),
        }
    }
}

struct BufferedSender<'a, O>
where
    O: ObjectPool<Item = FixedSizeEventBuffer>,
{
    buffer: Option<FixedSizeEventBuffer>,
    buffer_pool: &'a O,
    metrics: &'a ForwarderMetrics,
    sender: &'a mpsc::Sender<FixedSizeEventBuffer>,
}

impl<'a, O> BufferedSender<'a, O>
where
    O: ObjectPool<Item = FixedSizeEventBuffer>,
{
    fn new(buffer_pool: &'a O, metrics: &'a ForwarderMetrics, sender: &'a mpsc::Sender<FixedSizeEventBuffer>) -> Self {
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
        self.metrics.forwarding_latency.record(elapsed);

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
        // If our buffer is full, consume it and forward it, before acquiring a new one.
        let buffer_full = self.buffer.as_ref().map_or(false, |b| b.is_full());
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

enum SenderBackend<'a, O>
where
    O: ObjectPool<Item = FixedSizeEventBuffer>,
{
    Single(BufferedSender<'a, O>),
    Multiple(SmallVec<[BufferedSender<'a, O>; 4]>),
}

/// A buffered forwarder.
///
/// `BufferedForwarder` provides an efficient and ergonomic interface to `Forwarder` that allows for writing events
/// one-by-one into batches, which are then forwarded to the configured output as needed. This allows callers to focus
/// on the logic around what events to send, without needing to worry about the details of event buffer sizing or
/// flushing.
pub struct BufferedForwarder<'a, O>
where
    O: ObjectPool<Item = FixedSizeEventBuffer>,
{
    metrics: &'a ForwarderMetrics,
    flushed_len: usize,
    backend: SenderBackend<'a, O>,
}

impl<'a, O> BufferedForwarder<'a, O>
where
    O: ObjectPool<Item = FixedSizeEventBuffer>,
{
    fn new(
        buffer_pool: &'a O, metrics: &'a ForwarderMetrics, senders: &'a [mpsc::Sender<FixedSizeEventBuffer>],
    ) -> Self {
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

    /// Pushes an event into the buffered forwarder.
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

    /// Consumes this buffered forwarder and sends/flushes all input events to the underlying output.
    ///
    /// If flushing is successful, `Ok(flushed)` is returned, where `flushed` is the total number of events that
    /// have been flushed through this buffered forwarder.
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

    /// Consumes this buffered forwarder, flushing any buffered events to the underlying output.
    ///
    /// If flushing is successful, `Ok(flushed)` is returned, where `flushed` is the total number of events that have
    /// been flushed through this buffered forwarder.
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

/// Event forwarder.
///
/// [`Forwarder`] provides an ergonomic interface for forwarding events out of a component. It has support for multiple
/// outputs (a default output, and additional "named" outputs) and provides telemetry around the number of forwarded
/// events as well as the forwarding latency.
pub struct Forwarder {
    context: ComponentContext,
    event_buffer_pool: ElasticObjectPool<FixedSizeEventBuffer>,
    default: Option<(ForwarderMetrics, Vec<mpsc::Sender<FixedSizeEventBuffer>>)>,
    targets: AHashMap<String, (ForwarderMetrics, Vec<mpsc::Sender<FixedSizeEventBuffer>>)>,
}

impl Forwarder {
    /// Create a new `Forwarder` for the given component context.
    pub fn new(context: ComponentContext, event_buffer_pool: ElasticObjectPool<FixedSizeEventBuffer>) -> Self {
        Self {
            context,
            event_buffer_pool,
            default: None,
            targets: AHashMap::new(),
        }
    }

    /// Adds an output to the forwarder, attached to the given sender.
    pub fn add_output(&mut self, output_name: OutputName, sender: mpsc::Sender<FixedSizeEventBuffer>) {
        match output_name {
            OutputName::Default => {
                let (_, senders) = self.default.get_or_insert_with(|| {
                    let metrics = ForwarderMetrics::default_output(self.context.clone());
                    (metrics, Vec::new())
                });
                senders.push(sender);
            }
            OutputName::Given(name) => {
                let (_, senders) = self.targets.entry(name.to_string()).or_insert_with(|| {
                    let metrics = ForwarderMetrics::named_output(self.context.clone(), &name);
                    (metrics, Vec::new())
                });
                senders.push(sender);
            }
        }
    }

    /// Creates a buffered forwarder for the default output.
    ///
    /// This should generally be used if the events being forwarded are not already collected in a container, or exposed
    /// via an iterable type. It allows for efficiently buffering events one-by-one before forwarding them to the
    /// underlying output.
    ///
    /// # Errors
    ///
    /// If the default output has not been configured, an error will be returned.
    pub fn buffered(&self) -> Result<BufferedForwarder<'_, ElasticObjectPool<FixedSizeEventBuffer>>, GenericError> {
        let (metrics, senders) = self
            .default
            .as_ref()
            .ok_or_else(|| generic_error!("No default output declared."))?;
        Ok(BufferedForwarder::new(&self.event_buffer_pool, metrics, senders))
    }

    /// Creates a buffered forwarder for the given named output.
    ///
    /// This should generally be used if the events being forwarded are not already collected in a container, or exposed
    /// via an iterable type. It allows for efficiently buffering events one-by-one before forwarding them to the
    /// underlying output.
    ///
    /// # Errors
    ///
    /// If the given named output has not been configured, an error will be returned.
    pub fn buffered_named<N>(
        &self, output_name: N,
    ) -> Result<BufferedForwarder<'_, ElasticObjectPool<FixedSizeEventBuffer>>, GenericError>
    where
        N: AsRef<str>,
    {
        let (metrics, senders) = self
            .targets
            .get(output_name.as_ref())
            .ok_or_else(|| generic_error!("No output named '{}' declared.", output_name.as_ref()))?;
        Ok(BufferedForwarder::new(&self.event_buffer_pool, metrics, senders))
    }

    /// Forwards the given events to the default output.
    ///
    /// If forwarding is successful, `Ok(flushed)` is returned, where `flushed` is the total number of events that have
    /// been forwarded.    
    ///
    /// ## Errors
    ///
    /// If the default output is not set, or there is an error sending to the default output, an error is returned.
    pub async fn forward<I>(&self, events: I) -> Result<usize, GenericError>
    where
        I: IntoIterator<Item = Event>,
    {
        self.forward_inner::<String, _>(None, events).await
    }

    /// Forwards the given events to the given named output.
    ///
    /// If forwarding is successful, `Ok(flushed)` is returned, where `flushed` is the total number of events that have
    /// been forwarded.
    ///
    /// ## Errors
    ///
    /// If a output of the given name is not set, or there is an error sending to the output, an error is returned.
    pub async fn forward_named<N, I>(&self, output_name: N, events: I) -> Result<usize, GenericError>
    where
        N: AsRef<str>,
        I: IntoIterator<Item = Event>,
    {
        self.forward_inner(Some(output_name), events).await
    }

    async fn forward_inner<N, I>(&self, output_name: Option<N>, events: I) -> Result<usize, GenericError>
    where
        N: AsRef<str>,
        I: IntoIterator<Item = Event>,
    {
        let mut buffered_forwarder = match output_name {
            None => self.buffered()?,
            Some(name) => self.buffered_named(name)?,
        };

        for event in events {
            buffered_forwarder.push(event).await?;
        }

        buffered_forwarder.flush().await
    }

    /// Forwards the given event buffer to the default output.
    ///
    /// This is provided for special cases where an event buffer is already held and should be forwarded directly to an
    /// output, rather than needing to submit events one-by-one such as when using [`buffered`][Self::buffered].
    ///
    /// ## Errors
    ///
    /// If the default output is not set, or there is an error sending to the default output, an error is returned.
    pub async fn forward_buffer(&self, buffer: FixedSizeEventBuffer) -> Result<(), GenericError> {
        self.forward_buffer_inner::<String>(None, buffer).await
    }

    /// Forwards the given event buffer to the given named output.
    ///
    /// This is provided for special cases where an event buffer is already held and should be forwarded directly to an
    /// output, rather than needing to submit events one-by-one such as when using [`buffered_named`][Self::buffered_named].
    ///
    /// ## Errors
    ///
    /// If a output of the given name is not set, or there is an error sending to the output, an error is returned.
    pub async fn forward_buffer_named<N>(
        &self, output_name: N, buffer: FixedSizeEventBuffer,
    ) -> Result<(), GenericError>
    where
        N: AsRef<str>,
    {
        self.forward_buffer_inner(Some(output_name), buffer).await
    }

    async fn forward_buffer_inner<N>(
        &self, output_name: Option<N>, buffer: FixedSizeEventBuffer,
    ) -> Result<(), GenericError>
    where
        N: AsRef<str>,
    {
        let (metrics, senders) = match output_name {
            None => self
                .default
                .as_ref()
                .ok_or_else(|| generic_error!("No default output declared."))?,
            Some(name) => self
                .targets
                .get(name.as_ref())
                .ok_or_else(|| generic_error!("No output named '{}' declared.", name.as_ref()))?,
        };

        // Nothing to do if the buffer is empty.
        let buffer_len = buffer.len();
        if buffer_len == 0 {
            return Ok(());
        }

        // TODO: Ideally, we should be tracking the forward latency for each downstream component that we're sending to
        // here instead of for the overall forwarding operation.

        // Write all of the input events to each buffered sender, except for the last, which will be sent the original
        // event buffer directly. This lets us avoid an additional clone of the event buffer.
        let mut buffered_senders = senders
            .iter()
            .take(senders.len() - 1)
            .map(|sender| BufferedSender::new(&self.event_buffer_pool, metrics, sender))
            .collect::<Vec<_>>();

        for event in &buffer {
            for buffered_sender in &mut buffered_senders {
                buffered_sender.push(event.clone()).await?;
            }
        }

        // Flush the buffered senders to ensure all events are forwarded.
        for buffered_sender in buffered_senders {
            buffered_sender.flush().await?;
        }

        // Finally, send the original event buffer to the last sender.
        let last_sender = senders.last().expect("last sender should be present");
        last_sender
            .send(buffer)
            .await
            .map_err(|_| generic_error!("Failed to send to output; {} events lost", buffer_len))?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    // TODO: Tests asserting we emit metrics, and the right metrics.

    use saluki_event::{metric::Metric, Event};
    use tokio_test::{task::spawn as test_spawn, *};

    use super::*;
    use crate::topology::interconnect::FixedSizeEventBufferInner;

    fn create_forwarder(
        event_buffers: usize, buffer_size: usize,
    ) -> (Forwarder, ElasticObjectPool<FixedSizeEventBuffer>) {
        let component_context = ComponentContext::test_source("forwarder_test");
        let (event_buffer_pool, _) = ElasticObjectPool::with_builder("test", 1, event_buffers, move || {
            FixedSizeEventBufferInner::with_capacity(buffer_size)
        });

        (
            Forwarder::new(component_context, event_buffer_pool.clone()),
            event_buffer_pool,
        )
    }

    #[tokio::test]
    async fn default_output() {
        // Create the forwarder and wire up a sender to the default output so that we can receive what gets forwarded.
        let (mut forwarder, _) = create_forwarder(1, 1);

        let (tx, mut rx) = mpsc::channel(1);
        forwarder.add_output(OutputName::Default, tx);

        // Create a basic metric event and then forward it.
        let metric = Metric::counter("basic_metric", 42.0);

        let mut buffered = forwarder.buffered().unwrap();
        buffered.push(Event::Metric(metric.clone())).await.unwrap();
        let flushed_len = buffered.flush().await.unwrap();
        assert_eq!(flushed_len, 1);

        // Make sure there's an event buffer waiting for us and that it has one event: the one we sent.
        let forwarded_ebuf = rx.try_recv().expect("event buffer should have been forwarded");
        assert_eq!(forwarded_ebuf.len(), 1);

        let forwarded_metric = forwarded_ebuf
            .into_iter()
            .next()
            .and_then(|event| event.try_into_metric())
            .expect("should be single metric in the buffer");
        assert_eq!(forwarded_metric.context(), metric.context());
    }

    #[tokio::test]
    async fn named_output() {
        // Create the forwarder and wire up a sender to the named output so that we can receive what gets forwarded.
        let (mut forwarder, _) = create_forwarder(1, 1);

        let (tx, mut rx) = mpsc::channel(1);
        forwarder.add_output(OutputName::Given("metrics".into()), tx);

        // Create a basic metric event and then forward it.
        let metric = Metric::counter("basic_metric", 42.0);

        let mut buffered = forwarder.buffered_named("metrics").unwrap();
        buffered.push(Event::Metric(metric.clone())).await.unwrap();
        let flushed_len = buffered.flush().await.unwrap();
        assert_eq!(flushed_len, 1);

        // Make sure there's an event buffer waiting for us and that it has one event: the one we sent.
        let forwarded_ebuf = rx.try_recv().expect("event buffer should have been forwarded");
        assert_eq!(forwarded_ebuf.len(), 1);

        let forwarded_metric = forwarded_ebuf
            .into_iter()
            .next()
            .and_then(|event| event.try_into_metric())
            .expect("should be single metric in the buffer");
        assert_eq!(forwarded_metric.context(), metric.context());
    }

    #[tokio::test]
    async fn multiple_senders_default_output() {
        // Create the forwarder and wire up two senders to the default output so that we can receive what gets forwarded.
        let (mut forwarder, _) = create_forwarder(2, 1);

        let (tx1, mut rx1) = mpsc::channel(1);
        let (tx2, mut rx2) = mpsc::channel(1);
        forwarder.add_output(OutputName::Default, tx1);
        forwarder.add_output(OutputName::Default, tx2);

        // Create a basic metric event and then forward it.
        let metric = Metric::counter("basic_metric", 42.0);

        let mut buffered = forwarder.buffered().unwrap();
        buffered.push(Event::Metric(metric.clone())).await.unwrap();
        let flushed_len = buffered.flush().await.unwrap();
        assert_eq!(flushed_len, 1);

        // Make sure there's an event buffer waiting for us on each received and that it has one event: the one we sent.
        let forwarded_ebuf1 = rx1.try_recv().expect("event buffer should have been forwarded");
        let forwarded_ebuf2 = rx2.try_recv().expect("event buffer should have been forwarded");
        assert_eq!(forwarded_ebuf1.len(), 1);
        assert_eq!(forwarded_ebuf2.len(), 1);

        let forwarded_metric1 = forwarded_ebuf1
            .into_iter()
            .next()
            .and_then(|event| event.try_into_metric())
            .expect("should be single metric in the buffer");
        assert_eq!(forwarded_metric1.context(), metric.context());

        let forwarded_metric2 = forwarded_ebuf2
            .into_iter()
            .next()
            .and_then(|event| event.try_into_metric())
            .expect("should be single metric in the buffer");
        assert_eq!(forwarded_metric2.context(), metric.context());
    }

    #[tokio::test]
    async fn multiple_senders_named_output() {
        // Create the forwarder and wire up two senders to the default output so that we can receive what gets forwarded.
        let (mut forwarder, _) = create_forwarder(2, 1);

        let (tx1, mut rx1) = mpsc::channel(1);
        let (tx2, mut rx2) = mpsc::channel(1);
        forwarder.add_output(OutputName::Given("metrics".into()), tx1);
        forwarder.add_output(OutputName::Given("metrics".into()), tx2);

        // Create a basic metric event and then forward it.
        let metric = Metric::counter("basic_metric", 42.0);

        let mut buffered = forwarder.buffered_named("metrics").unwrap();
        buffered.push(Event::Metric(metric.clone())).await.unwrap();
        let flushed_len = buffered.flush().await.unwrap();
        assert_eq!(flushed_len, 1);

        // Make sure there's an event buffer waiting for us on each received and that it has one event: the one we sent.
        let forwarded_ebuf1 = rx1.try_recv().expect("event buffer should have been forwarded");
        let forwarded_ebuf2 = rx2.try_recv().expect("event buffer should have been forwarded");
        assert_eq!(forwarded_ebuf1.len(), 1);
        assert_eq!(forwarded_ebuf2.len(), 1);

        let forwarded_metric1 = forwarded_ebuf1
            .into_iter()
            .next()
            .and_then(|event| event.try_into_metric())
            .expect("should be single metric in the buffer");
        assert_eq!(forwarded_metric1.context(), metric.context());

        let forwarded_metric2 = forwarded_ebuf2
            .into_iter()
            .next()
            .and_then(|event| event.try_into_metric())
            .expect("should be single metric in the buffer");
        assert_eq!(forwarded_metric2.context(), metric.context());
    }

    #[tokio::test]
    async fn default_output_not_set() {
        // Create the forwarder and try to forward an event without setting up a default output.
        let (forwarder, ebuf_pool) = create_forwarder(1, 1);

        let result = forwarder.buffered();
        assert!(result.is_err());

        let ebuf = ebuf_pool.acquire().await;
        let result = forwarder.forward_buffer(ebuf).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn named_output_not_set() {
        // Create the forwarder and try to forward an event without setting up a named output.
        let (forwarder, ebuf_pool) = create_forwarder(1, 1);

        let result = forwarder.buffered_named("metrics");
        assert!(result.is_err());

        let ebuf = ebuf_pool.acquire().await;
        let result = forwarder.forward_buffer_named("metrics", ebuf).await;
        assert!(result.is_err());
    }

    #[test]
    fn multiple_senders_blocks_when_event_buffer_pool_empty() {
        // Create the forwarder and wire up two senders to the default output so that we can receive what gets
        // forwarded.
        //
        // Crucially, our event buffer pool is set at a fixed size of two, and we'll use this to control if the event
        // buffer pool is empty or not when calling `forward`, to ensure that forwarding blocks when the pool is empty
        // and completes when it can acquire the necessary additional buffer.
        let (mut forwarder, ebuf_pool) = create_forwarder(2, 1);

        let (tx1, mut rx1) = mpsc::channel(1);
        let (tx2, mut rx2) = mpsc::channel(1);
        forwarder.add_output(OutputName::Default, tx1);
        forwarder.add_output(OutputName::Default, tx2);

        // Create a basic metric event and then attempt to forward it.
        //
        // Before forwarding, we'll acquire an event buffer in the pool to ensure that the pool only has one event
        // buffer present, since forwarding to two outputs will require two event buffers overall.
        let metric = Metric::counter("basic_metric", 42.0);

        let mut receive1 = test_spawn(rx1.recv());
        let mut receive2 = test_spawn(rx2.recv());
        assert_pending!(receive1.poll());
        assert_pending!(receive2.poll());

        // Grab an event buffer from the pool and hold on to it before trying to forward:
        let mut acquire_ebuf = test_spawn(ebuf_pool.acquire());
        let ebuf = assert_ready!(acquire_ebuf.poll());

        let mut buffered = forwarder.buffered().unwrap();
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

        // Finally, flush the buffered forwarder to ensure all events are forwarded:
        let mut buffered_flush = test_spawn(buffered.flush());
        assert_ready_ok!(buffered_flush.poll());

        let forwarded_ebuf1 = assert_ready!(receive1.poll()).expect("event buffer should have been forwarded");
        assert_eq!(forwarded_ebuf1.len(), 1);

        let forwarded_ebuf2 = assert_ready!(receive2.poll()).expect("event buffer should have been forwarded");
        assert_eq!(forwarded_ebuf2.len(), 1);
    }
}
