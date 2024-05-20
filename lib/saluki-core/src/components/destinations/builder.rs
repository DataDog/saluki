use async_trait::async_trait;

use memory_accounting::MemoryBounds;
use saluki_error::GenericError;
use saluki_event::DataType;

use super::Destination;

#[async_trait]
pub trait DestinationBuilder: MemoryBounds {
    fn input_data_type(&self) -> DataType;

    async fn build(&self) -> Result<Box<dyn Destination + Send>, GenericError>;
}
