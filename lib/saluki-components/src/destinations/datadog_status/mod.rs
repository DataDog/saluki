use async_trait::async_trait;
use http::Request;
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_config::GenericConfiguration;
use saluki_core::components::{
    destinations::{Destination, DestinationBuilder, DestinationContext},
    ComponentContext,
};
use saluki_env::helpers::remote_agent::RemoteAgentClient;
use saluki_error::GenericError;
use saluki_event::DataType;
use serde::Deserialize;

/// TODO
#[derive(Deserialize)]
pub struct DatadogStatusConfiguration {}

impl DatadogStatusConfiguration {
    /// Creates a new `DatadogEventsServieCheckConfiguration` from the given configuration.
    pub async fn from_configuration(config: &GenericConfiguration) -> Result<Self, GenericError> {
        let mut client = RemoteAgentClient::from_configuration(config).await?;
        client.register_remote_agent_request();
        Ok(config.as_typed()?)
    }
}

#[async_trait]
impl DestinationBuilder for DatadogStatusConfiguration {
    fn input_data_type(&self) -> DataType {
        DataType::EventD | DataType::ServiceCheck
    }

    async fn build(&self, _context: ComponentContext) -> Result<Box<dyn Destination + Send>, GenericError> {
        Ok(Box::new(DatadogStatus {}))
    }
}

impl MemoryBounds for DatadogStatusConfiguration {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        builder
            .minimum()
            // Capture the size of the heap allocation when the component is built.
            .with_single_value::<DatadogStatus>()
            // Capture the size of the requests channel.
            .with_array::<(usize, Request<String>)>(32);
    }
}

pub struct DatadogStatus {}

#[async_trait]
#[allow(unused)]
impl Destination for DatadogStatus {
    async fn run(mut self: Box<Self>, mut context: DestinationContext) -> Result<(), GenericError> {
        Ok(())
    }
}
