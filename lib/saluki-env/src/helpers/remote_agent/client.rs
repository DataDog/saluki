use std::{
    future::Future,
    path::PathBuf,
    pin::Pin,
    task::{ready, Context, Poll},
    time::Duration,
};

use backon::{BackoffBuilder, ConstantBuilder, Retryable as _};
use datadog_protos::agent::v1::{
    RefreshRemoteAgentRequest, RefreshRemoteAgentResponse, RegisterRemoteAgentRequest, RegisterRemoteAgentResponse,
};
use datadog_protos::agent::{
    AgentClient, AgentSecureClient, AutodiscoveryStreamResponse, ConfigEvent, ConfigStreamRequest, EntityId,
    FetchEntityRequest, HostTagReply, HostTagRequest, HostnameRequest, StreamTagsRequest, StreamTagsResponse,
    TagCardinality, WorkloadmetaEventType, WorkloadmetaFilter, WorkloadmetaKind, WorkloadmetaSource,
    WorkloadmetaStreamRequest, WorkloadmetaStreamResponse,
};
use futures::Stream;
use pin_project_lite::pin_project;
use saluki_config::GenericConfiguration;
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use saluki_io::net::{build_datadog_agent_ipc_https_connector, get_ipc_cert_file_path};
use serde::Deserialize;
use tonic::{
    service::interceptor::InterceptedService,
    transport::{Channel, Endpoint, Uri},
    Code, Response, Status, Streaming,
};
use tracing::warn;

use crate::helpers::tonic::BearerAuthInterceptor;

fn default_agent_ipc_endpoint() -> Uri {
    Uri::from_static("https://127.0.0.1:5001")
}

fn default_agent_auth_token_file_path() -> PathBuf {
    PathBuf::from("/etc/datadog-agent/auth_token")
}

const fn default_connect_retry_attempts() -> usize {
    10
}

const fn default_connect_retry_backoff() -> Duration {
    Duration::from_secs(2)
}

#[derive(Deserialize)]
struct RemoteAgentClientConfiguration {
    /// Datadog Agent IPC endpoint to connect to.
    ///
    /// This is generally based on the configured `cmd_port` for the Datadog Agent, and must expose the `AgentSecure`
    /// gRPC service.
    ///
    /// Defaults to `https://127.0.0.1:5001`.
    #[serde(
        rename = "agent_ipc_endpoint",
        with = "http_serde_ext::uri",
        default = "default_agent_ipc_endpoint"
    )]
    ipc_endpoint: Uri,

    /// Path to the Agent authentication token file.
    ///
    /// The contents of the file are passed as a bearer token in RPC requests to the IPC endpoint.
    ///
    /// Defaults to `/etc/datadog-agent/auth_token`.
    #[serde(default = "default_agent_auth_token_file_path")]
    auth_token_file_path: PathBuf,

    /// Path to the Agent IPC TLS certificate file.
    ///
    /// The file is expected to be PEM-encoded, containing both a certificate and private key. The certificate will be
    /// used to verify the TLS server certificate presented by the Agent, and the certificate and private key will be
    /// used together to provide client authentication _to_ the Agent.
    ///
    /// Defaults to `ipc_cert.pem` in the same directory as the Agent authentication token file. (e.g., if
    /// `auth_token_file_path` is `/etc/datadog-agent/auth_token`, this will be `/etc/datadog-agent/ipc_cert.pem`.)
    #[serde(default)]
    ipc_cert_file_path: Option<PathBuf>,

    /// Number of allowed retry attempts when initially connecting.
    ///
    /// Defaults to `10`.
    #[serde(default = "default_connect_retry_attempts")]
    connect_retry_attempts: usize,

    /// Amount of time to wait between connection attempts when initially connecting.
    ///
    /// Defaults to 2 seconds.
    #[serde(default = "default_connect_retry_backoff")]
    connect_retry_backoff: Duration,
}

impl BackoffBuilder for &RemoteAgentClientConfiguration {
    type Backoff = <ConstantBuilder as BackoffBuilder>::Backoff;

    fn build(self) -> Self::Backoff {
        ConstantBuilder::default()
            .with_delay(self.connect_retry_backoff)
            .with_max_times(self.connect_retry_attempts)
            .build()
    }
}

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
    /// If the Agent gRPC client cannot be created (invalid API endpoint, missing authentication token, etc), or if the
    /// authentication token is invalid, an error will be returned.
    pub async fn from_configuration(config: &GenericConfiguration) -> Result<Self, GenericError> {
        let mut config = config
            .as_typed::<RemoteAgentClientConfiguration>()
            .error_context("Failed to parse configuration for Remote Agent client.")?;

        // The core-agent defaults to the empty string for the auth_token_file_path and ipc_cert_file_path. We need to handle this by resetting to the defaults.
        if config.auth_token_file_path.as_os_str().is_empty() {
            config.auth_token_file_path = default_agent_auth_token_file_path();
        }
        if let Some(ref cert_path) = config.ipc_cert_file_path {
            if cert_path.as_os_str().is_empty() {
                config.ipc_cert_file_path = None;
            }
        }

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
            let auth_interceptor = BearerAuthInterceptor::from_file(&config.auth_token_file_path).await?;
            let ipc_cert_file_path =
                get_ipc_cert_file_path(config.ipc_cert_file_path.as_ref(), &config.auth_token_file_path);
            let https_connector = build_datadog_agent_ipc_https_connector(ipc_cert_file_path).await?;
            let channel = Endpoint::from(config.ipc_endpoint.clone())
                .connect_timeout(Duration::from_secs(2))
                .connect_with_connector(https_connector)
                .await
                .with_error_context(|| {
                    format!("Failed to connect to Datadog Agent API at '{}'.", config.ipc_endpoint)
                })?;

            Ok::<_, GenericError>(InterceptedService::new(channel, auth_interceptor))
        };

        let service = service_builder
            .retry(&config)
            .notify(|e, delay| {
                warn!(error = %e, "Failed to create Datadog Agent API client. Retrying in {:?}...", delay);
            })
            .await
            .error_context("Failed to create Datadog Agent API client.")?;

        let client = AgentClient::new(service.clone());
        let mut secure_client = AgentSecureClient::new(service);

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
        &mut self, pid: u32, display_name: &str, api_endpoint: &str, services: Vec<String>,
    ) -> Result<Response<RegisterRemoteAgentResponse>, GenericError> {
        let mut client = self.secure_client.clone();
        let response = client
            .register_remote_agent(RegisterRemoteAgentRequest {
                pid: pid.to_string(),
                flavor: "saluki".to_string(),
                display_name: display_name.to_string(),
                api_endpoint_uri: api_endpoint.to_string(),
                services,
            })
            .await?;
        Ok(response)
    }

    /// Refreshes a Remote Agent with the Agent.
    ///
    /// If there is an error sending the request to the Agent API, an error will be returned.
    pub async fn refresh_remote_agent_request(
        &mut self, session_id: &str,
    ) -> Result<Response<RefreshRemoteAgentResponse>, GenericError> {
        let mut client = self.secure_client.clone();
        let response = client
            .refresh_remote_agent(RefreshRemoteAgentRequest {
                session_id: session_id.to_string(),
            })
            .await?;
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
    pub fn stream_config_events(&mut self) -> StreamingResponse<ConfigEvent> {
        let mut client = self.secure_client.clone();
        let app_details = saluki_metadata::get_app_details();
        let formatted_full_name = app_details
            .full_name()
            .replace(" ", "-")
            .replace("_", "-")
            .to_lowercase();
        StreamingResponse::from_response_future(async move {
            client
                .stream_config_events(ConfigStreamRequest {
                    name: formatted_full_name,
                })
                .await
        })
    }
}

pin_project! {
    /// A streaming gRPC response.
    ///
    /// Compared to the normal streaming response type from [`tonic`], `StreamingResponse` handles a special case where
    /// servers may not send an initial message that allows the RPC to "establish", which is required to create the
    /// `Streaming` object that can then be polled. This leads to an issue where calls can effectively appear to block
    /// until the first message is sent by the server, which is suboptimal.
    ///
    /// `StreamingResponse` exposes a unified [`Stream`] implementation that encompasses both the initial RPC
    /// establishment and subsequent messages sent by the server, to allow for a more seamless experience when working
    /// with streaming RPCs.
    #[project = StreamingResponseProj]
    pub enum StreamingResponse<T> {
        /// Waiting for the server to stream the first message.
        Initial { inner: Pin<Box<dyn Future<Output = Result<Response<Streaming<T>>, Status>> + Send>> },

        /// Waiting for the server to stream the next message.
        Streaming { #[pin] stream: Streaming<T> },
    }
}

impl<T> StreamingResponse<T> {
    fn from_response_future<F>(fut: F) -> Self
    where
        F: Future<Output = Result<Response<Streaming<T>>, Status>> + Send + 'static,
    {
        Self::Initial { inner: Box::pin(fut) }
    }
}

impl<T> Stream for StreamingResponse<T> {
    type Item = Result<T, Status>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            let this = self.as_mut().project();
            let new_state = match this {
                // When we get the initial response, we either get the streaming object or an error.
                //
                // The streaming object itself has to be polled to get an actual message, so we have to do a little
                // dance here to update our state to the `Streaming` variant when that happens, and then loop so we can
                // poll the streaming object for a message... but if we got an error, we just yield it like a normal
                // item on the stream.
                StreamingResponseProj::Initial { inner } => match ready!(inner.as_mut().poll(cx)) {
                    Ok(response) => {
                        let stream = response.into_inner();
                        StreamingResponse::Streaming { stream }
                    }
                    Err(status) => return Poll::Ready(Some(Err(status))),
                },
                StreamingResponseProj::Streaming { stream } => return stream.poll_next(cx),
            };

            self.set(new_state);
        }
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
