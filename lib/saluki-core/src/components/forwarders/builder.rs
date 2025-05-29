use async_trait::async_trait;
use memory_accounting::MemoryBounds;
use saluki_error::GenericError;

use super::Forwarder;
use crate::{components::ComponentContext, data_model::payload::PayloadType};

/// A forwarder builder.
///
/// Forwarder builders are responsible for creating instances of [`Forwarder`]s, as well as describing high-level
/// aspects of the built forwarder, such as the data types allowed for input events.
#[async_trait]
pub trait ForwarderBuilder: MemoryBounds {
    /// Data types allowed as input payloads to this forwarder.
    fn input_payload_type(&self) -> PayloadType;

    /// Builds an instance of the forwarder.
    ///
    /// ## Errors
    ///
    /// If the forwarder cannot be built for any reason, an error is returned.
    async fn build(&self, context: ComponentContext) -> Result<Box<dyn Forwarder + Send>, GenericError>;
}
