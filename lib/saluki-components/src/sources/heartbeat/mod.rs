use std::time::Duration;

use async_trait::async_trait;
use saluki_context::Context;
use saluki_core::accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_core::components::{sources::*, ComponentContext};
use saluki_core::data_model::event::{metric::Metric, Event, EventType};
use saluki_core::topology::OutputDefinition;
use saluki_error::GenericError;
use tokio::pin;
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
        let global_shutdown = context.take_shutdown_handle();
        pin!(global_shutdown);

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

#[cfg(test)]
mod tests {
    use std::mem::size_of;

    use saluki_core::{accounting::ComponentRegistry, support::SubsystemIdentifier};

    use super::*;

    #[test]
    fn default_configuration_uses_ten_second_interval() {
        let config = HeartbeatConfiguration::default();
        assert_eq!(config.heartbeat_interval_secs, 10);
    }

    #[test]
    fn declares_single_default_output_for_metrics() {
        // The source's documented behavior is to emit a heartbeat metric, so it must declare exactly one output --
        // the default (unnamed) one -- carrying metric events.
        let config = HeartbeatConfiguration::default();
        let outputs = config.outputs();

        assert_eq!(outputs.len(), 1);
        assert_eq!(
            outputs[0].output_name(),
            None,
            "heartbeat emits on the default (unnamed) output"
        );
        assert_eq!(outputs[0].data_ty(), EventType::Metric);
    }

    #[test]
    fn specify_bounds_accounts_only_for_the_boxed_component_struct() {
        // The bounds are documented as a "minimal memory footprint": a single boxed `Heartbeat` value and nothing
        // else. The firm limit includes the minimum, so with no additional firm usage both totals equal the struct
        // size.
        let config = HeartbeatConfiguration::default();

        let registry = ComponentRegistry::default();
        config.specify_bounds(&mut registry.bounds_builder(&SubsystemIdentifier::from_dotted("test")));
        let bounds = registry.as_bounds();

        assert_eq!(bounds.total_minimum_required_bytes(), size_of::<Heartbeat>());
        assert_eq!(bounds.total_firm_limit_bytes(), size_of::<Heartbeat>());
    }
}
