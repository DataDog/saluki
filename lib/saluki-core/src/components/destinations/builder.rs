use async_trait::async_trait;

use saluki_event::DataType;

use super::Destination;

#[async_trait]
pub trait DestinationBuilder {
    fn input_data_type(&self) -> DataType;

    async fn build(&self) -> Result<Box<dyn Destination + Send>, Box<dyn std::error::Error + Send + Sync>>;
}
