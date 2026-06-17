use async_trait::async_trait;
use datadog_agent_commons::ipc::client::RemoteAgentClient;
use resource_accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_env::HostProvider;
use saluki_error::GenericError;
use tokio::sync::OnceCell;

/// Datadog Agent-based host provider.
#[derive(Clone)]
pub struct RemoteAgentHostProvider {
    client: RemoteAgentClient,
    cached_hostname: OnceCell<String>,
}

impl RemoteAgentHostProvider {
    /// Creates a host provider from a typed Datadog Agent IPC client.
    pub fn from_client(client: RemoteAgentClient) -> Self {
        Self {
            client,
            cached_hostname: OnceCell::new(),
        }
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
        builder.minimum().with_single_value::<Self>("component struct");
    }
}
