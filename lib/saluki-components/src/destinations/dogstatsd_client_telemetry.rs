use async_trait::async_trait;
use metrics::Gauge;
use saluki_core::{
    accounting::{MemoryBounds, MemoryBoundsBuilder},
    components::{destinations::*, ComponentContext},
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

/// Configuration for the DogStatsD client telemetry destination.
#[derive(Default)]
pub struct DogStatsDClientTelemetryConfiguration;

#[async_trait]
impl DestinationBuilder for DogStatsDClientTelemetryConfiguration {
    fn input_event_type(&self) -> EventType {
        EventType::Metric
    }

    async fn build(&self, context: ComponentContext) -> Result<Box<dyn Destination + Send>, GenericError> {
        Ok(Box::new(DogStatsDClientTelemetry::new(
            MetricsBuilder::from_component_context(&context),
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
    metrics: Gauge,
    metrics_by_type: Gauge,
    events: Gauge,
    service_checks: Gauge,
    metric_dropped_on_receive: Gauge,
    bytes_sent: Gauge,
    bytes_dropped: Gauge,
    bytes_dropped_queue: Gauge,
    bytes_dropped_writer: Gauge,
    packets_sent: Gauge,
    packets_dropped: Gauge,
    packets_dropped_queue: Gauge,
    packets_dropped_writer: Gauge,
    aggregated_context: Gauge,
    aggregated_context_by_type: Gauge,
}

impl DogStatsDClientTelemetry {
    pub(super) fn new(metrics_builder: MetricsBuilder) -> Self {
        Self {
            metrics: metrics_builder.register_gauge("dogstatsd_client_telemetry_metrics"),
            metrics_by_type: metrics_builder.register_gauge("dogstatsd_client_telemetry_metrics_by_type"),
            events: metrics_builder.register_gauge("dogstatsd_client_telemetry_events"),
            service_checks: metrics_builder.register_gauge("dogstatsd_client_telemetry_service_checks"),
            metric_dropped_on_receive: metrics_builder
                .register_gauge("dogstatsd_client_telemetry_metric_dropped_on_receive"),
            bytes_sent: metrics_builder.register_gauge("dogstatsd_client_telemetry_bytes_sent"),
            bytes_dropped: metrics_builder.register_gauge("dogstatsd_client_telemetry_bytes_dropped"),
            bytes_dropped_queue: metrics_builder.register_gauge("dogstatsd_client_telemetry_bytes_dropped_queue"),
            bytes_dropped_writer: metrics_builder.register_gauge("dogstatsd_client_telemetry_bytes_dropped_writer"),
            packets_sent: metrics_builder.register_gauge("dogstatsd_client_telemetry_packets_sent"),
            packets_dropped: metrics_builder.register_gauge("dogstatsd_client_telemetry_packets_dropped"),
            packets_dropped_queue: metrics_builder.register_gauge("dogstatsd_client_telemetry_packets_dropped_queue"),
            packets_dropped_writer: metrics_builder.register_gauge("dogstatsd_client_telemetry_packets_dropped_writer"),
            aggregated_context: metrics_builder.register_gauge("dogstatsd_client_telemetry_aggregated_context"),
            aggregated_context_by_type: metrics_builder
                .register_gauge("dogstatsd_client_telemetry_aggregated_context_by_type"),
        }
    }

    pub(super) fn record_metric(&self, metric: &Metric) {
        let gauge = match metric.context().name().as_ref() {
            "datadog.dogstatsd.client.metrics" => &self.metrics,
            "datadog.dogstatsd.client.metrics_by_type" => &self.metrics_by_type,
            "datadog.dogstatsd.client.events" => &self.events,
            "datadog.dogstatsd.client.service_checks" => &self.service_checks,
            "datadog.dogstatsd.client.metric_dropped_on_receive" => &self.metric_dropped_on_receive,
            "datadog.dogstatsd.client.bytes_sent" => &self.bytes_sent,
            "datadog.dogstatsd.client.bytes_dropped" => &self.bytes_dropped,
            "datadog.dogstatsd.client.bytes_dropped_queue" => &self.bytes_dropped_queue,
            "datadog.dogstatsd.client.bytes_dropped_writer" => &self.bytes_dropped_writer,
            "datadog.dogstatsd.client.packets_sent" => &self.packets_sent,
            "datadog.dogstatsd.client.packets_dropped" => &self.packets_dropped,
            "datadog.dogstatsd.client.packets_dropped_queue" => &self.packets_dropped_queue,
            "datadog.dogstatsd.client.packets_dropped_writer" => &self.packets_dropped_writer,
            "datadog.dogstatsd.client.aggregated_context" => &self.aggregated_context,
            "datadog.dogstatsd.client.aggregated_context_by_type" => &self.aggregated_context_by_type,
            _ => return,
        };

        if let MetricValues::Rate(values, _) = metric.values() {
            for (_, value) in values {
                gauge.increment(value);
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
