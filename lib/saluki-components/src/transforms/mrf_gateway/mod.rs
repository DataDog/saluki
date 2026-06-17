//! MRF metrics gateway transform.

use std::collections::HashSet;

use async_trait::async_trait;
use resource_accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_component_config::forwarder::MrfConfig;
use saluki_component_config::ScopedConfig;
use saluki_core::{
    components::{
        transforms::{Transform, TransformBuilder, TransformContext},
        ComponentContext,
    },
    data_model::event::{Event, EventType},
    topology::{EventsBuffer, OutputDefinition},
};
use saluki_error::GenericError;
use tokio::select;
use tracing::{debug, error};

/// Returns whether metrics forwarding to the failover region is requested by configuration.
fn is_metrics_forwarding_requested(config: &MrfConfig) -> bool {
    config.enabled && config.failover_metrics
}

/// MRF metrics gateway transform configuration.
///
/// This transform sits between the enrichment stage and the MRF-specific encoder/forwarder. It owns
/// all routing and filtering decisions for the MRF metrics pipeline:
///
/// - When MRF is disabled, all events are dropped.
/// - When MRF is enabled with no allowlist, all events are forwarded.
/// - When MRF is enabled with an allowlist, only matching events are forwarded.
///
/// This is a dynamic-capable component: it consumes a [`ScopedConfig<MrfConfig>`] and rebuilds its
/// routing/filtering state whenever a new configuration value is published (notably
/// `failover_metrics` and `metric_allowlist`).
pub struct MrfMetricsGatewayConfiguration {
    config: ScopedConfig<MrfConfig>,
}

impl MrfMetricsGatewayConfiguration {
    /// Creates a new `MrfMetricsGatewayConfiguration` from the given native configuration handle.
    pub fn from_native(config: ScopedConfig<MrfConfig>) -> Self {
        Self { config }
    }
}

/// Routing and filtering state for the MRF metrics gateway.
#[derive(Debug)]
enum GatewayMode {
    /// MRF is disabled or improperly configured; drop all events.
    Inactive,
    /// MRF is active and no allowlist is configured; forward all events.
    ForwardAll,
    /// MRF is active and an allowlist is configured; forward only matching events.
    FilteredForward { allowlist: HashSet<String> },
}

impl GatewayMode {
    fn from_config(config: &MrfConfig) -> Self {
        if !is_metrics_forwarding_requested(config) {
            GatewayMode::Inactive
        } else if config.metric_allowlist.is_empty() {
            GatewayMode::ForwardAll
        } else {
            GatewayMode::FilteredForward {
                allowlist: config.metric_allowlist.iter().cloned().collect(),
            }
        }
    }
}

/// MRF metrics gateway transform.
pub struct MrfMetricsGateway {
    mode: GatewayMode,
    config: ScopedConfig<MrfConfig>,
}

impl MrfMetricsGateway {
    fn new(config: ScopedConfig<MrfConfig>) -> Self {
        let mode = GatewayMode::from_config(&config.current());

        Self { mode, config }
    }

    fn rebuild_mode(&mut self) {
        self.mode = GatewayMode::from_config(&self.config.current());
    }

    fn should_forward(&self, event: &Event) -> bool {
        match &self.mode {
            GatewayMode::Inactive => false,
            GatewayMode::ForwardAll => true,
            GatewayMode::FilteredForward { allowlist } => {
                let Event::Metric(metric) = event else {
                    return false;
                };
                allowlist.contains(metric.context().name().as_ref())
            }
        }
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
            "MRF metrics gateway processed event batch."
        );

        Ok(())
    }
}

#[async_trait]
impl TransformBuilder for MrfMetricsGatewayConfiguration {
    async fn build(&self, _context: ComponentContext) -> Result<Box<dyn Transform + Send>, GenericError> {
        Ok(Box::new(MrfMetricsGateway::new(self.config.clone())))
    }

    fn input_event_type(&self) -> EventType {
        EventType::Metric
    }

    fn outputs(&self) -> &[OutputDefinition<EventType>] {
        static OUTPUTS: &[OutputDefinition<EventType>] = &[OutputDefinition::default_output(EventType::Metric)];
        OUTPUTS
    }
}

impl MemoryBounds for MrfMetricsGatewayConfiguration {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        let current = self.config.current();
        let allowlist = &current.metric_allowlist;
        builder
            .minimum()
            .with_single_value::<MrfMetricsGateway>("component struct")
            .with_fixed_amount("hashset overhead", std::mem::size_of::<HashSet<String>>())
            .with_fixed_amount(
                "allowlist strings",
                allowlist
                    .iter()
                    .map(|name| name.len() + std::mem::size_of::<String>())
                    .sum::<usize>(),
            )
            .with_fixed_amount(
                "hashset buckets",
                allowlist.len() * std::mem::size_of::<Option<String>>() * 2,
            );
    }
}

#[async_trait]
impl Transform for MrfMetricsGateway {
    async fn run(mut self: Box<Self>, mut context: TransformContext) -> Result<(), GenericError> {
        let mut health = context.take_health_handle();

        health.mark_ready();
        debug!(mode = ?self.mode, "MRF metrics gateway transform started.");

        loop {
            select! {
                _ = health.live() => continue,
                maybe_events = context.events().next() => match maybe_events {
                    Some(events) => {
                        if let Err(e) = self.process_event_batch(events, &mut context).await {
                            error!(error = %e, "MRF metrics gateway failed to process event batch.");
                        }
                    }
                    None => {
                        debug!("Event stream terminated, shutting down MRF metrics gateway transform.");
                        break;
                    }
                },
                _ = self.config.changed() => {
                    self.rebuild_mode();
                    debug!(mode = ?self.mode, "MRF metrics gateway rebuilt routing state from updated configuration.");
                },
            }
        }

        debug!("MRF metrics gateway transform stopped.");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use saluki_core::data_model::event::{metric::Metric, Event};
    use tokio::sync::watch;

    use super::*;

    fn mrf_config(enabled: bool, failover_metrics: bool, allowlist: &[&str]) -> MrfConfig {
        MrfConfig {
            enabled,
            failover_metrics,
            metric_allowlist: allowlist.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn failover_metrics_dynamic_update_toggles_forwarding() {
        let initial = mrf_config(true, false, &[]);
        let (tx, rx) = watch::channel(initial.clone());
        let mut gw = MrfMetricsGateway::new(ScopedConfig::live(initial, rx));

        assert!(!gw.should_forward(&Event::Metric(Metric::counter("any.metric", 1.0))));

        tx.send(mrf_config(true, true, &[])).expect("receiver alive");
        gw.config.changed().await;
        gw.rebuild_mode();
        assert!(gw.should_forward(&Event::Metric(Metric::counter("any.metric", 1.0))));

        tx.send(mrf_config(true, false, &[])).expect("receiver alive");
        gw.config.changed().await;
        gw.rebuild_mode();
        assert!(!gw.should_forward(&Event::Metric(Metric::counter("any.metric", 1.0))));
    }

    #[tokio::test]
    async fn metric_allowlist_dynamic_update_changes_filtering() {
        let initial = mrf_config(true, true, &[]);
        let (tx, rx) = watch::channel(initial.clone());
        let mut gw = MrfMetricsGateway::new(ScopedConfig::live(initial, rx));

        assert!(gw.should_forward(&Event::Metric(Metric::counter("allowed.metric", 1.0))));
        assert!(gw.should_forward(&Event::Metric(Metric::counter("also.allowed", 1.0))));

        tx.send(mrf_config(true, true, &["also.allowed"]))
            .expect("receiver alive");
        gw.config.changed().await;
        gw.rebuild_mode();

        assert!(!gw.should_forward(&Event::Metric(Metric::counter("allowed.metric", 1.0))));
        assert!(gw.should_forward(&Event::Metric(Metric::counter("also.allowed", 1.0))));
        assert!(!gw.should_forward(&Event::Metric(Metric::counter("blocked.metric", 1.0))));
    }
}
