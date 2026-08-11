use async_trait::async_trait;
use metrics::Counter;
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
    bytes_sent: Counter,
    bytes_dropped: Counter,
    bytes_dropped_queue: Counter,
    bytes_dropped_writer: Counter,
}

impl DogStatsDClientTelemetry {
    pub(super) fn new(metrics_builder: MetricsBuilder) -> Self {
        Self {
            bytes_sent: metrics_builder.register_counter("dogstatsd_client_telemetry_bytes_sent"),
            bytes_dropped: metrics_builder.register_counter("dogstatsd_client_telemetry_bytes_dropped"),
            bytes_dropped_queue: metrics_builder.register_counter("dogstatsd_client_telemetry_bytes_dropped_queue"),
            bytes_dropped_writer: metrics_builder.register_counter("dogstatsd_client_telemetry_bytes_dropped_writer"),
        }
    }

    pub(super) fn record_metric(&self, metric: &Metric) {
        let counter = match metric.context().name().as_ref() {
            "datadog.dogstatsd.client.bytes_sent" => &self.bytes_sent,
            "datadog.dogstatsd.client.bytes_dropped" => &self.bytes_dropped,
            "datadog.dogstatsd.client.bytes_dropped_queue" => &self.bytes_dropped_queue,
            "datadog.dogstatsd.client.bytes_dropped_writer" => &self.bytes_dropped_writer,
            _ => return,
        };

        if let MetricValues::Rate(values, _) = metric.values() {
            // A delayed aggregate flush can contain several closed time buckets in one metric. Separate tag contexts
            // arrive as separate metrics and intentionally accumulate into this dimensionless COAT counter.
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
