//! The worker's one dependency on the Agent, separated so that tests can replace the Agent.

use async_trait::async_trait;
use datadog_agent_commons::ipc::client::RemoteAgentClient;
use datadog_protos::remote_config::{ClientGetConfigsRequest, ClientGetConfigsResponse};
use saluki_error::GenericError;
use tonic::{Code, Status};

/// Answers the worker's configuration polls.
///
/// The production implementation forwards to the Agent's `ClientGetConfigs` call. The crate's own tests implement it
/// with a scripted fake that returns prepared responses and errors in order and records every request, so the worker's
/// polling, integrity checks, status reporting, and backoff can be tested without an Agent, and under a paused clock.
///
/// This trait is private. Subscribers never see it: [`RemoteConfigurationClient::new`] takes a [`RemoteAgentClient`],
/// and the worker holds the source as a `Box<dyn RcAgent>` so that no public type gains a type parameter.
///
/// [`RemoteConfigurationClient::new`]: crate::RemoteConfigurationClient::new
#[async_trait]
pub(crate) trait RcAgent: Send + 'static {
    /// Sends one poll and returns the Agent's response.
    async fn get_configs(&mut self, request: ClientGetConfigsRequest) -> Result<ClientGetConfigsResponse, FetchError>;
}

/// Why a poll produced no response.
///
/// The worker's reaction depends only on which of these occurred, so the gRPC status codes stay inside the production
/// [`RcAgent`].
pub(crate) enum FetchError {
    /// The Agent does not support Remote Configuration or this RPC; the worker waits `max_backoff` between attempts.
    Unimplemented(GenericError),

    /// Another RPC failure; the worker retries with backoff.
    Rpc(GenericError),
}

impl From<Status> for FetchError {
    fn from(status: Status) -> Self {
        match status.code() {
            Code::Unimplemented => Self::Unimplemented(status.into()),
            _ => Self::Rpc(status.into()),
        }
    }
}

#[async_trait]
impl RcAgent for RemoteAgentClient {
    async fn get_configs(&mut self, request: ClientGetConfigsRequest) -> Result<ClientGetConfigsResponse, FetchError> {
        self.client_get_configs(request).await.map_err(FetchError::from)
    }
}
