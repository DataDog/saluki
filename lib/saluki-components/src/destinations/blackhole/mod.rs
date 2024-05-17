use std::time::{Duration, Instant};

use async_trait::async_trait;
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_core::components::destinations::*;
use saluki_error::GenericError;
use saluki_event::DataType;
use tracing::{debug, info};

/// Blackhole destination.
///
/// Does nothing with the events it receives. It's useful for testing, providing both a valid destination implementation
/// while also periodicially emitting the number of events it has received.
#[derive(Default)]
pub struct BlackholeConfiguration;

#[async_trait]
impl DestinationBuilder for BlackholeConfiguration {
    fn input_data_type(&self) -> DataType {
        DataType::all()
    }

    async fn build(&self) -> Result<Box<dyn Destination + Send>, GenericError> {
        Ok(Box::new(Blackhole))
    }
}

impl MemoryBounds for BlackholeConfiguration {
    fn specify_bounds(&self, _builder: &mut MemoryBoundsBuilder) {}
}

struct Blackhole;

#[async_trait]
impl Destination for Blackhole {
    async fn run(mut self: Box<Self>, mut context: DestinationContext) -> Result<(), ()> {
        let mut last_update = Instant::now();
        let mut event_counter = 0;

        debug!("Blackhole destination started.");

        while let Some(events) = context.events().next().await {
            event_counter += events.len();

            if last_update.elapsed() > Duration::from_secs(1) {
                info!("Received {} events.", event_counter);
                last_update = Instant::now();
                event_counter = 0;
            }
        }

        debug!("Datadog Metrics destination stopped.");

        Ok(())
    }
}
