use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use datadog_protos::agent::{
    GetFlareFilesRequest, GetFlareFilesResponse, GetStatusDetailsRequest, GetStatusDetailsResponse, RemoteAgent,
    RemoteAgentServer, StatusSection,
};
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder};
use rand::{distributions::Alphanumeric, thread_rng, Rng};
use saluki_config::GenericConfiguration;
use saluki_core::components::{
    destinations::{Destination, DestinationBuilder, DestinationContext},
    ComponentContext,
};
use saluki_env::helpers::remote_agent::RemoteAgentClient;
use saluki_error::GenericError;
use saluki_event::DataType;
use tokio::select;
use tokio::time::{interval, MissedTickBehavior};
use tracing::debug;
use uuid::Uuid;

// TODO: This should really come from the binary itself, since we don't actually control the server where our
// `RemoteAgent` gRPC service is exposed from... but it would be very clunky to pass that around so we're just aligning
// the default port here _for now_.
const DEFAULT_API_LISTEN_PORT: NonZeroUsize = unsafe { NonZeroUsize::new_unchecked(5101) };

/// Datadog Status and Flare Destination
///
/// Registers ADP as a remote agent to the Core Agent.
pub struct DatadogStatusFlareConfiguration {
    id: String,
    display_name: String,
    api_listen_port: NonZeroUsize,
    client: RemoteAgentClient,
}

impl DatadogStatusFlareConfiguration {
    /// Creates a new `DatadogStatusFlareConfiguration` from the given configuration.
    pub async fn from_configuration(config: &GenericConfiguration) -> Result<Self, GenericError> {
        let app_details = saluki_metadata::get_app_details();
        let formatted_full_name = app_details
            .full_name()
            .replace(" ", "-")
            .replace("_", "-")
            .to_lowercase();
        let api_listen_port = config
            .try_get_typed::<NonZeroUsize>("remote_agent_api_listen_port")?
            .unwrap_or(DEFAULT_API_LISTEN_PORT);
        let client = RemoteAgentClient::from_configuration(config).await?;

        Ok(Self {
            id: format!("{}-{}", formatted_full_name, Uuid::now_v7()),
            display_name: formatted_full_name,
            api_listen_port,
            client,
        })
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
            api_listen_port: self.api_listen_port,
            client: self.client.clone(),
        }))
    }
}

impl MemoryBounds for DatadogStatusFlareConfiguration {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        builder
            .minimum()
            // Capture the size of the heap allocation when the component is built.
            .with_single_value::<DatadogStatusFlare>("component struct");
    }
}

pub struct DatadogStatusFlare {
    id: String,
    display_name: String,
    api_listen_port: NonZeroUsize,
    client: RemoteAgentClient,
}

#[async_trait]
impl Destination for DatadogStatusFlare {
    async fn run(mut self: Box<Self>, mut context: DestinationContext) -> Result<(), GenericError> {
        let Self {
            id,
            display_name,
            api_listen_port,
            mut client,
        } = *self;

        let api_endpoint = format!("127.0.0.1:{}", api_listen_port);
        let auth_token: String = thread_rng()
            .sample_iter(&Alphanumeric)
            .take(64)
            .map(char::from)
            .collect();

        let mut register_agent = interval(Duration::from_secs(10));
        register_agent.set_missed_tick_behavior(MissedTickBehavior::Delay);

        let mut health = context.take_health_handle();
        health.mark_ready();
        debug!("Datadog Status and Flare destination started.");

        loop {
            select! {
                _ = health.live() => continue,

                result = context.events().next() => match result {
                    Some(_events) => {
                    },
                    None => break,
                },

                // Time to (re)register with the Core Agent.
                //
                // TODO: Consider spawning the registration as a task so that the component can keep polling and not
                // slow down the accepting of events and responding of health checks.
                _ = register_agent.tick() => {
                    match client.register_remote_agent_request(&id, &display_name, &api_endpoint, &auth_token).await {
                        Ok(resp) => {
                            let new_refresh_interval = resp.into_inner().recommended_refresh_interval_secs;
                            register_agent.reset_after(Duration::from_secs(new_refresh_interval as u64));
                            debug!("Refreshed registration with Core Agent");
                        }
                        Err(e) => {
                            debug!("Failed to refresh registration with Core Agent: {}", e);
                        }
                    }
                }
            }
        }

        debug!("Datadog Status Flare destination stopped.");
        Ok(())
    }
}

#[derive(Default)]
pub struct RemoteAgentImpl {
    started: DateTime<Utc>,
}

#[async_trait]
impl RemoteAgent for RemoteAgentImpl {
    async fn get_status_details(
        &self, _request: tonic::Request<GetStatusDetailsRequest>,
    ) -> Result<tonic::Response<GetStatusDetailsResponse>, tonic::Status> {
        let mut status_fields = HashMap::new();
        status_fields.insert("Started".to_string(), self.started.to_rfc3339());
        let response = GetStatusDetailsResponse {
            main_section: Some(StatusSection { fields: status_fields }),
            named_sections: HashMap::new(),
        };
        Ok(tonic::Response::new(response))
    }

    async fn get_flare_files(
        &self, _request: tonic::Request<GetFlareFilesRequest>,
    ) -> Result<tonic::Response<GetFlareFilesResponse>, tonic::Status> {
        let response = GetFlareFilesResponse {
            files: HashMap::default(),
        };
        Ok(tonic::Response::new(response))
    }
}

/// Create the RemoteAgent service.
pub fn new_remote_agent_service() -> RemoteAgentServer<RemoteAgentImpl> {
    let remote_agent = RemoteAgentImpl { started: Utc::now() };
    RemoteAgentServer::new(remote_agent)
}
