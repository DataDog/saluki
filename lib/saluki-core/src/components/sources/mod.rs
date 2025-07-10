//! Source component basics.

use async_trait::async_trait;
use saluki_error::GenericError;

mod builder;
use crate::topology::TopologyConfiguration;

pub use self::builder::SourceBuilder;

mod context;
pub use self::context::SourceContext;

/// A source.
///
/// Sources are the initial entrypoint to a topology, where events are ingested from external systems, or sometimes
/// generated. Examples of typical sources include StatsD, log files, and message queues.
#[async_trait]
pub trait Source {
    /// Runs the source.
    ///
    /// The source context provides access primarily to the forwarder, used to send events to the next component(s) in
    /// the topology, as well as other information such as the component context.
    ///
    /// Sources are expected to run indefinitely until their shutdown is triggered by the topology, or an error occurs.
    ///
    /// # Errors
    ///
    /// If an unrecoverable error occurs while running, an error is returned.
    async fn run<T: TopologyConfiguration>(self: Box<Self>, context: SourceContext<T>) -> Result<(), GenericError>;
}
