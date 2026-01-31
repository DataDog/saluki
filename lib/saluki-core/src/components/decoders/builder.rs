#![allow(dead_code)]

use async_trait::async_trait;
use memory_accounting::MemoryBounds;
use saluki_error::GenericError;

use super::Decoder;
use crate::{
    components::ComponentContext,
    data_model::{event::EventType, payload::PayloadType},
};

/// A decoder builder.
///
/// Decoder builders are responsible for creating instances of [`Decoder`]s, as well as describing high-level
/// aspects of the built decoder, such as the data types allowed for input events and the outputs exposed by the
/// decoder.
#[async_trait]
pub trait DecoderBuilder: MemoryBounds {
    /// Data types allowed as input payloads to this decoder.
    fn input_payload_type(&self) -> PayloadType;

    /// Data types emitted as output events by this decoder.
    fn output_event_type(&self) -> EventType;

    /// Builds an instance of the decoder.
    ///
    /// # Errors
    ///
    /// If the decoder cannot be built for any reason, an error is returned.
    async fn build(&self, context: ComponentContext) -> Result<Box<dyn Decoder + Send>, GenericError>;
}
