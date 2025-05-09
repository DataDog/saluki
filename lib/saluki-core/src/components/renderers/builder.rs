#![allow(dead_code)]

use async_trait::async_trait;
use memory_accounting::MemoryBounds;
use saluki_error::GenericError;

use super::{IncrementalRenderer, Renderer};
use crate::{
    components::ComponentContext,
    data_model::{event::EventType, payload::PayloadType},
};

/// A renderer builder.
///
/// Renderer builders are responsible for creating instances of [`Renderer`]s, as well as describing high-level
/// aspects of the built renderer, such as the data types allowed for input events and the outputs exposed by the
/// renderer.
pub trait RendererBuilder: MemoryBounds {
    /// Data types allowed as input payloads to this renderer.
    fn input_event_type(&self) -> EventType;

    /// Data types emitted as output payloads by this renderer.
    fn output_payload_type(&self) -> PayloadType;

    /// Builds an instance of the renderer.
    ///
    /// # Errors
    ///
    /// If the renderer cannot be built for any reason, an error is returned.
    fn build(&self, context: ComponentContext) -> Result<Box<dyn Renderer + Send>, GenericError>;
}

/// An incremental renderer builder.
///
/// Incremental renderer builders are responsible for creating instances of [`IncrementalRenderer`]s, as well as
/// describing high-level aspects of the built incremental renderer, such as the data types allowed for input events and
/// the outputs exposed by the incremental renderer.
#[async_trait]
pub trait IncrementalRendererBuilder: MemoryBounds {
    /// Type of the incremental renderer to be built.
    type Output: IncrementalRenderer;

    /// Data types allowed as input payloads to this incremental renderer.
    fn input_event_type(&self) -> EventType;

    /// Data types emitted as output payloads by this incremental renderer.
    fn output_payload_type(&self) -> PayloadType;

    /// Builds an instance of the incremental renderer.
    ///
    /// # Errors
    ///
    /// If the incremental renderer cannot be built for any reason, an error is returned.
    fn build(&self, context: ComponentContext) -> Result<Self::Output, GenericError>;
}
