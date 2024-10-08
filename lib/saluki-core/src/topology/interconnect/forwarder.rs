use std::time::Instant;

use ahash::AHashMap;
use metrics::{Counter, Histogram, SharedString};
use saluki_error::{generic_error, GenericError};
use saluki_event::Event;
use tokio::sync::mpsc;

use super::FixedSizeEventBuffer;
use crate::{
    components::{ComponentContext, MetricsBuilder},
    pooling::{ElasticObjectPool, ObjectPool as _},
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
        N: Into<SharedString> + Clone,
    {
        let output_labels = &[("output", output_name)];
        let metrics_builder = MetricsBuilder::from_component_context(context);

        Self {
            events_sent: metrics_builder.register_counter_with_labels("component_events_sent_total", output_labels),
            forwarding_latency: metrics_builder
                .register_histogram_with_labels("component_send_latency_seconds", output_labels),
        }
    }
}

struct BufferedSender<'a> {
    buffer: Option<FixedSizeEventBuffer>,
    buffer_pool: &'a ElasticObjectPool<FixedSizeEventBuffer>,
    sender: &'a mpsc::Sender<FixedSizeEventBuffer>,
}

impl<'a> BufferedSender<'a> {
    fn new(
        buffer_pool: &'a ElasticObjectPool<FixedSizeEventBuffer>, sender: &'a mpsc::Sender<FixedSizeEventBuffer>,
    ) -> Self {
        Self {
            buffer: None,
            buffer_pool,
            sender,
        }
    }

    async fn push(&mut self, event: Event) -> Result<(), GenericError> {
        // If our buffer is full, consume it and forward it, before acquiring a new one.
        let buffer_full = self.buffer.as_ref().map_or(false, |b| b.is_full());
        if buffer_full {
            let buffer = self.buffer.take().unwrap();
            let buffer_len = buffer.len();

            self.sender
                .send(buffer)
                .await
                .map_err(|_| generic_error!("Failed to send to output; {} events lost", buffer_len))?;
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

    async fn flush(self) -> Result<(), GenericError> {
        if let Some(buffer) = self.buffer {
            let buffer_len = buffer.len();
            if buffer_len > 0 {
                self.sender
                    .send(buffer)
                    .await
                    .map_err(|_| generic_error!("Failed to send to output; {} events lost", buffer_len))?;
            }
        }

        Ok(())
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

    /// Forwards the given events to the default output.
    ///
    /// ## Errors
    ///
    /// If the default output is not set, or there is an error sending to the default output, an error is returned.
    pub async fn forward<I>(&self, events: I) -> Result<(), GenericError>
    where
        I: IntoIterator<Item = Event>,
    {
        self.forward_inner_fixed::<String, _>(None, events).await
    }

    /// Forwards the given events to the given named output.
    ///
    /// ## Errors
    ///
    /// If a output of the given name is not set, or there is an error sending to the output, an error is returned.
    pub async fn forward_named<N, I>(&self, output_name: N, events: I) -> Result<(), GenericError>
    where
        N: AsRef<str>,
        I: IntoIterator<Item = Event>,
    {
        self.forward_inner_fixed(Some(output_name), events).await
    }

    async fn forward_inner_fixed<N, I>(&self, output_name: Option<N>, events: I) -> Result<(), GenericError>
    where
        N: AsRef<str>,
        I: IntoIterator<Item = Event>,
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

        if senders.len() > 1 {
            self.forward_inner_fixed_multi_sender(metrics, senders, events).await
        } else {
            let sender = senders.first().expect("output sender should be present");
            self.forward_inner_fixed_single_sender(metrics, sender, events).await
        }
    }

    async fn forward_inner_fixed_multi_sender<I>(
        &self, metrics: &ForwarderMetrics, senders: &[mpsc::Sender<FixedSizeEventBuffer>], events: I,
    ) -> Result<(), GenericError>
    where
        I: IntoIterator<Item = Event>,
    {
        let mut events_forwarded = 0;
        let start = Instant::now();

        // TODO: Ideally, we should be tracking the forward latency for each downstream component that we're sending to
        // here instead of for the overall forwarding operation.

        // Write all of the input events to each buffered sender, which handles flushing filled event buffers and
        // acquiring empty event buffers as necessary.
        let mut buffered_senders = senders
            .iter()
            .map(|sender| BufferedSender::new(&self.event_buffer_pool, sender))
            .collect::<Vec<_>>();

        let last_sender_idx = buffered_senders.len();
        for event in events {
            let mut event = Some(event);

            for (sender_idx, buffered_sender) in buffered_senders.iter_mut().enumerate() {
                let event = if sender_idx == last_sender_idx {
                    event.take().unwrap()
                } else {
                    event.clone().unwrap()
                };

                buffered_sender.push(event).await?;
            }

            events_forwarded += 1;
        }

        // Flush the buffered senders to ensure all events are forwarded.
        for buffered_sender in buffered_senders {
            buffered_sender.flush().await?;
        }

        // If we actually forwarded any events, update our telemetry.
        if events_forwarded > 0 {
            let latency = start.elapsed();
            metrics.forwarding_latency.record(latency);
            metrics.events_sent.increment(events_forwarded as u64);
        }

        Ok(())
    }

    async fn forward_inner_fixed_single_sender<I>(
        &self, metrics: &ForwarderMetrics, sender: &mpsc::Sender<FixedSizeEventBuffer>, events: I,
    ) -> Result<(), GenericError>
    where
        I: IntoIterator<Item = Event>,
    {
        let mut events_forwarded = 0;
        let start = Instant::now();

        // TODO: Ideally, we should be tracking the forward latency for each downstream component that we're sending to
        // here instead of for the overall forwarding operation.

        // Write all of the input events to the buffered sender, which handles flushing filled event buffers and
        // acquiring empty event buffers as necessary.
        let mut buffered_sender = BufferedSender::new(&self.event_buffer_pool, sender);
        for event in events {
            buffered_sender.push(event).await?;

            events_forwarded += 1;
        }

        // Flush the buffered sender to ensure all events are forwarded.
        buffered_sender.flush().await?;

        // If we actually forwarded any events, update our telemetry.
        if events_forwarded > 0 {
            let latency = start.elapsed();
            metrics.forwarding_latency.record(latency);
            metrics.events_sent.increment(events_forwarded as u64);
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    // TODO: Tests asserting we emit metrics, and the right metrics.

    use saluki_event::{metric::Metric, Event};
    use tokio_test::{task::spawn as test_spawn, *};

    use super::*;
    use crate::topology::interconnect::fixed_event_buffer::FixedSizeEventBufferInner;

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
        let events = vec![Event::Metric(metric.clone())];

        forwarder.forward(events).await.unwrap();

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
        let events = vec![Event::Metric(metric.clone())];

        forwarder.forward_named("metrics", events).await.unwrap();

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
        let events = vec![Event::Metric(metric.clone())];

        forwarder.forward(events).await.unwrap();

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
        let events = vec![Event::Metric(metric.clone())];

        forwarder.forward_named("metrics", events).await.unwrap();

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
    async fn error_when_default_output_not_set() {
        // Create the forwarder and try to forward an event without setting up a default output.
        let (forwarder, ebuf_pool) = create_forwarder(1, 1);

        // Create a basic metric event and then forward it.
        let metric = Metric::counter("basic_metric", 42.0);

        let mut ebuf = ebuf_pool.acquire().await;
        assert!(ebuf.try_push(Event::Metric(metric.clone())).is_none());

        let result = forwarder.forward(ebuf).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn error_when_named_output_not_set() {
        // Create the forwarder and try to forward an event without setting up a named output.
        let (forwarder, ebuf_pool) = create_forwarder(1, 1);

        // Create a basic metric event and then forward it.
        let metric = Metric::counter("basic_metric", 42.0);

        let mut ebuf = ebuf_pool.acquire().await;
        assert!(ebuf.try_push(Event::Metric(metric.clone())).is_none());

        let result = forwarder.forward_named("metrics", ebuf).await;
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
        let events = vec![Event::Metric(metric.clone())];

        let mut receive1 = test_spawn(rx1.recv());
        let mut receive2 = test_spawn(rx2.recv());
        assert_pending!(receive1.poll());
        assert_pending!(receive2.poll());

        // Grab an event buffer from the pool and hold on to it before trying to forward:
        let mut acquire_ebuf = test_spawn(ebuf_pool.acquire());
        let ebuf = assert_ready!(acquire_ebuf.poll());

        let mut forward = test_spawn(forwarder.forward(events));
        assert_pending!(forward.poll());

        // We know our `forward` call is pending, and neither receiver should have gotten anything yet because we have
        // to acquire all event buffers for each output before we start pushing any events into the output buffers:
        assert_pending!(receive1.poll());
        assert_pending!(receive2.poll());

        // Now drop the spare event buffer, which returns it to the pool and should unblock the forward:
        drop(ebuf);
        assert_ready_ok!(forward.poll());

        let forwarded_ebuf1 = assert_ready!(receive1.poll()).expect("event buffer should have been forwarded");
        assert_eq!(forwarded_ebuf1.len(), 1);

        let forwarded_ebuf2 = assert_ready!(receive2.poll()).expect("event buffer should have been forwarded");
        assert_eq!(forwarded_ebuf2.len(), 1);
    }
}
