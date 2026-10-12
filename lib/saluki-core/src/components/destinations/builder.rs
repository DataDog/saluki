use async_trait::async_trait;
use saluki_error::GenericError;

use super::Destination;
use crate::accounting::MemoryBounds;
use crate::{components::BuildContext, data_model::event::EventType, topology::interconnect::InputGate};

/// A destination builder.
///
/// Destination builders are responsible for creating instances of [`Destination`]s, as well as describing high-level
/// aspects of the built destination, such as the data types allowed for input events.
#[async_trait]
pub trait DestinationBuilder: MemoryBounds {
    /// Event types allowed as input events to this destination.
    fn input_event_type(&self) -> EventType;

    /// Returns the gate on this destination's input, for a destination that only observes what it receives at times
    /// of its choosing, such as a tap that collects statistics on demand.
    ///
    /// While the destination keeps the gate closed, the components upstream skip sending to it, and count what they
    /// skipped as received by it; see [`InputGate`]. The default has no gate: the destination receives everything.
    fn input_gate(&self) -> Option<InputGate> {
        None
    }

    /// Builds an instance of the destination.
    ///
    /// ## Errors
    ///
    /// If the destination can't be built for any reason, an error is returned.
    async fn build(&self, context: BuildContext) -> Result<Box<dyn Destination + Send>, GenericError>;
}
