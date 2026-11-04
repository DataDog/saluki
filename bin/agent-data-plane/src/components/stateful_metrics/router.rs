//! Route supported series to the configured sender and other metric types to HTTP.

use std::sync::LazyLock;

use async_trait::async_trait;
use saluki_core::{
    accounting::{MemoryBounds, MemoryBoundsBuilder},
    components::{
        transforms::{Transform, TransformBuilder, TransformContext},
        BuildContext,
    },
    data_model::event::{metric::MetricValues, Event, EventType},
    topology::OutputDefinition,
};
use saluki_error::GenericError;
use tokio::select;

static OUTPUTS: LazyLock<Vec<OutputDefinition<EventType>>> = LazyLock::new(|| {
    vec![
        OutputDefinition::named_output("stateful", EventType::Metric),
        OutputDefinition::named_output("http", EventType::Metric),
    ]
});

/// Routes count, rate, and gauge metrics to the stateful sender.
pub struct StatefulMetricsRouterConfiguration;
struct StatefulMetricsRouter;

pub(super) fn supports(event: &Event) -> bool {
    matches!(event, Event::Metric(metric) if matches!(metric.values(),
        MetricValues::Counter(_) | MetricValues::Gauge(_) | MetricValues::Rate(..)))
}

#[async_trait]
impl TransformBuilder for StatefulMetricsRouterConfiguration {
    fn input_event_type(&self) -> EventType {
        EventType::Metric
    }
    fn outputs(&self) -> &[OutputDefinition<EventType>] {
        &OUTPUTS
    }
    async fn build(&self, _context: BuildContext) -> Result<Box<dyn Transform + Send>, GenericError> {
        Ok(Box::new(StatefulMetricsRouter))
    }
}

impl MemoryBounds for StatefulMetricsRouterConfiguration {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        builder
            .minimum()
            .with_single_value::<StatefulMetricsRouter>("component struct");
    }
}

#[async_trait]
impl Transform for StatefulMetricsRouter {
    async fn run(self: Box<Self>, mut context: TransformContext) -> Result<(), GenericError> {
        let mut health = context.take_health_handle();
        health.mark_ready();
        loop {
            select! {
                _ = health.live() => {},
                events = context.events().next() => {
                    let Some(mut events) = events else { break };
                    let series = events.extract(supports);
                    context.dispatcher().buffered_named("http")?.send_all(events).await?;
                    context.dispatcher().buffered_named("stateful")?.send_all(series).await?;
                }
            }
        }
        Ok(())
    }
}
