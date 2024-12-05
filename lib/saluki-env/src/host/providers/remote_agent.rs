use async_trait::async_trait;
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_config::GenericConfiguration;
use saluki_error::GenericError;
use tokio::sync::OnceCell;

use crate::{helpers::remote_agent::RemoteAgentClient, HostProvider};

/// Datadog Agent-based host provider.
///
/// This provider is based on the internal gRPC API exposed by the Datadog Agent which exposes a way to get the hostname
/// as detected by the Datadog Agent.
#[derive(Clone)]
pub struct RemoteAgentHostProvider {
    client: RemoteAgentClient,
    cached_hostname: OnceCell<String>,
}

impl RemoteAgentHostProvider {
    /// Creates a new `RemoteAgentHostProvider` from the given configuration.
    ///
    /// # Errors
    ///
    /// If the Agent gRPC client cannot be created (invalid API endpoint, missing authentication token, etc), or if the
    /// authentication token is invalid, an error will be returned.
    pub async fn from_configuration(config: &GenericConfiguration) -> Result<Self, GenericError> {
        let client = RemoteAgentClient::from_configuration(config).await?;

        Ok(Self {
            client,
            cached_hostname: OnceCell::new(),
        })
    }

    async fn get_or_fetch_hostname(&self) -> Result<String, GenericError> {
        self.cached_hostname
            .get_or_try_init(|| {
                let mut client = self.client.clone();
                async move { client.get_hostname().await }
            })
            .await
            .cloned()
    }
}

#[async_trait]
impl HostProvider for RemoteAgentHostProvider {
    type Error = GenericError;

    async fn get_hostname(&self) -> Result<String, Self::Error> {
        self.get_or_fetch_hostname().await
    }
}

impl MemoryBounds for RemoteAgentHostProvider {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        builder.minimum().with_single_value::<Self>();
    }
}
