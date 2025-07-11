//! Transform component basics.

use async_trait::async_trait;
use saluki_error::GenericError;

use crate::topology::EventsBuffer;

mod builder;
pub use self::builder::{SynchronousTransformBuilder, TransformBuilder};

mod context;
pub use self::context::TransformContext;

/// A transform.
///
/// Transforms sit in the middle of a topology, where events can be manipulated (e.g. sampled, filtered, enriched)
/// before being sent to the next component(s) in the topology. Examples of typical transforms include origin
/// enrichment, aggregation, and sampling.
#[async_trait]
pub trait Transform {
    /// Runs the transform.
    ///
    /// The transform context provides access primarily to the event stream, used to receive events sent to the
    /// transforms, and the forwarder, used to send events to the next component(s) in the topology, as well as other
    /// information such as the component context.
    ///
    /// Transforms are expected to run indefinitely until their event stream is terminated, or an error occurs.
    ///
    /// # Errors
    ///
    /// If an unrecoverable error occurs while running, an error is returned.
    async fn run(self: Box<Self>, context: TransformContext) -> Result<(), GenericError>;
}

/// A synchronous transform.
///
/// Synchronous transforms are conceptually similar to transforms, designed to manipulate events. However, in some
/// cases, it can be beneficial to perform multiple transformation operations as a single step, without the overhead of
/// individual transform components. Synchronous transforms allow discrete transformation logic to be combined together
/// within a single transform component for processing efficiency.
pub trait SynchronousTransform {
    /// Transforms the events in the event buffer.
    fn transform_buffer(&mut self, buffer: &mut EventsBuffer);
}
