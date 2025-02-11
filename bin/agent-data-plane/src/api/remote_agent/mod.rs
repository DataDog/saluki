use std::time::Duration;
use std::{collections::HashMap, net::SocketAddr};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use datadog_protos::agent::{
    GetFlareFilesRequest, GetFlareFilesResponse, GetStatusDetailsRequest, GetStatusDetailsResponse, RemoteAgent,
    RemoteAgentServer, StatusSection,
};
use rand::{distributions::Alphanumeric, thread_rng, Rng};
use saluki_config::GenericConfiguration;
use saluki_env::helpers::remote_agent::RemoteAgentClient;
use saluki_error::GenericError;
use tokio::time::{interval, MissedTickBehavior};
use tracing::debug;
use uuid::Uuid;

/// Remote Agent helper configuration.
pub struct RemoteAgentHelperConfiguration {
    id: String,
    display_name: String,
    local_api_listen_addr: SocketAddr,
    client: RemoteAgentClient,
}

impl RemoteAgentHelperConfiguration {
    /// Creates a new `RemoteAgentHelperConfiguration` from the given configuration.
    pub async fn from_configuration(
        config: &GenericConfiguration, local_api_listen_addr: SocketAddr,
    ) -> Result<Self, GenericError> {
        let app_details = saluki_metadata::get_app_details();
        let formatted_full_name = app_details
            .full_name()
            .replace(" ", "-")
            .replace("_", "-")
            .to_lowercase();
        let client = RemoteAgentClient::from_configuration(config).await?;

        Ok(Self {
            id: format!("{}-{}", formatted_full_name, Uuid::now_v7()),
            display_name: formatted_full_name,
            local_api_listen_addr,
            client,
        })
    }

    /// Spawns the remote agent helper task.
    ///
    /// The spawned task ensures that this process is registered as a Remote Agent with the configured Datadog Agent
    /// instance. Additionally, an implementation of the `RemoteAgent` gRPC service is returned that must be installed
    /// on the API server that is listening at `local_api_listen_addr`.
    pub async fn spawn(self) -> RemoteAgentServer<RemoteAgentImpl> {
        let service_impl = RemoteAgentImpl { started: Utc::now() };
        let service = RemoteAgentServer::new(service_impl);

        tokio::spawn(run_remote_agent_helper(
            self.id,
            self.display_name,
            self.local_api_listen_addr,
            self.client,
        ));

        service
    }
}

async fn run_remote_agent_helper(
    id: String, display_name: String, local_api_listen_addr: SocketAddr, mut client: RemoteAgentClient,
) {
    let local_api_listen_addr = local_api_listen_addr.to_string();
    let auth_token: String = thread_rng()
        .sample_iter(&Alphanumeric)
        .take(64)
        .map(char::from)
        .collect();

    let mut register_agent = interval(Duration::from_secs(10));
    register_agent.set_missed_tick_behavior(MissedTickBehavior::Delay);

    debug!("Remote Agent helper started.");

    loop {
        register_agent.tick().await;
        match client
            .register_remote_agent_request(&id, &display_name, &local_api_listen_addr, &auth_token)
            .await
        {
            Ok(resp) => {
                let new_refresh_interval = resp.into_inner().recommended_refresh_interval_secs;
                register_agent.reset_after(Duration::from_secs(new_refresh_interval as u64));
                debug!("Refreshed registration with Datadog Agent");
            }
            Err(e) => {
                debug!("Failed to refresh registration with Datadog Agent: {}", e);
            }
        }
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
