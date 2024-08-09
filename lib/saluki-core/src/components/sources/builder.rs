use async_trait::async_trait;
use memory_accounting::MemoryBounds;
use saluki_error::GenericError;

use super::Source;
use crate::topology::OutputDefinition;

/// A source builder.
///
/// Source builders are responsible for creating instances of [`Source`]s, as well as describing high-level aspects of
/// the built source, such as the event outputs exposed by the source.
#[async_trait]
pub trait SourceBuilder: MemoryBounds {
    /// Event outputs exposed by this source.
    fn outputs(&self) -> &[OutputDefinition];

    /// Builds an instance of the source.
    ///
    /// ## Errors
    ///
    /// If the source cannot be built for any reason, an error is returned.
    async fn build(&self) -> Result<Box<dyn Source + Send>, GenericError>;
}
