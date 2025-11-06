use async_trait::async_trait;
use memory_accounting::MemoryBounds;
use saluki_error::GenericError;

use super::Relay;
use crate::{components::ComponentContext, data_model::payload::PayloadType};

/// A relay builder.
///
/// Relay builders are responsible for creating instances of [`Relay`]s, as well as describing high-level aspects of
/// the built relay, such as the payload types emitted.
#[async_trait]
pub trait RelayBuilder: MemoryBounds {
    /// Data types emitted as output payloads by this relay.
    fn output_payload_type(&self) -> PayloadType;

    /// Builds an instance of the relay.
    ///
    /// ## Errors
    ///
    /// If the relay cannot be built for any reason, an error is returned.
    async fn build(&self, context: ComponentContext) -> Result<Box<dyn Relay + Send>, GenericError>;
}
