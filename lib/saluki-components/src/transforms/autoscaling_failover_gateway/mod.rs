//! Autoscaling failover metrics gateway transform.

use std::collections::HashSet;

use async_trait::async_trait;
use resource_accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_config::GenericConfiguration;
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
    configuration: GenericConfiguration,
}

impl AutoscalingFailoverGatewayConfiguration {
    /// Creates a new `AutoscalingFailoverGatewayConfiguration` from the given failover configuration.
    pub fn new(failover_config: AutoscalingFailoverConfiguration, configuration: GenericConfiguration) -> Self {
        Self {
            failover_config,
            configuration,
        }
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
    failover_config: AutoscalingFailoverConfiguration,
    mode: GatewayMode,
    configuration: GenericConfiguration,
}

impl AutoscalingFailoverGateway {
    fn new(failover_config: AutoscalingFailoverConfiguration, configuration: GenericConfiguration) -> Self {
        let mode = Self::mode_for_config(&failover_config);

        Self {
            failover_config,
            mode,
            configuration,
        }
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

    fn update_enabled(&mut self, enabled: bool) {
        self.failover_config.set_enabled(enabled);
        self.mode = Self::mode_for_config(&self.failover_config);
    }

    fn update_metrics(&mut self, metrics: Vec<String>) {
        self.failover_config.set_metrics(metrics);
        self.mode = Self::mode_for_config(&self.failover_config);
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
        let input_count = events.len();
        events.remove_if(|event| !self.should_forward(event));
        let forwarded_count = events.len();
        let dropped_count = input_count.saturating_sub(forwarded_count);

        let sent_count = context.dispatcher().buffered()?.send_all(events).await?;
        debug!(
            forwarded_events = sent_count,
            dropped_events = dropped_count,
            "Autoscaling failover metrics gateway processed event batch."
        );

        Ok(())
    }
}

#[async_trait]
impl TransformBuilder for AutoscalingFailoverGatewayConfiguration {
    async fn build(&self, _context: ComponentContext) -> Result<Box<dyn Transform + Send>, GenericError> {
        Ok(Box::new(AutoscalingFailoverGateway::new(
            self.failover_config.clone(),
            self.configuration.clone(),
        )))
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
    async fn run(mut self: Box<Self>, mut context: TransformContext) -> Result<(), GenericError> {
        let mut health = context.take_health_handle();
        let mut enabled_watcher = self.configuration.watch_for_updates("autoscaling.failover.enabled");
        let mut metrics_watcher = self.configuration.watch_for_updates("autoscaling.failover.metrics");

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
                (_, maybe_enabled) = enabled_watcher.changed::<bool>() => {
                    if let Some(enabled) = maybe_enabled {
                        self.update_enabled(enabled);
                    }
                },
                (_, maybe_metrics) = metrics_watcher.changed::<Vec<String>>() => {
                    if let Some(metrics) = maybe_metrics {
                        self.update_metrics(metrics);
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

    use saluki_config::{dynamic::ConfigUpdate, ConfigurationLoader};
    use saluki_core::data_model::event::{metric::Metric, Event};
    use serde_json::json;

    use super::*;

    async fn gateway_from_config(value: serde_json::Value) -> AutoscalingFailoverGateway {
        let (config, _) = ConfigurationLoader::for_tests(Some(value), None, false).await;
        let failover_config = AutoscalingFailoverConfiguration::from_configuration(&config)
            .expect("autoscaling failover configuration should deserialize");
        AutoscalingFailoverGateway::new(failover_config, config)
    }

    async fn dynamic_gateway_from_config(
        value: serde_json::Value,
    ) -> (AutoscalingFailoverGateway, tokio::sync::mpsc::Sender<ConfigUpdate>) {
        let (config, sender) = ConfigurationLoader::for_tests(Some(value), None, true).await;
        let sender = sender.expect("dynamic sender should exist");
        sender
            .send(ConfigUpdate::Snapshot(json!({})))
            .await
            .expect("initial dynamic snapshot should be sent");
        config.ready().await;

        let failover_config = AutoscalingFailoverConfiguration::from_configuration(&config)
            .expect("autoscaling failover configuration should deserialize");
        (AutoscalingFailoverGateway::new(failover_config, config), sender)
    }

    #[tokio::test]
    async fn inactive_gateway_drops_everything() {
        let gw = gateway_from_config(json!({
            "autoscaling": {
                "failover": {
                    "enabled": false,
                    "metrics": ["allowed.metric"]
                }
            }
        }))
        .await;

        assert!(!gw.should_forward(&Event::Metric(Metric::counter("allowed.metric", 1.0))));
    }

    #[tokio::test]
    async fn empty_metric_allowlist_drops_everything() {
        let gw = gateway_from_config(json!({
            "autoscaling": {
                "failover": {
                    "enabled": true,
                    "metrics": []
                }
            }
        }))
        .await;

        assert!(!gw.should_forward(&Event::Metric(Metric::counter("allowed.metric", 1.0))));
    }

    #[tokio::test]
    async fn active_gateway_forwards_only_allowed_series_metrics() {
        let gw = gateway_from_config(json!({
            "autoscaling": {
                "failover": {
                    "enabled": true,
                    "metrics": ["allowed.counter", "allowed.gauge", "allowed.rate", "allowed.set"]
                }
            }
        }))
        .await;

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

    #[tokio::test]
    async fn active_gateway_drops_sketch_metrics_even_when_allowed() {
        let gw = gateway_from_config(json!({
            "autoscaling": {
                "failover": {
                    "enabled": true,
                    "metrics": ["allowed.histogram", "allowed.distribution"]
                }
            }
        }))
        .await;

        assert!(!gw.should_forward(&Event::Metric(Metric::histogram("allowed.histogram", [1.0, 2.0, 3.0]))));
        assert!(!gw.should_forward(&Event::Metric(Metric::distribution(
            "allowed.distribution",
            [1.0, 2.0, 3.0]
        ))));
    }

    #[tokio::test]
    async fn dynamic_enabled_update_toggles_forwarding() {
        let (mut gw, sender) = dynamic_gateway_from_config(json!({
            "autoscaling": {
                "failover": {
                    "enabled": false,
                    "metrics": ["allowed.metric"]
                }
            }
        }))
        .await;
        let mut watcher = gw.configuration.watch_for_updates("autoscaling.failover.enabled");

        assert!(!gw.should_forward(&Event::Metric(Metric::counter("allowed.metric", 1.0))));

        sender
            .send(ConfigUpdate::Partial {
                key: "autoscaling.failover.enabled".to_string(),
                value: json!(true),
            })
            .await
            .expect("dynamic update should be sent");
        let (_, maybe_enabled) = tokio::time::timeout(Duration::from_secs(2), watcher.changed::<bool>())
            .await
            .expect("enabled update should be received");
        gw.update_enabled(maybe_enabled.expect("update should have a new value"));
        assert!(gw.should_forward(&Event::Metric(Metric::counter("allowed.metric", 1.0))));
    }

    #[tokio::test]
    async fn dynamic_metrics_update_changes_filtering() {
        let (mut gw, sender) = dynamic_gateway_from_config(json!({
            "autoscaling": {
                "failover": {
                    "enabled": true,
                    "metrics": []
                }
            }
        }))
        .await;
        let mut watcher = gw.configuration.watch_for_updates("autoscaling.failover.metrics");

        assert!(!gw.should_forward(&Event::Metric(Metric::counter("first.metric", 1.0))));
        assert!(!gw.should_forward(&Event::Metric(Metric::counter("second.metric", 1.0))));

        sender
            .send(ConfigUpdate::Partial {
                key: "autoscaling.failover.metrics".to_string(),
                value: json!(["second.metric"]),
            })
            .await
            .expect("dynamic update should be sent");
        let updated_metrics = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let (_, maybe_metrics) = watcher.changed::<Vec<String>>().await;
                if let Some(metrics) = maybe_metrics {
                    if metrics == ["second.metric".to_string()] {
                        break metrics;
                    }
                }
            }
        })
        .await
        .expect("metrics update should be received");
        gw.update_metrics(updated_metrics);

        assert!(!gw.should_forward(&Event::Metric(Metric::counter("first.metric", 1.0))));
        assert!(gw.should_forward(&Event::Metric(Metric::counter("second.metric", 1.0))));
    }
}
