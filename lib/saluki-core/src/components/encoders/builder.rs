#![allow(dead_code)]

use async_trait::async_trait;
use memory_accounting::MemoryBounds;
use saluki_error::GenericError;

use super::{Encoder, IncrementalEncoder};
use crate::{
    components::ComponentContext,
    data_model::{event::EventType, payload::PayloadType},
};

/// A encoder builder.
///
/// Encoder builders are responsible for creating instances of [`Encoder`]s, as well as describing high-level
/// aspects of the built encoder, such as the data types allowed for input events and the outputs exposed by the
/// encoder.
pub trait EncoderBuilder: MemoryBounds {
    /// Data types allowed as input payloads to this encoder.
    fn input_event_type(&self) -> EventType;

    /// Data types emitted as output payloads by this encoder.
    fn output_payload_type(&self) -> PayloadType;

    /// Builds an instance of the encoder.
    ///
    /// # Errors
    ///
    /// If the encoder cannot be built for any reason, an error is returned.
    fn build(&self, context: ComponentContext) -> Result<Box<dyn Encoder + Send>, GenericError>;
}

/// An incremental encoder builder.
///
/// Incremental encoder builders are responsible for creating instances of [`IncrementalEncoder`]s, as well as
/// describing high-level aspects of the built incremental encoder, such as the data types allowed for input events and
/// the outputs exposed by the incremental encoder.
#[async_trait]
pub trait IncrementalEncoderBuilder: MemoryBounds {
    /// Type of the incremental encoder to be built.
    type Output: IncrementalEncoder;

    /// Data types allowed as input payloads to this incremental encoder.
    fn input_event_type(&self) -> EventType;

    /// Data types emitted as output payloads by this incremental encoder.
    fn output_payload_type(&self) -> PayloadType;

    /// Builds an instance of the incremental encoder.
    ///
    /// # Errors
    ///
    /// If the incremental encoder cannot be built for any reason, an error is returned.
    fn build(&self, context: ComponentContext) -> Result<Self::Output, GenericError>;
}
