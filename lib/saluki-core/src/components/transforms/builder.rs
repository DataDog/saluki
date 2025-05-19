use async_trait::async_trait;
use memory_accounting::MemoryBounds;
use saluki_error::GenericError;

use super::{SynchronousTransform, Transform};
use crate::{components::ComponentContext, data_model::event::DataType, topology::OutputDefinition};

/// A transform builder.
///
/// Transform builders are responsible for creating instances of [`Transform`]s, as well as describing high-level
/// aspects of the built transform, such as the data types allowed for input events and the outputs exposed by the
/// transform.
#[async_trait]
pub trait TransformBuilder: MemoryBounds {
    /// Data type allowed as input events to this transform.
    fn input_data_type(&self) -> DataType;

    /// Event outputs exposed by this transform.
    fn outputs(&self) -> &[OutputDefinition];

    /// Builds an instance of the transform.
    ///
    /// ## Errors
    ///
    /// If the transform cannot be built for any reason, an error is returned.
    async fn build(&self, context: ComponentContext) -> Result<Box<dyn Transform + Send>, GenericError>;
}

/// A synchronous transform builder.
///
/// Synchronous transforms are a special case of transforms that are executed synchronously, meaning that they do not
/// run as their own task. This is used for certain transforms, typically those where it is more efficient to run
/// multiple transformation steps in a single task.
#[async_trait]
pub trait SynchronousTransformBuilder: MemoryBounds {
    /// Builds an instance of the synchronous transform.
    ///
    /// ## Errors
    ///
    /// If the synchronous transform cannot be built for any reason, an error is returned.
    async fn build(&self) -> Result<Box<dyn SynchronousTransform + Send>, GenericError>;
}
