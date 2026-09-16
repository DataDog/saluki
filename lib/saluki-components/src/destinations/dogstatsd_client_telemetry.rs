use async_trait::async_trait;
use saluki_core::{
    accounting::{MemoryBounds, MemoryBoundsBuilder},
    components::{destinations::*, BuildContext, ComponentContext},
    data_model::event::{
        metric::{Metric, MetricValues},
        Event, EventType,
    },
};
use saluki_error::GenericError;
use saluki_metrics::{static_metrics, Counter};
use tokio::select;
use tracing::debug;

#[derive(Clone, Copy)]
struct ClientTelemetryTags {
    client: &'static str,
    client_transport: &'static str,
}

impl ClientTelemetryTags {
    fn from_metric(metric: &Metric) -> Self {
        let mut tags = Self {
            client: "unknown",
            client_transport: "unknown",
        };
        for tag in metric.context().tags() {
            match tag.name() {
                "client" => tags.client = normalize_client_library(tag.value()),
                "client_transport" => tags.client_transport = normalize_client_transport(tag.value()),
                _ => {}
            }
            if tags.client != "unknown" && tags.client_transport != "unknown" {
                break;
            }
        }
        tags
    }
}

pub(super) fn normalize_client_library(client: Option<&str>) -> &'static str {
    match client {
        Some("go") => "go",
        Some("py") => "py",
        Some("java") => "java",
        Some("ruby") => "ruby",
        Some("csharp") => "csharp",
        Some("php") => "php",
        Some("rust") => "rust",
        _ => "unknown",
    }
}

pub(super) fn normalize_client_transport(transport: Option<&str>) -> &'static str {
    match transport {
        Some("udp") => "udp",
        Some("uds") => "uds",
        Some("uds-stream") => "uds-stream",
        Some("uds-datagram") => "uds-datagram",
        Some("pipe") => "pipe",
        Some("namedpipe") => "namedpipe",
        Some("named_pipe") => "named_pipe",
        Some("custom") => "custom",
        Some("http") => "http",
        _ => "unknown",
    }
}

#[static_metrics(prefix = dogstatsd_client_telemetry, labels(component_id, component_type))]
#[derive(Clone)]
struct ClientTelemetryCounters {
    #[metric(mapped(client, client_transport))]
    bytes_sent: Counter,
    #[metric(mapped(client, client_transport))]
    bytes_dropped: Counter,
    #[metric(mapped(client, client_transport))]
    bytes_dropped_queue: Counter,
    #[metric(mapped(client, client_transport))]
    bytes_dropped_writer: Counter,
}

enum ClientTelemetryMetric {
    Sent,
    Dropped,
    DroppedQueue,
    DroppedWriter,
}

/// Configuration for the DogStatsD client telemetry destination.
#[derive(Default)]
pub struct DogStatsDClientTelemetryConfiguration;

#[async_trait]
impl DestinationBuilder for DogStatsDClientTelemetryConfiguration {
    fn input_event_type(&self) -> EventType {
        EventType::Metric
    }

    async fn build(&self, context: BuildContext) -> Result<Box<dyn Destination + Send>, GenericError> {
        Ok(Box::new(DogStatsDClientTelemetry::new(context.component_context())))
    }
}

impl MemoryBounds for DogStatsDClientTelemetryConfiguration {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        builder
            .minimum()
            .with_single_value::<DogStatsDClientTelemetry>("destination struct");
    }
}

/// Mirrors supported DogStatsD client telemetry metrics into ADP internal telemetry.
pub struct DogStatsDClientTelemetry {
    counters: ClientTelemetryCounters,
}

impl DogStatsDClientTelemetry {
    pub(super) fn new(component_context: &ComponentContext) -> Self {
        Self {
            counters: ClientTelemetryCounters::new(
                component_context.component_id(),
                component_context.component_type().as_str(),
            ),
        }
    }

    pub(super) fn record_metric(&self, metric: &Metric) {
        let metric_kind = match metric.context().name().as_ref() {
            "datadog.dogstatsd.client.bytes_sent" => ClientTelemetryMetric::Sent,
            "datadog.dogstatsd.client.bytes_dropped" => ClientTelemetryMetric::Dropped,
            "datadog.dogstatsd.client.bytes_dropped_queue" => ClientTelemetryMetric::DroppedQueue,
            "datadog.dogstatsd.client.bytes_dropped_writer" => ClientTelemetryMetric::DroppedWriter,
            _ => return,
        };

        if let MetricValues::Rate(values, _) = metric.values() {
            // A delayed aggregate flush can contain several closed time buckets in one metric. Separate tag contexts
            // arrive as separate metrics and accumulate into counters with their client dimensions preserved.
            let tags = ClientTelemetryTags::from_metric(metric);
            let counter = match metric_kind {
                ClientTelemetryMetric::Sent => self.counters.bytes_sent(tags.client, tags.client_transport),
                ClientTelemetryMetric::Dropped => self.counters.bytes_dropped(tags.client, tags.client_transport),
                ClientTelemetryMetric::DroppedQueue => {
                    self.counters.bytes_dropped_queue(tags.client, tags.client_transport)
                }
                ClientTelemetryMetric::DroppedWriter => {
                    self.counters.bytes_dropped_writer(tags.client, tags.client_transport)
                }
            };
            for (_, value) in values {
                if value.is_finite() && value >= 0.0 && value.fract() == 0.0 && value <= u64::MAX as f64 {
                    counter.increment(value as u64);
                }
            }
        }
    }
}

#[async_trait]
impl Destination for DogStatsDClientTelemetry {
    async fn run(self: Box<Self>, mut context: DestinationContext) -> Result<(), GenericError> {
        let mut health = context.take_health_handle();
        health.mark_ready();
        debug!("DogStatsD client telemetry destination started.");

        loop {
            select! {
                _ = health.live() => continue,
                maybe_events = context.events().next() => match maybe_events {
                    Some(events) => {
                        for event in events {
                            if let Event::Metric(metric) = event {
                                self.record_metric(&metric);
                            }
                        }
                    },
                    None => break,
                },
            }
        }

        debug!("DogStatsD client telemetry destination stopped.");
        Ok(())
    }
}
