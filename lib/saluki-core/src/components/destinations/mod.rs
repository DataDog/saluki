//! Destination component basics.

use async_trait::async_trait;
use saluki_error::GenericError;

mod builder;
pub use self::builder::DestinationBuilder;

mod context;
pub use self::context::DestinationContext;

/// A destination.
///
/// Destinations are the final step in a topology, where events are sent to an external system, or exposed for
/// collection. Examples of typical destinations include databases, HTTP services, log files, and object storage.
#[async_trait]
pub trait Destination {
    /// Runs the destination.
    ///
    /// The destination context provides access primarily to the event stream, used to receive events sent to the
    /// destination, as well as other information such as the component context.
    ///
    /// Destinations are expected to run indefinitely until their event stream is terminated, or an error occurs.
    ///
    /// ## Errors
    ///
    /// If an unrecoverable error occurs while running, an error is returned.
    async fn run(self: Box<Self>, context: DestinationContext) -> Result<(), GenericError>;
}
