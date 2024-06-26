use std::{collections::HashMap, time::Instant};

use metrics::{Counter, Histogram, SharedString};
use saluki_error::{generic_error, GenericError};
use tokio::sync::mpsc;

use crate::{
    components::{ComponentContext, MetricsBuilder},
    pooling::{FixedSizeObjectPool, ObjectPool as _},
    topology::OutputName,
};

use super::event_buffer::EventBuffer;

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
    targets: HashMap<String, (ForwarderMetrics, Vec<mpsc::Sender<EventBuffer>>)>,
}

impl Forwarder {
    /// Create a new `Forwarder` for the given component context.
    pub fn new(context: ComponentContext, event_buffer_pool: FixedSizeObjectPool<EventBuffer>) -> Self {
        Self {
            context,
            event_buffer_pool,
            default: None,
            targets: HashMap::new(),
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
        let (metrics, default_outputs) = self
            .default
            .as_ref()
            .ok_or_else(|| generic_error!("No default output declared."))?;

        let buf_len = buffer.len();
        let mut buffer = Some(buffer);
        let output_last_idx = default_outputs.len() - 1;

        // TODO: Ideally, we should be tracking the forward latency for each downstream component that we're sending to
        // here instead of for the overall forwarding operation.
        let start = Instant::now();
        for (output_idx, output) in default_outputs.iter().enumerate() {
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
