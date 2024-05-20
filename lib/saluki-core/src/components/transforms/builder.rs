use async_trait::async_trait;

use memory_accounting::MemoryBounds;
use saluki_error::GenericError;
use saluki_event::DataType;

use crate::topology::OutputDefinition;

use super::{SynchronousTransform, Transform};

#[async_trait]
pub trait TransformBuilder: MemoryBounds {
    fn input_data_type(&self) -> DataType;
    fn outputs(&self) -> &[OutputDefinition];

    async fn build(&self) -> Result<Box<dyn Transform + Send>, GenericError>;
}

#[async_trait]
pub trait SynchronousTransformBuilder: MemoryBounds {
    async fn build(&self) -> Result<Box<dyn SynchronousTransform + Send>, GenericError>;
}
