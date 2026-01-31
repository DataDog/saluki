//! Relay component basics.

use async_trait::async_trait;
use saluki_error::GenericError;

mod builder;
pub use self::builder::RelayBuilder;

mod context;
pub use self::context::RelayContext;

/// A relay.
///
/// Relays are the initial entrypoint to a topology for accepting raw payloads. Unlike sources,
/// which decode/parse incoming data into events, relays simply accept raw bytes from the network and
/// output them as payloads for downstream processing.
#[async_trait]
pub trait Relay {
    /// Runs the relay.
    ///
    /// The relay context provides access primarily to the payloads dispatcher, used to send raw payloads
    /// to the next component(s) in the topology, as well as other information such as the component context.
    ///
    /// Relays are expected to run indefinitely until their shutdown is triggered by the topology, or an error occurs.
    ///
    /// # Errors
    ///
    /// If an unrecoverable error occurs while running, an error is returned.
    async fn run(self: Box<Self>, context: RelayContext) -> Result<(), GenericError>;
}
