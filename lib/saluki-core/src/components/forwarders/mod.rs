//! Forwarder component basics.

use async_trait::async_trait;
use saluki_error::GenericError;

mod builder;
pub use self::builder::ForwarderBuilder;

mod context;
pub use self::context::ForwarderContext;

/// A forwarder.
///
/// Forwarders are the final step in a topology, where events are sent to an external system, or exposed for
/// collection. Examples of typical forwarders include databases, HTTP services, log files, and object storage.
#[async_trait]
pub trait Forwarder {
    /// Runs the forwarder.
    ///
    /// The forwarder context provides access primarily to the payload stream, used to receive payloads sent to the
    /// forwarder, as well as other information such as the component context.
    ///
    /// Forwarders are expected to run indefinitely until their payload stream is terminated, or an error occurs.
    ///
    /// # Errors
    ///
    /// If an unrecoverable error occurs while running, an error is returned.
    async fn run(self: Box<Self>, context: ForwarderContext) -> Result<(), GenericError>;
}
