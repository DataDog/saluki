//! Helpers for interacting with the Datadog Agent.

use std::time::Duration;

use backon::Retryable as _;
use datadog_protos::agent::v1::{RefreshRemoteAgentRequest, RegisterRemoteAgentRequest, RegisterRemoteAgentResponse};
use datadog_protos::agent::{
    AgentClient, AgentSecureClient, AutodiscoveryStreamResponse, ConfigEvent, ConfigStreamRequest, EntityId,
    FetchEntityRequest, HostTagReply, HostTagRequest, HostnameRequest, StreamTagsRequest, StreamTagsResponse,
    TagCardinality, WorkloadmetaEventType, WorkloadmetaFilter, WorkloadmetaKind, WorkloadmetaSource,
    WorkloadmetaStreamRequest, WorkloadmetaStreamResponse,
};
use saluki_config_tools::GenericConfiguration;
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use saluki_io::net::client::http::HttpsCapableConnectorBuilder;
use tonic::{
    service::interceptor::InterceptedService,
    transport::{Channel, Endpoint},
    Code, Request, Response,
};
use tracing::warn;

use crate::ipc::{config::RemoteAgentClientConfiguration, session::SessionId, tls::build_ipc_client_ipc_tls_config};

mod bearer_auth;
use self::bearer_auth::BearerAuthInterceptor;

mod streaming;
pub use self::streaming::StreamingResponse;

/// A client for interacting with the Datadog Agent's internal gRPC-based API.
#[derive(Clone)]
pub struct RemoteAgentClient {
    client: AgentClient<InterceptedService<Channel, BearerAuthInterceptor>>,
    secure_client: AgentSecureClient<InterceptedService<Channel, BearerAuthInterceptor>>,
}

impl RemoteAgentClient {
    /// Creates a new `RemoteAgentClient` from the given configuration.
    ///
    /// # Errors
    ///
    /// If the Agent gRPC client can't be created (invalid API endpoint, missing authentication token, etc), or if the
    /// authentication token is invalid, an error will be returned.
    pub async fn from_configuration(config: &GenericConfiguration) -> Result<Self, GenericError> {
        let config = RemoteAgentClientConfiguration::from_configuration(config)?;
        Self::from_client_configuration(config).await
    }

    /// Creates a new `RemoteAgentClient` from typed IPC client configuration.
    ///
    /// # Errors
    ///
    /// If the Agent gRPC client can't be created or authenticated, an error is returned.
    pub async fn from_client_configuration(config: RemoteAgentClientConfiguration) -> Result<Self, GenericError> {
        // TODO: We need to write a Tower middleware service that allows applying a backoff between failed calls,
        // specifically so that we can throttle reconnection attempts.
        //
        // When the remote Agent endpoint is not available -- Agent isn't running, etc -- the gRPC client will
        // essentially freewheel, trying to reconnect as quickly as possible, which spams the logs, wastes resources, so
        // on and so forth. We would want to essentially apply a backoff like any other client would for the RPC calls
        // themselves, but use it with the _connector_ instead.
        //
        // We could potentially just use a retry middleware, but Tonic does have its own reconnection logic, so we'd
        // have to test it out to make sure it behaves sensibly.
        let service_builder = || async {
            let auth_interceptor = BearerAuthInterceptor::from_file(&config.auth().auth_token_file_path()).await?;
            let ipc_cert_file_path = config.auth().ipc_cert_file_path();
            let client_tls_config = build_ipc_client_ipc_tls_config(ipc_cert_file_path).await?;
            let connector_builder = HttpsCapableConnectorBuilder::default();
            #[cfg(target_os = "linux")]
            let connector_builder = if let Some(addr) = config.vsock_addr()? {
                connector_builder.with_vsock_addr(addr)
            } else {
                connector_builder
            };
            let https_connector = connector_builder.build(client_tls_config)?;
            let endpoint = config.endpoint()?;
            let channel = Endpoint::from(endpoint.clone())
                .connect_timeout(Duration::from_secs(2))
                .connect_with_connector(https_connector)
                .await
                .with_error_context(|| format!("Failed to connect to Datadog Agent API at '{}'.", endpoint))?;

            Ok::<_, GenericError>(InterceptedService::new(channel, auth_interceptor))
        };

        let service = service_builder
            .retry(&config)
            .notify(|e, delay| {
                warn!(error = %e, "Failed to create Datadog Agent API client. Retrying in {:?}...", delay);
            })
            .await
            .error_context("Failed to create Datadog Agent API client.")?;

        let client = AgentClient::new(service.clone()).max_decoding_message_size(config.grpc_max_message_size());
        let mut secure_client =
            AgentSecureClient::new(service).max_decoding_message_size(config.grpc_max_message_size());

        // Try and do a basic health check to make sure we can connect and that our authentication token is valid.
        try_query_agent_api(&mut secure_client).await?;

        Ok(Self { client, secure_client })
    }

    /// Gets the detected hostname from the Agent.
    ///
    /// # Errors
    ///
    /// If there is an error querying the Agent API, an error will be returned.
    pub async fn get_hostname(&mut self) -> Result<String, GenericError> {
        let response = self
            .client
            .get_hostname(HostnameRequest {})
            .await
            .map(|r| r.into_inner())?;

        Ok(response.hostname)
    }

    /// Gets a stream of tagger entities at the given cardinality.
    ///
    /// If there is an error with the initial request, or an error occurs while streaming, the next message in the
    /// stream will be `Some(Err(status))`, where the status indicates the underlying error.
    pub fn get_tagger_stream(&mut self, cardinality: TagCardinality) -> StreamingResponse<StreamTagsResponse> {
        let mut client = self.secure_client.clone();
        StreamingResponse::from_response_future(async move {
            client
                .tagger_stream_entities(StreamTagsRequest {
                    cardinality: cardinality.into(),
                    ..Default::default()
                })
                .await
        })
    }

    /// Gets a stream of all workloadmeta entities.
    ///
    /// If there is an error with the initial request, or an error occurs while streaming, the next message in the
    /// stream will be `Some(Err(status))`, where the status indicates the underlying error.
    pub fn get_workloadmeta_stream(&mut self) -> StreamingResponse<WorkloadmetaStreamResponse> {
        let mut client = self.secure_client.clone();
        StreamingResponse::from_response_future(async move {
            client
                .workloadmeta_stream_entities(WorkloadmetaStreamRequest {
                    filter: Some(WorkloadmetaFilter {
                        kinds: vec![
                            WorkloadmetaKind::Container.into(),
                            WorkloadmetaKind::KubernetesPod.into(),
                            WorkloadmetaKind::EcsTask.into(),
                        ],
                        source: WorkloadmetaSource::All.into(),
                        event_type: WorkloadmetaEventType::EventTypeAll.into(),
                    }),
                })
                .await
        })
    }

    /// Registers a Remote Agent with the Agent.
    ///
    /// # Errors
    ///
    /// If there is an error sending the request to the Agent API, an error will be returned.
    pub async fn register_remote_agent_request(
        &mut self, pid: u32, display_name: &str, flavor: &str, api_endpoint: &str, services: Vec<String>,
    ) -> Result<Response<RegisterRemoteAgentResponse>, GenericError> {
        let mut client = self.secure_client.clone();
        let response = client
            .register_remote_agent(RegisterRemoteAgentRequest {
                pid: pid.to_string(),
                flavor: flavor.to_string(),
                display_name: display_name.to_string(),
                api_endpoint_uri: api_endpoint.to_string(),
                services,
            })
            .await?;
        Ok(response)
    }

    /// Refreshes the given remote agent session with the Agent.
    ///
    /// # Errors
    ///
    /// If there is an error sending the request to the Agent API, an error will be returned.
    pub async fn refresh_remote_agent_request(&mut self, session_id: &SessionId) -> Result<Response<()>, GenericError> {
        let mut client = self.secure_client.clone();
        let response = client
            .refresh_remote_agent(RefreshRemoteAgentRequest {
                session_id: session_id.to_string(),
            })
            .await?
            .map(|_| ());
        Ok(response)
    }

    /// Gets the host tags from the Agent.
    ///
    /// # Errors
    ///
    /// If there is an error querying the Agent API, an error will be returned.
    pub async fn get_host_tags(&self) -> Result<Response<HostTagReply>, GenericError> {
        let mut client = self.secure_client.clone();
        let response = client.get_host_tags(HostTagRequest {}).await?;
        Ok(response)
    }

    /// Gets a stream of autodiscovery config updates.
    ///
    /// If there is an error with the initial request, or an error occurs while streaming, the next message in the
    /// stream will be `Some(Err(status))`, where the status indicates the underlying error.
    pub fn get_autodiscovery_stream(&mut self) -> StreamingResponse<AutodiscoveryStreamResponse> {
        let mut client = self.secure_client.clone();
        StreamingResponse::from_response_future(async move { client.autodiscovery_stream_config(()).await })
    }

    /// Gets a stream of config events.
    ///
    /// If there is an error with the initial request, or an error occurs while streaming, the next message in the
    /// stream will be `Some(Err(status))`, where the status indicates the underlying error.
    pub fn stream_config_events(&mut self, session_id: &SessionId) -> StreamingResponse<ConfigEvent> {
        let mut client = self.secure_client.clone();
        let app_details = saluki_metadata::get_app_details();
        let formatted_full_name = app_details
            .full_name()
            .replace(" ", "-")
            .replace("_", "-")
            .to_lowercase();

        let mut request = Request::new(ConfigStreamRequest {
            name: formatted_full_name,
        });

        request
            .metadata_mut()
            .insert("session_id", session_id.to_grpc_header_value());

        StreamingResponse::from_response_future(async move { client.stream_config_events(request).await })
    }
}

async fn try_query_agent_api(
    client: &mut AgentSecureClient<InterceptedService<Channel, BearerAuthInterceptor>>,
) -> Result<(), GenericError> {
    let noop_fetch_request = FetchEntityRequest {
        id: Some(EntityId {
            prefix: "container_id".to_string(),
            uid: "nonexistent".to_string(),
        }),
        cardinality: TagCardinality::High.into(),
    };
    match client.tagger_fetch_entity(noop_fetch_request).await {
        Ok(_) => Ok(()),
        Err(e) => match e.code() {
            Code::Unauthenticated => Err(generic_error!(
                "Failed to authenticate to Datadog Agent API. Check that the configured authentication token is correct."
            )),
            _ => Err(e.into()),
        },
    }
}
