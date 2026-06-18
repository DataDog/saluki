use async_trait::async_trait;
use datadog_agent_commons::ipc::client::RemoteAgentClient;
use resource_accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_env::HostProvider;
use saluki_error::GenericError;
use tokio::sync::OnceCell;

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
    /// Creates a new `RemoteAgentHostProvider` from the host attachment's client.
    ///
    /// The client comes from the config-system's typed attachment bundle; this provider no longer
    /// builds its own client from raw configuration.
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
