//! Renderer component basics.

use async_trait::async_trait;
use saluki_error::GenericError;

mod builder;
pub use self::builder::RendererBuilder;

mod context;
pub use self::context::RendererContext;

/// A renderer.
///
/// Renderers are the bridge between forwarders and the rest of the topology. They are responsible for rendering
/// telemetry events into output payloads that can then be forwarded. Most renderers are specific to a particular system
/// and not simply equivalent to a certain encoding or serialization format: while two renderers may both produce JSON,
/// they may produce different JSON formats such that one format only works for product A and the other only works for
/// product B. In essence, the process of taking telemetry events and sending them to product A ends up becoming the sum
/// of a specific rendered and forwarder combination.
///
/// Examples of typical renderers include Datadog Metrics, Events, and Service Checks.
#[async_trait]
pub trait Renderer {
    /// Runs the renderer.
    ///
    /// The renderer context provides access primarily to the event stream, used to receive events sent to the renderer,
    /// and the forwarder, used to send payloads to the downstream forwarder in the topology, as well as other
    /// information such as the component context.
    ///
    /// Renderers are expected to run indefinitely until their event stream is terminated, or an error occurs.
    ///
    /// # Errors
    ///
    /// If an unrecoverable error occurs while running, an error is returned.
    async fn run(self: Box<Self>, context: RendererContext) -> Result<(), GenericError>;
}
