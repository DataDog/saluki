use async_trait::async_trait;

use saluki_event::DataType;

use crate::topology::OutputDefinition;

use super::{SynchronousTransform, Transform};

#[async_trait]
pub trait TransformBuilder {
    fn input_data_type(&self) -> DataType;
    fn outputs(&self) -> &[OutputDefinition];

    async fn build(&self) -> Result<Box<dyn Transform + Send>, Box<dyn std::error::Error + Send + Sync>>;
}

#[async_trait]
pub trait SynchronousTransformBuilder {
    async fn build(&self) -> Result<Box<dyn SynchronousTransform + Send>, Box<dyn std::error::Error + Send + Sync>>;
}
