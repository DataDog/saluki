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
        let (metrics, outputs) = match output_name {
            None => self
                .default
                .as_ref()
                .ok_or_else(|| generic_error!("No default output declared."))?,
            Some(name) => self
                .targets
                .get(name.as_ref())
                .ok_or_else(|| generic_error!("No output named '{}' declared.", name.as_ref()))?,
        };

        let mut buf_len = 0;
        let start = Instant::now();

        // TODO: Ideally, we should be tracking the forward latency for each downstream component that we're sending to
        // here instead of for the overall forwarding operation.

        // Iterate over the input events, writing them into each output buffer. When any of the output buffers is
        // filled, we consume it and send it to the corresponding output. We'll only replace the event buffer for a
        // given output if we have an event to write to it.
        //
        // We repeat this until all input events have been forwarded.

        // TODO: We could also bundle this together with what we store in `self.outputs`, so that it's all allocated
        // ahead of time and we aren't ever allocating per forward operation.
        let mut output_buffers = Vec::with_capacity(outputs.len());

        for event in events {
            // Lazily acquire event buffers for each output.
            //
            // We do this in case the input events iterator is empty, since we don't want to acquire event buffers only
            // to just throw them back into the pool.
            if output_buffers.is_empty() {
                for _ in 0..outputs.len() {
                    output_buffers.push(Some(self.event_buffer_pool.acquire().await));
                }
            }

            for (output_idx, output_buffer) in output_buffers.iter_mut().enumerate() {
                // If the output's event buffer is full, consume it and forward it, before acquiring a new one.
                let buffer_full = output_buffer.as_ref().map_or(false, |b| b.is_full());
                if buffer_full {
                    let filled_buffer = output_buffer.take().unwrap();
                    let filled_buffer_len = filled_buffer.len();

                    let output = &outputs[output_idx];
                    output
                        .send(filled_buffer)
                        .await
                        .map_err(|_| generic_error!("Failed to send to output; {} events lost", filled_buffer_len))?;

                    // Acquire a new event buffer for the output.
                    *output_buffer = Some(self.event_buffer_pool.acquire().await);
                }

                // Write the event into the output buffer.
                let output_buffer = output_buffer
                    .as_mut()
                    .expect("output event buffer should never be unset at this point");

                if output_buffer.try_push(event.clone()).is_some() {
                    panic!("output event buffer should never be full at this point");
                }
            }

            buf_len += 1;
        }

        // Flush any remaining events in the output buffers.
        for (output_idx, output_buffer) in output_buffers.iter_mut().enumerate() {
            if let Some(output_buffer) = output_buffer.take() {
                if !output_buffer.is_empty() {
                    let output_buffer_len = output_buffer.len();
                    let output = &outputs[output_idx];
                    output
                        .send(output_buffer)
                        .await
                        .map_err(|_| generic_error!("Failed to send to output; {} events lost", output_buffer_len))?;
                }
            }
        }

        // If we actually forwarded any events, update our telemetry.
        if buf_len > 0 {
            let latency = start.elapsed();
            metrics.forwarding_latency.record(latency);

            metrics.events_sent.increment(buf_len as u64);
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
