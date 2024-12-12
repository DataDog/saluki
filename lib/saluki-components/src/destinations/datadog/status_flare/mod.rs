use std::collections::HashMap;
use std::time::Duration;

use async_trait::async_trait;
use datadog_protos::agent::{
    GetFlareFilesRequest, GetFlareFilesResponse, GetStatusDetailsRequest, GetStatusDetailsResponse, RemoteAgent,
};
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder};
use rand::distributions::Alphanumeric;
use rand::{thread_rng, Rng};
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
use uuid::Uuid;

const DEFAULT_API_ENDPOINT: u64 = 5102;

/// Datadog Status and Flare Destination
///
/// Registers ADP as a remote agent to the Core Agent.
///
/// ## Missing
///
/// - grpc server to respond to Core Agent
#[derive(Deserialize)]
pub struct DatadogStatusFlareConfiguration {
    #[serde(skip)]
    id: String,

    #[serde(skip)]
    display_name: String,

    #[serde(default = "default_api_port", rename = "remote_agent_api_port")]
    api_port: u64,

    #[serde(skip)]
    client: Option<RemoteAgentClient>,
}

fn default_api_port() -> u64 {
    DEFAULT_API_ENDPOINT
}

impl DatadogStatusFlareConfiguration {
    /// Creates a new `DatadogStatusFlareConfiguration` from the given configuration.
    pub async fn from_configuration(config: &GenericConfiguration) -> Result<Self, GenericError> {
        let mut status_flare_configuration: DatadogStatusFlareConfiguration = config.as_typed()?;
        let app_details = saluki_metadata::get_app_details();

        let formatted_full_name = app_details
            .full_name()
            .replace(" ", "-")
            .replace("_", "-")
            .to_lowercase();

        let id = Uuid::now_v7();
        let agent_id = format!("{}-{}", formatted_full_name, id);
        status_flare_configuration.id = agent_id;
        status_flare_configuration.display_name = formatted_full_name;
        status_flare_configuration.client = Some(RemoteAgentClient::from_configuration(config).await?);
        Ok(status_flare_configuration)
    }
}

#[async_trait]
impl DestinationBuilder for DatadogStatusFlareConfiguration {
    fn input_data_type(&self) -> DataType {
        DataType::Metric | DataType::EventD | DataType::ServiceCheck
    }

    async fn build(&self, _context: ComponentContext) -> Result<Box<dyn Destination + Send>, GenericError> {
        Ok(Box::new(DatadogStatusFlare {
            id: self.id.clone(),
            display_name: self.display_name.clone(),
            api_port: self.api_port,
            client: self.client.clone().unwrap(),
        }))
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

    api_port: u64,

    client: RemoteAgentClient,
}

#[async_trait]
#[allow(unused)]
impl Destination for DatadogStatusFlare {
    async fn run(mut self: Box<Self>, mut context: DestinationContext) -> Result<(), GenericError> {
        let mut client = self.client.clone();
        tokio::spawn(async move {
            let auth_token: String = thread_rng()
                .sample_iter(&Alphanumeric)
                .take(64)
                .map(char::from)
                .collect();

            loop {
                match client
                    .register_remote_agent_request(&self.id, &self.display_name, self.api_port, &auth_token)
                    .await
                {
                    Ok(resp) => {
                        let refresh_interval = resp.into_inner().recommended_refresh_interval_secs;
                        tokio::time::sleep(Duration::from_secs(refresh_interval as u64)).await;
                        debug!("Refreshed registration with Core Agent");
                    }
                    Err(e) => {
                        debug!("Failed to refresh registration with Core Agent.");
                    }
                }
            }
        });

        /// TODO
        loop {
            tokio::time::sleep(Duration::from_secs(30)).await;
        }
        Ok(())
    }
}

#[allow(unused)]
struct RemoteAgentImpl;

#[async_trait]
impl RemoteAgent for RemoteAgentImpl {
    async fn get_status_details(
        &self, _request: tonic::Request<GetStatusDetailsRequest>,
    ) -> std::result::Result<tonic::Response<GetStatusDetailsResponse>, tonic::Status> {
        let response = GetStatusDetailsResponse {
            main_section: None,
            named_sections: HashMap::new(),
        };
        Ok(tonic::Response::new(response))
    }

    async fn get_flare_files(
        &self, _request: tonic::Request<GetFlareFilesRequest>,
    ) -> std::result::Result<tonic::Response<GetFlareFilesResponse>, tonic::Status> {
        let response = GetFlareFilesResponse {
            files: HashMap::default(),
        };
        Ok(tonic::Response::new(response))
    }
}
