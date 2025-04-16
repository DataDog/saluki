use std::time::Duration;

use async_trait::async_trait;
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_context::Context;
use saluki_core::components::{sources::*, ComponentContext};
use saluki_core::topology::OutputDefinition;
use saluki_error::GenericError;
use saluki_event::{metric::Metric, DataType, Event};
use tokio::{select, time::interval};
use tracing::{debug, error};

/// Heartbeat source.
///
/// Emits a "heartbeat" metric on a configurable interval.
#[derive(Clone, Debug)]
pub struct HeartbeatConfiguration {
    /// Interval for heartbeat metrics in seconds
    pub heartbeat_interval_secs: u64,
}

impl Default for HeartbeatConfiguration {
    fn default() -> Self {
        Self {
            heartbeat_interval_secs: 10,
        }
    }
}

/// Source that doesn't produce any events or optionally emits heartbeat metrics.
struct HeartbeatSource {
    heartbeat_interval_secs: u64,
}

#[async_trait]
impl Source for HeartbeatSource {
    async fn run(self: Box<Self>, mut context: SourceContext) -> Result<(), GenericError> {
        let mut global_shutdown = context.take_shutdown_handle();
        let mut health = context.take_health_handle();
        let mut tick_interval = interval(Duration::from_secs(self.heartbeat_interval_secs));

        health.mark_ready();
        debug!("Heartbeat source started.");

        loop {
            select! {
                _ = &mut global_shutdown => {
                    debug!("Received shutdown signal.");
                    break;
                },
                _ = health.live() => continue,
                _ = tick_interval.tick() => {
                    // Create a simple heartbeat metric
                    let metric_context = Context::from_static_name("none_source.heartbeat");
                    let metric = Metric::gauge(metric_context, 1.0);

                    let events = vec![Event::Metric(metric)];
                    if let Err(e) = context.forwarder().forward(events).await {
                        error!(error = %e, "Failed to forward heartbeat event.");
                    } else {
                        debug!("Emitted heartbeat metric.");
                    }
                }
            }
        }

        debug!("Heartbeat source stopped.");
        Ok(())
    }
}

#[async_trait]
impl SourceBuilder for HeartbeatConfiguration {
    async fn build(&self, _context: ComponentContext) -> Result<Box<dyn Source + Send>, GenericError> {
        Ok(Box::new(HeartbeatSource {
            heartbeat_interval_secs: self.heartbeat_interval_secs,
        }))
    }

    fn outputs(&self) -> &[OutputDefinition] {
        static OUTPUTS: [OutputDefinition; 1] = [OutputDefinition::default_output(DataType::Metric)];

        &OUTPUTS
    }
}

impl MemoryBounds for HeartbeatConfiguration {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        // Minimal memory footprint when emitting heartbeat metrics
        builder
            .minimum()
            .with_single_value::<HeartbeatSource>("component struct");
    }
}
