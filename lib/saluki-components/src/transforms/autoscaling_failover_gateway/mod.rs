//! Autoscaling failover metrics gateway transform.

use std::collections::HashSet;

use async_trait::async_trait;
use saluki_core::accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_core::{
    components::{
        transforms::{Transform, TransformBuilder, TransformContext},
        ComponentContext,
    },
    data_model::event::{metric::MetricValues, Event, EventType},
    topology::{EventsBuffer, OutputDefinition},
};
use saluki_error::GenericError;
use tokio::select;
use tracing::{debug, error};

use crate::config::AutoscalingFailoverConfiguration;

/// Autoscaling failover metrics gateway transform configuration.
///
/// This transform sits between the shared metrics enrichment stage and the Cluster Agent encoder/forwarder branch. It
/// forwards only configured series metrics to the Cluster Agent branch and drops everything else.
pub struct AutoscalingFailoverGatewayConfiguration {
    failover_config: AutoscalingFailoverConfiguration,
}

impl AutoscalingFailoverGatewayConfiguration {
    /// Creates a new `AutoscalingFailoverGatewayConfiguration` from the given failover configuration.
    pub fn new(failover_config: AutoscalingFailoverConfiguration) -> Self {
        Self { failover_config }
    }
}

/// Routing and filtering state for the autoscaling failover metrics gateway.
#[derive(Debug)]
enum GatewayMode {
    /// Autoscaling failover is disabled or has no allowed metrics; drop all events.
    Inactive,
    /// Autoscaling failover is active; forward only matching series metrics.
    FilteredForward { allowlist: HashSet<String> },
}

/// Autoscaling failover metrics gateway transform.
pub struct AutoscalingFailoverGateway {
    mode: GatewayMode,
}

impl AutoscalingFailoverGateway {
    fn new(failover_config: AutoscalingFailoverConfiguration) -> Self {
        let mode = Self::mode_for_config(&failover_config);

        Self { mode }
    }

    fn mode_for_config(failover_config: &AutoscalingFailoverConfiguration) -> GatewayMode {
        if !failover_config.is_branch_requested() {
            GatewayMode::Inactive
        } else {
            GatewayMode::FilteredForward {
                allowlist: failover_config.metrics().iter().cloned().collect(),
            }
        }
    }

    fn should_forward(&self, event: &Event) -> bool {
        let GatewayMode::FilteredForward { allowlist } = &self.mode else {
            return false;
        };
        let Event::Metric(metric) = event else {
            return false;
        };
        if !matches!(
            metric.values(),
            MetricValues::Counter(..) | MetricValues::Rate(..) | MetricValues::Gauge(..) | MetricValues::Set(..)
        ) {
            return false;
        }

        allowlist.contains(metric.context().name().as_ref())
    }

    async fn process_event_batch(
        &self, mut events: EventsBuffer, context: &mut TransformContext,
    ) -> Result<(), GenericError> {
        let input_events = events.len();
        events.remove_if(|event| !self.should_forward(event));

        let sent_events = context.dispatcher().buffered()?.send_all(events).await?;
        let dropped_events = input_events - sent_events;
        debug!(
            sent_events,
            dropped_events, "Autoscaling failover metrics gateway processed event batch."
        );

        Ok(())
    }
}

#[async_trait]
impl TransformBuilder for AutoscalingFailoverGatewayConfiguration {
    async fn build(&self, _context: ComponentContext) -> Result<Box<dyn Transform + Send>, GenericError> {
        Ok(Box::new(AutoscalingFailoverGateway::new(self.failover_config.clone())))
    }

    fn input_event_type(&self) -> EventType {
        EventType::Metric
    }

    fn outputs(&self) -> &[OutputDefinition<EventType>] {
        static OUTPUTS: &[OutputDefinition<EventType>] = &[OutputDefinition::default_output(EventType::Metric)];
        OUTPUTS
    }
}

impl MemoryBounds for AutoscalingFailoverGatewayConfiguration {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        let metrics = self.failover_config.metrics();
        builder
            .minimum()
            .with_single_value::<AutoscalingFailoverGateway>("component struct")
            .with_fixed_amount("hashset overhead", std::mem::size_of::<HashSet<String>>())
            .with_fixed_amount(
                "allowlist strings",
                metrics
                    .iter()
                    .map(|name| name.len() + std::mem::size_of::<String>())
                    .sum::<usize>(),
            )
            .with_fixed_amount(
                "hashset buckets",
                metrics.len() * std::mem::size_of::<Option<String>>() * 2,
            );
    }
}

#[async_trait]
impl Transform for AutoscalingFailoverGateway {
    async fn run(self: Box<Self>, mut context: TransformContext) -> Result<(), GenericError> {
        let mut health = context.take_health_handle();

        health.mark_ready();
        debug!(mode = ?self.mode, "Autoscaling failover metrics gateway transform started.");

        loop {
            select! {
                _ = health.live() => continue,
                maybe_events = context.events().next() => match maybe_events {
                    Some(events) => {
                        if let Err(e) = self.process_event_batch(events, &mut context).await {
                            error!(error = %e, "Autoscaling failover metrics gateway failed to process event batch.");
                        }
                    }
                    None => {
                        debug!("Event stream terminated, shutting down autoscaling failover metrics gateway transform.");
                        break;
                    }
                },
            }
        }

        debug!("Autoscaling failover metrics gateway transform stopped.");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use agent_data_plane_config::shared::AutoscalingFailover;
    use saluki_core::data_model::event::{metric::Metric, Event};

    use super::*;

    fn gateway_from_config(enabled: bool, metrics: Vec<&str>) -> AutoscalingFailoverGateway {
        let failover_config = AutoscalingFailoverConfiguration::from_configuration(&AutoscalingFailover {
            enabled,
            metrics: metrics.into_iter().map(String::from).collect(),
        })
        .expect("autoscaling failover configuration should build");
        AutoscalingFailoverGateway::new(failover_config)
    }

    #[test]
    fn inactive_gateway_drops_everything() {
        let gw = gateway_from_config(false, vec!["allowed.metric"]);

        assert!(!gw.should_forward(&Event::Metric(Metric::counter("allowed.metric", 1.0))));
    }

    #[test]
    fn empty_metric_allowlist_drops_everything() {
        let gw = gateway_from_config(true, vec![]);

        assert!(!gw.should_forward(&Event::Metric(Metric::counter("allowed.metric", 1.0))));
    }

    #[test]
    fn active_gateway_forwards_only_allowed_series_metrics() {
        let gw = gateway_from_config(
            true,
            vec!["allowed.counter", "allowed.gauge", "allowed.rate", "allowed.set"],
        );

        assert!(gw.should_forward(&Event::Metric(Metric::counter("allowed.counter", 1.0))));
        assert!(gw.should_forward(&Event::Metric(Metric::gauge("allowed.gauge", 1.0))));
        assert!(gw.should_forward(&Event::Metric(Metric::rate(
            "allowed.rate",
            1.0,
            Duration::from_secs(10)
        ))));
        assert!(gw.should_forward(&Event::Metric(Metric::set("allowed.set", "value"))));
        assert!(!gw.should_forward(&Event::Metric(Metric::counter("blocked.counter", 1.0))));
    }

    #[test]
    fn active_gateway_drops_sketch_metrics_even_when_allowed() {
        let gw = gateway_from_config(true, vec!["allowed.histogram", "allowed.distribution"]);

        assert!(!gw.should_forward(&Event::Metric(Metric::histogram("allowed.histogram", [1.0, 2.0, 3.0]))));
        assert!(!gw.should_forward(&Event::Metric(Metric::distribution(
            "allowed.distribution",
            [1.0, 2.0, 3.0]
        ))));
    }
}
