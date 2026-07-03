use async_trait::async_trait;
use resource_accounting::MemoryBounds;
use saluki_error::GenericError;

use super::Relay;
use crate::{components::ComponentContext, data_model::payload::PayloadType, topology::OutputDefinition};

/// A relay builder.
///
/// Relay builders are responsible for creating instances of [`Relay`]s, as well as describing high-level aspects of
/// the built relay, such as the payload types emitted.
#[async_trait]
pub trait RelayBuilder: MemoryBounds {
    /// Returns the Rust type name for this builder.
    fn rust_type(&self) -> &'static str {
        std::any::type_name::<Self>()
    }

    /// Payload outputs exposed by this source.
    fn outputs(&self) -> &[OutputDefinition<PayloadType>];

    /// Builds an instance of the relay.
    ///
    /// ## Errors
    ///
    /// If the relay can't be built for any reason, an error is returned.
    async fn build(&self, context: ComponentContext) -> Result<Box<dyn Relay + Send>, GenericError>;
}
