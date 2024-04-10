use async_trait::async_trait;

use crate::topology::OutputDefinition;

use super::Source;

#[async_trait]
pub trait SourceBuilder {
    fn outputs(&self) -> &[OutputDefinition];

    async fn build(&self) -> Result<Box<dyn Source + Send>, Box<dyn std::error::Error + Send + Sync>>;
}
