use std::time::Instant;

use ahash::AHashMap;
use metrics::{Counter, Histogram, SharedString};
use saluki_error::{generic_error, GenericError};
use tokio::sync::mpsc;

use super::event_buffer::EventBuffer;
use crate::{
    components::{ComponentContext, MetricsBuilder},
    pooling::{FixedSizeObjectPool, ObjectPool as _},
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
    event_buffer_pool: FixedSizeObjectPool<EventBuffer>,
    default: Option<(ForwarderMetrics, Vec<mpsc::Sender<EventBuffer>>)>,
    targets: AHashMap<String, (ForwarderMetrics, Vec<mpsc::Sender<EventBuffer>>)>,
}

impl Forwarder {
    /// Create a new `Forwarder` for the given component context.
    pub fn new(context: ComponentContext, event_buffer_pool: FixedSizeObjectPool<EventBuffer>) -> Self {
        Self {
            context,
            event_buffer_pool,
            default: None,
            targets: AHashMap::new(),
        }
    }

    /// Adds an output to the forwarder, attached to the given sender.
    pub fn add_output(&mut self, output_name: OutputName, sender: mpsc::Sender<EventBuffer>) {
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

    /// Forwards the event buffer to the default output.
    ///
    /// ## Errors
    ///
    /// If the default output is not set, or there is an error sending to the default output, an error is returned.
    pub async fn forward(&self, buffer: EventBuffer) -> Result<(), GenericError> {
        self.forward_inner::<String>(None, buffer).await
    }

    /// Forwards the event buffer to the given named output.
    ///
    /// ## Errors
    ///
    /// If a output of the given name is not set, or there is an error sending to the output, an error is returned.
    pub async fn forward_named<N>(&self, output_name: N, buffer: EventBuffer) -> Result<(), GenericError>
    where
        N: AsRef<str>,
    {
        self.forward_inner(Some(output_name), buffer).await
    }

    async fn forward_inner<N>(&self, output_name: Option<N>, buffer: EventBuffer) -> Result<(), GenericError>
    where
        N: AsRef<str>,
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

        let buf_len = buffer.len();
        let mut buffer = Some(buffer);
        let output_last_idx = outputs.len() - 1;

        // TODO: Ideally, we should be tracking the forward latency for each downstream component that we're sending to
        // here instead of for the overall forwarding operation.
        let start = Instant::now();
        for (output_idx, output) in outputs.iter().enumerate() {
            // Handle potentially forwarding to multiple downstream components.
            //
            // When sending to the last downstream component attached to this output, which might also be the only
            // attached component, we consume the original event buffer. Otherwise, we acquire a new event buffer from
            // the event buffer pool and extend it with the events from the original buffer.
            let output_buffer = if output_idx == output_last_idx {
                buffer.take().unwrap()
            } else {
                let mut new_output_buffer = self.event_buffer_pool.acquire().await;
                new_output_buffer.extend(buffer.as_ref().map(|b| b.into_iter().cloned()).unwrap());
                new_output_buffer
            };

            output
                .send(output_buffer)
                .await
                .map_err(|_| generic_error!("Failed to send to default output; {} events lost", buf_len))?;
        }
        let latency = start.elapsed();
        metrics.forwarding_latency.record(latency);

        metrics.events_sent.increment(buf_len as u64);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    // TODO: Tests asserting we emit metrics, and the right metrics.

    use saluki_event::{metric::Metric, Event};
    use tokio_test::{task::spawn as test_spawn, *};

    use super::*;
    use crate::topology::ComponentId;

    fn create_forwarder(event_buffers: usize) -> (Forwarder, FixedSizeObjectPool<EventBuffer>) {
        let component_context = ComponentId::try_from("forwarder_test")
            .map(ComponentContext::source)
            .expect("component ID should never be invalid");
        let event_buffer_pool = FixedSizeObjectPool::with_capacity(event_buffers);

        (
            Forwarder::new(component_context, event_buffer_pool.clone()),
            event_buffer_pool,
        )
    }

    #[tokio::test]
    async fn default_output() {
        // Create the forwarder and wire up a sender to the default output so that we can receive what gets forwarded.
        let (mut forwarder, ebuf_pool) = create_forwarder(1);

        let (tx, mut rx) = mpsc::channel(1);
        forwarder.add_output(OutputName::Default, tx);

        // Create a basic metric event and then forward it.
        let metric = Metric::counter("basic_metric", 42.0);

        let mut ebuf = ebuf_pool.acquire().await;
        ebuf.push(Event::Metric(metric.clone()));

        forwarder.forward(ebuf).await.unwrap();

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
        let (mut forwarder, ebuf_pool) = create_forwarder(1);

        let (tx, mut rx) = mpsc::channel(1);
        forwarder.add_output(OutputName::Given("metrics".into()), tx);

        // Create a basic metric event and then forward it.
        let metric = Metric::counter("basic_metric", 42.0);

        let mut ebuf = ebuf_pool.acquire().await;
        ebuf.push(Event::Metric(metric.clone()));

        forwarder.forward_named("metrics", ebuf).await.unwrap();

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
        let (mut forwarder, ebuf_pool) = create_forwarder(2);

        let (tx1, mut rx1) = mpsc::channel(1);
        let (tx2, mut rx2) = mpsc::channel(1);
        forwarder.add_output(OutputName::Default, tx1);
        forwarder.add_output(OutputName::Default, tx2);

        // Create a basic metric event and then forward it.
        let metric = Metric::counter("basic_metric", 42.0);

        let mut ebuf = ebuf_pool.acquire().await;
        ebuf.push(Event::Metric(metric.clone()));

        forwarder.forward(ebuf).await.unwrap();

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
        let (mut forwarder, ebuf_pool) = create_forwarder(2);

        let (tx1, mut rx1) = mpsc::channel(1);
        let (tx2, mut rx2) = mpsc::channel(1);
        forwarder.add_output(OutputName::Given("metrics".into()), tx1);
        forwarder.add_output(OutputName::Given("metrics".into()), tx2);

        // Create a basic metric event and then forward it.
        let metric = Metric::counter("basic_metric", 42.0);

        let mut ebuf = ebuf_pool.acquire().await;
        ebuf.push(Event::Metric(metric.clone()));

        forwarder.forward_named("metrics", ebuf).await.unwrap();

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
        let (forwarder, ebuf_pool) = create_forwarder(1);

        // Create a basic metric event and then forward it.
        let metric = Metric::counter("basic_metric", 42.0);

        let mut ebuf = ebuf_pool.acquire().await;
        ebuf.push(Event::Metric(metric.clone()));

        let result = forwarder.forward(ebuf).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn error_when_named_output_not_set() {
        // Create the forwarder and try to forward an event without setting up a named output.
        let (forwarder, ebuf_pool) = create_forwarder(1);

        // Create a basic metric event and then forward it.
        let metric = Metric::counter("basic_metric", 42.0);

        let mut ebuf = ebuf_pool.acquire().await;
        ebuf.push(Event::Metric(metric.clone()));

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
        let (mut forwarder, ebuf_pool) = create_forwarder(2);

        let (tx1, mut rx1) = mpsc::channel(1);
        let (tx2, mut rx2) = mpsc::channel(1);
        forwarder.add_output(OutputName::Default, tx1);
        forwarder.add_output(OutputName::Default, tx2);

        // Create a basic metric event and then attempt to forward it.
        //
        // Before forwarding, we'll acquire the second event buffer in the pool to ensure that the pool is empty, which
        // should allow us to control the blocking behavior of the forwarder.
        let metric = Metric::counter("basic_metric", 42.0);

        let mut acquire_ebuf1 = test_spawn(ebuf_pool.acquire());
        let mut ebuf1 = assert_ready!(acquire_ebuf1.poll());
        ebuf1.push(Event::Metric(metric.clone()));

        let mut acquire_ebuf2 = test_spawn(ebuf_pool.acquire());
        let ebuf2 = assert_ready!(acquire_ebuf2.poll());

        let mut receive1 = test_spawn(rx1.recv());
        let mut receive2 = test_spawn(rx2.recv());
        assert_pending!(receive1.poll());
        assert_pending!(receive2.poll());

        let mut forward = test_spawn(forwarder.forward(ebuf1));
        assert_pending!(forward.poll());

        // Since we clone the event buffer for each sender except the last one, both receivers should still be pending:
        assert_pending!(receive1.poll());
        assert_pending!(receive2.poll());

        // Now drop the spare event buffer, which returns it to the pool and should unblock the forward:
        drop(ebuf2);
        assert_ready_ok!(forward.poll());

        let forwarded_ebuf1 = assert_ready!(receive1.poll()).expect("event buffer should have been forwarded");
        assert_eq!(forwarded_ebuf1.len(), 1);

        let forwarded_ebuf2 = assert_ready!(receive2.poll()).expect("event buffer should have been forwarded");
        assert_eq!(forwarded_ebuf2.len(), 1);
    }
}
