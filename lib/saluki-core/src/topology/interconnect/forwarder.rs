use std::{collections::HashMap, time::Instant};

use metrics::{Counter, Histogram, SharedString};
use tokio::sync::mpsc;

use crate::{
    components::{metrics::MetricsBuilder, ComponentContext},
    topology::OutputName,
};

use super::event_buffer::EventBuffer;

struct ForwarderMetrics {
    events_sent: Counter,
    forwarding_latency: Histogram,
}

impl ForwarderMetrics {
    pub fn default_output(context: ComponentContext) -> Self {
        Self::with_output_name(context, "default")
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
            events_sent: metrics_builder.register_counter_with_labels("component_events_sent", output_labels),
            forwarding_latency: metrics_builder
                .register_histogram_with_labels("component_send_latency_secs", output_labels),
        }
    }
}

pub struct Forwarder {
    context: ComponentContext,
    default: Option<(ForwarderMetrics, mpsc::Sender<EventBuffer>)>,
    targets: HashMap<String, (ForwarderMetrics, Vec<mpsc::Sender<EventBuffer>>)>,
}

impl Forwarder {
    pub fn new(context: ComponentContext) -> Self {
        Self {
            context,
            default: None,
            targets: HashMap::new(),
        }
    }

    pub fn add_output(&mut self, output_name: OutputName, sender: mpsc::Sender<EventBuffer>) {
        match output_name {
            OutputName::Default => {
                let metrics = ForwarderMetrics::default_output(self.context.clone());
                self.default = Some((metrics, sender));
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

    pub async fn forward(&self, buffer: EventBuffer) -> Result<(), String> {
        let (metrics, default_output) = self.default.as_ref().ok_or("no default output")?;

        let buf_len = buffer.len();
        let start = Instant::now();
        default_output
            .send(buffer)
            .await
            .map_err(|_| format!("failed to send to default output; {} events lost", buf_len))?;
        let latency = start.elapsed();
        metrics.forwarding_latency.record(latency);

        metrics.events_sent.increment(buf_len as u64);
        Ok(())
    }
}
