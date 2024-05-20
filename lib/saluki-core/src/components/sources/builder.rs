use async_trait::async_trait;
use memory_accounting::MemoryBounds;
use saluki_error::GenericError;

use crate::topology::OutputDefinition;

use super::Source;

#[async_trait]
pub trait SourceBuilder: MemoryBounds {
    fn outputs(&self) -> &[OutputDefinition];

    async fn build(&self) -> Result<Box<dyn Source + Send>, GenericError>;
}
