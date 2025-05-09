use async_trait::async_trait;
use memory_accounting::MemoryBounds;
use saluki_error::GenericError;

use super::Renderer;
use crate::{
    components::ComponentContext,
    data_model::{event::EventType, payload::PayloadType},
};

/// A renderer builder.
///
/// Renderer builders are responsible for creating instances of [`Renderer`]s, as well as describing high-level
/// aspects of the built renderer, such as the data types allowed for input events and the outputs exposed by the
/// renderer.
#[async_trait]
pub trait RendererBuilder: MemoryBounds {
    /// Data types allowed as input payloads to this renderer.
    fn input_event_type(&self) -> EventType;

    /// Data types emitted as output payloads by this renderer.
    fn output_payload_type(&self) -> PayloadType;

    /// Builds an instance of the renderer.
    ///
    /// ## Errors
    ///
    /// If the renderer cannot be built for any reason, an error is returned.
    async fn build(&self, context: ComponentContext) -> Result<Box<dyn Renderer + Send>, GenericError>;
}
