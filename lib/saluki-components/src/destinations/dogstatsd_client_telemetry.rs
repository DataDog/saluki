use async_trait::async_trait;
use metrics::Counter;
use saluki_common::collections::FastHashMap;
use saluki_core::{
    accounting::{MemoryBounds, MemoryBoundsBuilder},
    components::{destinations::*, BuildContext},
    data_model::event::{
        metric::{Metric, MetricValues},
        Event, EventType,
    },
    observability::ComponentMetricsExt as _,
};
use saluki_error::GenericError;
use saluki_metrics::MetricsBuilder;
use tokio::select;
use tracing::debug;

const BYTES_SENT_METRIC: &str = "dogstatsd_client_telemetry_bytes_sent";
const BYTES_DROPPED_METRIC: &str = "dogstatsd_client_telemetry_bytes_dropped";
const BYTES_DROPPED_QUEUE_METRIC: &str = "dogstatsd_client_telemetry_bytes_dropped_queue";
const BYTES_DROPPED_WRITER_METRIC: &str = "dogstatsd_client_telemetry_bytes_dropped_writer";

#[derive(Clone, Copy, Eq, Hash, PartialEq)]
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

    fn as_metric_tags(&self) -> [(&'static str, &'static str); 2] {
        [("client", self.client), ("client_transport", self.client_transport)]
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

struct ClientTelemetryCounters {
    bytes_sent: Counter,
    bytes_dropped: Counter,
    bytes_dropped_queue: Counter,
    bytes_dropped_writer: Counter,
}

impl ClientTelemetryCounters {
    fn new(metrics_builder: &MetricsBuilder, tags: &ClientTelemetryTags) -> Self {
        let metric_tags = tags.as_metric_tags();
        Self {
            bytes_sent: metrics_builder.register_counter_with_tags(BYTES_SENT_METRIC, metric_tags),
            bytes_dropped: metrics_builder.register_counter_with_tags(BYTES_DROPPED_METRIC, metric_tags),
            bytes_dropped_queue: metrics_builder.register_counter_with_tags(BYTES_DROPPED_QUEUE_METRIC, metric_tags),
            bytes_dropped_writer: metrics_builder.register_counter_with_tags(BYTES_DROPPED_WRITER_METRIC, metric_tags),
        }
    }
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
        Ok(Box::new(DogStatsDClientTelemetry::new(
            MetricsBuilder::from_component_context(context.component_context()),
        )))
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
    metrics_builder: MetricsBuilder,
    counters_by_tags: FastHashMap<ClientTelemetryTags, ClientTelemetryCounters>,
}

impl DogStatsDClientTelemetry {
    pub(super) fn new(metrics_builder: MetricsBuilder) -> Self {
        Self {
            metrics_builder,
            counters_by_tags: FastHashMap::default(),
        }
    }

    pub(super) fn record_metric(&mut self, metric: &Metric) {
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
            let metrics_builder = &self.metrics_builder;
            let counters = self
                .counters_by_tags
                .entry(tags)
                .or_insert_with_key(|tags| ClientTelemetryCounters::new(metrics_builder, tags));
            let counter = match metric_kind {
                ClientTelemetryMetric::Sent => &counters.bytes_sent,
                ClientTelemetryMetric::Dropped => &counters.bytes_dropped,
                ClientTelemetryMetric::DroppedQueue => &counters.bytes_dropped_queue,
                ClientTelemetryMetric::DroppedWriter => &counters.bytes_dropped_writer,
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
    async fn run(mut self: Box<Self>, mut context: DestinationContext) -> Result<(), GenericError> {
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
