use std::time::{Duration, Instant};

use async_trait::async_trait;
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_core::components::{destinations::*, ComponentContext};
use saluki_error::GenericError;
use saluki_event::DataType;
use tokio::select;
use tracing::{debug, info};

/// Blackhole destination.
///
/// Does nothing with the events it receives. It's useful for testing, providing both a valid destination implementation
/// while also periodically emitting the number of events it has received.
#[derive(Default)]
pub struct BlackholeConfiguration;

#[async_trait]
impl DestinationBuilder for BlackholeConfiguration {
    fn input_data_type(&self) -> DataType {
        DataType::all_bits()
    }

    async fn build(&self, _context: ComponentContext) -> Result<Box<dyn Destination + Send>, GenericError> {
        Ok(Box::new(Blackhole))
    }
}

impl MemoryBounds for BlackholeConfiguration {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        // Capture the size of the heap allocation when the component is built.
        builder.minimum().with_single_value::<Blackhole>();
    }
}

struct Blackhole;

#[async_trait]
impl Destination for Blackhole {
    async fn run(mut self: Box<Self>, mut context: DestinationContext) -> Result<(), GenericError> {
        let mut health = context.take_health_handle();

        let mut last_update = Instant::now();
        let mut event_counter = 0;

        health.mark_ready();
        debug!("Blackhole destination started.");

        loop {
            select! {
                _ = health.live() => continue,
                result = context.events().next() => match result {
                    Some(events) => {
                        event_counter += events.len();

                        if last_update.elapsed() > Duration::from_secs(1) {
                            info!("Received {} events.", event_counter);
                            last_update = Instant::now();
                            event_counter = 0;
                        }
                    },
                    None => break,
                },
            }
        }

        debug!("Blackhole destination stopped.");

        Ok(())
    }
}
