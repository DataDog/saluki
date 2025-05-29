use async_trait::async_trait;
use memory_accounting::MemoryBounds;
use saluki_error::GenericError;

use super::Destination;
use crate::{components::ComponentContext, data_model::event::EventType};

/// A destination builder.
///
/// Destination builders are responsible for creating instances of [`Destination`]s, as well as describing high-level
/// aspects of the built destination, such as the data types allowed for input events.
#[async_trait]
pub trait DestinationBuilder: MemoryBounds {
    /// Event types allowed as input events to this destination.
    fn input_event_type(&self) -> EventType;

    /// Builds an instance of the destination.
    ///
    /// ## Errors
    ///
    /// If the destination cannot be built for any reason, an error is returned.
    async fn build(&self, context: ComponentContext) -> Result<Box<dyn Destination + Send>, GenericError>;
}
