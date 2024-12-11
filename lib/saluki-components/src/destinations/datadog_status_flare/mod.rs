#[allow(unused)]
use std::collections::HashMap;

use async_trait::async_trait;
#[allow(unused)]
use datadog_protos::agent::{
    GetFlareFilesRequest, GetFlareFilesResponse, GetStatusDetailsRequest, GetStatusDetailsResponse, RemoteAgent,
};
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
use tracing::debug;

const DEFAULT_ID: &str = "agent-data-plane";
const DEFAULT_DISPLAY_NAME: &str = "Agent Data Plane";
const DEFAULT_API_ENDPOINT: &str = "localhost:13580";

/// TODO
/// wrapped client inside an `Option` to fix Deserialization problems
#[derive(Deserialize)]
pub struct DatadogStatusFlareConfiguration {
    #[serde(default = "default_id", rename = "remote_agent_id")]
    id: String,

    #[serde(default = "default_display_name", rename = "remote_agent_display_name")]
    display_name: String,

    #[serde(default = "default_api_endpoint", rename = "remote_agent_api_endpoint")]
    api_endpoint: String,

    #[serde(default = "default_auth_token", rename = "remote_agent_auth_token")]
    auth_token: String,

    #[serde(skip)]
    client: Option<RemoteAgentClient>,
}

fn default_id() -> String {
    DEFAULT_ID.to_owned()
}

fn default_display_name() -> String {
    DEFAULT_DISPLAY_NAME.to_owned()
}
fn default_api_endpoint() -> String {
    DEFAULT_API_ENDPOINT.to_owned()
}

fn default_auth_token() -> String {
    "TODO".to_string()
}

impl DatadogStatusFlareConfiguration {
    /// Creates a new `DatadogStatusConfiguration` from the given configuration.
    pub async fn from_configuration(config: &GenericConfiguration) -> Result<Self, GenericError> {
        // let client = RemoteAgentClient::from_configuration(config).await?;
        Ok(config.as_typed()?)
    }

    ///  TODO
    pub fn set_remote_agent_client(&mut self, client: RemoteAgentClient) {
        self.client = Some(client);
    }
}

#[async_trait]
impl DestinationBuilder for DatadogStatusFlareConfiguration {
    fn input_data_type(&self) -> DataType {
        DataType::Metric
    }

    async fn build(&self, _context: ComponentContext) -> Result<Box<dyn Destination + Send>, GenericError> {
        let mut client = self.client.clone().unwrap();

        match client
            .register_remote_agent_request(&self.id, &self.display_name, &self.api_endpoint, &self.auth_token)
            .await
        {
            Ok(_) => {
                debug!("Registered as remote agent to Datadog Agent.");
                Ok(Box::new(DatadogStatusFlare {
                    id: self.id.clone(),
                    display_name: self.display_name.clone(),
                    api_endpoint: self.api_endpoint.clone(),
                    auth_token: self.auth_token.clone(),
                    client,
                }))
            }
            Err(e) => {
                debug!("Failed to register remote agent to Datadog Agent.");
                Err(e)
            }
        }
    }
}

impl MemoryBounds for DatadogStatusFlareConfiguration {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        builder
            .minimum()
            // Capture the size of the heap allocation when the component is built.
            .with_single_value::<DatadogStatusFlare>();
    }
}

pub struct DatadogStatusFlare {
    id: String,

    display_name: String,

    api_endpoint: String,

    auth_token: String,

    client: RemoteAgentClient,
}

#[async_trait]
#[allow(unused)]
impl Destination for DatadogStatusFlare {
    async fn run(mut self: Box<Self>, mut context: DestinationContext) -> Result<(), GenericError> {
        let mut client = self.client.clone();
        tokio::spawn(async move {
            match client
                .register_remote_agent_request(&self.id, &self.display_name, &self.api_endpoint, &self.auth_token)
                .await
            {
                Ok(resp) => {
                    let refresh_interval = resp.into_inner().recommended_refresh_interval_secs;
                    debug!("Refreshed registration with Core Agent");
                }
                Err(e) => {
                    debug!("Failed to refresh registration with Core Agent.");
                }
            }
        });
        loop {}
        Ok(())
    }
}

// #[allow(unused)]
// struct RemoteAgentImpl;

// #[async_trait]
// impl RemoteAgent for RemoteAgentImpl {
//     async fn get_status_details(
//         &self, _request: tonic::Request<GetStatusDetailsRequest>,
//     ) -> std::result::Result<GetStatusDetailsResponse, tonic::Status> {
//         let response = GetStatusDetailsResponse {
//             main_section: None,
//             named_sections: HashMap::new(),
//         };
//         Ok(response)
//     }

//     async fn get_flare_files(
//         &self, _request: tonic::Request<GetFlareFilesRequest>,
//     ) -> std::result::Result<tonic::Response<GetFlareFilesResponse>, tonic::Status> {
//         let response = GetFlareFilesResponse {
//             files: HashMap::default(),
//         };
//         Ok(tonic::Response::new(response))
//     }
// }
