//! MRF metrics gateway transform.

use std::collections::HashSet;

use async_trait::async_trait;
use resource_accounting::{MemoryBounds, MemoryBoundsBuilder};
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
use tracing::{debug, error, warn};

use crate::config::MrfConfiguration;

/// MRF metrics gateway transform configuration.
///
/// This transform sits between the enrichment stage and the MRF-specific encoder/forwarder. It owns
/// all routing and filtering decisions for the MRF metrics pipeline:
///
/// - When MRF is disabled, all events are dropped.
/// - When MRF is enabled with no allowlist, all events are forwarded.
/// - When MRF is enabled with an allowlist, only matching events are forwarded.
///
/// The transform reads from a snapshot of [`MrfConfiguration`] taken at build time.
pub struct MrfMetricsGatewayConfiguration {
    mrf_config: MrfConfiguration,
}

impl MrfMetricsGatewayConfiguration {
    /// Creates a new `MrfMetricsGatewayConfiguration` from the given [`MrfConfiguration`].
    pub fn new(mrf_config: MrfConfiguration) -> Self {
        Self { mrf_config }
    }
}

/// Routing and filtering state for the MRF metrics gateway.
#[derive(Debug)]
enum GatewayMode {
    /// MRF is disabled or improperly configured — drop all events.
    Inactive,
    /// MRF is active and no allowlist is configured — forward all events.
    ForwardAll,
    /// MRF is active and an allowlist is configured — forward only matching events.
    FilteredForward { allowlist: HashSet<String> },
}

/// MRF metrics gateway transform.
#[derive(Debug)]
pub struct MrfMetricsGateway {
    mode: GatewayMode,
}

impl MrfMetricsGateway {
    fn new(mrf_config: &MrfConfiguration) -> Self {
        let mode = if !mrf_config.is_metrics_active() {
            GatewayMode::Inactive
        } else if mrf_config.metric_allowlist().is_empty() {
            GatewayMode::ForwardAll
        } else {
            GatewayMode::FilteredForward {
                allowlist: mrf_config.metric_allowlist().iter().cloned().collect(),
            }
        };

        Self { mode }
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
        let gateway = MrfMetricsGateway::new(&self.mrf_config);

        match &gateway.mode {
            GatewayMode::Inactive => {
                warn!("MRF metrics gateway is inactive — all metrics will be dropped.");
            }
            GatewayMode::ForwardAll => {
                debug!("MRF metrics gateway active — forwarding all metrics.");
            }
            GatewayMode::FilteredForward { allowlist } => {
                debug!(
                    allowlist_size = allowlist.len(),
                    "MRF metrics gateway active — forwarding allowlisted metrics only."
                );
            }
        }

        Ok(Box::new(gateway))
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
        let allowlist = self.mrf_config.metric_allowlist();
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
    async fn run(self: Box<Self>, mut context: TransformContext) -> Result<(), GenericError> {
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
                }
            }
        }

        debug!("MRF metrics gateway transform stopped.");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use saluki_core::data_model::event::{metric::Metric, Event};

    use super::*;

    fn inactive_gateway() -> MrfMetricsGateway {
        MrfMetricsGateway {
            mode: GatewayMode::Inactive,
        }
    }

    fn forward_all_gateway() -> MrfMetricsGateway {
        MrfMetricsGateway {
            mode: GatewayMode::ForwardAll,
        }
    }

    fn filtered_gateway(names: &[&str]) -> MrfMetricsGateway {
        MrfMetricsGateway {
            mode: GatewayMode::FilteredForward {
                allowlist: names.iter().map(|s| s.to_string()).collect(),
            },
        }
    }

    #[test]
    fn inactive_drops_all_events() {
        let gw = inactive_gateway();
        assert!(!gw.should_forward(&Event::Metric(Metric::counter("any.metric", 1.0))));
    }

    #[test]
    fn forward_all_passes_all_events() {
        let gw = forward_all_gateway();
        assert!(gw.should_forward(&Event::Metric(Metric::counter("any.metric", 1.0))));
        assert!(gw.should_forward(&Event::Metric(Metric::counter("other.metric", 1.0))));
    }

    #[test]
    fn filtered_forward_passes_only_matching_events() {
        let gw = filtered_gateway(&["allowed.metric", "also.allowed"]);
        assert!(gw.should_forward(&Event::Metric(Metric::counter("allowed.metric", 1.0))));
        assert!(gw.should_forward(&Event::Metric(Metric::counter("also.allowed", 1.0))));
        assert!(!gw.should_forward(&Event::Metric(Metric::counter("blocked.metric", 1.0))));
    }
}
