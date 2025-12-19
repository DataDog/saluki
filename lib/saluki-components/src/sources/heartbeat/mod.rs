use std::time::Duration;

use async_trait::async_trait;
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_context::Context;
use saluki_core::components::{sources::*, ComponentContext};
use saluki_core::data_model::event::{metric::Metric, Event, EventType};
use saluki_core::topology::OutputDefinition;
use saluki_error::GenericError;
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

struct Heartbeat {
    heartbeat_interval_secs: u64,
}

#[async_trait]
impl Source for Heartbeat {
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
                    let metric_context = Context::from_static_name("heartbeat");
                    let metric = Metric::gauge(metric_context, 1.0);
                    let mut buffered_dispatcher = context.dispatcher().buffered().expect("default output must always exist");

                    if let Err(e) = buffered_dispatcher.push(Event::Metric(metric)).await {
                        error!(error = %e, "Failed to dispatch event.");
                    } else if let Err(e) = buffered_dispatcher.flush().await {
                        error!(error = %e, "Failed to dispatch events.");
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
        Ok(Box::new(Heartbeat {
            heartbeat_interval_secs: self.heartbeat_interval_secs,
        }))
    }

    fn outputs(&self) -> &[OutputDefinition<EventType>] {
        static OUTPUTS: &[OutputDefinition<EventType>] = &[OutputDefinition::default_output(EventType::Metric)];
        OUTPUTS
    }
}

impl MemoryBounds for HeartbeatConfiguration {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        // Minimal memory footprint when emitting heartbeat metrics
        builder.minimum().with_single_value::<Heartbeat>("component struct");
    }
}
