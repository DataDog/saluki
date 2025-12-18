//! Encoder component basics.

use async_trait::async_trait;
use saluki_error::GenericError;

mod builder;
pub use self::builder::DecoderBuilder;

mod context;
pub use self::context::DecoderContext;

/// A decoder.
///
/// Decoders are the bridge between relays and the rest of the topology. They are responsible for decoding payloads into
/// events that the rest of the topology can operate on. Decoders are generally specific to a particular
/// system/protocol: while the _codec_ used by one decoder might be the same as another decoder, the format of the
/// payload is likely to be highly contextual and so decoders cannot easily be swapped in and out.
///
/// Examples of typical decoders include OTLP and DogStatsD.
#[async_trait]
pub trait Decoder {
    /// Runs the decoder.
    ///
    /// The decoder context provides access primarily to the payload stream, used to receive payloads sent to the decoder,
    /// and the dispatcher, used to send events to the downstream component in the topology, as well as other
    /// information such as the component context.
    ///
    /// Decoders are expected to run indefinitely until their payload stream is terminated, or an error occurs.
    ///
    /// # Errors
    ///
    /// If an unrecoverable error occurs while running, an error is returned.
    async fn run(self: Box<Self>, context: DecoderContext) -> Result<(), GenericError>;
}
